package com.kairo.vistrel.capture;

//? if gpu_device {
import com.kairo.vistrel.Constants;
import com.kairo.vistrel.streaming.EncoderChannel;
import com.kairo.vistrel.capture.BlitPipelines.Nv12Blit;
import com.mojang.blaze3d.pipeline.RenderTarget;
import com.mojang.blaze3d.systems.RenderSystem;
//? if renderpearl_era {
import com.mojang.blaze3d.pipeline.PipelineCache;
import com.mojang.renderpearl.api.GpuFormat;
//? } elif vulkan_backend {
/*import com.mojang.renderpearl.api.GpuFormat;*/
//? } else {
/*import com.mojang.blaze3d.textures.TextureFormat;*/
//? }
//? if !vulkan_backend {
/*import org.lwjgl.opengl.GL11;*/
//? }
import com.mojang.renderpearl.api.buffers.GpuBuffer;
import com.mojang.renderpearl.api.buffers.GpuBufferSlice;
import com.mojang.renderpearl.api.commands.CommandEncoder;
import com.mojang.renderpearl.api.commands.RenderPass;
import com.mojang.renderpearl.api.device.GpuDevice;
import com.mojang.renderpearl.api.pipeline.RenderPipeline;
import com.mojang.renderpearl.api.textures.FilterMode;
import com.mojang.renderpearl.api.textures.GpuTexture;
import com.mojang.renderpearl.api.textures.GpuTextureView;
import net.minecraft.client.Minecraft;
import java.nio.ByteBuffer;
import java.util.Optional;

/**
 * 基于 Mojang 设备抽象 API (GpuDevice) 的双缓冲 (Ping-Pong) 异步 DMA 内存回读管道（26.1+，跨 blaze3d/renderpearl 两代）
 * 源码恒以 26.3 renderpearl 形态落笔，26.1/26.2 视图由 replacements 表换装为 blaze3d 限定名
 * RAII：构造即初始化双 PBO 与 Blit 纹理，捕获、投递与缓冲区映射的生命周期都在本类内管理。
 * 线程归属：capture 与 close 均在渲染线程执行（stop 会把拆除投递到渲染线程）
 *
 * <p>
 * 自建 NV12 管线的 GLSL 以字符串内联（见 {@link BlitPipelines}），经设备源码注入接口编译，
 * 与资源重载状态完全解耦，无启动窗口限制。
 */
public class RenderpearlRamPipeline implements VideoPipeline {
    private final EncoderChannel channel;
    private final GpuDevice device;
    private final CommandEncoder encoder;
    private final GpuBuffer[] pboBuffers = new GpuBuffer[2];
    // 两张附件：全分辨率亮度平面、色度平面（4:2:0 采样，承载面因后端而异）
    private final GpuTexture lumaTexture;
    private final GpuTextureView lumaView;
    private final GpuTexture chromaTexture;
    private final GpuTextureView chromaView;
    private final int width;
    private final int height;
    /**
     * 构造期或首个采集帧判定不可用（无设备 / 奇数边长 / 管线编不过）：本次会话不采集
     */
    private boolean disabled;
    /**
     * 管线编译失败闩：失败即锁定本次会话不采集，不逐帧重试（仅渲染线程读写）
     */
    private boolean pipelineCompileFailed;
    //? if renderpearl_era {
    /**
     * 自建管线专属编译缓存：以内嵌源按需编译，与 vanilla 资源驱动的管线缓存隔离
     */
    private PipelineCache blitCache;
    //? } else {
    /*// 26.1/26.2 逐帧预热设备管线缓存用的内嵌源
    private InlineShaderSource inlineSource;*/
    //? }
    private int currentBufferIndex = 0;
    private boolean hasPreviousFrame = false;

    /**
     * 构造即初始化双 PBO 与 NV12 两平面附件（RAII），须在渲染线程执行。
     * 无可用 GpuDevice 或奇数边长时置为禁用态：capture 恒 {@code Dropped}，
     * 本次会话不采集，构造不抛异常
     *
     * @param channel 系统帧投递通道
     * @param width   采集宽度（NV12 平面要求偶数）
     * @param height  采集高度（NV12 平面要求偶数）
     */
    public RenderpearlRamPipeline(EncoderChannel channel, int width, int height) {
        this.channel = channel;
        this.width = width;
        this.height = height;
        this.device = resolveDevice();
        // 主机帧只能是 NV12（编码侧唯一接受的布局）。
        boolean usable = device != null;
        if (device == null) {
            Constants.LOG.warn("RenderpearlRamPipeline 构造时无可用 GpuDevice，本次会话不采集");
        } else if ((width & 1) != 0 || (height & 1) != 0) {
            usable = false;
            Constants.LOG.error("采集尺寸 {}x{} 非偶数边长，构不出 NV12 平面，本次会话不采集", width, height);
        }

        this.disabled = !usable;
        if (!usable) {
            this.encoder = null;
            this.lumaTexture = null;
            this.lumaView = null;
            this.chromaTexture = null;
            this.chromaView = null;
            return;
        }

        long bufferSize = (long) width * height + (long) (width / 2) * (height / 2) * 2;
        int bufferUsage = GpuBuffer.USAGE_COPY_DST | GpuBuffer.USAGE_MAP_READ;
        pboBuffers[0] = device.createBuffer(() -> "Vistrel Capture Buffer 0", bufferUsage, bufferSize);
        pboBuffers[1] = device.createBuffer(() -> "Vistrel Capture Buffer 1", bufferUsage, bufferSize);

        int textureUsage = GpuTexture.USAGE_RENDER_ATTACHMENT | GpuTexture.USAGE_COPY_SRC;
        this.lumaTexture = device.createTexture(
                "Vistrel Luma Frame",
                textureUsage,
                GpuFormat.R8_UNORM,
                width,
                height,
                1,
                1
        );
        this.lumaView = device.createTextureView(this.lumaTexture);
        //? if vulkan_backend {
        this.chromaTexture = device.createTexture(
                "Vistrel Chroma Frame",
                textureUsage,
                GpuFormat.RG8_UNORM,
                width / 2,
                height / 2,
                1,
                1
        );
        //? } else {
        /*// 26.1 缺 RG8：色度平面以宽翻倍的 R8 交错面承载（width × height/2），
        // 着色器按目标列奇偶交错写 Cb/Cr（NV12 的 UV 交错序），整面拷贝回读恰为
        // NV12 色度面：PBO 与编码侧契约保持 NV12 布局不变
        this.chromaTexture = device.createTexture(
                "Vistrel Chroma Frame",
                textureUsage,
                GpuFormat.R8_UNORM,
                width,
                height / 2,
                1,
                1
        );*/
        //? }
        this.chromaView = device.createTextureView(this.chromaTexture);
        this.encoder = device.createCommandEncoder();
        // 专属编译缓存与内嵌源：自建管线以内嵌 GLSL 编译，不依赖资源重载
        //? if renderpearl_era {
        this.blitCache = new PipelineCache(
                this.device,
                new InlineShaderSource(Minecraft.getInstance().getResourceManager())
        );
        //? } else {
        /*this.inlineSource = new InlineShaderSource(Minecraft.getInstance().getResourceManager());*/
        //? }
    }

    /**
     * 逐帧预热自建管线编译：命中即查表；26.1/26.2 的设备缓存会被资源重载清空，
     * 逐帧补种自愈。失败即锁定本次会话不采集，不逐帧重试
     */
    private boolean ensurePipelines() {
        if (pipelineCompileFailed) {
            return false;
        }
        try {
            //? if renderpearl_era {
            if (this.blitCache.get(Nv12Blit.LUMA) == null || this.blitCache.get(Nv12Blit.CHROMA) == null) {
                throw new IllegalStateException("管线编译返回空结果");
            }
            //? } else {
            /*if (!RenderSystem.getDevice().precompilePipeline(Nv12Blit.LUMA, inlineSource).isValid()
                    || !RenderSystem.getDevice().precompilePipeline(Nv12Blit.CHROMA, inlineSource).isValid()) {
                throw new IllegalStateException("管线预编译返回无效状态");
            }*/
            //? }
            return true;
        } catch (Throwable t) {
            pipelineCompileFailed = true;
            Constants.LOG.error("NV12 采集管线不可用，本次会话不采集: {}", t.getMessage());
            return false;
        }
    }

    /**
     * 取当前 GpuDevice；启动早期 `tryGetDevice` 会抛，退到 `getDevice` 再试一次
     */
    private static GpuDevice resolveDevice() {
        try {
            return RenderSystem.tryGetDevice();
        } catch (Throwable ignored) {
            try {
                return RenderSystem.getDevice();
            } catch (Throwable ignored2) {
                return null;
            }
        }
    }

    @Override
    public CaptureOutcome capture() {
        long startNanos = CaptureProfiler.begin();
        try {
            return captureInternal();
        } finally {
            CaptureProfiler.end(CaptureProfiler.Stage.RAM_TOTAL, startNanos);
        }
    }

    private CaptureOutcome captureInternal() {
        if (disabled || encoder == null || channel == null) {
            return CaptureOutcome.Dropped;
        }
        long prepStart = CaptureProfiler.begin();
        if (!ensurePipelines()) {
            CaptureProfiler.end(CaptureProfiler.Stage.RAM_PREP, prepStart);
            return CaptureOutcome.Dropped;
        }

        Minecraft mc = Minecraft.getInstance();
        GpuTextureView mainColorView = null;
        if (mc.gameRenderer != null) {
            //? if >=26.2 {
            RenderTarget mainTarget = mc.gameRenderer.mainRenderTarget();
            //? } else {
            /*RenderTarget mainTarget = mc.getMainRenderTarget();*/
            //? }
            mainColorView = mainTarget == null ? null : mainTarget.getColorTextureView();
        }
        if (mainColorView == null) {
            CaptureProfiler.end(CaptureProfiler.Stage.RAM_PREP, prepStart);
            return CaptureOutcome.Failed;
        }

        int writeIdx = currentBufferIndex;
        GpuBuffer writeBuffer = pboBuffers[writeIdx];
        if (writeBuffer == null || writeBuffer.isClosed()) {
            CaptureProfiler.end(CaptureProfiler.Stage.RAM_PREP, prepStart);
            return CaptureOutcome.Failed;
        }
        CaptureProfiler.end(CaptureProfiler.Stage.RAM_PREP, prepStart);

        try {
            // 1. 两趟自建 pass 分别产出亮度平面与色度平面
            long stageStart = CaptureProfiler.begin();
            blitInto(this.lumaView, Nv12Blit.LUMA, mainColorView);
            CaptureProfiler.end(CaptureProfiler.Stage.RAM_BLIT_Y, stageStart);
            stageStart = CaptureProfiler.begin();
            blitInto(this.chromaView, Nv12Blit.CHROMA, mainColorView);
            CaptureProfiler.end(CaptureProfiler.Stage.RAM_BLIT_UV, stageStart);
            // 2. 两个平面按 NV12 布局写进同一块 PBO：色度紧跟亮度之后
            long copyStart = CaptureProfiler.begin();
            //? if vulkan_backend {
            this.encoder.copyTextureToBuffer(this.lumaTexture, writeBuffer, 0L, () -> {}, 0);
            this.encoder.copyTextureToBuffer(this.chromaTexture, writeBuffer, (long) width * height, () -> {}, 0);
            CaptureProfiler.end(CaptureProfiler.Stage.RAM_COPY, copyStart);
            // 26.2+ 为命令缓冲语义，显式提交；26.1 的 GL 后端即时执行，无提交概念
            long submitStart = CaptureProfiler.begin();
            this.encoder.submit();
            CaptureProfiler.end(CaptureProfiler.Stage.RAM_SUBMIT, submitStart);
            //? } else {
            /*// 26.1 的 GL 后端不管理 GL_PACK_ALIGNMENT（copyTextureToBuffer 只设 PACK_ROW_LENGTH），
            // 环境值停留在 GL 默认 4：R8 平面行宽非 4 倍数时 glReadPixels 逐行填充，既溢出 PBO
            // 又在行间垫隙破坏 NV12 紧凑布局。拷贝期间收紧为 1（与 26.2+ 按 byteAlignment 设
            // PACK_ALIGNMENT 的语义一致），结束后恢复查询到的原值
            int prevPackAlignment = GL11.glGetInteger(GL11.GL_PACK_ALIGNMENT);
            GL11.glPixelStorei(GL11.GL_PACK_ALIGNMENT, 1);
            try {
                this.encoder.copyTextureToBuffer(this.lumaTexture, writeBuffer, 0L, () -> {}, 0);
                this.encoder.copyTextureToBuffer(this.chromaTexture, writeBuffer, (long) width * height, () -> {}, 0);
            } finally {
                GL11.glPixelStorei(GL11.GL_PACK_ALIGNMENT, prevPackAlignment);
            }
            CaptureProfiler.end(CaptureProfiler.Stage.RAM_COPY, copyStart);*/
            //? }
            // 3. 读取并映射上一个 PBO (已完成 GPU-CPU DMA 传输)，就地投递至 Channel
            int readIdx = 1 - writeIdx;
            boolean delivered = false;
            if (hasPreviousFrame) {
                GpuBuffer readBuffer = pboBuffers[readIdx];
                if (readBuffer != null && !readBuffer.isClosed()) {
                    //? if vulkan_backend {
                    long mapStart = CaptureProfiler.begin();
                    GpuBufferSlice.MappedView mappedView = readBuffer.map(true, false);
                    CaptureProfiler.end(CaptureProfiler.Stage.RAM_MAP, mapStart);
                    if (mappedView != null) {
                        try {
                            ByteBuffer data = mappedView.data();
                            if (data != null) {
                                long pushStart = CaptureProfiler.begin();
                                try {
                                    channel.pushSystemFrame(data);
                                    delivered = true;
                                } finally {
                                    CaptureProfiler.end(CaptureProfiler.Stage.RAM_PUSH, pushStart);
                                }
                            }
                        } finally {
                            long unmapStart = CaptureProfiler.begin();
                            try {
                                mappedView.close();
                            } finally {
                                CaptureProfiler.end(CaptureProfiler.Stage.RAM_UNMAP, unmapStart);
                            }
                        }
                    }
                    //? } else {
                    /*// 26.1 的映射入口在 CommandEncoder 上，视图嵌套类型也不同
                    long mapStart = CaptureProfiler.begin();
                    GpuBuffer.MappedView mappedView = this.encoder.mapBuffer(readBuffer, true, false);
                    CaptureProfiler.end(CaptureProfiler.Stage.RAM_MAP, mapStart);
                    if (mappedView != null) {
                        try {
                            ByteBuffer data = mappedView.data();
                            if (data != null) {
                                long pushStart = CaptureProfiler.begin();
                                try {
                                    channel.pushSystemFrame(data);
                                    delivered = true;
                                } finally {
                                    CaptureProfiler.end(CaptureProfiler.Stage.RAM_PUSH, pushStart);
                                }
                            }
                        } finally {
                            long unmapStart = CaptureProfiler.begin();
                            try {
                                mappedView.close();
                            } finally {
                                CaptureProfiler.end(CaptureProfiler.Stage.RAM_UNMAP, unmapStart);
                            }
                        }
                    }*/
                    //? }
                }
            }

            hasPreviousFrame = true;
            currentBufferIndex = readIdx;
            // 首帧尚无上一帧可映射，属主动丢弃而非故障
            return delivered ? CaptureOutcome.Delivered : CaptureOutcome.Dropped;
        } catch (Throwable t) {
            Constants.LOG.error("RenderpearlRamPipeline.capture 异常: {}", t.getMessage());
            return CaptureOutcome.Failed;
        }
    }

    /**
     * 全屏绘制：目标附件尺寸决定缩放，源为当前主画面视图
     */
    private void blitInto(GpuTextureView target, RenderPipeline pipeline, GpuTextureView source) {
        //? if vulkan_backend {
        try (RenderPass pass = this.encoder.createRenderPass(() -> "Vistrel Blit", target, Optional.empty())) {
            RenderSystem.bindDefaultUniforms(pass);
            //? if renderpearl_era {
            pass.setPipeline(this.blitCache.get(pipeline));
            pass.setUniform("InSampler", source, RenderSystem.getSamplerCache().getClampToEdge(FilterMode.LINEAR));
            //? } else {
            /*pass.setPipeline(pipeline);
            pass.bindTexture("InSampler", source, RenderSystem.getSamplerCache().getClampToEdge(FilterMode.LINEAR));*/
            //? }
            pass.draw(3, 1, 0, 0);
        }
        //? } else {
        /*// 26.1：清屏色为 OptionalInt；draw 两参为（首顶点, 顶点数），最终落 glDrawArrays(mode, first, count)，
        // 与 26.3 的（顶点数, 实例数, 首顶点, 首实例）名序不同，勿照抄
        try (RenderPass pass = this.encoder.createRenderPass(() -> "Vistrel Blit", target, java.util.OptionalInt.empty())) {
            RenderSystem.bindDefaultUniforms(pass);
            pass.setPipeline(pipeline);
            pass.bindTexture("InSampler", source, RenderSystem.getSamplerCache().getClampToEdge(FilterMode.LINEAR));
            pass.draw(0, 3);
        }*/
        //? }
    }

    @Override
    public void close() {
        //? if renderpearl_era {
        if (this.blitCache != null) {
            this.blitCache.close();
            this.blitCache = null;
        }
        //? }
        try {
            if (pboBuffers[0] != null) {
                pboBuffers[0].close();
                pboBuffers[0] = null;
            }
            if (pboBuffers[1] != null) {
                pboBuffers[1].close();
                pboBuffers[1] = null;
            }
            if (lumaView != null) {
                try {
                    lumaView.close();
                } catch (Throwable ignored) {
                }
            }
            if (lumaTexture != null) {
                try {
                    lumaTexture.close();
                } catch (Throwable ignored) {
                }
            }
            if (chromaView != null) {
                try {
                    chromaView.close();
                } catch (Throwable ignored) {
                }
            }
            if (chromaTexture != null) {
                try {
                    chromaTexture.close();
                } catch (Throwable ignored) {
                }
            }
            hasPreviousFrame = false;
        } catch (Throwable t) {
            Constants.LOG.error("清理 RenderpearlRamPipeline 异常: {}", t.getMessage());
        }
    }
}
//? }

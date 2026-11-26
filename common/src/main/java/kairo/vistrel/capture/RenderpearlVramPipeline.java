package kairo.vistrel.capture;

//? if vulkan_backend {
import kairo.vistrel.Constants;
import kairo.vistrel.capture.BlitPipelines.FlipBlit;
import kairo.vistrel.streaming.ControlStatus;
import kairo.vistrel.streaming.EncoderChannel;
import kairo.vistrel.streaming.SessionServer;
import kairo.vistrel.streaming.VramBinding;
//? if renderpearl_era {
import kairo.vistrel.mixin.FrontendGpuDeviceAccessor;
//? } else {
/*import kairo.vistrel.mixin.GpuDeviceBackendAccessor;*/
//? }
import com.mojang.blaze3d.pipeline.RenderTarget;
import com.mojang.blaze3d.systems.RenderSystem;
//? if renderpearl_era {
import com.mojang.blaze3d.pipeline.PipelineCache;
//? }
import com.mojang.renderpearl.api.commands.CommandEncoder;
import com.mojang.renderpearl.api.commands.RenderPass;
import com.mojang.renderpearl.api.device.GpuDevice;
import com.mojang.renderpearl.api.textures.FilterMode;
import com.mojang.renderpearl.api.textures.GpuTextureView;
import com.mojang.renderpearl.backend.api.GpuDeviceBackend;
import com.mojang.renderpearl.backend.vulkan.VulkanCommandEncoder;
import com.mojang.renderpearl.backend.vulkan.VulkanDevice;
//? if renderpearl_era {
import com.mojang.renderpearl.frontend.FrontendGpuDevice;
//? }
import net.minecraft.client.Minecraft;
import org.lwjgl.system.MemoryStack;
import org.lwjgl.vulkan.VK10;
import org.lwjgl.vulkan.VK12;
import org.lwjgl.vulkan.VkCommandBuffer;
import org.lwjgl.vulkan.VkImageMemoryBarrier;
import java.util.Optional;

/**
 * Minecraft 26.2+ 显存直通管线（vulkan_backend 门，驱动扩展支持运行期另判）。
 *
 * <p>资源分两层：{@link VramExportSet} 只随几何变化；{@link VramBinding} 只对导入时的
 * 绑定句柄有效。控制面新代次时 {@link #rebind} 先把旧绑定从 Native 注销：获得确认
 * 就复用导出集重新导入，未获确认则弃置整个导出集并新建，避免编码器仍持有跨 API 导入时
 * 销毁游戏侧资源。
 *
 * <p>线程归属：capture、rebind 与 close 均在渲染线程执行（stop 会把拆除投递到渲染线程）。
 */
public class RenderpearlVramPipeline implements FrameSource {
    /**
     * 槽位压力汇总上报周期，避免逐帧刷日志
     */
    private static final long SLOT_PRESSURE_REPORT_NS = 5_000_000_000L;
    private final EncoderChannel channel;
    private final GpuDevice device;
    private final VulkanDevice vulkanDevice;
    /**
     * 构造期几何：导出集被弃置时仍需要原始尺寸来重建
     */
    private final int width;
    private final int height;
    /**
     * 设备级共享前端编码器（blit pass 用）：createCommandEncoder 返回的是设备构造时就建好的同一实例
     */
    private final CommandEncoder encoder;
    /**
     * 设备级共享后端编码器（barrier/信号量用）：与上面是同一提交流的两个 API 层
     */
    private final VulkanCommandEncoder vkEncoder;
    private final ReportGate pressureGate = ReportGate.windowed(SLOT_PRESSURE_REPORT_NS);
    /**
     * 编码停滞自愈回调：心跳超时触发时在渲染线程调用一次，实现方必须立即移交后台线程
     */
    private final Runnable onEncoderStall;
    /**
     * 几何绑定的导出资源；{@code null} 表示尚无（或已弃置）导出集
     */
    private VramExportSet exportSet = null;
    /**
     * 已整批导入的显存绑定：槽位认领、投递、心跳与注销都由它承担；为 {@code null} 时不可采集
     */
    private VramBinding vramBinding = null;
    /**
     * 当前绑定对应的控制面编码器代次
     */
    private SessionServer.EncoderGeneration boundGeneration;
    private long globalFrameIndex = 0;
    /**
     * 心跳超时已触发（一次性标记，仅渲染线程写）：此后直接停推显存帧，
     * 等待 {@link #onEncoderStall} 引擎自愈
     */
    private boolean stallTriggered = false;
    /**
     * 翻转 blit 管线编译失败闩：失败即锁定降级，不逐帧重试（仅渲染线程读写）
     */
    private boolean flipCompileFailed;
    //? if renderpearl_era {
    /**
     * 翻转 blit 专属编译缓存：以内嵌源按需编译，与 vanilla 资源驱动的管线缓存隔离
     */
    private PipelineCache flipCache;

    //? } else {
    /*private InlineShaderSource inlineSource;*/
    //? }
    /**
     * 构造显存直通管线（RAII）：导出集与槽位导入一次建齐，失败逆序清理后抛出；
     * 构造须在渲染线程执行
     *
     * @param channel        数据面通道；须能打开显存绑定
     * @param width          采集宽度
     * @param height         采集高度
     * @param generation     首次绑定的编码器代次，不得为 {@code null}
     * @param onEncoderStall 编码停滞自愈回调，不得为 {@code null}；心跳超时判定在渲染线程
     *                       触发一次，实现方必须立即移交后台线程，回调内不得做任何显存操作
     * @throws IllegalArgumentException channel、generation 或回调为 {@code null}
     * @throws IllegalStateException    GpuDevice 不可用、后端非 Vulkan 或翻转 blit 管线
     *                                  预编译失败，调用方据此降级 RAM 回读
     * @throws RuntimeException         导出资源创建 / 句柄导出 / 整批导入任一环节失败
     *                                  （失败路径已清理已建资源）
     */
    public RenderpearlVramPipeline(
            EncoderChannel channel,
            int width,
            int height,
            SessionServer.EncoderGeneration generation,
            Runnable onEncoderStall
    ) {
        if (channel == null) {
            throw new IllegalArgumentException("EncoderChannel 不能为空");
        }
        if (generation == null) {
            throw new IllegalArgumentException("编码器代次不能为空");
        }
        if (onEncoderStall == null) {
            throw new IllegalArgumentException("编码停滞回调不能为空");
        }
        this.channel = channel;
        this.boundGeneration = generation;
        this.onEncoderStall = onEncoderStall;
        this.width = width;
        this.height = height;
        GpuDevice dev = null;
        try {
            dev = RenderSystem.tryGetDevice();
        } catch (Throwable ignored) {
            try {
                dev = RenderSystem.getDevice();
            } catch (Throwable ignored2) {
            }
        }
        if (dev == null) {
            throw new IllegalStateException("Renderpearl GpuDevice 不可用，无法构造显存直通管线");
        }
        //? if renderpearl_era {
        if (!(dev instanceof FrontendGpuDevice frontend)) {
            throw new IllegalStateException("Renderpearl 运行于非 Vulkan 后端 (OpenGL/其他)，显存直通不支持，安全降级至 RAM 回读管线");
        }
        GpuDeviceBackend deviceBackend = ((FrontendGpuDeviceAccessor) frontend).vistrel$getBackend();
        //? } else {
        /*GpuDeviceBackend deviceBackend = ((GpuDeviceBackendAccessor) dev).vistrel$getBackend();*/
        //? }
        if (!(deviceBackend instanceof VulkanDevice vulkanDev)) {
            throw new IllegalStateException("Renderpearl 运行于非 Vulkan 后端 (OpenGL/其他)，显存直通不支持，安全降级至 RAM 回读管线");
        }
        this.device = dev;
        this.vulkanDevice = vulkanDev;
        // 两个编码器都是设备级共享实例（VulkanDevice 构造期创建、设备销毁时释放），管线仅持引用
        this.encoder = dev.createCommandEncoder();
        this.vkEncoder = vulkanDev.createCommandEncoder();
        // 专属编译缓存与内嵌源：翻转 blit 以内嵌 GLSL 编译，不依赖资源重载
        //? if renderpearl_era {
        this.flipCache = new PipelineCache(
                this.device,
                new InlineShaderSource(Minecraft.getInstance().getResourceManager())
        );
        //? } else {
        /*this.inlineSource = new InlineShaderSource(Minecraft.getInstance().getResourceManager());*/
        //? }
        try {
            if (!ensureFlipPipeline()) {
                throw new IllegalStateException("翻转 blit 管线预编译失败");
            }
            this.exportSet = new VramExportSet(device, vulkanDevice, width, height);
            // 一次性注册整批槽位 (要么全成要么全败)，失败由通道抛出且 Native 侧已整批回滚
            this.vramBinding = channel.bindVram(exportSet.descriptors());
        } catch (Throwable t) {
            cleanupResources();
            if (t instanceof RuntimeException re) {
                throw re;
            }
            throw new RuntimeException("初始化 Renderpearl 显存直通管线异常", t);
        }
    }

    /**
     * 把当前导出集重新绑定到新的编码器代次。
     *
     * <p>旧绑定获 Native 确认则复用导出集，否则整个弃置新建；成功后记录新代次。
     *
     * @param generation 新编码器代次；与控制面快照中的对象身份一致
     * @throws IllegalArgumentException                     generation 为 {@code null}
     * @throws kairo.vistrel.streaming.EncoderChannel.ControlException Native 拒绝重新导入
     *         或新编码器不可用；失败后本管线已无可用的显存绑定，且导出集句柄已被 Native
     *         消费，不得用同一导出集重试
     */
    public void rebind(SessionServer.EncoderGeneration generation) {
        if (generation == null) {
            throw new IllegalArgumentException("编码器代次不能为空");
        }
        if (generation == boundGeneration) {
            return;
        }
        VramBinding.ReleaseStatus status = VramBinding.ReleaseStatus.RELEASED;
        if (this.vramBinding != null) {
            try {
                status = this.vramBinding.close();
            } catch (Throwable t) {
                Constants.LOG.warn("Failed to release VRAM binding during rebind: {}", t.getMessage());
                status = VramBinding.ReleaseStatus.RETAINED;
            }
            this.vramBinding = null;
        }
        if (status == VramBinding.ReleaseStatus.RETAINED) {
            // 旧导出集可能仍被编码器导入：整个弃置（GpuTexture 无 Cleaner，随 VkDevice 关停回收）
            Constants.LOG.error("显存槽位注销未获原生侧确认，弃置当前导出集并在新导出集上重新绑定");
            this.exportSet = null;
        }
        if (this.exportSet == null) {
            this.exportSet = new VramExportSet(device, vulkanDevice, width, height);
        }
        this.vramBinding = channel.bindVram(exportSet.descriptors());
        this.boundGeneration = generation;
        // 新编码器代次：停滞判定针对旧编码器的编码线程，不继承
        this.stallTriggered = false;
    }

    /**
     * 周期性汇总槽位压力。
     *
     * 全忙丢帧说明编码器暂时没消费完，属常态波动；持续静默由心跳超时接管
     * （停推 + 引擎自愈）。
     */
    private void reportSlotPressure() {
        long drained = pressureGate.drainIfDue(System.nanoTime());
        if (drained > 0L) {
            Constants.LOG.warn("显存槽位压力：近 {}s 槽位全在途丢帧 {} 帧", SLOT_PRESSURE_REPORT_NS / 1_000_000_000L, drained);
        }
    }

    /**
     * 心跳超时：判定编码线程卡死，停推显存帧并移交自愈回调
     */
    private void onHeartbeatTimeout() {
        stopVramPush(String.format(
                "显存直通停滞：%ds 内编码线程心跳冻结（最后阶段：%s），判定编码线程死亡，停推显存帧并触发引擎自愈",
                VramBinding.STALL_HEARTBEAT_TIMEOUT_NS / 1_000_000_000L,
                vramBinding.stallDiagnostics()
        ));
    }

    /**
     * 毒化：等待超时后，在途提交可能仍在读槽位纹理，必须停推并自愈
     */
    private void onEncoderPoisoned() {
        stopVramPush("显存直通毒化：编码器等待超时，在途提交可能仍在读槽位纹理，停推显存帧并触发引擎自愈");
    }

    /**
     * 一次性停推显存帧并移交自愈回调：卡死与毒化共用同一条出口。
     *
     * @param message 面向日志的原因；只打一次
     */
    private void stopVramPush(String message) {
        if (stallTriggered) {
            return;
        }
        stallTriggered = true;
        Constants.LOG.error(message);
        try {
            onEncoderStall.run();
        } catch (Throwable t) {
            Constants.LOG.error("编码停滞自愈回调异常: {}", t.getMessage());
        }
    }

    /**
     * 编译/预热翻转 blit 管线：构造期首编，逐帧命中即查表
     * （26.1/26.2 的设备缓存会被资源重载清空，逐帧补种自愈）；失败即锁定降级，不逐帧重试
     */
    private boolean ensureFlipPipeline() {
        if (flipCompileFailed) {
            return false;
        }
        try {
            //? if renderpearl_era {
            if (this.flipCache.get(FlipBlit.FLIP) == null) {
                throw new IllegalStateException("管线编译返回空结果");
            }
            //? } else {
            /*if (!RenderSystem.getDevice().precompilePipeline(FlipBlit.FLIP, inlineSource).isValid()) {
                throw new IllegalStateException("管线预编译返回无效状态");
            }*/
            //? }
            return true;
        } catch (Throwable t) {
            flipCompileFailed = true;
            Constants.LOG.error("翻转 blit 管线不可用，本次会话降级: {}", t.getMessage());
            return false;
        }
    }

    @Override
    public CaptureOutcome capture() {
        long startNanos = CaptureProfiler.begin();
        try {
            return captureInternal();
        } finally {
            CaptureProfiler.end(CaptureProfiler.Stage.VRAM_TOTAL, startNanos);
        }
    }

    private CaptureOutcome captureInternal() {
        // 心跳超时已触发：编码器可能仍映射着槽位纹理，一律停推，等引擎自愈
        if (stallTriggered) {
            return CaptureOutcome.Dropped;
        }
        VramBinding binding = this.vramBinding;
        if (binding == null) {
            // 重新绑定尚未成功：没有可投递的编码器代次
            return CaptureOutcome.Failed;
        }
        long prepStart = CaptureProfiler.begin();
        if (!ensureFlipPipeline()) {
            CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PREP, prepStart);
            return CaptureOutcome.Failed;
        }
        Minecraft mc = Minecraft.getInstance();
        GpuTextureView mainColorView = null;
        if (mc.gameRenderer != null) {
            RenderTarget mainTarget = mc.gameRenderer.mainRenderTarget();
            mainColorView = mainTarget == null ? null : mainTarget.getColorTextureView();
        }
        if (mainColorView == null) {
            CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PREP, prepStart);
            return CaptureOutcome.Failed;
        }

        try {
            int preferredIdx = (int) (globalFrameIndex % VramExportSet.SLOT_COUNT);
            // 每 tick 拉一次快照：选槽与心跳共用同一次 JNI 往返
            int slotIdx = binding.acquire(preferredIdx);
            // 全忙也要判停滞：卡死后 push 会停
            if (binding.heartbeatTimedOut()) {
                CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PREP, prepStart);
                onHeartbeatTimeout();
                return CaptureOutcome.Dropped;
            }
            if (slotIdx == VramBinding.BINDING_UNAVAILABLE) {
                // 绑定已被摘除或引擎不在：不是槽位压力，停滞判定会接管自愈
                CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PREP, prepStart);
                return CaptureOutcome.Dropped;
            }
            // 全忙是**主动**丢帧：管线健康，只是编码器暂时没消费完。必须与 Failed 区分——
            // 调度器若把它当故障，一次槽位压力就会永久降级到 RAM 回读
            if (slotIdx < 0) {
                CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PREP, prepStart);
                pressureGate.accumulate(1L);
                reportSlotPressure();
                return CaptureOutcome.Dropped;
            }
            VramExportSet.VulkanSlot slot = exportSet.slot(slotIdx);
            if (slot == null || slot.blitTextureView == null) {
                CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PREP, prepStart);
                return CaptureOutcome.Failed;
            }
            CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PREP, prepStart);
            // 1. 等比 Blit 缩放绘制到槽位专属导出颜色附件（行序契约见 FlipBlit 类文档）
            long stageStart = CaptureProfiler.begin();
            try (RenderPass pass =
                    encoder.createRenderPass(() -> "Vistrel Blit", slot.blitTextureView, Optional.empty())) {
                RenderSystem.bindDefaultUniforms(pass);
                //? if renderpearl_era {
                pass.setPipeline(this.flipCache.get(FlipBlit.FLIP));
                pass.setUniform(
                        "InSampler",
                        mainColorView,
                        RenderSystem.getSamplerCache().getClampToEdge(FilterMode.LINEAR)
                );
                //? } else {
                /*pass.setPipeline(FlipBlit.FLIP);
                pass.bindTexture(
                        "InSampler",
                        mainColorView,
                        RenderSystem.getSamplerCache().getClampToEdge(FilterMode.LINEAR)
                );*/
                //? }
                pass.draw(3, 1, 0, 0);
            }
            CaptureProfiler.end(CaptureProfiler.Stage.VRAM_BLIT, stageStart);
            // 2. 转换 Image Layout 为 VK_IMAGE_LAYOUT_GENERAL 并提交 pipeline barrier
            long signalVal = globalFrameIndex + 1;
            stageStart = CaptureProfiler.begin();
            try (MemoryStack stack = MemoryStack.stackPush()) {
                VkImageMemoryBarrier.Buffer imageBarrier = VkImageMemoryBarrier
                    .calloc(1, stack)
                    .sType$Default()
                    .oldLayout(VK10.VK_IMAGE_LAYOUT_GENERAL)
                    .newLayout(VK10.VK_IMAGE_LAYOUT_GENERAL)
                    .srcAccessMask(VK10.VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT)
                    .dstAccessMask(VK10.VK_ACCESS_MEMORY_READ_BIT | VK10.VK_ACCESS_MEMORY_WRITE_BIT)
                    .srcQueueFamilyIndex(VK10.VK_QUEUE_FAMILY_IGNORED)
                    .dstQueueFamilyIndex(VK10.VK_QUEUE_FAMILY_IGNORED)
                    .image(slot.vkImage);
                imageBarrier
                    .subresourceRange()
                    .aspectMask(VK10.VK_IMAGE_ASPECT_COLOR_BIT)
                    .baseMipLevel(0)
                    .levelCount(1)
                    .baseArrayLayer(0)
                    .layerCount(1);

                VkCommandBuffer cmd = vkEncoder.allocateAndBeginTransientCommandBuffer();
                VK10.vkCmdPipelineBarrier(
                        cmd,
                        VK10.VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                        VK10.VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                        0,
                        null,
                        null,
                        imageBarrier
                );
                VK12.vkEndCommandBuffer(cmd);
                vkEncoder.execute(cmd);
            }
            CaptureProfiler.end(CaptureProfiler.Stage.VRAM_BARRIER, stageStart);
            // 3. 挂载时间线信号量至当前提交流：signal 先于 push，是所有权协议的
            //    数据依赖不变量（渲染完成先于编码可读）
            stageStart = CaptureProfiler.begin();
            vkEncoder.signalSemaphore(slot.vkSemaphore, signalVal, 0x10000L);
            CaptureProfiler.end(CaptureProfiler.Stage.VRAM_SIGNAL, stageStart);

            stageStart = CaptureProfiler.begin();
            encoder.submit();
            CaptureProfiler.end(CaptureProfiler.Stage.VRAM_SUBMIT, stageStart);
            stageStart = CaptureProfiler.begin();
            boolean queued;
            try {
                queued = binding.push(slotIdx, signalVal);
            } finally {
                // submit 已在途，信号量必被信号到 signalVal：帧计数照常推进
                globalFrameIndex++;
                CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PUSH, stageStart);
            }
            // Native 未受理（旧绑定）：帧已被入口丢弃，按主动丢帧处理
            return queued ? CaptureOutcome.Delivered : CaptureOutcome.Dropped;
        } catch (EncoderChannel.ControlException e) {
            if (e.status == ControlStatus.POISONED) {
                onEncoderPoisoned();
                return CaptureOutcome.Dropped;
            }
            Constants.LOG.error("RenderpearlVramPipeline.capture 控制面异常: {}", e.getMessage());
            return CaptureOutcome.Failed;
        } catch (Throwable t) {
            Constants.LOG.error("RenderpearlVramPipeline.capture 异常: {}", t.getMessage());
            return CaptureOutcome.Failed;
        }
    }

    private void cleanupResources() {
        //? if renderpearl_era {
        // 编译缓存先于显存资源释放
        if (this.flipCache != null) {
            try {
                this.flipCache.close();
            } catch (Throwable ignored) {
            }
            this.flipCache = null;
        }
        //? }
        if (vulkanDevice != null && vulkanDevice.graphicsQueue() != null) {
            try {
                vulkanDevice.graphicsQueue().waitIdle();
            } catch (Throwable ignored) {
            }
        }
        // 未获确认说明编码器仍持有跨 API 导入，此刻销毁是驱动层未定义行为；
        // GpuTexture 无 Cleaner，弃置的资源随 VkDevice 关停统一回收
        boolean released = true;
        if (this.vramBinding != null) {
            try {
                released = this.vramBinding.close().released();
            } catch (Throwable t) {
                Constants.LOG.warn("Failed to release VRAM binding: {}", t.getMessage());
                released = false;
            }
        }
        this.vramBinding = null;
        if (!released) {
            Constants.LOG.error("显存槽位注销未获原生侧确认，导出纹理/信号量/导出池保留不销毁 (随引擎关停统一回收)");
            this.exportSet = null;
            return;
        }
        if (this.exportSet != null) {
            this.exportSet.close();
            this.exportSet = null;
        }
    }

    @Override
    public void close() {
        cleanupResources();
    }
}
//? } else {
/*public class RenderpearlVramPipeline {
}*/
//? }

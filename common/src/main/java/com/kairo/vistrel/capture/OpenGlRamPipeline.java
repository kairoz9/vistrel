package com.kairo.vistrel.capture;

//? if <1.21.5 {
/*import com.mojang.blaze3d.platform.GlStateManager;*/
//? } elif <26.1 {
/*import com.mojang.blaze3d.opengl.GlStateManager;
import com.mojang.blaze3d.opengl.GlTexture;*/
//? }
//? if <26.1 {
/*import com.kairo.vistrel.Constants;
import com.kairo.vistrel.streaming.EncoderChannel;
import com.mojang.blaze3d.pipeline.RenderTarget;
import net.minecraft.client.Minecraft;
import org.lwjgl.opengl.GL11;
import org.lwjgl.opengl.GL12;
import org.lwjgl.opengl.GL13;
import org.lwjgl.opengl.GL15;
import org.lwjgl.opengl.GL20;
import org.lwjgl.opengl.GL21;
import org.lwjgl.opengl.GL30;
import org.lwjgl.opengl.GL32;
import org.lwjgl.opengl.GL33;

import java.nio.ByteBuffer;

// 历史版本 OpenGL 双 PBO 内存回读管道（26.1 起由 GpuDevice 管线接管）
// RAII：构造即初始化双 PBO 与 NV12 两平面目标，捕获、投递与解映射都在本类内完成
// 不加锁：capture 与 close 均在渲染线程执行（stop 会把拆除投递到渲染线程）
public class OpenGlRamPipeline implements VideoPipeline {
    private final EncoderChannel channel;
    private final int width;
    private final int height;
    private final int frameBytes;

    private int lumaSamplerLoc = -1;
    private int chromaSamplerLoc = -1;
    private int chromaTapLoc = -1;
    // 采样器对象独立于纹理参数：直接采游戏主画面纹理时无需改动其过滤与寻址设置
    private int samplerObjectId = 0;
    // NV12 两平面各备两份轮替：上一帧的 DMA 可能仍在读这对目标，本轮只写另一对
    private final int[] lumaFboIds = new int[2];
    private final int[] lumaTextureIds = new int[2];
    private final int[] chromaFboIds = new int[2];
    private final int[] chromaTextureIds = new int[2];
    private final long[] readFences = new long[2];
    private boolean sourceChecked = false;
    private boolean sourceRejected = false;
    private int vaoId = 0;
    private int lumaProgramId = 0;
    private int chromaProgramId = 0;
    private final int[] pboIds = new int[2];
    private int currentPboIndex = 0;
    private boolean hasPreviousFrame = false;
    private final int[] prevViewport = new int[4];

    // 全屏三角：不依赖顶点缓冲，三个顶点覆盖裁剪空间
    private static final String FULLSCREEN_VERTEX = """
        #version 150
        out vec2 vUv;
        void main() {
            vec2 uv = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);
            vUv = uv;
            gl_Position = vec4(uv * 2.0 - 1.0, 0.0, 1.0);
        }
        """;

    // 亮度平面：BT.709 有限范围（16..235），与硬编路径的取值一致
    private static final String LUMA_FRAGMENT = """
        #version 150
        uniform sampler2D InSampler;
        in vec2 vUv;
        out vec4 fragColor;
        void main() {
            // 线格式翻转为顶行在前：源纹理行序为底行在前，V 反转采样
            vec3 rgb = texture(InSampler, vec2(vUv.x, 1.0 - vUv.y)).rgb;
            float luma = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
            fragColor = vec4((16.0 + luma * 219.0) / 255.0, 0.0, 0.0, 1.0);
        }
        """;

    // 色度平面：每目标纹素取源画面 2x2 盒式平均，输出 U(Cb)、V(Cr)
    private static final String CHROMA_FRAGMENT = """
        #version 150
        uniform sampler2D InSampler;
        // 单次采样在源纹理上的跨度（UV 单位）：由采集尺寸决定，与源分辨率无关；为 0 时四采样塌缩为单点
        uniform vec2 uTap;
        in vec2 vUv;
        out vec4 fragColor;
        void main() {
            // 线格式翻转为顶行在前：源行序底行在前，V 反转后四采样命中镜像 2x2 块
            vec2 flipped = vec2(vUv.x, 1.0 - vUv.y);
            vec3 rgb = texture(InSampler, flipped + vec2(-uTap.x, -uTap.y)).rgb
                     + texture(InSampler, flipped + vec2( uTap.x, -uTap.y)).rgb
                     + texture(InSampler, flipped + vec2(-uTap.x,  uTap.y)).rgb
                     + texture(InSampler, flipped + vec2( uTap.x,  uTap.y)).rgb;
            rgb *= 0.25;
            float luma = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
            float cb = (rgb.b - luma) / 1.8556;
            float cr = (rgb.r - luma) / 1.5748;
            fragColor = vec4((128.0 + cb * 224.0) / 255.0, (128.0 + cr * 224.0) / 255.0, 0.0, 1.0);
        }
        """;

    private static boolean hasGlContext() {
        try {
            return org.lwjgl.opengl.GL.getCapabilities() != null;
        } catch (Throwable t) {
            return false;
        }
    }

    public OpenGlRamPipeline(EncoderChannel channel, int width, int height) {
        this.channel = channel;
        this.width = width;
        this.height = height;
        // NV12 色度平面是 4:2:0，奇数边长无法表达
        this.frameBytes = (width & 1) == 0 && (height & 1) == 0 ? width * height * 3 / 2 : 0;

        if (!hasGlContext()) {
            Constants.LOG.warn("OpenGlRamPipeline 构造时无当前 OpenGL 上下文");
            return;
        }
        if (frameBytes == 0) {
            Constants.LOG.error("OpenGlRamPipeline 不支持奇数采集尺寸 {}x{}", width, height);
            return;
        }

        try {
            initResources();
            Constants.LOG.info("OpenGlRamPipeline 成功初始化双 PBO NV12 管线 ({}x{})", width, height);
        } catch (Throwable t) {
            Constants.LOG.error("初始化 OpenGlRamPipeline 异常: {}", t.getMessage());
            cleanupResources();
        }
    }

    // 构造期同样绕过状态机直接分配资源：动过的纹理/FBO/PBO 绑定必须恢复原值（约束同 capture()）
    private void initResources() {
        int prevDrawFbo = GL11.glGetInteger(GL30.GL_DRAW_FRAMEBUFFER_BINDING);
        int prevReadFbo = GL11.glGetInteger(GL30.GL_READ_FRAMEBUFFER_BINDING);
        int prevActiveTexture = GL11.glGetInteger(GL13.GL_ACTIVE_TEXTURE);
        GL13.glActiveTexture(GL13.GL_TEXTURE0);
        int prevTexture0 = GL11.glGetInteger(GL11.GL_TEXTURE_BINDING_2D);
        int prevPackBuffer = GL11.glGetInteger(GL21.GL_PIXEL_PACK_BUFFER_BINDING);
        try {
            // 1. 采样器对象：过滤与寻址只在本对象上设定，采样时不动用游戏纹理自身参数
            samplerObjectId = GL33.glGenSamplers();
            GL33.glSamplerParameteri(samplerObjectId, GL11.GL_TEXTURE_MIN_FILTER, GL11.GL_LINEAR);
            GL33.glSamplerParameteri(samplerObjectId, GL11.GL_TEXTURE_MAG_FILTER, GL11.GL_LINEAR);
            GL33.glSamplerParameteri(samplerObjectId, GL12.GL_TEXTURE_WRAP_S, GL12.GL_CLAMP_TO_EDGE);
            GL33.glSamplerParameteri(samplerObjectId, GL12.GL_TEXTURE_WRAP_T, GL12.GL_CLAMP_TO_EDGE);

            // 2. NV12 两个平面目标：R8 全分辨率 + RG8 半分辨率，双份轮替
            for (int i = 0; i < 2; i++) {
                lumaTextureIds[i] = createTexture(GL30.GL_R8, GL11.GL_RED, width, height);
                lumaFboIds[i] = createFramebuffer(lumaTextureIds[i]);
                chromaTextureIds[i] = createTexture(GL30.GL_RG8, GL30.GL_RG, width / 2, height / 2);
                chromaFboIds[i] = createFramebuffer(chromaTextureIds[i]);
            }

            // 3. 两条转换程序与一个空 VAO（core profile 下画全屏三角必须有绑定的 VAO）
            vaoId = GL30.glGenVertexArrays();
            lumaProgramId = compileProgram(FULLSCREEN_VERTEX, LUMA_FRAGMENT);
            chromaProgramId = compileProgram(FULLSCREEN_VERTEX, CHROMA_FRAGMENT);
            if (lumaProgramId == 0 || chromaProgramId == 0) {
                Constants.LOG.error("OpenGlRamPipeline NV12 着色器编译失败，采集不可用");
                cleanupResources();
                return;
            }
            // 采样器位置必须在程序链接后才有效，提前取只会拿到 -1
            lumaSamplerLoc = GL20.glGetUniformLocation(lumaProgramId, "InSampler");
            chromaSamplerLoc = GL20.glGetUniformLocation(chromaProgramId, "InSampler");
            chromaTapLoc = GL20.glGetUniformLocation(chromaProgramId, "uTap");

            // 4. 双 PBO：容量正好容纳 Y 平面 + UV 平面
            pboIds[0] = GL15.glGenBuffers();
            pboIds[1] = GL15.glGenBuffers();
            for (int pbo : pboIds) {
                GL15.glBindBuffer(GL21.GL_PIXEL_PACK_BUFFER, pbo);
                GL15.glBufferData(GL21.GL_PIXEL_PACK_BUFFER, frameBytes, GL15.GL_STREAM_READ);
            }
            GL15.glBindBuffer(GL21.GL_PIXEL_PACK_BUFFER, 0);
        } finally {
            GL15.glBindBuffer(GL21.GL_PIXEL_PACK_BUFFER, prevPackBuffer);
            GL30.glBindFramebuffer(GL30.GL_READ_FRAMEBUFFER, prevReadFbo);
            GL30.glBindFramebuffer(GL30.GL_DRAW_FRAMEBUFFER, prevDrawFbo);
            GL13.glActiveTexture(GL13.GL_TEXTURE0);
            GL11.glBindTexture(GL11.GL_TEXTURE_2D, prevTexture0);
            GL13.glActiveTexture(prevActiveTexture);
        }
    }

    private static int createTexture(int internalFormat, int format, int texWidth, int texHeight) {
        int id = GL11.glGenTextures();
        GL11.glBindTexture(GL11.GL_TEXTURE_2D, id);
        GL11.glTexImage2D(GL11.GL_TEXTURE_2D, 0, internalFormat, texWidth, texHeight, 0, format, GL11.GL_UNSIGNED_BYTE, (ByteBuffer) null);
        GL11.glTexParameteri(GL11.GL_TEXTURE_2D, GL11.GL_TEXTURE_MIN_FILTER, GL11.GL_LINEAR);
        GL11.glTexParameteri(GL11.GL_TEXTURE_2D, GL11.GL_TEXTURE_MAG_FILTER, GL11.GL_LINEAR);
        GL11.glTexParameteri(GL11.GL_TEXTURE_2D, GL12.GL_TEXTURE_WRAP_S, GL12.GL_CLAMP_TO_EDGE);
        GL11.glTexParameteri(GL11.GL_TEXTURE_2D, GL12.GL_TEXTURE_WRAP_T, GL12.GL_CLAMP_TO_EDGE);
        return id;
    }

    private static int createFramebuffer(int textureId) {
        int id = GL30.glGenFramebuffers();
        GL30.glBindFramebuffer(GL30.GL_FRAMEBUFFER, id);
        GL30.glFramebufferTexture2D(GL30.GL_FRAMEBUFFER, GL30.GL_COLOR_ATTACHMENT0, GL11.GL_TEXTURE_2D, textureId, 0);
        int status = GL30.glCheckFramebufferStatus(GL30.GL_FRAMEBUFFER);
        GL30.glBindFramebuffer(GL30.GL_FRAMEBUFFER, 0);
        if (status != GL30.GL_FRAMEBUFFER_COMPLETE) {
            throw new IllegalStateException("NV12 平面 FBO 不完整: 0x" + Integer.toHexString(status));
        }
        return id;
    }

    private static int compileProgram(String vertexSource, String fragmentSource) {
        int vertex = compileShader(GL20.GL_VERTEX_SHADER, vertexSource);
        int fragment = compileShader(GL20.GL_FRAGMENT_SHADER, fragmentSource);
        if (vertex == 0 || fragment == 0) {
            return 0;
        }
        int program = GL20.glCreateProgram();
        GL20.glAttachShader(program, vertex);
        GL20.glAttachShader(program, fragment);
        GL20.glLinkProgram(program);
        GL20.glDeleteShader(vertex);
        GL20.glDeleteShader(fragment);
        if (GL20.glGetProgrami(program, GL20.GL_LINK_STATUS) == GL11.GL_FALSE) {
            Constants.LOG.error("NV12 着色器链接失败: {}", GL20.glGetProgramInfoLog(program));
            GL20.glDeleteProgram(program);
            return 0;
        }
        return program;
    }

    private static int compileShader(int type, String source) {
        int shader = GL20.glCreateShader(type);
        GL20.glShaderSource(shader, source);
        GL20.glCompileShader(shader);
        if (GL20.glGetShaderi(shader, GL20.GL_COMPILE_STATUS) == GL11.GL_FALSE) {
            Constants.LOG.error("NV12 着色器编译失败: {}", GL20.glGetShaderInfoLog(shader));
            GL20.glDeleteShader(shader);
            return 0;
        }
        return shader;
    }

    @Override
    public CaptureOutcome capture() {
        if (samplerObjectId == 0 || lumaFboIds[0] == 0 || chromaFboIds[0] == 0 || channel == null) {
            return CaptureOutcome.Failed;
        }

        Minecraft mc = Minecraft.getInstance();
        RenderTarget mainTarget = mc.getMainRenderTarget();
        if (mainTarget == null) return CaptureOutcome.Failed;
        var window = mc.getWindow();
        if (window == null) return CaptureOutcome.Failed;
        int windowWidth = window.getWidth();
        int windowHeight = window.getHeight();
        if (windowWidth <= 0 || windowHeight <= 0) return CaptureOutcome.Failed;
        // 本管线绕过 GlStateManager 直改全局绑定，而游戏对 VAO/Program/Texture 均有「相同即跳过重绑」
        // 的状态缓存：离开时必须把每个动过的绑定恢复原值，否则游戏后续绘制会跳过重绑，
        // 逐帧报 GL_INVALID_OPERATION
        int prevDrawFbo = GL11.glGetInteger(GL30.GL_DRAW_FRAMEBUFFER_BINDING);
        int prevReadFbo = GL11.glGetInteger(GL30.GL_READ_FRAMEBUFFER_BINDING);
        int prevVao = GL11.glGetInteger(GL30.GL_VERTEX_ARRAY_BINDING);
        int prevProgram = GL11.glGetInteger(GL20.GL_CURRENT_PROGRAM);
        int prevActiveTexture = GL11.glGetInteger(GL13.GL_ACTIVE_TEXTURE);
        GL13.glActiveTexture(GL13.GL_TEXTURE0);
        int prevTexture0 = GL11.glGetInteger(GL11.GL_TEXTURE_BINDING_2D);
        int prevSampler = GL11.glGetInteger(GL33.GL_SAMPLER_BINDING);
        GL11.glGetIntegerv(GL11.GL_VIEWPORT, prevViewport);
        int prevPackAlignment = GL11.glGetInteger(GL11.GL_PACK_ALIGNMENT);
        int prevPackBuffer = GL11.glGetInteger(GL21.GL_PIXEL_PACK_BUFFER_BINDING);
        int prevPackRowLength = GL11.glGetInteger(GL11.GL_PACK_ROW_LENGTH);
        int prevPackSkipRows = GL11.glGetInteger(GL11.GL_PACK_SKIP_ROWS);
        int prevPackSkipPixels = GL11.glGetInteger(GL11.GL_PACK_SKIP_PIXELS);
        int prevPackSwapBytes = GL11.glGetInteger(GL11.GL_PACK_SWAP_BYTES);
        // 帧尾继承的游戏状态可能开着裁剪/混合/背面剔除/sRGB 编码，结束时按原状态交还
        boolean scissorWasEnabled = GL11.glIsEnabled(GL11.GL_SCISSOR_TEST);
        boolean blendWasEnabled = GL11.glIsEnabled(GL11.GL_BLEND);
        boolean cullWasEnabled = GL11.glIsEnabled(GL11.GL_CULL_FACE);
        boolean srgbWasEnabled = GL11.glIsEnabled(GL30.GL_FRAMEBUFFER_SRGB);
        GlStateManager._disableScissorTest();
        GlStateManager._disableBlend();
        GlStateManager._disableCull();
        GL11.glDisable(GL30.GL_FRAMEBUFFER_SRGB);

        try {
            // 1. 直采主画面颜色纹理：缩放由采样器的线性过滤完成*/
//? }
//? if <1.21.5 {
/*            int mainFbo = mainTarget.frameBufferId;
            if (mainFbo == 0) return CaptureOutcome.Failed;
            GL30.glBindFramebuffer(GL30.GL_READ_FRAMEBUFFER, mainFbo);
            // 源只接受非整数格式（整数纹理需 isampler2D）。vanilla 主画面
            // 恒单采样，有多采样源时下方 draw 报 INVALID_OPERATION 走 Failed，日志可定位
            if (!sourceChecked) {
                sourceChecked = true;
                int componentType = GL30.glGetFramebufferAttachmentParameteri(GL30.GL_READ_FRAMEBUFFER, GL30.GL_COLOR_ATTACHMENT0, GL30.GL_FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE);
                boolean integerFormat = componentType == GL11.GL_UNSIGNED_INT || componentType == GL11.GL_INT;
                if (integerFormat) {
                    sourceRejected = true;
                    Constants.LOG.error("采集源不可用 (组件类型 0x{}): 整数纹理需 isampler2D，本次采集保持关闭",
                            Integer.toHexString(componentType));
                }
            }
            if (sourceRejected) return CaptureOutcome.Failed;
            int sourceTexture = mainTarget.getColorTextureId();
            if (sourceTexture == 0) return CaptureOutcome.Failed;*/
//? } elif <26.1 {
/*            // 新版 RenderTarget 不再持有 GL id：纹理 id 经 GlTexture 取，格式由设备侧统一管理
            if (mainTarget.getColorTexture() == null) return CaptureOutcome.Failed;
            int sourceTexture = ((GlTexture) mainTarget.getColorTexture()).glId();
            if (sourceTexture == 0) return CaptureOutcome.Failed;*/
//? }
//? if <26.1 {
/*            int cur = currentPboIndex;

            // 2. 两趟自建 pass：全分辨率亮度 + 半分辨率色度
            GL13.glActiveTexture(GL13.GL_TEXTURE0);
            GL11.glBindTexture(GL11.GL_TEXTURE_2D, sourceTexture);
            GL33.glBindSampler(0, samplerObjectId);
            GL30.glBindVertexArray(vaoId);
            GL30.glUseProgram(lumaProgramId);
            GL20.glUniform1i(lumaSamplerLoc, 0);
            GL30.glBindFramebuffer(GL30.GL_FRAMEBUFFER, lumaFboIds[cur]);
            GL11.glViewport(0, 0, width, height);
            GL11.glDrawArrays(GL11.GL_TRIANGLES, 0, 3);

            GL30.glUseProgram(chromaProgramId);
            GL20.glUniform1i(chromaSamplerLoc, 0);
            // 色度目标每纹素覆盖 2 个采集像素：缩小方向才需要四采样拼出面积核，
            // 等比或放大时退回单次双线性
            GL20.glUniform2f(chromaTapLoc, windowWidth > width ? 0.5f / width : 0.0f, windowHeight > height ? 0.5f / height : 0.0f);
            GL30.glBindFramebuffer(GL30.GL_FRAMEBUFFER, chromaFboIds[cur]);
            GL11.glViewport(0, 0, width / 2, height / 2);
            GL11.glDrawArrays(GL11.GL_TRIANGLES, 0, 3);
            GL30.glBindVertexArray(0);
            GL30.glUseProgram(0);

            // 3. 异步 DMA 回读到当前写入 PBO：Y 在偏移 0、UV 紧跟其后
            // 原版取画面缩略图的路径会把 PACK_ROW_LENGTH 留成窗口宽度且不恢复：
            // 行跨距一旦偏离本次读取宽度，UV 平面就会越过 PBO 容量，驱动判非法并熔断拷贝引擎
            GL11.glPixelStorei(GL11.GL_PACK_ROW_LENGTH, 0);
            GL11.glPixelStorei(GL11.GL_PACK_SKIP_ROWS, 0);
            GL11.glPixelStorei(GL11.GL_PACK_SKIP_PIXELS, 0);
            GL11.glPixelStorei(GL11.GL_PACK_SWAP_BYTES, 0);
            GL11.glPixelStorei(GL11.GL_PACK_ALIGNMENT, 1);
            int writePbo = pboIds[cur];
            GL15.glBindBuffer(GL21.GL_PIXEL_PACK_BUFFER, writePbo);
            GL30.glBindFramebuffer(GL30.GL_READ_FRAMEBUFFER, lumaFboIds[cur]);
            GL11.glReadPixels(0, 0, width, height, GL11.GL_RED, GL11.GL_UNSIGNED_BYTE, 0);
            GL30.glBindFramebuffer(GL30.GL_READ_FRAMEBUFFER, chromaFboIds[cur]);
            GL11.glReadPixels(0, 0, width / 2, height / 2, GL30.GL_RG, GL11.GL_UNSIGNED_BYTE, (long) width * height);
            // 围栏标记本次 DMA 提交点，供下一帧无等待探询是否已完成
            readFences[cur] = GL32.glFenceSync(GL32.GL_SYNC_GPU_COMMANDS_COMPLETE, 0);

            // 4. 读取并映射上一个 PBO (已完成 DMA 传输)，投递至 Channel 后就地 unmap
            int readPboIndex = 1 - cur;
            boolean delivered = false;
            // 传输未完成就 map 会把渲染线程阻塞到 DMA 结束：先做零超时围栏探询，未就绪按主动丢帧处理
            boolean transferDone = true;
            long fence = readFences[readPboIndex];
            if (fence != 0) {
                int status = GL32.glClientWaitSync(fence, 0, 0);
                GL32.glDeleteSync(fence);
                readFences[readPboIndex] = 0;
                transferDone = status == GL32.GL_ALREADY_SIGNALED || status == GL32.GL_CONDITION_SATISFIED;
            }
            if (hasPreviousFrame && transferDone) {
                int readPbo = pboIds[readPboIndex];
                GL15.glBindBuffer(GL21.GL_PIXEL_PACK_BUFFER, readPbo);
                ByteBuffer mapped = GL30.glMapBufferRange(GL21.GL_PIXEL_PACK_BUFFER, 0, frameBytes, GL30.GL_MAP_READ_BIT);
                if (mapped != null) {
                    try {
                        channel.pushSystemFrame(mapped);
                        delivered = true;
                    } finally {
                        GL15.glUnmapBuffer(GL21.GL_PIXEL_PACK_BUFFER);
                    }
                }
            }
            hasPreviousFrame = true;
            currentPboIndex = readPboIndex;
            // 首帧尚无上一帧可映射，属主动丢弃而非故障
            return delivered ? CaptureOutcome.Delivered : CaptureOutcome.Dropped;
        } catch (Throwable t) {
            Constants.LOG.error("OpenGlRamPipeline.capture 异常: {}", t.getMessage());
            return CaptureOutcome.Failed;
        } finally {
            if (scissorWasEnabled) GlStateManager._enableScissorTest();
            if (blendWasEnabled) GlStateManager._enableBlend();
            if (cullWasEnabled) GlStateManager._enableCull();
            if (srgbWasEnabled) GL11.glEnable(GL30.GL_FRAMEBUFFER_SRGB);
            GL15.glBindBuffer(GL21.GL_PIXEL_PACK_BUFFER, prevPackBuffer);
            GL11.glPixelStorei(GL11.GL_PACK_ALIGNMENT, prevPackAlignment);
            GL11.glPixelStorei(GL11.GL_PACK_ROW_LENGTH, prevPackRowLength);
            GL11.glPixelStorei(GL11.GL_PACK_SKIP_ROWS, prevPackSkipRows);
            GL11.glPixelStorei(GL11.GL_PACK_SKIP_PIXELS, prevPackSkipPixels);
            GL11.glPixelStorei(GL11.GL_PACK_SWAP_BYTES, prevPackSwapBytes);
            GL30.glBindFramebuffer(GL30.GL_READ_FRAMEBUFFER, prevReadFbo);
            GL30.glBindFramebuffer(GL30.GL_DRAW_FRAMEBUFFER, prevDrawFbo);
            GL30.glUseProgram(prevProgram);
            GL30.glBindVertexArray(prevVao);
            GL13.glActiveTexture(GL13.GL_TEXTURE0);
            GL33.glBindSampler(0, prevSampler);
            GL11.glBindTexture(GL11.GL_TEXTURE_2D, prevTexture0);
            GL13.glActiveTexture(prevActiveTexture);
            GL11.glViewport(prevViewport[0], prevViewport[1], prevViewport[2], prevViewport[3]);
        }
    }

    private void cleanupResources() {
        try {
            if (hasGlContext()) {
                if (pboIds[0] != 0 || pboIds[1] != 0) {
                    GL15.glDeleteBuffers(pboIds);
                    pboIds[0] = 0;
                    pboIds[1] = 0;
                }
                if (samplerObjectId != 0) {
                    GL33.glDeleteSamplers(samplerObjectId);
                    samplerObjectId = 0;
                }
                for (int i = 0; i < 2; i++) {
                    if (lumaTextureIds[i] != 0) {
                        GL11.glDeleteTextures(lumaTextureIds[i]);
                        lumaTextureIds[i] = 0;
                    }
                    if (lumaFboIds[i] != 0) {
                        GL30.glDeleteFramebuffers(lumaFboIds[i]);
                        lumaFboIds[i] = 0;
                    }
                    if (chromaTextureIds[i] != 0) {
                        GL11.glDeleteTextures(chromaTextureIds[i]);
                        chromaTextureIds[i] = 0;
                    }
                    if (chromaFboIds[i] != 0) {
                        GL30.glDeleteFramebuffers(chromaFboIds[i]);
                        chromaFboIds[i] = 0;
                    }
                    if (readFences[i] != 0) {
                        GL32.glDeleteSync(readFences[i]);
                        readFences[i] = 0;
                    }
                }
                if (lumaProgramId != 0) {
                    GL20.glDeleteProgram(lumaProgramId);
                    lumaProgramId = 0;
                }
                if (chromaProgramId != 0) {
                    GL20.glDeleteProgram(chromaProgramId);
                    chromaProgramId = 0;
                }
                lumaSamplerLoc = -1;
                chromaSamplerLoc = -1;
                chromaTapLoc = -1;
                if (vaoId != 0) {
                    GL30.glDeleteVertexArrays(vaoId);
                    vaoId = 0;
                }
            }
            hasPreviousFrame = false;
        } catch (Throwable t) {
            Constants.LOG.error("清理 OpenGlRamPipeline 异常: {}", t.getMessage());
        }
    }

    @Override
    public void close() {
        cleanupResources();
    }
}*/
//? }

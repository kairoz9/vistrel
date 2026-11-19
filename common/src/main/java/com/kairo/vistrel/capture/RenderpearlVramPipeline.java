package com.kairo.vistrel.capture;

//? if vulkan_backend {
import com.kairo.vistrel.Constants;
import com.kairo.vistrel.streaming.EncoderChannel;
import com.kairo.vistrel.capture.BlitPipelines.FlipBlit;
//? if renderpearl_era {
import com.kairo.vistrel.mixin.FrontendGpuDeviceAccessor;
//? } else {
/*import com.kairo.vistrel.mixin.GpuDeviceBackendAccessor;*/
//? }
import com.kairo.vistrel.mixin.VulkanGpuTextureAccessor;
import com.mojang.blaze3d.pipeline.RenderTarget;
import com.mojang.blaze3d.systems.RenderSystem;
//? if renderpearl_era {
import com.mojang.blaze3d.pipeline.PipelineCache;
import com.mojang.renderpearl.api.GpuFormat;
//? } else {
/*import com.mojang.renderpearl.api.GpuFormat;*/
//? }
import com.mojang.renderpearl.api.commands.CommandEncoder;
import com.mojang.renderpearl.api.commands.RenderPass;
import com.mojang.renderpearl.api.device.GpuDevice;
import com.mojang.renderpearl.api.textures.FilterMode;
import com.mojang.renderpearl.api.textures.GpuTexture;
import com.mojang.renderpearl.api.textures.GpuTextureView;
import com.mojang.renderpearl.backend.api.GpuDeviceBackend;
import com.mojang.renderpearl.backend.vulkan.VulkanCommandEncoder;
import com.mojang.renderpearl.backend.vulkan.VulkanDevice;
import com.mojang.renderpearl.backend.vulkan.VulkanGpuTexture;
//? if renderpearl_era {
import com.mojang.renderpearl.frontend.FrontendGpuDevice;
//? }
import net.minecraft.client.Minecraft;
import org.lwjgl.PointerBuffer;
import org.lwjgl.system.MemoryStack;
import org.lwjgl.util.vma.Vma;
import org.lwjgl.util.vma.VmaAllocationCreateInfo;
import org.lwjgl.util.vma.VmaAllocationInfo;
import org.lwjgl.util.vma.VmaPoolCreateInfo;
import org.lwjgl.vulkan.KHRExternalMemoryFd;
import org.lwjgl.vulkan.KHRExternalMemoryWin32;
import org.lwjgl.vulkan.KHRExternalSemaphoreFd;
import org.lwjgl.vulkan.KHRExternalSemaphoreWin32;
import org.lwjgl.vulkan.VK10;
import org.lwjgl.vulkan.VK11;
import org.lwjgl.vulkan.VK12;
import org.lwjgl.vulkan.VkCommandBuffer;
import org.lwjgl.vulkan.VkDevice;
import org.lwjgl.vulkan.VkExportMemoryAllocateInfoKHR;
import org.lwjgl.vulkan.VkExportSemaphoreCreateInfoKHR;
import org.lwjgl.vulkan.VkExternalMemoryImageCreateInfo;
import org.lwjgl.vulkan.VkImageCreateInfo;
import org.lwjgl.vulkan.VkImageMemoryBarrier;
import org.lwjgl.vulkan.VkMemoryGetFdInfoKHR;
import org.lwjgl.vulkan.VkMemoryGetWin32HandleInfoKHR;
import org.lwjgl.vulkan.VkSemaphoreCreateInfo;
import org.lwjgl.vulkan.VkSemaphoreGetFdInfoKHR;
import org.lwjgl.vulkan.VkSemaphoreGetWin32HandleInfoKHR;
import org.lwjgl.vulkan.VkSemaphoreTypeCreateInfo;
import java.lang.foreign.MemorySegment;
import java.nio.ByteBuffer;
import java.nio.IntBuffer;
import java.nio.LongBuffer;
import java.util.Optional;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.VarHandle;

/**
 * Minecraft 26.2+ 显存直通管线（vulkan_backend 门，驱动扩展支持运行期另判），按统一的 {@link VideoPipeline} 接口实现。
 * 线程归属：capture 与 close 均在渲染线程执行（stop 会把拆除投递到渲染线程）
 */
public class RenderpearlVramPipeline implements VideoPipeline {
    public static final int SLOT_COUNT = 3;
    /**
     * 槽位纹理标签前缀：MixinVulkanGpuTexture 靠这个名字识别本管线的纹理；
     * 即使导出作用域因异常泄漏到无关的创建路径，也不会波及别的纹理分配
     */
    public static final String SLOT_TEXTURE_LABEL_PREFIX = "Vistrel Vulkan Dedicated Frame ";

    /**
     * 获取当前平台外部显存导出句柄类型 (Linux: OPAQUE_FD, Windows: OPAQUE_WIN32)
     *
     * @return {@code VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_*} 常量值
     */
    public static int getExternalMemoryHandleType() {
        return Constants.IS_WINDOWS
                ? VK11.VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT
                : VK11.VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;
    }

    /**
     * 获取当前平台外部时间线信号量导出句柄类型 (Linux: OPAQUE_FD, Windows: OPAQUE_WIN32)
     *
     * @return {@code VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_*} 常量值
     */
    public static int getExternalSemaphoreHandleType() {
        return Constants.IS_WINDOWS
                ? VK11.VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT
                : VK11.VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT;
    }

    private static final ThreadLocal<ScopedExportAllocation> ACTIVE_EXPORT_SCOPE = new ThreadLocal<>();

    /**
     * 获取当前渲染线程活动的 VRAM 导出分配作用域 (供 MixinVulkanGpuTexture 读取)
     *
     * @return 当前线程的活动作用域（栈顶）；无活动作用域时返回 {@code null}
     */
    public static ScopedExportAllocation getActiveExportScope() {
        return ACTIVE_EXPORT_SCOPE.get();
    }

    /**
     * 显存导出分配作用域 (RAII，栈序嵌套)
     */
    public static final class ScopedExportAllocation implements AutoCloseable {
        private final long exportPool;
        private final ScopedExportAllocation previous;
        private boolean closed = false;

        private ScopedExportAllocation(long exportPool, ScopedExportAllocation previous) {
            this.exportPool = exportPool;
            this.previous = previous;
        }

        public long exportPool() {
            return exportPool;
        }

        @Override
        public void close() {
            if (!closed) {
                closed = true;
                if (previous != null) {
                    ACTIVE_EXPORT_SCOPE.set(previous);
                } else {
                    ACTIVE_EXPORT_SCOPE.remove();
                }
            }
        }
    }

    private static final class VulkanSlot {
        final GpuTexture blitTexture;
        final GpuTextureView blitTextureView;
        final long vkImage;
        final long vkSemaphore;

        VulkanSlot(GpuTexture blitTexture, GpuTextureView blitTextureView, long vkImage, long vkSemaphore) {
            this.blitTexture = blitTexture;
            this.blitTextureView = blitTextureView;
            this.vkImage = vkImage;
            this.vkSemaphore = vkSemaphore;
        }
    }

    /**
     * 槽位所有权：CPU 共享块的 Java 端视图——busy 位认领/归还与心跳超时判定。
     * 块布局与换代语义见 {@link com.kairo.vistrel.streaming.EncoderChannel#slotStateBlock}
     * （跨语言契约的 Java 端唯一定义处）；本类访问走 acquire/release 与原子 CAS
     * （VarHandle 内存序模式）。
     */
    static final class SlotOwnership {
        /**
         * 心跳超时：持续推流下心跳停在基准值超过此时长，判定编码线程卡死。
         * 编码端每轮循环推进心跳，不推进只有卡死一种解释
         */
        static final long STALL_HEARTBEAT_TIMEOUT_NS = 1_000_000_000L;
        private static final long HEARTBEAT_OFFSET = 32L;
        private static final long STAGE_OFFSET = 40L;
        private static final long BUSY_STRIDE = ValueLayout.JAVA_INT.byteSize();
        /**
         * 阶段码名称：下标是跨语言约定，与 Rust `ActorStage` 的判别值一一对应。
         * 改动必须两端同步
         */
        private static final String[] STAGE_NAMES =
                {"空闲等待", "重配编码器", "排空编码器", "导入显存槽位", "注销显存槽位", "编码器关停", "等待时间线信号量", "映射输入资源", "提交编码", "锁定码流", "帧编码"};
        /**
         * 共享块字段的内存序视图：原生端序，读 acquire、写 release，认领走 CAS
         */
        private static final VarHandle BUSY_HANDLE = ValueLayout.JAVA_INT.varHandle();
        private static final VarHandle HEARTBEAT_HANDLE = ValueLayout.JAVA_LONG.varHandle();
        private static final VarHandle STAGE_HANDLE = ValueLayout.JAVA_LONG.varHandle();
        private final MemorySegment block;
        private final int slotCount;
        /**
         * 心跳基准：push 成功时记录的心跳值与时刻。判定放在每次采集 tick 里——
         * 卡死后 push 会停，判定不能跟着停
         */
        private long snapshotHeartbeat;
        private long snapshotNanos;

        SlotOwnership(MemorySegment block, int slotCount) {
            this.block = block;
            this.slotCount = slotCount;
            // 构造时先记一次当前心跳与时刻：之后心跳只要还在推进就比对不上，
            // 真卡死则一直相等，满 1s 必触发
            this.snapshotHeartbeat = heartbeat();
            this.snapshotNanos = System.nanoTime();
        }

        /**
         * 认领一个空闲槽位：CAS 0→1 原子占位，轮转优先、索引序降级；全忙返回 -1。
         *
         * <p>
         * CAS 保证多个认领者不会同时占住同一位。读到 0 就表示旧帧已归还、当前无人在用；
         * 就算位出错，最坏也只是覆盖自己邮箱里还没被取走的帧——最新帧优先本就允许，
         * 不会造成跨 API 竞态
         *
         * @return 认领的槽位索引；全忙 -1 对应主动丢帧 (Dropped)，不是故障 (Failed)
         */
        int claim(int preferredIdx) {
            if (preferredIdx >= 0 && preferredIdx < slotCount && tryClaim(preferredIdx)) {
                return preferredIdx;
            }
            for (int i = 0; i < slotCount; i++) {
                if (tryClaim(i)) {
                    return i;
                }
            }
            return -1;
        }

        /**
         * 渲染线程异常路径归还槽位（正常归还由 Actor 在编码出口完成，幂等）
         */
        void release(int slotIdx) {
            BUSY_HANDLE.setRelease(block, slotIdx * BUSY_STRIDE, 0);
        }

        /**
         * push 成功后记录心跳基准；时刻用本帧开头那个，误差只有 blit+submit 的耗时
         */
        void markPushed(long nowNanos) {
            this.snapshotHeartbeat = heartbeat();
            this.snapshotNanos = nowNanos;
        }

        /**
         * 心跳超时判定（每次采集 tick 都执行，含全忙丢帧的 tick）：
         * 心跳停在基准值且距基准已超时限
         */
        boolean heartbeatTimedOut(long nowNanos) {
            return heartbeat() == snapshotHeartbeat && nowNanos - snapshotNanos >= STALL_HEARTBEAT_TIMEOUT_NS;
        }

        /**
         * 编码线程最后进入的阶段名：心跳冻结时据此判断卡在哪一段原生调用，
         * 超出约定范围的码按原值展示
         */
        String stageName() {
            long code = (long) STAGE_HANDLE.getAcquire(block, STAGE_OFFSET);
            return code >= 0 && code < STAGE_NAMES.length ? STAGE_NAMES[(int) code] : "未知阶段(" + code + ")";
        }

        /**
         * 单槽位原子认领：读带 acquire（对上 Actor 归还时的 release 写），成功写带 release
         */
        private boolean tryClaim(int slotIdx) {
            return (int) BUSY_HANDLE.compareAndExchange(block, slotIdx * BUSY_STRIDE, 0, 1) == 0;
        }

        private long heartbeat() {
            return (long) HEARTBEAT_HANDLE.getAcquire(block, HEARTBEAT_OFFSET);
        }
    }

    private final GpuDevice device;
    private final VulkanDevice vulkanDevice;
    private final VkDevice vkDevice;
    /**
     * 设备级共享前端编码器（blit pass 用）：createCommandEncoder 返回的是设备构造时就建好的同一实例
     */
    private final CommandEncoder encoder;
    /**
     * 设备级共享后端编码器（barrier/信号量用）：与上面是同一提交流的两个 API 层
     */
    private final VulkanCommandEncoder vkEncoder;
    private final VulkanSlot[] slots = new VulkanSlot[SLOT_COUNT];
    /**
     * 由 importVramSlots 取得，负责投递与注销；构造中途失败时可能还没拿到
     */
    private EncoderChannel.SlotLease slotLease = null;
    private long globalFrameIndex = 0;
    private long exportPool = 0L;
    private VkExportMemoryAllocateInfoKHR exportAllocInfo = null;
    /**
     * 槽位所有权协议（busy 位认领/归还 + 心跳超时），跨语言约定见 SlotOwnership；
     * 块与引擎同代，每次重建管线时从引擎取一次
     */
    private final SlotOwnership ownership;
    /**
     * 槽位压力汇总上报周期，避免逐帧刷日志
     */
    private static final long SLOT_PRESSURE_REPORT_NS = 5_000_000_000L;
    private final ReportGate pressureGate = ReportGate.windowed(SLOT_PRESSURE_REPORT_NS);
    /**
     * 心跳超时已触发（一次性标记，仅渲染线程写）：此后直接停推显存帧，
     * 等待 {@link #onEncoderStall} 引擎自愈
     */
    private boolean stallTriggered = false;
    /**
     * 编码停滞自愈回调：心跳超时触发时在渲染线程调用一次，实现方必须立即移交后台线程
     */
    private final Runnable onEncoderStall;
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
     * 构造三槽位显存直通管线（RAII）：导出池、槽位纹理与时间线信号量一次建齐，
     * 并整批注册到通道（全成全败，任一环节失败逆序清理后抛出）；构造须在渲染线程执行
     *
     * @param channel        数据面通道；须已提供槽位所有权块
     * @param width          采集宽度
     * @param height         采集高度
     * @param onEncoderStall 编码停滞自愈回调，不得为 {@code null}；心跳超时判定在渲染线程
     *                       触发一次，实现方必须立即移交后台线程，回调内不得做任何显存操作
     * @throws IllegalArgumentException channel 或回调为 {@code null}
     * @throws IllegalStateException    槽位所有权块缺失、GpuDevice 不可用、后端非 Vulkan
     *                                  或翻转 blit 管线预编译失败，调用方据此降级 RAM 回读管线
     * @throws RuntimeException         槽位资源创建 / 句柄导出 / 整批注册任一环节失败
     *                                  （失败路径已清理已建资源）
     */
    public RenderpearlVramPipeline(EncoderChannel channel, int width, int height, Runnable onEncoderStall) {
        if (channel == null) {
            throw new IllegalArgumentException("EncoderChannel 不能为空");
        }
        if (onEncoderStall == null) {
            throw new IllegalArgumentException("编码停滞回调不能为空");
        }
        this.onEncoderStall = onEncoderStall;
        // 所有权块与引擎同代：每次构建管线取一次，块缺失即降级 RAM
        ByteBuffer stateBlock = channel.slotStateBlock();
        if (stateBlock == null || !stateBlock.isDirect()) {
            throw new IllegalStateException("引擎未提供槽位所有权块，显存直通降级至 RAM 回读");
        }
        this.ownership = new SlotOwnership(MemorySegment.ofBuffer(stateBlock), SLOT_COUNT);
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
        // 仅在 Vulkan 后端下构造显存直通管线，非 Vulkan (OpenGL 等) 直接抛出异常触发安全降级至 RAM 管线
        //? if renderpearl_era {
        if (!(dev instanceof FrontendGpuDevice frontend)) {
            throw new IllegalStateException("Renderpearl 运行于非 Vulkan 后端 (OpenGL/其他)，显存直通不支持，安全降级至 RAM 管线");
        }
        GpuDeviceBackend deviceBackend = ((FrontendGpuDeviceAccessor) frontend).vistrel$getBackend();
        //? } else {
        /*GpuDeviceBackend deviceBackend = ((GpuDeviceBackendAccessor) dev).vistrel$getBackend();*/
        //? }
        if (!(deviceBackend instanceof VulkanDevice vulkanDev)) {
            throw new IllegalStateException("Renderpearl 运行于非 Vulkan 后端 (OpenGL/其他)，显存直通不支持，安全降级至 RAM 管线");
        }
        this.device = dev;
        this.vulkanDevice = vulkanDev;
        this.vkDevice = vulkanDev.vkDevice();
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
        int textureUsage = GpuTexture.USAGE_RENDER_ATTACHMENT | GpuTexture.USAGE_COPY_SRC;

        try {
            if (!ensureFlipPipeline()) {
                throw new IllegalStateException("翻转 blit 管线预编译失败");
            }
            // 1. 创建携带 VkExportMemoryAllocateInfoKHR 的专用 VmaPool
            try (MemoryStack stack = MemoryStack.stackPush()) {
                VkImageCreateInfo testImgInfo = VkImageCreateInfo
                    .calloc(stack)
                    .sType$Default()
                    .imageType(VK10.VK_IMAGE_TYPE_2D)
                    .format(VK10.VK_FORMAT_R8G8B8A8_UNORM)
                    .extent(e -> e.set(width, height, 1))
                    .mipLevels(1)
                    .arrayLayers(1)
                    .samples(VK10.VK_SAMPLE_COUNT_1_BIT)
                    .tiling(VK10.VK_IMAGE_TILING_OPTIMAL)
                    .usage(VK10.VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK10.VK_IMAGE_USAGE_TRANSFER_SRC_BIT);

                VkExternalMemoryImageCreateInfo extImageInfo =
                        VkExternalMemoryImageCreateInfo
                    .calloc(stack)
                    .sType$Default()
                    .handleTypes(getExternalMemoryHandleType());
                testImgInfo.pNext(extImageInfo.address());

                VmaAllocationCreateInfo testAllocInfo = VmaAllocationCreateInfo
                    .calloc(stack)
                    .usage(Vma.VMA_MEMORY_USAGE_GPU_ONLY)
                    .flags(Vma.VMA_ALLOCATION_CREATE_DEDICATED_MEMORY_BIT);

                IntBuffer pMemType = stack.callocInt(1);
                int res =
                        Vma.vmaFindMemoryTypeIndexForImageInfo(vulkanDevice.vma(), testImgInfo, testAllocInfo, pMemType);
                if (res != VK10.VK_SUCCESS) {
                    throw new IllegalStateException(
                            "Failed to find memory type index for VMA export pool: vkResult=" + res
                    );
                }
                int memoryTypeIndex = pMemType.get(0);

                this.exportAllocInfo = VkExportMemoryAllocateInfoKHR
                    .calloc()
                    .sType$Default()
                    .handleTypes(getExternalMemoryHandleType());

                VmaPoolCreateInfo poolInfo =
                        VmaPoolCreateInfo
                    .calloc(stack)
                    .memoryTypeIndex(memoryTypeIndex)
                    .pMemoryAllocateNext(this.exportAllocInfo);

                PointerBuffer pPool = stack.callocPointer(1);
                res = Vma.vmaCreatePool(vulkanDevice.vma(), poolInfo, pPool);
                if (res != VK10.VK_SUCCESS) {
                    throw new IllegalStateException("Failed to create VMA export pool: vkResult=" + res);
                }
                this.exportPool = pPool.get(0);
            }
            // 2. 循环分配 3 槽位 Dedicated 纹理与 1:1 时间线信号量
            long[] descriptors = new long[SLOT_COUNT * EncoderChannel.VRAM_SLOT_STRIDE];
            try (ScopedExportAllocation _ = enterExportScope()) {
                for (int i = 0; i < SLOT_COUNT; i++) {
                    GpuTexture tex = device.createTexture(
                            SLOT_TEXTURE_LABEL_PREFIX + i,
                            textureUsage,
                            GpuFormat.RGBA8_UNORM,
                            width,
                            height,
                            1,
                            1
                    );

                    if (!(tex instanceof VulkanGpuTexture vkTex)) {
                        throw new IllegalStateException("Texture is not VulkanGpuTexture for slot " + i);
                    }

                    VulkanGpuTextureAccessor texAccessor = (VulkanGpuTextureAccessor) (Object) vkTex;
                    long vmaAlloc = texAccessor.vistrel$getVmaAllocation();
                    if (vmaAlloc == 0L) {
                        throw new IllegalStateException("VMA allocation handle is null for slot " + i);
                    }

                    long memHandle = 0;
                    long sizeBytes = 0;
                    long offset = 0;

                    try (MemoryStack stack = MemoryStack.stackPush()) {
                        VmaAllocationInfo allocInfo = VmaAllocationInfo.calloc(stack);
                        Vma.vmaGetAllocationInfo(vulkanDevice.vma(), vmaAlloc, allocInfo);
                        long vkDeviceMemory = allocInfo.deviceMemory();
                        sizeBytes = allocInfo.size();
                        offset = allocInfo.offset();

                        if (Constants.IS_WINDOWS) {
                            PointerBuffer pHandle = stack.callocPointer(1);
                            VkMemoryGetWin32HandleInfoKHR getWin32Info = VkMemoryGetWin32HandleInfoKHR
                                .calloc(stack)
                                .sType$Default()
                                .memory(vkDeviceMemory)
                                .handleType(VK11.VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT);

                            int res = KHRExternalMemoryWin32.vkGetMemoryWin32HandleKHR(vkDevice, getWin32Info, pHandle);
                            if (res != VK10.VK_SUCCESS || pHandle.get(0) == 0L) {
                                throw new IllegalStateException(
                                        "Failed to export Memory Win32 handle for slot " + i + ": vkResult=" + res
                                );
                            }
                            memHandle = pHandle.get(0);
                        } else {
                            IntBuffer pFd = stack.callocInt(1);
                            VkMemoryGetFdInfoKHR getFdInfo = VkMemoryGetFdInfoKHR
                                .calloc(stack)
                                .sType$Default()
                                .memory(vkDeviceMemory)
                                .handleType(VK11.VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT);

                            int res = KHRExternalMemoryFd.vkGetMemoryFdKHR(vkDevice, getFdInfo, pFd);
                            if (res != VK10.VK_SUCCESS || pFd.get(0) < 0) {
                                throw new IllegalStateException(
                                        "Failed to export Memory fd for slot " + i + ": vkResult=" + res
                                );
                            }
                            memHandle = pFd.get(0);
                        }
                    }

                    long vkSemaphore = 0L;
                    long semHandle = 0;
                    try (MemoryStack stack = MemoryStack.stackPush()) {
                        VkExportSemaphoreCreateInfoKHR expSemInfo =
                                VkExportSemaphoreCreateInfoKHR
                            .calloc(stack)
                            .sType$Default()
                            .handleTypes(getExternalSemaphoreHandleType());

                        VkSemaphoreTypeCreateInfo typeInfo = VkSemaphoreTypeCreateInfo
                            .calloc(stack)
                            .sType$Default()
                            .semaphoreType(VK12.VK_SEMAPHORE_TYPE_TIMELINE)
                            .initialValue(0L)
                            .pNext(expSemInfo.address());

                        VkSemaphoreCreateInfo semInfo =
                                VkSemaphoreCreateInfo.calloc(stack).sType$Default().pNext(typeInfo.address());

                        LongBuffer pSem = stack.callocLong(1);
                        int res = VK10.vkCreateSemaphore(vkDevice, semInfo, null, pSem);
                        if (res != VK10.VK_SUCCESS || pSem.get(0) == 0L) {
                            throw new IllegalStateException(
                                    "Failed to create exportable timeline semaphore for slot " + i + ": vkResult=" + res
                            );
                        }
                        vkSemaphore = pSem.get(0);

                        if (Constants.IS_WINDOWS) {
                            PointerBuffer pSemHandle = stack.callocPointer(1);
                            VkSemaphoreGetWin32HandleInfoKHR getSemWin32Info = VkSemaphoreGetWin32HandleInfoKHR
                                .calloc(stack)
                                .sType$Default()
                                .semaphore(vkSemaphore)
                                .handleType(VK11.VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT);

                            res = KHRExternalSemaphoreWin32.vkGetSemaphoreWin32HandleKHR(
                                    vkDevice,
                                    getSemWin32Info,
                                    pSemHandle
                            );
                            if (res != VK10.VK_SUCCESS || pSemHandle.get(0) == 0L) {
                                throw new IllegalStateException(
                                        "Failed to export Semaphore Win32 handle for slot " + i + ": vkResult=" + res
                                );
                            }
                            semHandle = pSemHandle.get(0);
                        } else {
                            int[] pSemFd = new int[1];
                            VkSemaphoreGetFdInfoKHR getSemFdInfo = VkSemaphoreGetFdInfoKHR
                                .calloc(stack)
                                .sType$Default()
                                .semaphore(vkSemaphore)
                                .handleType(VK11.VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT);

                            res = KHRExternalSemaphoreFd.vkGetSemaphoreFdKHR(vkDevice, getSemFdInfo, pSemFd);
                            if (res != VK10.VK_SUCCESS || pSemFd[0] < 0) {
                                throw new IllegalStateException(
                                        "Failed to export Semaphore fd for slot " + i + ": vkResult=" + res
                                );
                            }
                            semHandle = pSemFd[0];
                        }
                    }

                    int base = i * EncoderChannel.VRAM_SLOT_STRIDE;
                    descriptors[base] = i;
                    descriptors[base + 1] = memHandle;
                    descriptors[base + 2] = semHandle;
                    descriptors[base + 3] = width;
                    descriptors[base + 4] = height;
                    descriptors[base + 5] = sizeBytes;
                    descriptors[base + 6] = offset;

                    GpuTextureView view = device.createTextureView(tex);
                    slots[i] = new VulkanSlot(tex, view, vkTex.vkImage(), vkSemaphore);
                }
            }
            // 3. 一次性注册整批槽位 (要么全成要么全败)，失败由通道抛出且 Native 侧已整批回滚
            this.slotLease = channel.importVramSlots(descriptors);
        } catch (Throwable t) {
            cleanupResources();
            if (t instanceof RuntimeException re) {
                throw re;
            }
            throw new RuntimeException("初始化 Renderpearl 显存直通管线异常", t);
        }
    }

    private ScopedExportAllocation enterExportScope() {
        if (this.exportPool == 0L) {
            throw new IllegalStateException("Cannot enter export scope: exportPool is not initialized");
        }
        ScopedExportAllocation scope = new ScopedExportAllocation(this.exportPool, ACTIVE_EXPORT_SCOPE.get());
        ACTIVE_EXPORT_SCOPE.set(scope);
        return scope;
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
     * 心跳超时：判定编码线程卡死，一次性停推显存帧并移交自愈回调。
     * 此刻编码器可能仍映射着槽位纹理，任何写入都是跨 API 竞态，必须立即收手
     */
    private void onHeartbeatTimeout() {
        if (stallTriggered) {
            return;
        }
        stallTriggered = true;
        Constants.LOG.error(
                "显存直通停滞：{}s 内编码线程心跳冻结（最后阶段：{}），判定编码线程死亡，停推显存帧并触发引擎自愈",
                SlotOwnership.STALL_HEARTBEAT_TIMEOUT_NS / 1_000_000_000L,
                ownership.stageName()
        );
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

        int claimed = -1;
        try {
            // 停滞判定每次采集 tick 都执行（含全忙丢帧的 tick）：卡死后 push 会停，判定不能跟着停。
            // 心跳代表编码线程还活着——Actor 每轮循环都会推进，不推进只有卡死一种解释
            long nowNanos = System.nanoTime();
            if (ownership.heartbeatTimedOut(nowNanos)) {
                CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PREP, prepStart);
                onHeartbeatTimeout();
                return CaptureOutcome.Dropped;
            }
            int preferredIdx = (int) (globalFrameIndex % SLOT_COUNT);
            VulkanSlot preferred = slots[preferredIdx];
            if (preferred == null || preferred.blitTextureView == null) {
                CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PREP, prepStart);
                return CaptureOutcome.Failed;
            }
            // 槽位所有权：读到 0 即占位为 1，绝不覆写编码器可能仍在读取的显存。
            // 全忙是**主动**丢帧：管线健康，只是编码器暂时没消费完。必须与 Failed
            // 区分——调度器若把它当故障，一次槽位压力就会永久降级到 RAM 回读
            int slotIdx = ownership.claim(preferredIdx);
            if (slotIdx < 0) {
                CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PREP, prepStart);
                pressureGate.accumulate(1L);
                reportSlotPressure();
                return CaptureOutcome.Dropped;
            }
            claimed = slotIdx;
            VulkanSlot slot = slots[slotIdx];
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
            try {
                this.slotLease.push(slotIdx, signalVal);
            } catch (Throwable pushError) {
                // submit 已在途，信号量必被信号到 signalVal：帧计数照常推进
                globalFrameIndex++;
                throw pushError;
            } finally {
                CaptureProfiler.end(CaptureProfiler.Stage.VRAM_PUSH, stageStart);
            }
            // 认领位随帧移交给编码器：编码出口（成功/失败）归还。此处记录心跳基准，
            // 时刻用本帧开头那个，误差只是 blit+submit 的耗时（~0.1ms）
            ownership.markPushed(nowNanos);
            globalFrameIndex++;
            return CaptureOutcome.Delivered;
        } catch (Throwable t) {
            // push 之前的任何异常路径归还认领位：帧没交出去，编码器不会来读
            if (claimed >= 0) {
                ownership.release(claimed);
            }
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
        // 注销必须等到原生侧真正确认：没有租约 = 从未导入，游戏资源可放心释放；
        // 有租约但未确认（编码线程卡死 ACK 超时 / 控制队列满）时，编码器还持有
        // 导出纹理/信号量的跨 API 导入并在上面等待，此刻销毁是驱动层
        // 未定义行为。GpuTexture 没有 Cleaner，丢掉引用不会触发释放，
        // 这些资源随 VkDevice 在引擎关停时统一回收，
        // 代价只是本次会话泄漏 3 张纹理与导出池——好过整卡挂死。
        boolean released = true;
        if (this.slotLease != null) {
            try {
                this.slotLease.close();
                released = this.slotLease.isReleased();
            } catch (Throwable t) {
                Constants.LOG.warn("Failed to release VRAM slot lease: {}", t.getMessage());
                released = false;
            }
        }
        this.slotLease = null;
        if (!released) {
            Constants.LOG.error("显存槽位注销未获原生侧确认，导出纹理/信号量/导出池保留不销毁 (随引擎关停统一回收)");
            return;
        }

        for (int i = 0; i < SLOT_COUNT; i++) {
            VulkanSlot slot = slots[i];
            if (slot != null) {
                if (slot.blitTextureView != null) {
                    try {
                        slot.blitTextureView.close();
                    } catch (Throwable ignored) {
                    }
                }
                if (slot.blitTexture != null) {
                    try {
                        slot.blitTexture.close();
                    } catch (Throwable ignored) {
                    }
                }
                if (slot.vkSemaphore != 0 && vkDevice != null) {
                    try {
                        VK10.vkDestroySemaphore(vkDevice, slot.vkSemaphore, null);
                    } catch (Throwable ignored) {
                    }
                }
                slots[i] = null;
            }
        }

        if (exportPool != 0L && vulkanDevice != null) {
            try {
                Vma.vmaDestroyPool(vulkanDevice.vma(), exportPool);
            } catch (Throwable ignored) {
            }
            exportPool = 0L;
        }

        if (exportAllocInfo != null) {
            try {
                exportAllocInfo.free();
            } catch (Throwable ignored) {
            }
            exportAllocInfo = null;
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

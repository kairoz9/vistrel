package kairo.vistrel.capture;

//? if vulkan_backend {
import kairo.vistrel.Constants;
import kairo.vistrel.streaming.EncoderChannel.VramSlotDescriptor;
import com.mojang.renderpearl.api.device.GpuDevice;
import com.mojang.renderpearl.api.GpuFormat;
import com.mojang.renderpearl.api.textures.GpuTexture;
import com.mojang.renderpearl.api.textures.GpuTextureView;
import com.mojang.renderpearl.backend.vulkan.VulkanDevice;
import com.mojang.renderpearl.backend.vulkan.VulkanGpuTexture;
import kairo.vistrel.mixin.VulkanGpuTextureAccessor;
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
import org.lwjgl.vulkan.VkDevice;
import org.lwjgl.vulkan.VkExportMemoryAllocateInfoKHR;
import org.lwjgl.vulkan.VkExportSemaphoreCreateInfoKHR;
import org.lwjgl.vulkan.VkExternalMemoryImageCreateInfo;
import org.lwjgl.vulkan.VkImageCreateInfo;
import org.lwjgl.vulkan.VkMemoryGetFdInfoKHR;
import org.lwjgl.vulkan.VkMemoryGetWin32HandleInfoKHR;
import org.lwjgl.vulkan.VkSemaphoreCreateInfo;
import org.lwjgl.vulkan.VkSemaphoreGetFdInfoKHR;
import org.lwjgl.vulkan.VkSemaphoreGetWin32HandleInfoKHR;
import org.lwjgl.vulkan.VkSemaphoreTypeCreateInfo;
import java.nio.IntBuffer;
import java.nio.LongBuffer;
import java.util.ArrayList;
import java.util.List;

/**
 * 几何绑定的显存导出集：同一采集尺寸下的导出池、槽位纹理、纹理视图与时间线信号量。
 *
 * <p>导出集不感知编码器代次：复用、弃置与新建由显存管线决定。
 *
 * <p>线程归属：构造、绑定与释放都在渲染线程；{@link #close()} 只能在该导出集已确认不再被
 * Native 引用时调用。
 */
public final class VramExportSet {
    /**
     * 槽位环容量：采集侧的三重缓冲选择，不是线协议常量——Native 按实际导入的槽位数
     * 工作，只要求不超过槽位掩码容量。Rust 侧同槽数的导出替身对齐此值。
     */
    static final int SLOT_COUNT = 3;

    /**
     * 单个导出槽位的游戏侧资源：纹理、视图以及导出句柄对应的原生对象。
     *
     * <p>字段在构造过程中逐步填充，一旦持有任一原生资源就立即挂到 {@link #slots}
     * 上，使后续任一步失败都能由统一清理路径逆序释放。
     */
    static final class VulkanSlot {
        final GpuTexture blitTexture;
        GpuTextureView blitTextureView;
        final long vkImage;
        long vkSemaphore;

        VulkanSlot(GpuTexture blitTexture, GpuTextureView blitTextureView, long vkImage, long vkSemaphore) {
            this.blitTexture = blitTexture;
            this.blitTextureView = blitTextureView;
            this.vkImage = vkImage;
            this.vkSemaphore = vkSemaphore;
        }
    }

    private final VulkanDevice vulkanDevice;
    private final VkDevice vkDevice;
    private final VulkanSlot[] slots = new VulkanSlot[SLOT_COUNT];
    private final List<VramSlotDescriptor> descriptors;
    private long exportPool = 0L;
    private VkExportMemoryAllocateInfoKHR exportAllocInfo = null;
    private boolean closed = false;

    /**
     * 构造三槽位导出集（RAII）：导出池、槽位纹理与时间线信号量一次建齐；
     * 任一环节失败逆序清理后抛出。构造须在渲染线程执行
     *
     * @param device        Renderpearl 设备
     * @param vulkanDevice  Vulkan 后端
     * @param width         采集宽度
     * @param height        采集高度
     * @throws RuntimeException 导出池 / 纹理 / 信号量 / 句柄导出任一环节失败
     *                          （失败路径已清理已建资源）
     */
    VramExportSet(GpuDevice device, VulkanDevice vulkanDevice, int width, int height) {
        this.vulkanDevice = vulkanDevice;
        this.vkDevice = vulkanDevice.vkDevice();
        int textureUsage = GpuTexture.USAGE_RENDER_ATTACHMENT | GpuTexture.USAGE_COPY_SRC;
        List<VramSlotDescriptor> built = new ArrayList<>(SLOT_COUNT);
        try {
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

                VkExternalMemoryImageCreateInfo extImageInfo = VkExternalMemoryImageCreateInfo
                    .calloc(stack)
                    .sType$Default()
                    .handleTypes(VramExportScope.memoryHandleType());
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
                    .handleTypes(VramExportScope.memoryHandleType());

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
            try (VramExportScope _ = VramExportScope.open(exportPool)) {
                for (int i = 0; i < SLOT_COUNT; i++) {
                    GpuTexture tex =
                            device.createTexture(
                                    VramExportScope.slotLabel(i),
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
                    // 纹理一建成即登记：后续校验/句柄导出/信号量创建失败时由统一清理路径释放
                    VulkanSlot slot = new VulkanSlot(tex, null, vkTex.vkImage(), 0L);
                    slots[i] = slot;
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
                        VkExportSemaphoreCreateInfoKHR expSemInfo = VkExportSemaphoreCreateInfoKHR
                            .calloc(stack)
                            .sType$Default()
                            .handleTypes(VramExportScope.semaphoreHandleType());

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
                        // 信号量一建成即登记，后续句柄导出失败也由统一清理路径销毁
                        slot.vkSemaphore = vkSemaphore;

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

                    built.add(new VramSlotDescriptor(i, memHandle, semHandle, width, height, sizeBytes, offset));
                    slot.blitTextureView = device.createTextureView(tex);
                }
            }
            this.descriptors = List.copyOf(built);
        } catch (Throwable t) {
            closeResources();
            throw t;
        }
    }

    /**
     * 整批槽位描述：索引、导出句柄、几何与像素偏移，按槽位序排列
     *
     * @return 不可变槽位描述列表
     */
    List<VramSlotDescriptor> descriptors() {
        return descriptors;
    }

    /**
     * 取指定槽位的游戏侧资源
     *
     * @param index 槽位索引
     * @return 槽位资源
     */
    VulkanSlot slot(int index) {
        return slots[index];
    }

    /**
     * 释放导出集全部资源：只在 Native 已确认注销后调用；资源释放顺序为逆构造序。
     */
    void close() {
        if (!closed) {
            closed = true;
            closeResources();
        }
    }

    /**
     * 释放已建资源；构造失败路径也走这里，因此必须容忍半成品状态。
     */
    private void closeResources() {
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
}
//? } else {
/*public final class VramExportSet {
}*/
//? }

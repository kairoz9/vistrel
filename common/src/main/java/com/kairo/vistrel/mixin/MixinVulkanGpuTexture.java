package com.kairo.vistrel.mixin;

//? if vulkan_backend {
import com.kairo.vistrel.capture.RenderpearlVramPipeline;
import com.llamalad7.mixinextras.sugar.Local;
import com.mojang.renderpearl.api.GpuFormat;
import com.mojang.renderpearl.backend.vulkan.VulkanDevice;
import com.mojang.renderpearl.backend.vulkan.VulkanGpuTexture;
import org.lwjgl.system.MemoryStack;
import org.lwjgl.util.vma.Vma;
import org.lwjgl.util.vma.VmaAllocationCreateInfo;
import org.lwjgl.vulkan.VkExternalMemoryImageCreateInfo;
import org.lwjgl.vulkan.VkImageCreateInfo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(value = VulkanGpuTexture.class, remap = false)
public abstract class MixinVulkanGpuTexture {
    @Inject(
            method = "<init>",
            at = @At(
                    value = "INVOKE",
                    target = "Lorg/lwjgl/util/vma/Vma;"
                    + "vmaCreateImage(JLorg/lwjgl/vulkan/VkImageCreateInfo;"
                    + "Lorg/lwjgl/util/vma/VmaAllocationCreateInfo;Ljava/nio/LongBuffer;Lorg/lwjgl/PointerBuffer;"
                    + "Lorg/lwjgl/util/vma/VmaAllocationInfo;)I"
            )
    )
    private void vistrel$injectExportFlags(
            VulkanDevice device,
            int usage,
            String label,
            GpuFormat format,
            int width,
            int height,
            int depth,
            int mipLevels,
            CallbackInfo ci,
            @Local VkImageCreateInfo imageCreateInfo,
            @Local VmaAllocationCreateInfo allocInfo
    ) {
        RenderpearlVramPipeline.ScopedExportAllocation exportScope = RenderpearlVramPipeline.getActiveExportScope();
        // 名字匹配：即使导出作用域因异常泄漏到
        // 无关创建路径，也只有本管线的槽位纹理会被收进导出池
        if (exportScope != null
                && label != null
                && label.startsWith(RenderpearlVramPipeline.SLOT_TEXTURE_LABEL_PREFIX)) {
            VkExternalMemoryImageCreateInfo extInfo = VkExternalMemoryImageCreateInfo
                .calloc(MemoryStack.stackGet())
                .sType$Default()
                .handleTypes(RenderpearlVramPipeline.getExternalMemoryHandleType());

            extInfo.pNext(imageCreateInfo.pNext());
            imageCreateInfo.pNext(extInfo.address());

            allocInfo.flags(allocInfo.flags() | Vma.VMA_ALLOCATION_CREATE_DEDICATED_MEMORY_BIT);
            if (exportScope.exportPool() != 0L) {
                allocInfo.pool(exportScope.exportPool());
            }
        }
    }
}
//? } else {
/*public abstract class MixinVulkanGpuTexture {
}*/
//? }

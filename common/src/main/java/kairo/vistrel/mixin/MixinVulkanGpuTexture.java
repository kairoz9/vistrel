package kairo.vistrel.mixin;

//? if vulkan_backend {
import kairo.vistrel.capture.VramExportScope;
import com.llamalad7.mixinextras.sugar.Local;
import com.mojang.renderpearl.api.GpuFormat;
import com.mojang.renderpearl.backend.vulkan.VulkanDevice;
import com.mojang.renderpearl.backend.vulkan.VulkanGpuTexture;
import org.lwjgl.util.vma.VmaAllocationCreateInfo;
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
        VramExportScope.markExportable(label, imageCreateInfo, allocInfo);
    }
}
//? } else {
/*public abstract class MixinVulkanGpuTexture {
}*/
//? }

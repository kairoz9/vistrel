package com.kairo.vistrel.mixin;

//? if vulkan_backend {
import com.mojang.renderpearl.backend.vulkan.VulkanDevice;
import com.mojang.renderpearl.backend.vulkan.VulkanGpuTexture;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

@Mixin(value = VulkanGpuTexture.class, remap = false)
public interface VulkanGpuTextureAccessor {
    @Accessor("vmaAllocation")
    long vistrel$getVmaAllocation();

    @Accessor("device")
    VulkanDevice vistrel$getDevice();
}
//? } else {
/*public interface VulkanGpuTextureAccessor {
}*/
//? }

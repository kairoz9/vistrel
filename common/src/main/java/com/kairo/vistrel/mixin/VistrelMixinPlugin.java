package com.kairo.vistrel.mixin;

import org.objectweb.asm.tree.ClassNode;
import org.spongepowered.asm.mixin.extensibility.IMixinConfigPlugin;
import org.spongepowered.asm.mixin.extensibility.IMixinInfo;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;

/**
 * 跨版本 Mixin 动态配置插件
 */
public class VistrelMixinPlugin implements IMixinConfigPlugin {
    @Override
    public void onLoad(String mixinPackage) {}

    @Override
    public String getRefMapperConfig() {
        return null;
    }

    @Override
    public boolean shouldApplyMixin(String targetClassName, String mixinClassName) {
        if (mixinClassName.contains("Vulkan") || mixinClassName.contains("FrontendGpuDevice")) {
            try {
                String resourcePath = targetClassName.replace('.', '/') + ".class";
                return getClass().getClassLoader().getResource(resourcePath) != null;
            } catch (Throwable t) {
                return false;
            }
        }
        //? if !renderpearl_era {
        /*if (mixinClassName.contains("MixinMouseHandler")) {
            return false;
        }*/
        //? }
        return true;
    }

    @Override
    public void acceptTargets(Set<String> myTargets, Set<String> otherTargets) {}

    @Override
    public List<String> getMixins() {
        List<String> mixins = new ArrayList<>();
        // 采集时机：26.1+ 的限频器独立成类，旧版仍在 RenderSystem 上
        //? if gpu_device {
        mixins.add("MixinFramerateLimiter");
        //? } else {
        /*mixins.add("MixinRenderSystem");*/
        //? }
        //? if renderpearl_era {
        mixins.addAll(List.of(
                "MixinVulkanFeatureSets",
                "FrontendGpuDeviceAccessor",
                "VulkanGpuTextureAccessor",
                "MixinVulkanGpuTexture",
                "MixinBlaze3D"
        ));
        //? } elif vulkan_backend {
        /*mixins.addAll(List.of(
                "MixinVulkanBackend",
                "GpuDeviceBackendAccessor",
                "VulkanGpuTextureAccessor",
                "MixinVulkanGpuTexture",
                "MixinUtilOs"
        ));*/
        //? } else {
        /*mixins.add("MixinUtilOs");*/
        //? }
        return mixins;
    }

    @Override
    public void preApply(String targetClassName, ClassNode targetClass, String mixinClassName, IMixinInfo mixinInfo) {}

    @Override
    public void postApply(String targetClassName, ClassNode targetClass, String mixinClassName, IMixinInfo mixinInfo) {}
}

package kairo.vistrel.mixin;

//? if renderpearl_era {
import kairo.vistrel.Constants;
import com.llamalad7.mixinextras.injector.ModifyReturnValue;
import com.mojang.renderpearl.backend.vulkan.VulkanFeatureSets;
import com.mojang.renderpearl.backend.vulkan.init.FeatureSet;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import java.util.HashSet;
import java.util.Set;

@Mixin(value = VulkanFeatureSets.class, remap = false)
public abstract class MixinVulkanFeatureSets {
    private static final FeatureSet VISTREL_EXPORT_FEATURE_SET = new FeatureSet(
            "Vistrel_Vulkan_Export",
            Constants.IS_WINDOWS
            ? Set.of("VK_KHR_external_memory_win32", "VK_KHR_external_semaphore_win32")
            : Set.of("VK_KHR_external_memory_fd", "VK_KHR_external_semaphore_fd"),
            Set.of()
    );

    @ModifyReturnValue(method = "optionalFeatureSets", at = @At("RETURN"))
    private static Set<FeatureSet> vistrel$addExportFeatureSets(Set<FeatureSet> original) {
        Set<FeatureSet> modified = new HashSet<>(original);
        modified.add(VISTREL_EXPORT_FEATURE_SET);
        return modified;
    }
}
//? } else {
/*public abstract class MixinVulkanFeatureSets {
}*/
//? }

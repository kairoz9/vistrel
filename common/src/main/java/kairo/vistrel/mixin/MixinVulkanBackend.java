package kairo.vistrel.mixin;

//? if vulkan_backend && !renderpearl_era {
/*import kairo.vistrel.Constants;
import com.mojang.blaze3d.shaders.GpuDebugOptions;
import com.mojang.blaze3d.shaders.ShaderSource;
import com.mojang.blaze3d.vulkan.VulkanPhysicalDevice;
import com.mojang.blaze3d.vulkan.VulkanBackend;
import com.mojang.blaze3d.vulkan.init.VulkanFeature;
import java.util.Collection;
import java.util.HashSet;
import java.util.Set;
import org.lwjgl.vulkan.VkDevice;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Invoker;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Redirect;

// 26.2 的 Vulkan 设备扩展注入缝：VulkanBackend.createDevice 从固化的
// REQUIRED_DEVICE_EXTENSIONS 集合构造设备，没有 26.3 的 FeatureSet 声明式机制。
// 在私有重载调用点改写集合，按驱动支持追加外部显存/信号量扩展——
// 驱动不支持的自然缺席，vramDirect 构造期自行降级
@Mixin(value = VulkanBackend.class, remap = false)
public abstract class MixinVulkanBackend {
    @Invoker("createDevice")
    static VkDevice vistrel$invokeCreateDevice(
            Collection<String> deviceExtensions,
            VulkanPhysicalDevice physicalDevice,
            Set<VulkanFeature> vulkanFeatures
    ) throws com.mojang.blaze3d.systems.BackendCreationException {
        throw new AssertionError();
    }

    @Redirect(
            method = "createDevice(JLcom/mojang/blaze3d/shaders/ShaderSource;"
                    + "Lcom/mojang/blaze3d/shaders/GpuDebugOptions;Ljava/lang/Runnable;)"
                    + "Lcom/mojang/blaze3d/systems/GpuDevice;",
            at = @At(value = "INVOKE",
                    target = "Lcom/mojang/blaze3d/vulkan/VulkanBackend;createDevice("
                            + "Ljava/util/Collection;Lcom/mojang/blaze3d/vulkan/VulkanPhysicalDevice;"
                            + "Ljava/util/Set;)Lorg/lwjgl/vulkan/VkDevice;"),
            remap = false
    )
    private static VkDevice vistrel$injectExternalExtensions(
            Collection<String> deviceExtensions,
            VulkanPhysicalDevice physicalDevice,
            Set<VulkanFeature> vulkanFeatures
    ) throws com.mojang.blaze3d.systems.BackendCreationException {
        Set<String> patched = new HashSet<>(deviceExtensions);
        String memoryExt = Constants.IS_WINDOWS
                ? "VK_KHR_external_memory_win32"
                : "VK_KHR_external_memory_fd";
        String semaphoreExt = Constants.IS_WINDOWS
                ? "VK_KHR_external_semaphore_win32"
                : "VK_KHR_external_semaphore_fd";
        if (physicalDevice.hasDeviceExtension(memoryExt)) {
            patched.add(memoryExt);
        }
        if (physicalDevice.hasDeviceExtension(semaphoreExt)) {
            patched.add(semaphoreExt);
        }
        return vistrel$invokeCreateDevice(patched, physicalDevice, vulkanFeatures);
    }
}*/
//? }

package kairo.vistrel.mixin;

//? if gpu_device && !renderpearl_era {
/*import com.mojang.renderpearl.api.device.GpuDevice;
import com.mojang.renderpearl.backend.api.GpuDeviceBackend;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

// 26.1/26.2 的设备后端访问链：GpuDevice 为具体类，backend 字段直连后端实现
//（26.3 起由 FrontendGpuDeviceAccessor 接管同一职责）
@Mixin(value = GpuDevice.class, remap = false)
public interface GpuDeviceBackendAccessor {
    @Accessor("backend")
    GpuDeviceBackend vistrel$getBackend();
}*/
//? }

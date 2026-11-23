package kairo.vistrel.mixin;

//? if renderpearl_era {
import com.mojang.renderpearl.backend.api.GpuDeviceBackend;
import com.mojang.renderpearl.frontend.FrontendGpuDevice;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

@Mixin(value = FrontendGpuDevice.class, remap = false)
public interface FrontendGpuDeviceAccessor {
    @Accessor("backend")
    GpuDeviceBackend vistrel$getBackend();
}
//? } else {
/*public interface FrontendGpuDeviceAccessor {
}*/
//? }

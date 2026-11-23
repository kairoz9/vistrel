package kairo.vistrel.mixin;

//? if !gpu_device {
/*import kairo.vistrel.VistrelService;
import com.mojang.blaze3d.systems.RenderSystem;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/^*
 * 把采集时机提前到帧限频器入睡之前（1.20 ~ 1.21.9）
 *
 * 限频器在窗口交换之后、runTick 尾部之前被调用；在此提交采集可让编码器在睡眠窗口内工作，
 * 而不是等睡眠结束才拿到帧。framerateLimit >= 260 时限频器不被调用，由 runTick 尾部兜底
 ^/
@Mixin(RenderSystem.class)
public class MixinRenderSystem {
    @Inject(at = @At("HEAD"), method = "limitDisplayFPS")
    private static void onLimitDisplayFPS(int limit, CallbackInfo info) {
        VistrelService.onBeforeFrameLimitSleep();
    }
}
*/
//? }

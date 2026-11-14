package com.kairo.vistrel.mixin;

//? if renderpearl_era {
import com.kairo.vistrel.VistrelService;
import com.mojang.blaze3d.Blaze3D;
import java.net.URI;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/**
 * 26.3 起打开外部网址的统一汇聚点：串流中 http/https 透传远程浏览器并取消本地打开，
 * 其余协议（如 file:// 打开文件夹）保留宿主行为
 */
@Mixin(value = Blaze3D.class, remap = false)
public abstract class MixinBlaze3D {
    @Inject(method = "openUri(Ljava/net/URI;)V", at = @At("HEAD"), cancellable = true)
    private static void forwardOpenUriWhenStreaming(URI uri, CallbackInfo ci) {
        if (VistrelService.forwardOpenUrl(uri)) {
            ci.cancel();
        }
    }
}
//? }

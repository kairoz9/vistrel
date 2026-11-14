package com.kairo.vistrel.mixin;

//? if <26.3 {
/*import com.kairo.vistrel.VistrelService;
import java.net.URI;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;*/
//? }
/**
 * 串流中接管 vanilla 打开外部网址：openUri 是聊天链接、确认屏确认与文件打开的统一汇聚点
 *
 * <p>http/https 透传远程浏览器后取消本地打开；其余协议（如 file:// 打开文件夹）保留行为。
 * fabric 侧同代两个名字落在同一个 intermediary 类上。26.3 起汇聚点搬到 Blaze3D.openUri
 * （见 MixinBlaze3D）
 */
//? if >=1.21.9 && <26.3 {
/*@Mixin(targets = {"net.minecraft.util.Util$OS", "net.minecraft.Util$OS"})*/
//? } elif <1.21.9 {
/*@Mixin(net.minecraft.Util.OS.class)*/
//? }
//? if <26.3 {
/*public abstract class MixinUtilOs {
    @Inject(method = "openUri(Ljava/net/URI;)V", at = @At("HEAD"), cancellable = true)
    private void forwardOpenUriWhenStreaming(URI uri, CallbackInfo ci) {
        if (VistrelService.forwardOpenUrl(uri)) {
            ci.cancel();
        }
    }
}*/
//? }

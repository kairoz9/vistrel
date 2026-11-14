package com.kairo.vistrel.mixin;

import com.kairo.vistrel.VistrelService;
import net.minecraft.client.KeyboardHandler;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/**
 * 旁听 vanilla 剪贴板写入：串流中同步转发远程浏览器
 *
 * <p>非取消注入——本地写入照常执行，维持游戏内复制后 Ctrl+V 粘贴闭环
 */
@Mixin(KeyboardHandler.class)
public class MixinKeyboardHandler {
    @Inject(method = "setClipboard(Ljava/lang/String;)V", at = @At("HEAD"))
    private void onSetClipboard(String clipboard, CallbackInfo ci) {
        VistrelService.onClipboardWritten(clipboard);
    }
}

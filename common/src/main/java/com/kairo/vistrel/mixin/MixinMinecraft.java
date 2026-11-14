package com.kairo.vistrel.mixin;

import com.llamalad7.mixinextras.injector.ModifyReturnValue;
import net.minecraft.client.Minecraft;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(Minecraft.class)
public class MixinMinecraft {
    @Inject(at = @At("TAIL"), method = "<init>")
    private void init(CallbackInfo info) {}

    //? if gpu_device {
    /**
     * 串流中屏蔽失焦自动暂停：26.1+ 的 pauseIfInactive 直接查 Window.isFocused()，
     * 绕过 isWindowActive 伪装；不取消则「回到游戏」关屏后下一 tick 又被暂停
     */
    @Inject(at = @At("HEAD"), method = "pauseIfInactive", cancellable = true)
    private void onPauseIfInactive(CallbackInfo info) {
        if (com.kairo.vistrel.VistrelService.isStreaming()) {
            info.cancel();
        }
    }

    //? }
    // runTick 是各版本共同的主循环入口；26.3 的 renderFrame 由它调用，
    // 额外调用点（加载/Overlay/resize）不承载串流输入与采集语义
    @Inject(at = @At("HEAD"), method = "runTick")
    private void onRunTickHead(boolean renderLevel, CallbackInfo info) {
        com.kairo.vistrel.VistrelService.onRenderFrameHead();
    }

    @Inject(at = @At("TAIL"), method = "runTick")
    private void onRunTick(boolean renderLevel, CallbackInfo info) {
        com.kairo.vistrel.VistrelService.onRenderFrameTail();
    }

    /**
     * 串流中强制窗口活跃为真：链式改写返回值，与其他 mod 的同类注入可叠加且与顺序无关
     */
    @ModifyReturnValue(method = "isWindowActive", at = @At("RETURN"))
    private boolean spoofWindowActiveWhenStreaming(boolean original) {
        return com.kairo.vistrel.VistrelService.isStreaming() || original;
    }

    @Inject(at = @At("HEAD"), method = "close")
    private void onClose(CallbackInfo info) {
        com.kairo.vistrel.VistrelService.stop();
    }
}

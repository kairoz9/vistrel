package com.kairo.vistrel.mixin;

import com.kairo.vistrel.VistrelService;
import net.minecraft.client.MouseHandler;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(MouseHandler.class)
public class MixinMouseHandler {
    //? if renderpearl_era {
    @Inject(method = "resyncMousePosition", at = @At("HEAD"), cancellable = true)
    private void cancelResyncWhenStreaming(CallbackInfo ci) {
        if (VistrelService.isStreaming()) {
            ci.cancel();
        }
    }
    //? }
}

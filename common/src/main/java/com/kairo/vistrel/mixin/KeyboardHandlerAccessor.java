package com.kairo.vistrel.mixin;

import net.minecraft.client.KeyboardHandler;
import org.spongepowered.asm.mixin.Mixin;

//? if >=1.21.9 && <26.3 {
/*import net.minecraft.client.input.CharacterEvent;
import net.minecraft.client.input.KeyEvent;*/
//? }
@Mixin(KeyboardHandler.class)
public interface KeyboardHandlerAccessor {
    //? if <1.21.9 {
    /*@org.spongepowered.asm.mixin.gen.Invoker("charTyped")
    void vistrel$charTyped(long window, int codePoint, int modifiers);*/
    //? } elif <26.3 {
    /*@org.spongepowered.asm.mixin.gen.Invoker("charTyped")
    void vistrel$charTyped(long window, CharacterEvent characterEvent);

    @org.spongepowered.asm.mixin.gen.Invoker("keyPress")
    void vistrel$keyPress(long window, int action, KeyEvent keyEvent);*/
    //? }
}

package com.kairo.vistrel.mixin;

import net.minecraft.client.MouseHandler;
import org.spongepowered.asm.mixin.Mixin;

//? if >=1.21.9 && <26.3 {
/*import net.minecraft.client.input.MouseButtonInfo;*/
//? }
@Mixin(MouseHandler.class)
public interface MouseHandlerAccessor {
    //? if <1.21.9 {
    /*@org.spongepowered.asm.mixin.gen.Invoker("onPress")
    void vistrel$onPress(long window, int button, int action, int mods);*/
    //? } elif <26.3 {
    /*@org.spongepowered.asm.mixin.gen.Invoker("onButton")
    void vistrel$onPress(long window, MouseButtonInfo buttonInfo, int action);*/
    //? }
    //? if <26.3 {
    /*@org.spongepowered.asm.mixin.gen.Invoker("onMove")
    void vistrel$onMove(long window, double xpos, double ypos);

    @org.spongepowered.asm.mixin.gen.Invoker("onScroll")
    void vistrel$onScroll(long window, double xoffset, double yoffset);*/
    //? }
}

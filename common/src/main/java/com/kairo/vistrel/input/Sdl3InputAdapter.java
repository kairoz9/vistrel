package com.kairo.vistrel.input;

//? if >=26.3 {
import com.mojang.blaze3d.platform.InputConstants;
import net.minecraft.client.Minecraft;
import net.minecraft.client.input.KeyEvent;
import net.minecraft.client.input.MouseButtonInfo;
import org.lwjgl.sdl.SDLKeyboard;
import org.lwjgl.sdl.SDLKeycode;

/**
 * SDL3 输入适配器 (适用于 Minecraft 26.3+)
 * 采用 SDL_SCANCODE、SDLKeycode 与 KeyEvent / MouseButtonInfo 规范
 */
public class Sdl3InputAdapter extends InputAdapter {
    @Override
    public long windowHandle(com.mojang.blaze3d.platform.Window window) {
        return window.handle();
    }

    @Override
    protected void updateModifiers(int scancode, int action) {
        int mask = switch (scancode) {
            case HID_KEY_LEFT_SHIFT -> SDLKeycode.SDL_KMOD_LSHIFT;
            case HID_KEY_RIGHT_SHIFT -> SDLKeycode.SDL_KMOD_RSHIFT;
            case HID_KEY_LEFT_CONTROL -> SDLKeycode.SDL_KMOD_LCTRL;
            case HID_KEY_RIGHT_CONTROL -> SDLKeycode.SDL_KMOD_RCTRL;
            case HID_KEY_LEFT_ALT -> SDLKeycode.SDL_KMOD_LALT;
            case HID_KEY_RIGHT_ALT -> SDLKeycode.SDL_KMOD_RALT;
            case HID_KEY_LEFT_SUPER -> SDLKeycode.SDL_KMOD_LGUI;
            case HID_KEY_RIGHT_SUPER -> SDLKeycode.SDL_KMOD_RGUI;
            default -> 0;
        };
        applyModifierMask(mask, action);
    }

    @Override
    protected void doInjectKey(Minecraft mc, long windowHandle, int scancode, int rawAction) {
        int action = (rawAction == ACTION_REPEAT) ? -1 : rawAction;
        int sdlKey = SDLKeyboard.SDL_GetKeyFromScancode(scancode, (short) 0, false);
        if (sdlKey == 0) {
            sdlKey = scancode;
        }

        KeyEvent keyEvent = new KeyEvent(scancode, sdlKey, currentModifiers);
        mc.keyboardHandler.keyPress(windowHandle, action, keyEvent);
    }

    @Override
    protected void doInjectMouseButton(Minecraft mc, long windowHandle, int jsBtn, int action) {
        int sdlBtn = sdlButtonFromJs(jsBtn);
        if (sdlBtn < 0) {
            return;
        }
        mc.mouseHandler.onButton(windowHandle, new MouseButtonInfo(sdlBtn, 0), action);
    }

    /**
     * JS {@code MouseEvent.button} 编号 (0=左,1=中,2=右,3=X1,4=X2) 重映射为 SDL3 按钮编号
     * (1=左,2=中,3=右,4=X1,5=X2)
     *
     * @return SDL3 按钮编号；协议外的未知按钮返回 -1，调用方必须丢弃注入，
     *         不得折算成任何真实按钮（与 GLFW 键表查无此键的处理一致）
     */
    static int sdlButtonFromJs(int jsBtn) {
        return switch (jsBtn) {
            case 0 -> InputConstants.MOUSE_BUTTON_LEFT;
            case 1 -> InputConstants.MOUSE_BUTTON_MIDDLE;
            case 2 -> InputConstants.MOUSE_BUTTON_RIGHT;
            case 3 -> InputConstants.MOUSE_BUTTON_4;
            case 4 -> InputConstants.MOUSE_BUTTON_5;
            default -> -1;
        };
    }

    @Override
    public void injectMouseMove(Minecraft mc, long windowHandle, double newX, double newY, double dx, double dy) {
        mc.mouseHandler.onMove(windowHandle, newX, newY, dx, dy);
    }

    @Override
    public void injectMouseScroll(Minecraft mc, long windowHandle, double xOffset, double yOffset) {
        mc.mouseHandler.onScroll(windowHandle, xOffset, yOffset);
    }

    @Override
    protected void doInjectText(Minecraft mc, long windowHandle, String text) {
        // vanilla 公开入口：内部清预编辑后按码点逐个转 charTyped
        mc.keyboardHandler.textInput(windowHandle, text);
    }
}
//? }

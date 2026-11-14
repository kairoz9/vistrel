package com.kairo.vistrel.input;

import net.minecraft.client.Minecraft;

//? if >=1.21.9 && <26.3 {
/*import net.minecraft.client.input.CharacterEvent;
import net.minecraft.client.input.KeyEvent;
import net.minecraft.client.input.MouseButtonInfo;*/
//? }
//? if >=26.3 {
/**
 * 26.3+ 占位存根 (运行期全面由 Sdl3InputAdapter 接管)
 */
public class GlfwInputAdapter extends InputAdapter {
    @Override
    public long windowHandle(com.mojang.blaze3d.platform.Window window) {
        return window.handle();
    }

    @Override
    protected void updateModifiers(int hidCode, int action) {}

    @Override
    protected void doInjectKey(Minecraft mc, long windowHandle, int hidCode, int rawAction) {}

    @Override
    protected void doInjectMouseButton(Minecraft mc, long windowHandle, int jsBtn, int action) {}

    @Override
    public void injectMouseMove(Minecraft mc, long windowHandle, double newX, double newY, double dx, double dy) {}

    @Override
    public void injectMouseScroll(Minecraft mc, long windowHandle, double xOffset, double yOffset) {}

    @Override
    protected void doInjectText(Minecraft mc, long windowHandle, String text) {}

    public static int hidToGlfw(int hid) {
        return 0;
    }
}
//? } else {
/*import static org.lwjgl.glfw.GLFW.*;

// GLFW 输入适配器 (适用于 Minecraft 1.20 ~ 1.21.x)
// 采用 GLFW 传统键码与 5 参数方法签名 (window, key, scancode, action, mods)
public class GlfwInputAdapter extends InputAdapter {
    public GlfwInputAdapter() {
    }*/
//? }
//? if <1.21.9 {
/*    @Override
    public long windowHandle(com.mojang.blaze3d.platform.Window window) {
        return window.getWindow();
    }*/
//? } elif <26.3 {
/*    @Override
    public long windowHandle(com.mojang.blaze3d.platform.Window window) {
        return window.handle();
    }*/
//? }
//? if <26.3 {
/*    @Override
    protected void updateModifiers(int hidCode, int action) {
        int mask = switch (hidCode) {
            case HID_KEY_LEFT_SHIFT, HID_KEY_RIGHT_SHIFT -> GLFW_MOD_SHIFT;
            case HID_KEY_LEFT_CONTROL, HID_KEY_RIGHT_CONTROL -> GLFW_MOD_CONTROL;
            case HID_KEY_LEFT_ALT, HID_KEY_RIGHT_ALT -> GLFW_MOD_ALT;
            case HID_KEY_LEFT_SUPER, HID_KEY_RIGHT_SUPER -> GLFW_MOD_SUPER;
            default -> 0;
        };
        applyModifierMask(mask, action);
    }*/
//? }
//? if <1.21.9 {
/*    @Override
    protected void doInjectKey(Minecraft mc, long windowHandle, int hidCode, int rawAction) {
        int glfwKey = hidToGlfw(hidCode);
        if (glfwKey == 0) {
            return;
        }
        if (mc.keyboardHandler != null) {
            mc.keyboardHandler.keyPress(windowHandle, glfwKey, 0, rawAction, currentModifiers);
        }
    }*/
//? } elif <26.3 {
/*    @Override
    protected void doInjectKey(Minecraft mc, long windowHandle, int hidCode, int rawAction) {
        int glfwKey = hidToGlfw(hidCode);
        if (glfwKey == 0) {
            return;
        }
        if (mc.keyboardHandler != null) {
            // 1.21.9 起 keyPress 收归 private 且改收 KeyEvent，经 Invoker 走完整路由
            ((com.kairo.vistrel.mixin.KeyboardHandlerAccessor) mc.keyboardHandler)
                    .vistrel$keyPress(windowHandle, rawAction, new KeyEvent(glfwKey, 0, currentModifiers));
        }
    }*/
//? }
//? if <26.3 {
/*    private static final int[] HID_TO_GLFW = new int[256];

    static {
        // A..Z (4..29 -> GLFW_KEY_A..Z)
        for (int i = 0; i < 26; i++) HID_TO_GLFW[4 + i] = GLFW_KEY_A + i;
        // 1..9 (30..38 -> GLFW_KEY_1..9), 0 (39 -> GLFW_KEY_0)
        for (int i = 0; i < 9; i++) HID_TO_GLFW[30 + i] = GLFW_KEY_1 + i;
        HID_TO_GLFW[39] = GLFW_KEY_0;
        // F1..F12 (58..69 -> GLFW_KEY_F1..F12)
        for (int i = 0; i < 12; i++) HID_TO_GLFW[58 + i] = GLFW_KEY_F1 + i;
        // Keypad 1..9 (89..97 -> GLFW_KEY_KP_1..9), Keypad 0 (98 -> GLFW_KEY_KP_0)
        for (int i = 0; i < 9; i++) HID_TO_GLFW[89 + i] = GLFW_KEY_KP_1 + i;
        HID_TO_GLFW[98] = GLFW_KEY_KP_0;

        // 控制、标点与导航键
        HID_TO_GLFW[40] = GLFW_KEY_ENTER;
        HID_TO_GLFW[41] = GLFW_KEY_ESCAPE;
        HID_TO_GLFW[42] = GLFW_KEY_BACKSPACE;
        HID_TO_GLFW[43] = GLFW_KEY_TAB;
        HID_TO_GLFW[44] = GLFW_KEY_SPACE;
        HID_TO_GLFW[45] = GLFW_KEY_MINUS;
        HID_TO_GLFW[46] = GLFW_KEY_EQUAL;
        HID_TO_GLFW[47] = GLFW_KEY_LEFT_BRACKET;
        HID_TO_GLFW[48] = GLFW_KEY_RIGHT_BRACKET;
        HID_TO_GLFW[49] = GLFW_KEY_BACKSLASH;
        HID_TO_GLFW[51] = GLFW_KEY_SEMICOLON;
        HID_TO_GLFW[52] = GLFW_KEY_APOSTROPHE;
        HID_TO_GLFW[53] = GLFW_KEY_GRAVE_ACCENT;
        HID_TO_GLFW[54] = GLFW_KEY_COMMA;
        HID_TO_GLFW[55] = GLFW_KEY_PERIOD;
        HID_TO_GLFW[56] = GLFW_KEY_SLASH;
        HID_TO_GLFW[57] = GLFW_KEY_CAPS_LOCK;
        HID_TO_GLFW[70] = GLFW_KEY_PRINT_SCREEN;
        HID_TO_GLFW[71] = GLFW_KEY_SCROLL_LOCK;
        HID_TO_GLFW[72] = GLFW_KEY_PAUSE;
        HID_TO_GLFW[73] = GLFW_KEY_INSERT;
        HID_TO_GLFW[74] = GLFW_KEY_HOME;
        HID_TO_GLFW[75] = GLFW_KEY_PAGE_UP;
        HID_TO_GLFW[76] = GLFW_KEY_DELETE;
        HID_TO_GLFW[77] = GLFW_KEY_END;
        HID_TO_GLFW[78] = GLFW_KEY_PAGE_DOWN;
        HID_TO_GLFW[79] = GLFW_KEY_RIGHT;
        HID_TO_GLFW[80] = GLFW_KEY_LEFT;
        HID_TO_GLFW[81] = GLFW_KEY_DOWN;
        HID_TO_GLFW[82] = GLFW_KEY_UP;
        HID_TO_GLFW[83] = GLFW_KEY_NUM_LOCK;
        HID_TO_GLFW[84] = GLFW_KEY_KP_DIVIDE;
        HID_TO_GLFW[85] = GLFW_KEY_KP_MULTIPLY;
        HID_TO_GLFW[86] = GLFW_KEY_KP_SUBTRACT;
        HID_TO_GLFW[87] = GLFW_KEY_KP_ADD;
        HID_TO_GLFW[88] = GLFW_KEY_KP_ENTER;
        HID_TO_GLFW[99] = GLFW_KEY_KP_DECIMAL;

        // 修饰键 (0xE0 ~ 0xE7)
        HID_TO_GLFW[HID_KEY_LEFT_CONTROL]  = GLFW_KEY_LEFT_CONTROL;
        HID_TO_GLFW[HID_KEY_LEFT_SHIFT]    = GLFW_KEY_LEFT_SHIFT;
        HID_TO_GLFW[HID_KEY_LEFT_ALT]      = GLFW_KEY_LEFT_ALT;
        HID_TO_GLFW[HID_KEY_LEFT_SUPER]    = GLFW_KEY_LEFT_SUPER;
        HID_TO_GLFW[HID_KEY_RIGHT_CONTROL] = GLFW_KEY_RIGHT_CONTROL;
        HID_TO_GLFW[HID_KEY_RIGHT_SHIFT]   = GLFW_KEY_RIGHT_SHIFT;
        HID_TO_GLFW[HID_KEY_RIGHT_ALT]     = GLFW_KEY_RIGHT_ALT;
        HID_TO_GLFW[HID_KEY_RIGHT_SUPER]   = GLFW_KEY_RIGHT_SUPER;

        // 注：HID 0x32 (50, Non-US #~) 与 0x64 (100, IntlBackslash) 等 ISO 特殊物理键无标准 GLFW 对应，
        // 查表默认返回 0，并在 doInjectKey 中显式忽略丢弃。
    }

    public static int hidToGlfw(int hid) {
        return (hid >= 0 && hid < HID_TO_GLFW.length) ? HID_TO_GLFW[hid] : 0;
    }

    @Override
    protected void doInjectMouseButton(Minecraft mc, long windowHandle, int jsBtn, int action) {
        int glfwBtn = switch (jsBtn) {
            case 0 -> 0; // GLFW_MOUSE_BUTTON_LEFT
            case 1 -> 2; // GLFW_MOUSE_BUTTON_MIDDLE
            case 2 -> 1; // GLFW_MOUSE_BUTTON_RIGHT
            default -> jsBtn;
        };*/
//? }
//? if <1.21.9 {
/*        if (mc.mouseHandler != null) {
            ((com.kairo.vistrel.mixin.MouseHandlerAccessor) mc.mouseHandler).vistrel$onPress(windowHandle, glfwBtn, action, currentModifiers);
        }
    }*/
//? } elif <26.3 {
/*        if (mc.mouseHandler != null) {
            ((com.kairo.vistrel.mixin.MouseHandlerAccessor) mc.mouseHandler)
                    .vistrel$onPress(windowHandle, new MouseButtonInfo(glfwBtn, currentModifiers), action);
        }
    }*/
//? }
//? if <26.3 {
/*    @Override
    public void injectMouseMove(Minecraft mc, long windowHandle, double newX, double newY, double dx, double dy) {
        if (mc.mouseHandler != null) {
            ((com.kairo.vistrel.mixin.MouseHandlerAccessor) mc.mouseHandler).vistrel$onMove(windowHandle, newX, newY);
        }
    }

    @Override
    public void injectMouseScroll(Minecraft mc, long windowHandle, double xOffset, double yOffset) {
        if (mc.mouseHandler != null) {
            ((com.kairo.vistrel.mixin.MouseHandlerAccessor) mc.mouseHandler).vistrel$onScroll(windowHandle, xOffset, yOffset);
        }
    }

    @Override
    protected void doInjectText(Minecraft mc, long windowHandle, String text) {
        if (mc.keyboardHandler == null) {
            return;
        }
        // 旧版 KeyboardHandler 的 charTyped 是 private，经 Invoker 走 vanilla 完整路由
        com.kairo.vistrel.mixin.KeyboardHandlerAccessor handler =
                (com.kairo.vistrel.mixin.KeyboardHandlerAccessor) mc.keyboardHandler;*/
//? }
//? if <1.21.9 {
/*        text.codePoints().forEach(cp -> handler.vistrel$charTyped(windowHandle, cp, currentModifiers));
    }*/
//? } elif <26.1 {
/*        text.codePoints().forEach(cp -> handler.vistrel$charTyped(windowHandle, new CharacterEvent(cp, currentModifiers)));
    }*/
//? } elif <26.3 {
/*        text.codePoints().forEach(cp -> handler.vistrel$charTyped(windowHandle, new CharacterEvent(cp)));
    }*/
//? }
//? if <26.3 {
/*}*/
//? }

package com.kairo.vistrel.input;

import net.minecraft.client.Minecraft;

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

import com.kairo.vistrel.Constants;
import org.lwjgl.glfw.GLFW;
import org.lwjgl.glfw.GLFWCharCallback;
import org.lwjgl.glfw.GLFWCharModsCallback;
import org.lwjgl.glfw.GLFWCursorPosCallback;
import org.lwjgl.glfw.GLFWKeyCallback;
import org.lwjgl.glfw.GLFWMouseButtonCallback;
import org.lwjgl.glfw.GLFWScrollCallback;

// GLFW 输入适配器（适用于 Minecraft 1.20 ~ 26.2）。
//
// 注入机制 = 复调用 MC 注册的 GLFW 回调（取回 → 立即还原 → 按标准签名 invoke）。
// GLFW 回调 ABI 由 GLFW 定义、库未混淆，任何版本与 loader 的运行时命名都一致。
//
// 线程契约：本类全部入口经帧头钩子（FramerateLimiter/RenderSystem 限频点）跑在渲染线程，
// 与 glfwPollEvents 同线程顺序执行，取回与还原之间不存在轮询交错窗口。
// 每次注入成对取回/还原（两次 JNI 调用）。
public class GlfwInputAdapter extends InputAdapter {
    // 上下文为空时句柄为 0，vanilla 的窗口比对会丢弃全部注入；只提示一次
    private static boolean reportedEmptyContext;

    public GlfwInputAdapter() {
    }

    @Override
    public long windowHandle(com.mojang.blaze3d.platform.Window window) {
        // 渲染线程上当前 GL 上下文的窗口即 MC 主窗口：绕开 Window 取句柄方法的 1.21.9 改名
        long handle = GLFW.glfwGetCurrentContext();
        if (handle == 0 && !reportedEmptyContext) {
            reportedEmptyContext = true;
            Constants.LOG.error("GLFW 当前上下文为空：窗口句柄为 0，远程键鼠注入将被全部丢弃");
        }
        return handle;
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
    }

    @Override
    protected void doInjectKey(Minecraft mc, long windowHandle, int hidCode, int rawAction) {
        int glfwKey = hidToGlfw(hidCode);
        if (glfwKey != 0) {
            invokeKey(windowHandle, glfwKey, rawAction);
        }
    }

    @Override
    protected void doInjectMouseButton(Minecraft mc, long windowHandle, int jsBtn, int action) {
        int glfwBtn = switch (jsBtn) {
            case 0 -> GLFW_MOUSE_BUTTON_LEFT;
            case 1 -> GLFW_MOUSE_BUTTON_MIDDLE;
            case 2 -> GLFW_MOUSE_BUTTON_RIGHT;
            default -> jsBtn;
        };
        invokeMouseButton(windowHandle, glfwBtn, action);
    }

    @Override
    public void injectMouseMove(Minecraft mc, long windowHandle, double newX, double newY, double dx, double dy) {
        invokeCursorPos(windowHandle, newX, newY);
    }

    @Override
    public void injectMouseScroll(Minecraft mc, long windowHandle, double xOffset, double yOffset) {
        invokeScroll(windowHandle, xOffset, yOffset);
    }

    @Override
    protected void doInjectText(Minecraft mc, long windowHandle, String text) {
        // GLFW 字符事件本身不携带修饰键；回调包装器如何构造 CharacterEvent / 传 mods 由 MC 决定，
        // 与真实键盘输入路径一致
        text.codePoints().forEach(cp -> invokeChar(windowHandle, cp));
    }

    public static int hidToGlfw(int hid) {
        return (hid >= 0 && hid < HID_TO_GLFW.length) ? HID_TO_GLFW[hid] : 0;
    }

    private static final int[] HID_TO_GLFW = new int[256];

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

    // 取回 → 立即还原 → 复调用；还原先行使分离窗口缩到两次连续 JNI 调用，
    // 复调用发生在回调链完整还原之后。回调缺失（未注册）时静默跳过。
    private void invokeKey(long win, int key, int action) {
        GLFWKeyCallback cb = GLFW.glfwSetKeyCallback(win, null);
        GLFW.glfwSetKeyCallback(win, cb);
        if (cb != null) {
            cb.invoke(win, key, 0, action, currentModifiers);
        }
    }

    private void invokeMouseButton(long win, int btn, int action) {
        GLFWMouseButtonCallback cb = GLFW.glfwSetMouseButtonCallback(win, null);
        GLFW.glfwSetMouseButtonCallback(win, cb);
        if (cb != null) {
            cb.invoke(win, btn, action, currentModifiers);
        }
    }

    private void invokeCursorPos(long win, double xpos, double ypos) {
        GLFWCursorPosCallback cb = GLFW.glfwSetCursorPosCallback(win, null);
        GLFW.glfwSetCursorPosCallback(win, cb);
        if (cb != null) {
            cb.invoke(win, xpos, ypos);
        }
    }

    private void invokeScroll(long win, double xOffset, double yOffset) {
        GLFWScrollCallback cb = GLFW.glfwSetScrollCallback(win, null);
        GLFW.glfwSetScrollCallback(win, cb);
        if (cb != null) {
            cb.invoke(win, xOffset, yOffset);
        }
    }

    private void invokeChar(long win, int codepoint) {
        // 字符回调注册形态分两代：26.1+ 走标准 char 回调；≤1.21.x 走已废弃的 char-mods 回调
        // 两条路径只会有其一被注册。
        GLFWCharCallback cb = GLFW.glfwSetCharCallback(win, null);
        GLFW.glfwSetCharCallback(win, cb);
        if (cb != null) {
            cb.invoke(win, codepoint);
            return;
        }
        GLFWCharModsCallback modsCb = GLFW.glfwSetCharModsCallback(win, null);
        GLFW.glfwSetCharModsCallback(win, modsCb);
        if (modsCb != null) {
            modsCb.invoke(win, codepoint, currentModifiers);
        }
    }
}*/
//? }

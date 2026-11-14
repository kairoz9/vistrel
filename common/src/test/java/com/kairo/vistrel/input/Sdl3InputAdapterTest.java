package com.kairo.vistrel.input;

//? if >=26.3 {
import net.minecraft.client.Minecraft;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * SDL3 适配器纯映射用例：HID 修饰键码 → SDL_KMOD 位掩码（经 {@code injectKey} 公开路径
 * 驱动真实 {@code updateModifiers}），与 JS 鼠标按钮 → SDL3 按钮编号的重映射表。
 * 手写字面量为独立 oracle：InputConstants / SDLKeycode 任何一端漂移都会在此变红，
 * 与 GLFW 侧 {@code hidToGlfw} 表测试同构。
 */
class Sdl3InputAdapterTest {
    /**
     * 只重定向 {@code doInjectKey} 的哑适配器：修饰键位掩码走真实 {@code updateModifiers}，
     * 键注入路径（SDL native 调用）不在无头测试范围
     */
    private static final class RecordingAdapter extends Sdl3InputAdapter {
        @Override
        protected void doInjectKey(Minecraft mc, long windowHandle, int code, int rawAction) {}
    }

    @Test
    void testHidModifierCodesMapToSdlKmodBits() {
        RecordingAdapter adapter = new RecordingAdapter();
        // 每个修饰键位独立验证按下/释放的置位与清位
        int[][] hidToKmod = {
                // SDL_KMOD_LSHIFT
                {InputAdapter.HID_KEY_LEFT_SHIFT, 1},
                // SDL_KMOD_RSHIFT
                {InputAdapter.HID_KEY_RIGHT_SHIFT, 2},
                // SDL_KMOD_LCTRL
                {InputAdapter.HID_KEY_LEFT_CONTROL, 64},
                // SDL_KMOD_RCTRL
                {InputAdapter.HID_KEY_RIGHT_CONTROL, 128},
                // SDL_KMOD_LALT
                {InputAdapter.HID_KEY_LEFT_ALT, 256},
                // SDL_KMOD_RALT
                {InputAdapter.HID_KEY_RIGHT_ALT, 512},
                // SDL_KMOD_LGUI
                {InputAdapter.HID_KEY_LEFT_SUPER, 1024},
                // SDL_KMOD_RGUI
                {InputAdapter.HID_KEY_RIGHT_SUPER, 2048}
        };
        for (int[] mapping : hidToKmod) {
            adapter.injectKey(null, 0L, mapping[0], InputAdapter.ACTION_PRESS);
            Assertions.assertEquals(
                    mapping[1],
                    adapter.getCurrentModifiers(),
                    "HID 0x" + Integer.toHexString(mapping[0]) + " 按下必须映射为 SDL_KMOD 位 " + mapping[1]
            );
            adapter.injectKey(null, 0L, mapping[0], InputAdapter.ACTION_RELEASE);
            Assertions.assertEquals(0, adapter.getCurrentModifiers(), "修饰键释放必须清位");
        }
    }

    @Test
    void testConcurrentHoldsAccumulateAndClearIndividually() {
        RecordingAdapter adapter = new RecordingAdapter();
        adapter.injectKey(null, 0L, InputAdapter.HID_KEY_LEFT_SHIFT, InputAdapter.ACTION_PRESS);
        adapter.injectKey(null, 0L, InputAdapter.HID_KEY_LEFT_CONTROL, InputAdapter.ACTION_PRESS);
        Assertions.assertEquals(1 | 64, adapter.getCurrentModifiers(), "同时按住的修饰键位必须按位累积");
        adapter.injectKey(null, 0L, InputAdapter.HID_KEY_LEFT_SHIFT, InputAdapter.ACTION_RELEASE);
        Assertions.assertEquals(64, adapter.getCurrentModifiers(), "只释放 Shift 不得清掉 Ctrl 位");
    }

    @Test
    void testUnknownHidCodeLeavesMaskUntouched() {
        RecordingAdapter adapter = new RecordingAdapter();
        adapter.injectKey(null, 0L, InputAdapter.HID_KEY_LEFT_SHIFT, InputAdapter.ACTION_PRESS);
        adapter.injectKey(null, 0L, 65, InputAdapter.ACTION_PRESS);
        Assertions.assertEquals(1, adapter.getCurrentModifiers(), "非修饰键的普通键码不得触碰位掩码");
    }

    @Test
    void testJsMouseButtonsRemapToSdlNumbering() {
        // SDL3 规范编号：左 1、中 2、右 3、X1 4、X2 5
        Assertions.assertEquals(1, Sdl3InputAdapter.sdlButtonFromJs(0), "JS 左键(0) → SDL 左键(1)");
        Assertions.assertEquals(2, Sdl3InputAdapter.sdlButtonFromJs(1), "JS 中键(1) → SDL 中键(2)");
        Assertions.assertEquals(3, Sdl3InputAdapter.sdlButtonFromJs(2), "JS 右键(2) → SDL 右键(3)");
        Assertions.assertEquals(4, Sdl3InputAdapter.sdlButtonFromJs(3), "JS X1(3) → SDL X1(4)");
        Assertions.assertEquals(5, Sdl3InputAdapter.sdlButtonFromJs(4), "JS X2(4) → SDL X2(5)");
    }

    @Test
    void testUnknownJsButtonRequestsDrop() {
        // 协议外按钮必须请求丢弃（-1），不得折算成左键 fail-open
        Assertions.assertEquals(-1, Sdl3InputAdapter.sdlButtonFromJs(5));
        Assertions.assertEquals(-1, Sdl3InputAdapter.sdlButtonFromJs(-1));
    }
}
//? }

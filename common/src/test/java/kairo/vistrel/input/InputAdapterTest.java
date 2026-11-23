package kairo.vistrel.input;

import net.minecraft.client.Minecraft;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import java.util.ArrayList;
import java.util.List;

public class InputAdapterTest {
    static class TestAdapter extends InputAdapter {
        @Override
        public long windowHandle(com.mojang.blaze3d.platform.Window window) {
            return 0L;
        }

        final List<String> injectedKeyLog = new ArrayList<>();
        final List<String> injectedMouseLog = new ArrayList<>();

        @Override
        protected void updateModifiers(int code, int action) {
            if (code == 100) {
                applyModifierMask(1, action);
            }
        }

        @Override
        protected void doInjectKey(Minecraft mc, long windowHandle, int code, int rawAction) {
            injectedKeyLog.add("Key:" + code + ":" + rawAction);
        }

        @Override
        protected void doInjectMouseButton(Minecraft mc, long windowHandle, int jsBtn, int action) {
            injectedMouseLog.add("Mouse:" + jsBtn + ":" + action);
        }

        @Override
        public void injectMouseMove(Minecraft mc, long windowHandle, double newX, double newY, double dx, double dy) {}

        @Override
        public void injectMouseScroll(Minecraft mc, long windowHandle, double xOffset, double yOffset) {}

        @Override
        protected void doInjectText(Minecraft mc, long windowHandle, String text) {
            injectedKeyLog.add("Text:" + text);
        }
    }

    @Test
    void testKeyTrackingAndReleaseAllOnReset() {
        TestAdapter adapter = new TestAdapter();
        // 按下两键并按下修饰键 100
        adapter.injectKey(null, 0, 65, InputAdapter.ACTION_PRESS);
        adapter.injectKey(null, 0, 87, InputAdapter.ACTION_PRESS);
        adapter.injectKey(null, 0, 100, InputAdapter.ACTION_PRESS);

        Assertions.assertEquals(3, adapter.getPressedKeyCount());
        Assertions.assertTrue(adapter.isKeyPressed(65));
        Assertions.assertTrue(adapter.isKeyPressed(87));
        Assertions.assertTrue(adapter.isKeyPressed(100));
        Assertions.assertEquals(1, adapter.getCurrentModifiers());
        // 正常释放 87 键
        adapter.injectKey(null, 0, 87, InputAdapter.ACTION_RELEASE);
        Assertions.assertEquals(2, adapter.getPressedKeyCount());
        Assertions.assertFalse(adapter.isKeyPressed(87));
        // 调用 reset()
        adapter.reset(null, 0);
        // 验证剩余按下的键 (65, 100) 均被合成派发了 0 (RELEASE)
        Assertions.assertEquals(0, adapter.getPressedKeyCount());
        Assertions.assertEquals(0, adapter.getCurrentModifiers());
        Assertions.assertTrue(adapter.injectedKeyLog.contains("Key:65:0"));
        Assertions.assertTrue(adapter.injectedKeyLog.contains("Key:100:0"));
    }

    @Test
    void testMouseTrackingAndReleaseAllOnReset() {
        TestAdapter adapter = new TestAdapter();
        // 左键按下
        adapter.injectMouseButton(null, 0, 0, InputAdapter.ACTION_PRESS);
        // 右键按下
        adapter.injectMouseButton(null, 0, 2, InputAdapter.ACTION_PRESS);
        Assertions.assertEquals(2, adapter.getPressedMouseButtonCount());
        Assertions.assertTrue(adapter.isMouseButtonPressed(0));
        Assertions.assertTrue(adapter.isMouseButtonPressed(2));

        adapter.reset(null, 0);

        Assertions.assertEquals(0, adapter.getPressedMouseButtonCount());
        Assertions.assertTrue(adapter.injectedMouseLog.contains("Mouse:0:0"));
        Assertions.assertTrue(adapter.injectedMouseLog.contains("Mouse:2:0"));
    }
    //? if <26.3 {
    /*@Test
    void testGlfwHidScancodeTranslation() {
        // WASD (HID 26=W, 4=A, 22=S, 7=D)
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_W, GlfwInputAdapter.hidToGlfw(26));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_A, GlfwInputAdapter.hidToGlfw(4));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_S, GlfwInputAdapter.hidToGlfw(22));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_D, GlfwInputAdapter.hidToGlfw(7));

        // 控制与功能键
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_SPACE, GlfwInputAdapter.hidToGlfw(44));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_ESCAPE, GlfwInputAdapter.hidToGlfw(41));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_ENTER, GlfwInputAdapter.hidToGlfw(40));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_TAB, GlfwInputAdapter.hidToGlfw(43));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_F3, GlfwInputAdapter.hidToGlfw(60)); // F3 (Debug Screen)
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_F11, GlfwInputAdapter.hidToGlfw(68)); // F11 (Fullscreen)

        // 数字小键盘 (Keypad)
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_KP_0, GlfwInputAdapter.hidToGlfw(98));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_KP_1, GlfwInputAdapter.hidToGlfw(89));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_KP_ADD, GlfwInputAdapter.hidToGlfw(87));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_KP_ENTER, GlfwInputAdapter.hidToGlfw(88));

        // 修饰键 (Left / Right)
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_LEFT_SHIFT, GlfwInputAdapter.hidToGlfw(InputAdapter.HID_KEY_LEFT_SHIFT));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_RIGHT_SHIFT, GlfwInputAdapter.hidToGlfw(InputAdapter.HID_KEY_RIGHT_SHIFT));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_LEFT_CONTROL, GlfwInputAdapter.hidToGlfw(InputAdapter.HID_KEY_LEFT_CONTROL));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_RIGHT_CONTROL, GlfwInputAdapter.hidToGlfw(InputAdapter.HID_KEY_RIGHT_CONTROL));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_LEFT_ALT, GlfwInputAdapter.hidToGlfw(InputAdapter.HID_KEY_LEFT_ALT));
        Assertions.assertEquals(org.lwjgl.glfw.GLFW.GLFW_KEY_RIGHT_ALT, GlfwInputAdapter.hidToGlfw(InputAdapter.HID_KEY_RIGHT_ALT));

        // ISO 特殊物理键：显式丢弃（返回 0）
        Assertions.assertEquals(0, GlfwInputAdapter.hidToGlfw(50));  // 0x32 (Non-US #~)
        Assertions.assertEquals(0, GlfwInputAdapter.hidToGlfw(100)); // 0x64 (IntlBackslash)
    }*/
    //? }
}

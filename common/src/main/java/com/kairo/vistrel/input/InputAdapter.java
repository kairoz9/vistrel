package com.kairo.vistrel.input;

import com.mojang.blaze3d.platform.Window;
import net.minecraft.client.Minecraft;
import java.util.BitSet;

/**
 * 跨版本输入注入适配器抽象基类：隔离 SDL3 与 GLFW，
 * 集中管理按键追踪与失焦重置
 */
public abstract class InputAdapter {
    public static final int ACTION_RELEASE = 0;
    public static final int ACTION_PRESS = 1;
    public static final int ACTION_REPEAT = 2;
    // USB HID Usage Tables (Page 0x07 - Keyboard/Keypad) 修饰键标准码 (0xE0 ~ 0xE7)
    public static final int HID_KEY_LEFT_CONTROL = 0xE0;
    public static final int HID_KEY_LEFT_SHIFT = 0xE1;
    public static final int HID_KEY_LEFT_ALT = 0xE2;
    public static final int HID_KEY_LEFT_SUPER = 0xE3;
    public static final int HID_KEY_RIGHT_CONTROL = 0xE4;
    public static final int HID_KEY_RIGHT_SHIFT = 0xE5;
    public static final int HID_KEY_RIGHT_ALT = 0xE6;
    public static final int HID_KEY_RIGHT_SUPER = 0xE7;
    protected final BitSet pressedKeys = new BitSet();
    protected final BitSet pressedMouseButtons = new BitSet();
    protected int currentModifiers = 0;

    /**
     * 注入键盘事件并追踪按键状态
     *
     * @param mc          当前 Minecraft 实例
     * @param windowHandle 适配器语义的窗口句柄
     * @param code        键位码（HID 键码域，适配器内部转平台码）；负值跳过状态追踪
     * @param rawAction   动作值：{@link #ACTION_RELEASE} / {@link #ACTION_PRESS} /
     *                    {@link #ACTION_REPEAT}
     */
    public final void injectKey(Minecraft mc, long windowHandle, int code, int rawAction) {
        if (code >= 0) {
            if (rawAction == ACTION_PRESS) {
                pressedKeys.set(code);
            } else if (rawAction == ACTION_RELEASE) {
                pressedKeys.clear(code);
            }
        }
        updateModifiers(code, rawAction);
        doInjectKey(mc, windowHandle, code, rawAction);
    }

    /**
     * 注入鼠标按键事件并追踪状态
     *
     * @param mc          当前 Minecraft 实例
     * @param windowHandle 适配器语义的窗口句柄
     * @param jsBtn       鼠标键位索引；负值跳过状态追踪
     * @param action      动作值：{@link #ACTION_RELEASE} / {@link #ACTION_PRESS} /
     *                    {@link #ACTION_REPEAT}
     */
    public final void injectMouseButton(Minecraft mc, long windowHandle, int jsBtn, int action) {
        if (jsBtn >= 0) {
            if (action == ACTION_PRESS) {
                pressedMouseButtons.set(jsBtn);
            } else if (action == ACTION_RELEASE) {
                pressedMouseButtons.clear(jsBtn);
            }
        }
        doInjectMouseButton(mc, windowHandle, jsBtn, action);
    }

    /**
     * 注入鼠标移动事件
     *
     * @param mc           当前 Minecraft 实例
     * @param windowHandle 适配器语义的窗口句柄（见 {@link #windowHandle(Window)}）
     * @param newX         光标目标绝对坐标 X（窗口客户区坐标）
     * @param newY         光标目标绝对坐标 Y（窗口客户区坐标）
     * @param dx           本次位移增量 X（浏览器 movementX 语义）
     * @param dy           本次位移增量 Y（浏览器 movementY 语义）
     */
    public abstract void injectMouseMove(
            Minecraft mc,
            long windowHandle,
            double newX,
            double newY,
            double dx,
            double dy
    );

    /**
     * 注入鼠标滚轮事件
     *
     * @param mc           当前 Minecraft 实例
     * @param windowHandle 适配器语义的窗口句柄（见 {@link #windowHandle(Window)}）
     * @param xOffset      水平滚动增量，正值方向与原生滚轮事件一致
     * @param yOffset      垂直滚动增量，正值方向与原生滚轮事件一致
     */
    public abstract void injectMouseScroll(Minecraft mc, long windowHandle, double xOffset, double yOffset);

    /**
     * 注入文本：整段交游戏侧转字符事件（GUI 文本框输入，IME 组合结果与英文打字共用）
     *
     * @param mc           当前 Minecraft 实例
     * @param windowHandle 适配器语义的窗口句柄（见 {@link #windowHandle(Window)}）
     * @param text         待注入文本；null 或空串直接忽略
     */
    public final void injectText(Minecraft mc, long windowHandle, String text) {
        if (text != null && !text.isEmpty()) {
            doInjectText(mc, windowHandle, text);
        }
    }

    /**
     * 取平台窗口句柄：GLFW 侧为渲染线程当前 GL 上下文的窗口（{@code glfwGetCurrentContext}，
     * SDL3 侧为 {@code Window#handle()}
     *
     * @param window Minecraft 窗口实例
     * @return 适配器语义的窗口句柄，须回传给本类其余注入方法
     */
    public abstract long windowHandle(Window window);

    /**
     * 遍历所有按住的按键与鼠标键补发释放事件，防止卡键，零对象分配
     *
     * @param mc           当前 Minecraft 实例
     * @param windowHandle 适配器语义的窗口句柄（见 {@link #windowHandle(Window)}）
     */
    public final void reset(Minecraft mc, long windowHandle) {
        for (int key = pressedKeys.nextSetBit(0); key >= 0; key = pressedKeys.nextSetBit(key + 1)) {
            doInjectKey(mc, windowHandle, key, ACTION_RELEASE);
        }
        pressedKeys.clear();

        for (int btn = pressedMouseButtons.nextSetBit(0); btn >= 0; btn = pressedMouseButtons.nextSetBit(btn + 1)) {
            doInjectMouseButton(mc, windowHandle, btn, ACTION_RELEASE);
        }
        pressedMouseButtons.clear();

        currentModifiers = 0;
    }

    public int getCurrentModifiers() {
        return currentModifiers;
    }

    public boolean isKeyPressed(int code) {
        return code >= 0 && pressedKeys.get(code);
    }

    public boolean isMouseButtonPressed(int btn) {
        return btn >= 0 && pressedMouseButtons.get(btn);
    }

    public int getPressedKeyCount() {
        return pressedKeys.cardinality();
    }

    public int getPressedMouseButtonCount() {
        return pressedMouseButtons.cardinality();
    }

    /**
     * 辅助更新修饰键位掩码
     *
     * @param mask 修饰键位掩码
     * @param action 动作 (0 为释放，非 0 为按下)
     */
    protected void applyModifierMask(int mask, int action) {
        if (mask != 0) {
            if (action != ACTION_RELEASE) {
                currentModifiers |= mask;
            } else {
                currentModifiers &= ~mask;
            }
        }
    }

    protected abstract void updateModifiers(int code, int action);

    protected abstract void doInjectKey(Minecraft mc, long windowHandle, int code, int rawAction);

    protected abstract void doInjectMouseButton(Minecraft mc, long windowHandle, int jsBtn, int action);

    protected abstract void doInjectText(Minecraft mc, long windowHandle, String text);
}

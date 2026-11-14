package com.kairo.vistrel.input;

import com.kairo.vistrel.Constants;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;

/**
 * Input Frame 二进制解码器：零复制解析线格式字节，产出类型化事件
 *
 * <p>
 * 严格按线格式解析 JNI DirectBuffer；遇到未知 Opcode 或数据截断时立即停止，
 * 丢弃本批次剩余数据，避免错位解析。
 */
public final class InputEventDecoder {
    private InputEventDecoder() {
    }

    /**
     * 解码缓冲区内的类型化输入事件
     *
     * @param buffer     包含连续输入帧二进制的 DirectBuffer
     * @param eventCount 预期事件总数
     * @return 成功解码的事件（遭遇截断或未知 Opcode 时为部分前缀）
     */
    public static List<Event> decode(ByteBuffer buffer, int eventCount) {
        if (buffer == null || eventCount <= 0) {
            return List.of();
        }

        List<Event> events = new ArrayList<>(eventCount);
        for (int i = 0; i < eventCount && buffer.hasRemaining(); i++) {
            byte opcode = buffer.get();
            switch (opcode) {
                case MouseMove.OPCODE -> {
                    if (buffer.remaining() < 4) {
                        Constants.LOG.warn("Input Frame 截断 (MouseMove): 剩余 {} 字节，期望 4 字节，丢弃本批次剩余数据", buffer.remaining());
                        return events;
                    }
                    events.add(new MouseMove(buffer.getShort(), buffer.getShort()));
                }
                case MouseButton.OPCODE -> {
                    if (buffer.remaining() < 2) {
                        Constants.LOG.warn(
                                "Input Frame 截断 (MouseButton): 剩余 {} 字节，期望 2 字节，丢弃本批次剩余数据",
                                buffer.remaining()
                        );
                        return events;
                    }
                    events.add(new MouseButton(buffer.get() & 0xFF, buffer.get() & 0xFF));
                }
                case Key.OPCODE -> {
                    if (buffer.remaining() < 5) {
                        Constants.LOG.warn("Input Frame 截断 (KeyEvent): 剩余 {} 字节，期望 5 字节，丢弃本批次剩余数据", buffer.remaining());
                        return events;
                    }
                    events.add(new Key(buffer.getInt(), buffer.get() & 0xFF));
                }
                case MouseScroll.OPCODE -> {
                    if (buffer.remaining() < 1) {
                        Constants.LOG.warn(
                                "Input Frame 截断 (MouseScroll): 剩余 {} 字节，期望 1 字节，丢弃本批次剩余数据",
                                buffer.remaining()
                        );
                        return events;
                    }
                    events.add(new MouseScroll(buffer.get()));
                }
                case AbsoluteMouseMove.OPCODE -> {
                    if (buffer.remaining() < 4) {
                        Constants.LOG.warn(
                                "Input Frame 截断 (AbsoluteMouseMove): 剩余 {} 字节，期望 4 字节，丢弃本批次剩余数据",
                                buffer.remaining()
                        );
                        return events;
                    }
                    events.add(new AbsoluteMouseMove(buffer.getShort() & 0xFFFF, buffer.getShort() & 0xFFFF));
                }
                default -> {
                    Constants.LOG.warn(
                            "Input Frame 遭遇未知 Opcode 0x{}，丢弃本批次剩余数据以避免错位解析",
                            Integer.toHexString(opcode & 0xFF)
                    );
                    return events;
                }
            }
        }
        return events;
    }

    /**
     * 类型化输入事件
     */
    public sealed interface Event permits MouseMove, MouseButton, Key, MouseScroll, AbsoluteMouseMove {
    }

    /**
     * 相对鼠标位移 (1 字节 Opcode + 2×int16 BE 有符号)
     *
     * @param dx X 轴相对位移（浏览器 movementX 语义），int16 BE 有符号
     * @param dy Y 轴相对位移（浏览器 movementY 语义），int16 BE 有符号
     */
    public record MouseMove(short dx, short dy) implements Event {
        public static final byte OPCODE = 0x01;
    }

    /**
     * 鼠标按键 (1 字节 Opcode + 2×uint8: 按钮, 动作)
     *
     * @param button 鼠标按钮索引（uint8，0 起：0 左键 / 1 中键 / 2 右键）
     * @param action 动作码（uint8）：0 释放 / 1 按下（与
     *               {@link com.kairo.vistrel.input.InputAdapter#ACTION_RELEASE} 等域对齐）
     */
    public record MouseButton(int button, int action) implements Event {
        public static final byte OPCODE = 0x02;
    }

    /**
     * 键盘事件 (1 字节 Opcode + int32 BE HID 码 + uint8 动作)
     *
     * @param code   键位码（int32 BE，HID 键码域）
     * @param action 动作码（uint8）：0 释放 / 1 按下
     */
    public record Key(int code, int action) implements Event {
        public static final byte OPCODE = 0x03;
    }

    /**
     * 垂直滚轮增量 (1 字节 Opcode + int8)
     *
     * @param sy 垂直滚动增量（int8 有符号，正值方向与原生滚轮事件一致）
     */
    public record MouseScroll(byte sy) implements Event {
        public static final byte OPCODE = 0x04;
    }

    /**
     * 绝对鼠标位置 (1 字节 Opcode + 2×uint16 BE 归一化坐标)
     *
     * @param x 归一化 X 坐标（uint16 BE，0..65535）
     * @param y 归一化 Y 坐标（uint16 BE，0..65535）
     */
    public record AbsoluteMouseMove(int x, int y) implements Event {
        public static final byte OPCODE = 0x05;
    }
}

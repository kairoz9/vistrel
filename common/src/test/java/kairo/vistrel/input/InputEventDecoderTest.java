package kairo.vistrel.input;

import kairo.vistrel.input.InputEventDecoder.AbsoluteMouseMove;
import kairo.vistrel.input.InputEventDecoder.Event;
import kairo.vistrel.input.InputEventDecoder.Key;
import kairo.vistrel.input.InputEventDecoder.MouseButton;
import kairo.vistrel.input.InputEventDecoder.MouseMove;
import kairo.vistrel.input.InputEventDecoder.MouseScroll;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.List;

public class InputEventDecoderTest {
    private static List<Event> decode(byte[] wire, int eventCount) {
        ByteBuffer buf = ByteBuffer.allocateDirect(wire.length).order(ByteOrder.BIG_ENDIAN);
        buf.put(wire).flip();
        return InputEventDecoder.decode(buf, eventCount);
    }

    @Test
    void testDecodeAllFiveEventTypes() {
        ByteBuffer buf = ByteBuffer.allocateDirect(128).order(ByteOrder.BIG_ENDIAN);
        // [BE] 0x01: dx = 10, dy = -10 (5 bytes)
        buf.put((byte) 0x01).putShort((short) 10).putShort((short) -10);
        // [BE] 0x02: btn = 1, action = 1 (3 bytes)
        buf.put((byte) 0x02).put((byte) 1).put((byte) 1);
        // [BE] 0x03: code = 65, action = 2 (6 bytes)
        buf.put((byte) 0x03).putInt(65).put((byte) 2);
        // [BE] 0x04: sy = 1 (2 bytes)
        buf.put((byte) 0x04).put((byte) 1);
        // [BE] 0x05: x = 32768, y = 16384 (5 bytes)
        buf.put((byte) 0x05).putShort((short) 32768).putShort((short) 16384);

        buf.flip();

        List<Event> events = InputEventDecoder.decode(buf, 5);

        Assertions.assertEquals(5, events.size());
        Assertions.assertEquals(new MouseMove((short) 10, (short) -10), events.get(0));
        Assertions.assertEquals(new MouseButton(1, 1), events.get(1));
        Assertions.assertEquals(new Key(65, 2), events.get(2));
        Assertions.assertEquals(new MouseScroll((byte) 1), events.get(3));
        Assertions.assertEquals(new AbsoluteMouseMove(32768, 16384), events.get(4));
    }

    @Test
    void testTruncatedPayloadFailsSafe() {
        // [BE] 正常事件 0x04: sy = 1 (2 bytes)，随后截断事件 0x01: 仅 2 字节（期望 4 字节负载）
        byte[] wire = {0x04, 0x01, 0x01, 0x00, 0x05};

        List<Event> events = decode(wire, 2);
        // 仅第 1 个事件正常解码，截断事件终止并安全丢弃
        Assertions.assertEquals(1, events.size());
        Assertions.assertEquals(new MouseScroll((byte) 1), events.get(0));
    }

    @Test
    void testUnknownOpcodeFailsSafe() {
        // [BE] 未知 Opcode 0x99
        byte[] wire = {(byte) 0x99, 0x01, 0x02};

        Assertions.assertTrue(decode(wire, 1).isEmpty());
    }

    @Test
    void testEmptyBuffer() {
        ByteBuffer buf = ByteBuffer.allocateDirect(0);
        Assertions.assertTrue(InputEventDecoder.decode(buf, 0).isEmpty());
        Assertions.assertTrue(InputEventDecoder.decode(null, 5).isEmpty());
    }

    @Test
    void testWireFormatContractParity() {
        // [BE] 0x01: MouseMove (dx=10, dy=-10 -> [0x01, 0x00, 0x0A, (byte) 0xFF, (byte) 0xF6])
        List<Event> move = decode(new byte[] {0x01, 0x00, 0x0A, (byte) 0xFF, (byte) 0xF6}, 1);
        Assertions.assertEquals(List.of(new MouseMove((short) 10, (short) -10)), move);
        // [BE] 0x02: MouseButton (btn=1, action=1 -> [0x02, 0x01, 0x01])
        List<Event> btn = decode(new byte[] {0x02, 0x01, 0x01}, 1);
        Assertions.assertEquals(List.of(new MouseButton(1, 1)), btn);
        // [BE] 0x03: Key (HID code=26 'KeyW', action=1 -> [0x03, 0x00, 0x00, 0x00, 0x1A, 0x01])
        List<Event> key = decode(new byte[] {0x03, 0x00, 0x00, 0x00, 0x1A, 0x01}, 1);
        Assertions.assertEquals(List.of(new Key(26, 1)), key);
        // [BE] 0x04: MouseScroll (sy=1 -> [0x04, 0x01])
        List<Event> scroll = decode(new byte[] {0x04, 0x01}, 1);
        Assertions.assertEquals(List.of(new MouseScroll((byte) 1)), scroll);
        // [BE] 0x05: AbsoluteMouseMove (x=32768, y=16384 -> [0x05, (byte) 0x80, 0x00, 0x40, 0x00])
        List<Event> abs = decode(new byte[] {0x05, (byte) 0x80, 0x00, 0x40, 0x00}, 1);
        Assertions.assertEquals(List.of(new AbsoluteMouseMove(32768, 16384)), abs);
    }
}

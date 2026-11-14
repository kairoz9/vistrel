package com.kairo.vistrel.streaming.signaling;

import io.netty.buffer.ByteBuf;
import io.netty.buffer.Unpooled;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import java.nio.charset.StandardCharsets;

/**
 * WebSocket 帧编解码对撞测试：测试手搓 RFC 6455 线格式字节作为独立 oracle，
 * 被测 codec 只负责解析/生成——协议知识不与被测代码共享，codec 写错即红。
 */
class WebSocketFrameCodecTest {
    private static final byte[] MASK = {0x12, 0x34, 0x56, 0x78};
    private final WebSocketFrameCodec codec = new WebSocketFrameCodec();

    /**
     * 手搓带掩码的客户端帧（RFC 6455 规范客户端必须掩码）
     */
    private static ByteBuf maskedClientFrame(int opcode, boolean fin, byte[] payload) {
        ByteBuf buf = Unpooled.buffer();
        buf.writeByte((fin ? 0x80 : 0x00) | opcode);
        if (payload.length <= 125) {
            buf.writeByte(0x80 | payload.length);
        } else if (payload.length <= 65535) {
            buf.writeByte(0x80 | 126);
            buf.writeShort(payload.length);
        } else {
            buf.writeByte(0x80 | 127);
            buf.writeLong(payload.length);
        }
        buf.writeBytes(MASK);
        byte[] masked = new byte[payload.length];
        for (int i = 0; i < payload.length; i++) {
            masked[i] = (byte) (payload[i] ^ MASK[i % 4]);
        }
        buf.writeBytes(masked);
        return buf;
    }

    private static ByteBuf unmaskedFrame(int opcode, boolean fin, byte[] payload) {
        ByteBuf buf = Unpooled.buffer();
        buf.writeByte((fin ? 0x80 : 0x00) | opcode);
        if (payload.length <= 125) {
            buf.writeByte(payload.length);
        } else if (payload.length <= 65535) {
            buf.writeByte(126);
            buf.writeShort(payload.length);
        } else {
            buf.writeByte(127);
            buf.writeLong(payload.length);
        }
        buf.writeBytes(payload);
        return buf;
    }

    private static String text(WebSocketFrameCodec.Inbound inbound) {
        return new String(inbound.payload(), StandardCharsets.UTF_8);
    }

    @Test
    void testMaskedTextFrameUnmasksPayload() {
        ByteBuf in = maskedClientFrame(0x1, true, "{\"type\":\"offer\"}".getBytes(StandardCharsets.UTF_8));
        WebSocketFrameCodec.Inbound inbound = codec.decode(in);
        Assertions.assertNotNull(inbound);
        Assertions.assertEquals(WebSocketFrameCodec.Inbound.Kind.TEXT, inbound.kind());
        Assertions.assertEquals("{\"type\":\"offer\"}", text(inbound), "掩码应被正确异或还原");
        Assertions.assertFalse(in.readableBytes() > 0, "完整帧应被整帧消费");
    }

    @Test
    void testUnmaskedTextFrameAccepted() {
        ByteBuf in = unmaskedFrame(0x1, true, "hello".getBytes(StandardCharsets.UTF_8));
        WebSocketFrameCodec.Inbound inbound = codec.decode(in);
        Assertions.assertNotNull(inbound);
        Assertions.assertEquals("hello", text(inbound));
    }

    @Test
    void testLengthSegmentBoundaries() {
        // 125 = 7 位长度上限；126 = 触发 16 位；65535 = 16 位上限；65536 = 触发 64 位
        int[] lengths = {125, 126, 65535, 65536};
        for (int len : lengths) {
            WebSocketFrameCodec fresh = new WebSocketFrameCodec();
            byte[] payload = new byte[len];
            for (int i = 0; i < len; i++) {
                payload[i] = (byte) ('a' + (i % 26));
            }
            WebSocketFrameCodec.Inbound inbound = fresh.decode(maskedClientFrame(0x1, true, payload));
            Assertions.assertNotNull(inbound, "长度 " + len + " 应可解");
            Assertions.assertEquals(len, inbound.payload().length, "长度 " + len + " 载荷应完整");
        }
    }

    @Test
    void testPartialFrameWaitsForCumulation() {
        ByteBuf full = maskedClientFrame(0x1, true, new byte[300]);
        // 只喂前半：长度字段已完整但载荷不足 → 必须回滚等待，不得错位消费
        ByteBuf half = Unpooled.copiedBuffer(full.slice(0, 60));
        Assertions.assertNull(codec.decode(half), "半包应返回 null 等待");
        Assertions.assertEquals(0, half.readerIndex(), "半包必须回滚读取索引");
        // 喂完整帧（模拟 ByteToMessageDecoder 累计缓冲）后应整帧交付
        ByteBuf cumulated = Unpooled.wrappedBuffer(half, full.slice(60, full.readableBytes() - 60));
        WebSocketFrameCodec.Inbound inbound = codec.decode(cumulated);
        Assertions.assertNotNull(inbound);
        Assertions.assertEquals(300, inbound.payload().length);
    }

    @Test
    void testContinuationFragmentsReassembleIntoOneMessage() {
        codec.decode(maskedClientFrame(0x1, false, "Hel".getBytes(StandardCharsets.UTF_8)));
        ByteBuf in = maskedClientFrame(0x0, false, "lo ".getBytes(StandardCharsets.UTF_8));
        Assertions.assertNull(codec.decode(in), "非终帧 continuation 不应交付");
        WebSocketFrameCodec.Inbound done =
                codec.decode(maskedClientFrame(0x0, true, "world".getBytes(StandardCharsets.UTF_8)));
        Assertions.assertNotNull(done);
        Assertions.assertEquals("Hello world", text(done), "分片应重组为完整文本消息");
    }

    @Test
    void testNewFragmentedTextResetsPendingBuffer() {
        codec.decode(maskedClientFrame(0x1, false, "abandoned".getBytes(StandardCharsets.UTF_8)));
        // 既有宽松行为：新的非终帧 text 重置缓冲重新开始一条消息
        codec.decode(maskedClientFrame(0x1, false, "new ".getBytes(StandardCharsets.UTF_8)));
        WebSocketFrameCodec.Inbound done =
                codec.decode(maskedClientFrame(0x0, true, "message".getBytes(StandardCharsets.UTF_8)));
        Assertions.assertNotNull(done);
        Assertions.assertEquals("new message", text(done));
    }

    @Test
    void testControlFramesCarryPayload() {
        WebSocketFrameCodec.Inbound ping = codec.decode(maskedClientFrame(0x9, true, new byte[] {1, 2, 3}));
        Assertions.assertEquals(WebSocketFrameCodec.Inbound.Kind.PING, ping.kind());
        Assertions.assertArrayEquals(new byte[] {1, 2, 3}, ping.payload());

        WebSocketFrameCodec.Inbound pong = codec.decode(maskedClientFrame(0xA, true, new byte[] {9}));
        Assertions.assertEquals(WebSocketFrameCodec.Inbound.Kind.PONG, pong.kind());

        WebSocketFrameCodec.Inbound close = codec.decode(maskedClientFrame(0x8, true, new byte[] {0x0F, (byte) 0xA1}));
        Assertions.assertEquals(WebSocketFrameCodec.Inbound.Kind.CLOSE, close.kind());
        Assertions.assertEquals(4001, ((close.payload()[0] & 0xFF) << 8) | (close.payload()[1] & 0xFF));
    }

    @Test
    void testUnknownOpcodeSkippedWithoutBlockingNextFrame() {
        // 同一缓冲内：未知 opcode (0x3) 帧在前、合法 text 帧在后 → 未知帧被忽略且不挡住后续帧
        ByteBuf unknown = maskedClientFrame(0x3, true, new byte[] {1});
        ByteBuf text = maskedClientFrame(0x1, true, "next".getBytes(StandardCharsets.UTF_8));
        ByteBuf in = Unpooled.wrappedBuffer(unknown, text);
        WebSocketFrameCodec.Inbound inbound = codec.decode(in);
        Assertions.assertNotNull(inbound, "未知帧被忽略后应继续解出后续 text 帧");
        Assertions.assertEquals("next", text(inbound));
    }

    @Test
    void testOversizedLengthFieldIsProtocolViolation() {
        // 64 位长度字段声明 11MB > 10MB 上限 → fail-closed 违规
        ByteBuf in = Unpooled.buffer();
        in.writeByte(0x81);
        in.writeByte(0x80 | 127);
        in.writeLong(11L * 1024 * 1024);
        Assertions.assertThrows(WebSocketFrameCodec.ProtocolViolationException.class, () -> codec.decode(in));
    }

    @Test
    void testNegativeLengthFieldIsProtocolViolation() {
        // 64 位长度最高位置位 → 负值长度 → fail-closed 违规
        ByteBuf in = Unpooled.buffer();
        in.writeByte(0x81);
        in.writeByte(0x80 | 127);
        in.writeLong(1L << 63);
        Assertions.assertThrows(WebSocketFrameCodec.ProtocolViolationException.class, () -> codec.decode(in));
    }

    @Test
    void testTextFrameEncodingMatchesWireFormat() {
        // 短载荷：7 位长度
        ByteBuf shortFrame = WebSocketFrameCodec.textFrame("hi");
        Assertions.assertEquals(0x81, shortFrame.readUnsignedByte());
        Assertions.assertEquals(2, shortFrame.readUnsignedByte());
        // 中载荷：126 + 16 位大端长度
        byte[] medium = new byte[300];
        ByteBuf mediumFrame = WebSocketFrameCodec.textFrame(new String(medium, StandardCharsets.ISO_8859_1));
        Assertions.assertEquals(0x81, mediumFrame.readUnsignedByte());
        Assertions.assertEquals(126, mediumFrame.readUnsignedByte());
        Assertions.assertEquals(300, mediumFrame.readUnsignedShort());
    }

    @Test
    void testCloseFrameEncodingCarriesStatusCodeBigEndian() {
        ByteBuf close = WebSocketFrameCodec.closeFrame(4001, "Preempted by new session");
        Assertions.assertEquals(0x88, close.readUnsignedByte());
        int len = close.readUnsignedByte();
        Assertions.assertEquals(4001, close.readUnsignedShort(), "状态码应 16 位大端紧跟长度");
        Assertions.assertTrue(len >= 2);
    }

    @Test
    void testPongFrameEncodingEchoesPayload() {
        ByteBuf pong = WebSocketFrameCodec.pongFrame(new byte[] {5, 6});
        Assertions.assertEquals(0x8A, pong.readUnsignedByte());
        Assertions.assertEquals(2, pong.readUnsignedByte());
        Assertions.assertEquals(5, pong.readUnsignedByte());
        Assertions.assertEquals(6, pong.readUnsignedByte());
    }

    @Test
    void testEncodedFramesDecodeBackRoundTrip() {
        // 编解码往返自证（弱于手搓 oracle，但钉住编解码对称性）
        WebSocketFrameCodec fresh = new WebSocketFrameCodec();
        WebSocketFrameCodec.Inbound inbound = fresh.decode(WebSocketFrameCodec.textFrame("对称性"));
        Assertions.assertEquals("对称性", text(inbound));
    }
}

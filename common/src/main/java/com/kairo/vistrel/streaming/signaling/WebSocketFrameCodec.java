package com.kairo.vistrel.streaming.signaling;

import io.netty.buffer.ByteBuf;
import io.netty.buffer.Unpooled;
import java.nio.charset.StandardCharsets;

/**
 * WebSocket 帧编解码 (RFC 6455)，
 * package-private 便于测试：ByteBuf 进出，不涉及网络与信令逻辑。
 *
 * {@link #decode} 只交付完整入站单元（重组完毕的文本消息或控制帧）；
 * 半包与未知 opcode 返回 {@code null}（后者按忽略处理），长度字段非法抛
 * {@link ProtocolViolationException}，由调用方断连。
 *
 * <p>编码侧为静态工厂：只产帧字节，网络写出仍由调用方执行。
 */
final class WebSocketFrameCodec {
    /**
     * 单帧载荷上限
     */
    private static final long MAX_FRAME_PAYLOAD = 10L * 1024 * 1024;
    // ===== RFC 6455 §5.2 线格式常量 =====
    /**
     * 首字节 FIN 位：该帧为消息末帧
     */
    private static final int BIT_FIN = 0x80;
    /**
     * 次字节 MASK 位：载荷被 4 字节掩码异或
     */
    private static final int BIT_MASK = 0x80;
    /**
     * 首字节 opcode 位段掩码
     */
    private static final int MASK_OPCODE = 0x0F;
    /**
     * 次字节 7 位载荷长度掩码
     */
    private static final int MASK_LEN7 = 0x7F;
    /**
     * Opcode：分片续帧
     */
    private static final int OP_CONTINUATION = 0x0;
    /**
     * Opcode：文本帧
     */
    private static final int OP_TEXT = 0x1;
    /**
     * Opcode：连接关闭
     */
    private static final int OP_CLOSE = 0x8;
    /**
     * Opcode：心跳探测
     */
    private static final int OP_PING = 0x9;
    /**
     * Opcode：心跳应答
     */
    private static final int OP_PONG = 0xA;
    /**
     * 7 位长度上限；更长载荷以哨兵值声明扩展长度
     */
    private static final int MAX_LEN_7BIT = 125;
    /**
     * 扩展长度哨兵：后随 16 位长度
     */
    private static final int LEN_EXT16 = 126;
    /**
     * 16 位长度上限
     */
    private static final int MAX_LEN_16BIT = 65535;
    /**
     * 扩展长度哨兵：后随 64 位长度
     */
    private static final int LEN_EXT64 = 127;

    /**
     * 解码产出的完整入站单元
     */
    record Inbound(Kind kind, byte[] payload) {
        enum Kind {
            TEXT,
            CLOSE,
            PING,
            PONG
        }
    }

    /**
     * 协议违规：长度字段负值或超上限，调用方应立即断连
     */
    static final class ProtocolViolationException extends RuntimeException {
        private static final long serialVersionUID = 1L;

        ProtocolViolationException(String message) {
            super(message);
        }
    }

    private final StringBuilder textMessageBuffer = new StringBuilder();

    /**
     * 从累计缓冲解码下一个完整入站单元。
     *
     * <p>continuation 重组保持宽松：分片追加、text 非终帧重置缓冲、
     * 终帧交付；不校验 continuation 前面是不是 text 帧。
     *
     * @return 完整单元；{@code null} 表示半包等待或缓冲已无完整可交付内容
     */
    Inbound decode(ByteBuf in) {
        while (in.readableBytes() >= 2) {
            in.markReaderIndex();
            int b0 = in.readUnsignedByte();
            int b1 = in.readUnsignedByte();

            int opcode = b0 & MASK_OPCODE;
            boolean masked = (b1 & BIT_MASK) != 0;
            long len = b1 & MASK_LEN7;

            if (len == LEN_EXT16) {
                if (in.readableBytes() < 2) {
                    in.resetReaderIndex();
                    // 长度字段未收全，等待后续字节
                    return null;
                }
                len = in.readUnsignedShort();
            } else if (len == LEN_EXT64) {
                if (in.readableBytes() < 8) {
                    in.resetReaderIndex();
                    return null;
                }
                len = in.readLong();
            }

            if (len < 0 || len > MAX_FRAME_PAYLOAD) {
                throw new ProtocolViolationException("帧长度非法: " + len);
            }

            int maskLen = masked ? 4 : 0;
            if (in.readableBytes() < maskLen + (int) len) {
                // 半包到达：回滚读取索引，由 ByteToMessageDecoder 保留缓冲等待后续数据
                in.resetReaderIndex();
                return null;
            }

            byte[] mask = null;
            if (masked) {
                mask = new byte[4];
                in.readBytes(mask);
            }

            byte[] payload = new byte[(int) len];
            in.readBytes(payload);
            if (mask != null) {
                for (int i = 0; i < payload.length; i++) {
                    payload[i] ^= mask[i % 4];
                }
            }

            boolean fin = (b0 & BIT_FIN) != 0;
            switch (opcode) {
                case OP_CONTINUATION -> {
                    textMessageBuffer.append(new String(payload, StandardCharsets.UTF_8));
                    if (fin) {
                        return deliverText();
                    }
                }
                case OP_TEXT -> {
                    if (fin) {
                        if (textMessageBuffer.length() > 0) {
                            textMessageBuffer.append(new String(payload, StandardCharsets.UTF_8));
                            return deliverText();
                        }
                        return new Inbound(Inbound.Kind.TEXT, payload);
                    }
                    // 新的非终帧 text：重置缓冲重新开始一条消息
                    textMessageBuffer.setLength(0);
                    textMessageBuffer.append(new String(payload, StandardCharsets.UTF_8));
                }
                case OP_CLOSE -> {
                    return new Inbound(Inbound.Kind.CLOSE, payload);
                }
                case OP_PING -> {
                    return new Inbound(Inbound.Kind.PING, payload);
                }
                case OP_PONG -> {
                    return new Inbound(Inbound.Kind.PONG, payload);
                }
                default -> {
                    // 未知 opcode 按既有行为忽略，继续尝试解同一缓冲内的下一帧
                }
            }
        }
        return null;
    }

    private Inbound deliverText() {
        String fullText = textMessageBuffer.toString();
        textMessageBuffer.setLength(0);
        return new Inbound(Inbound.Kind.TEXT, fullText.getBytes(StandardCharsets.UTF_8));
    }

    // ===== 编码侧：只产帧字节，写出由调用方执行 =====
    /**
     * FIN + Text 帧
     */
    static ByteBuf textFrame(String text) {
        byte[] payload = text.getBytes(StandardCharsets.UTF_8);
        ByteBuf buf = Unpooled.buffer(payload.length + 10);
        buf.writeByte(BIT_FIN | OP_TEXT);
        writeLength(buf, payload.length);
        buf.writeBytes(payload);
        return buf;
    }

    /**
     * FIN + Close 帧（状态码 + 可选原因）
     */
    static ByteBuf closeFrame(int statusCode, String reason) {
        byte[] reasonBytes = reason != null ? reason.getBytes(StandardCharsets.UTF_8) : new byte[0];
        ByteBuf buf = Unpooled.buffer(reasonBytes.length + 4);
        buf.writeByte(BIT_FIN | OP_CLOSE);
        writeLength(buf, 2 + reasonBytes.length);
        buf.writeShort(statusCode);
        buf.writeBytes(reasonBytes);
        return buf;
    }

    /**
     * FIN + Pong 帧（回显 Ping 载荷）
     */
    static ByteBuf pongFrame(byte[] pingPayload) {
        ByteBuf buf = Unpooled.buffer(pingPayload.length + 10);
        buf.writeByte(BIT_FIN | OP_PONG);
        writeLength(buf, pingPayload.length);
        buf.writeBytes(pingPayload);
        return buf;
    }

    /**
     * RFC 6455 长度分段：7 位 / 16 位 / 64 位
     */
    private static void writeLength(ByteBuf buf, int len) {
        if (len <= MAX_LEN_7BIT) {
            buf.writeByte(len);
        } else if (len <= MAX_LEN_16BIT) {
            buf.writeByte(LEN_EXT16);
            buf.writeShort(len);
        } else {
            buf.writeByte(LEN_EXT64);
            buf.writeLong(len);
        }
    }
}

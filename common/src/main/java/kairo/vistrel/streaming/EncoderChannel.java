package kairo.vistrel.streaming;

import java.nio.ByteBuffer;
import java.util.List;

/**
 * 推流数据面端口：系统内存帧、音频 PCM 与显存直通会话的投递入口，以及编码能力查询。
 * 上行输入事件由 {@link kairo.vistrel.input.InputEventSource} 端口承载，数据面实现
 * 同时提供两个面。
 */
public interface EncoderChannel {
    /**
     * 投递系统内存帧 (RGBA DirectByteBuffer)。
     *
     * @param buffer 包含未压缩 RGBA 像素数据的堆外缓冲区
     * @param width  可见区宽度（像素）
     * @param height 可见区高度（像素）
     */
    void pushSystemFrame(ByteBuffer buffer, int width, int height);

    /**
     * 投递音频 PCM 数据帧。
     *
     * @param buffer            包含 16-bit PCM 的堆外 DirectBuffer
     * @param samplesPerChannel 单声道采样点数 (通常为 960，对应 20ms)
     */
    default void pushAudioPcm(ByteBuffer buffer, int samplesPerChannel) {}

    /**
     * 本机编码能力快照，拉式纯读，无等待、无设备调用
     *
     * <p>
     * 会话实际在跑什么由 {@link SessionServer.EncoderGeneration#backend()} 承载。
     *
     * @return 能力快照，不为 {@code null}
     */
    default EncoderCaps caps() {
        return EncoderCaps.EMPTY;
    }

    /**
     * 把当前导出资源整批绑定到 Native 编码器。
     *
     * <p>整批处理：要么全成要么全败。绑定只对导入时取得的绑定句柄有效；编码器重建后
     * 旧绑定的推送在 Native 入口丢弃，调用方应重新导入。
     *
     * @param slots 槽位描述，索引与几何由调用方与 Native 双重校验
     * @return 已导入的显存绑定
     * @throws UnsupportedOperationException 实现不支持显存直通
     * @throws ControlException              Native 拒绝整批导入
     */
    default VramBinding bindVram(List<VramSlotDescriptor> slots) {
        throw new UnsupportedOperationException("此通道不支持显存直通");
    }

    /**
     * 一个可被 Native 硬件编码器导入的显存槽位描述。
     *
     * <p>这是 Java 侧的类型化视图；JNI 的 {@code long[]} 7 元组编码只发生在
     * {@code NativeEncoderChannel} 内，采集来源不接触线协议。
     *
     * @param slotIdx   槽位索引，0 起始，必须小于槽位环容量
     * @param memHandle 外部显存句柄（Linux fd / Win32 handle / OpenGL 纹理 ID）
     * @param semHandle 时间线信号量句柄；OpenGL 后端无信号量，固定为 0
     * @param width     纹理宽度（像素）
     * @param height    纹理高度（像素）
     * @param sizeBytes 外部显存总大小（字节）；OpenGL 后端固定为 0
     * @param offset    纹理在外部显存中的字节偏移；OpenGL 后端固定为 0
     */
    record VramSlotDescriptor(
            int slotIdx,
            long memHandle,
            long semHandle,
            int width,
            int height,
            long sizeBytes,
            long offset
    ) {
        public VramSlotDescriptor {
            if (slotIdx < 0) {
                throw new IllegalArgumentException("槽位索引不能为负: " + slotIdx);
            }
            if (memHandle == 0L) {
                throw new IllegalArgumentException("槽位 " + slotIdx + " 缺少外部显存句柄");
            }
            if (width <= 0 || height <= 0) {
                throw new IllegalArgumentException("槽位 " + slotIdx + " 几何非法: " + width + "x" + height);
            }
            if (sizeBytes < 0L || offset < 0L) {
                throw new IllegalArgumentException("槽位 " + slotIdx + " 大小/偏移不能为负");
            }
        }
    }

    /**
     * 控制命令失败的异常载体，消费方按 {@link #status} 分支。
     */
    class ControlException extends IllegalStateException {
        private static final long serialVersionUID = 1L;
        /**
         * Native 原始判别值；表外码也保留。
         */
        public final int code;
        /**
         * 对应枚举；表外码为 {@code null}。
         */
        public final ControlStatus status;

        public ControlException(String message, int code) {
            super(message + " (" + code + ": " + ControlStatus.describe(code) + ")");
            this.code = code;
            this.status = ControlStatus.fromCode(code);
        }

        public ControlException(String message, ControlStatus status) {
            this(message, status.code());
        }

        /**
         * 是否属于「编码器不可用」类故障（无硬编 / 建成失败）：降级采集救不了，
         * 唯一恢复路径是整引擎自愈重启。
         *
         * @return true 表示应触发引擎自愈而不是降级到 RAM
         */
        public boolean encoderUnavailable() {
            return status == ControlStatus.NO_HARDWARE_ENCODER
                    || status == ControlStatus.ENCODER_BUILD_FAILED
                    // Actor 已死：降级 RAM 救不了没有编码器的流，只能整引擎自愈
            || status == ControlStatus.PIPELINE_DEAD;
        }
    }
}

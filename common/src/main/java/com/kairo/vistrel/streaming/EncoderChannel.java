package com.kairo.vistrel.streaming;

import java.nio.ByteBuffer;

/**
 * 推流数据面编码通道：面向 Native 编码管线的下行媒体投递（系统内存帧、音频 PCM、
 * 显存槽位）与编码能力查询。上行输入事件由 {@link com.kairo.vistrel.input.InputEventSource}
 * 端口承载，数据面实现同时提供两个面。
 */
public interface EncoderChannel {
    /**
     * 投递系统内存帧 (RGBA DirectByteBuffer)。
     *
     * @param buffer 包含未压缩 RGBA 像素数据的堆外缓冲区
     */
    void pushSystemFrame(ByteBuffer buffer);

    /**
     * 投递音频 PCM 数据帧。
     *
     * @param buffer            包含 16-bit PCM 的堆外 DirectBuffer
     * @param samplesPerChannel 单声道采样点数 (通常为 960，对应 20ms)
     */
    default void pushAudioPcm(ByteBuffer buffer, int samplesPerChannel) {}

    /**
     * 查询常驻编码能力快照（拉式纯读，无等待、无设备调用）
     *
     * @return 线格式 {@code 后端|codec,codec[|显卡]}：{@code nvenc|H264,H265} /
     *         {@code vulkan_video|...} / {@code software|H264}（已探测且无硬编）；
     *         {@code unknown} 表示引擎尚未启动过；空串表示 Native 库不可用
     */
    default String caps() {
        return "";
    }

    /**
     * 显存槽位扁平协议的每槽位 long 数。与 Native 侧 {@code cuda::FLAT_STRIDE}
     * 是同一条约定的两端，改动必须同步。
     *
     * <p>
     * 每槽位 7 元组:
     * <ol>
     * <li>{@code [0] slotIdx} —— 槽位索引，多槽位循环复用</li>
     * <li>{@code [1] memHandle} —— 外部显存句柄；OpenGL 后端为 GL 纹理 ID</li>
     * <li>{@code [2] semHandle} —— 时间线信号量句柄；OpenGL 后端无信号量，固定为 0</li>
     * <li>{@code [3] width} —— 纹理宽度（像素）</li>
     * <li>{@code [4] height} —— 纹理高度（像素）</li>
     * <li>{@code [5] sizeBytes} —— 外部显存总大小（字节）；OpenGL 后端固定为 0</li>
     * <li>{@code [6] offset} —— 纹理在外部显存中的字节偏移；OpenGL 后端固定为 0</li>
     * </ol>
     * 数组长度不是本值的整数倍时，整批拒绝并返回 {@link #CONTROL_MALFORMED_BATCH}。
     */
    int VRAM_SLOT_STRIDE = 7;
    /**
     * 控制结果码：数值与 Rust 侧 {@code encode::control::ControlError} 的判别值一一对应
     * （{@code rust-native/src/encode/control.rs}），改一处必须同步另一处。
     *
     * {@link #CONTROL_RECONFIGURE_FAILED} 目前只在 Native 内部产生（档位重配失败的会话内上报），
     * 还没有 JNI 出口能把它带回 Java；补齐它是为了码表与 Rust 枚举保持一对一。
     */
    int CONTROL_OK = 0;
    int CONTROL_MALFORMED_BATCH = 1;
    int CONTROL_NO_HARDWARE_ENCODER = 2;
    int CONTROL_IMPORT_FAILED = 3;
    int CONTROL_PIPELINE_DEAD = 4;
    int CONTROL_ACK_TIMEOUT = 5;
    int CONTROL_RECONFIGURE_FAILED = 6;
    int CONTROL_QUEUE_FULL = 7;
    int CONTROL_RELEASE_FAILED = 8;

    /**
     * 控制结果码转可读原因
     */
    static String describeControlResult(int code) {
        return switch (code) {
            case CONTROL_OK -> "成功";
            case CONTROL_MALFORMED_BATCH -> "槽位批量协议错位 (数组长度不是每槽位 long 数的整数倍)";
            case CONTROL_NO_HARDWARE_ENCODER -> "本机无可用硬件编码器";
            case CONTROL_IMPORT_FAILED -> "外部显存或时间线信号量导入失败 (整批已回滚)";
            case CONTROL_PIPELINE_DEAD -> "编码管线已死 (fail-closed)";
            case CONTROL_ACK_TIMEOUT -> "Actor ACK 超时";
            case CONTROL_RECONFIGURE_FAILED -> "档位重配被编码线程拒绝 (编码器重配失败，档位保持不变)";
            case CONTROL_QUEUE_FULL -> "控制队列已满 (编码线程已卡住不消费)";
            case CONTROL_RELEASE_FAILED -> "注销前排空失败 (导入引用与槽位表已保留)";
            default -> "未知错误码 " + code;
        };
    }

    /**
     * 显存槽位租约 (RAII)：由 {@link #importVramSlots(long[])} 成功后返回，
     * 投递与注销都挂在租约上——没注册就拿不到租约，不 close 就完不成注销。
     *
     * <p>
     * 线程约定：push 在渲染线程调用；close 幂等，且必须在销毁槽位纹理/信号量之前
     * 调用（先停写者、再释放资源的固定次序），所以不做 Cleaner 兜底。
     * close 后再 push 属编程错误，抛 {@link IllegalStateException}。
     */
    interface SlotLease extends AutoCloseable {
        /**
         * 向 Native 引擎直通投递一帧已注册槽位（单向投递，不等待编码结果）。
         *
         * @param slotIdx   显存环形缓冲区的槽位索引 (0 &lt;= slotIdx &lt; SLOT_COUNT)
         * @param signalVal 本帧与 GPU 提交流原子绑定的时间线信号量信号值
         */
        void push(int slotIdx, long signalVal);

        /**
         * {@link #close()} 之后查询原生侧是否确认完成槽位注销。
         *
         * <p>
         * 返回 {@code false} 表示注销没得到编码线程确认（ACK 超时 / 控制队列满 / 排空失败）：
         * 编码器可能还在读这些导出资源，此时销毁游戏侧的纹理/信号量/导出池是驱动层
         * 未定义行为。必须把这些资源留着不销毁（GpuTexture 没有 Cleaner，丢掉引用不会
         * 触发释放），等引擎关停时随 VkDevice 一起回收。返回 {@code true} 才能继续销毁。
         */
        default boolean isReleased() {
            return true;
        }

        @Override
        void close();
    }

    /**
     * 批量导入外部显存槽位与同步原语，成功返回租约。
     *
     * <p>
     * <b>整批处理</b>：要么全成要么全败。任一槽位导入失败即整批回滚，
     * 整体回退 RAM 回读采集。失败抛 {@link IllegalStateException}，
     * 可读原因见 {@link #describeControlResult(int)}。
     *
     * @param slots 扁平协议数组，长度必须是 {@link #VRAM_SLOT_STRIDE} 的整数倍
     */
    default SlotLease importVramSlots(long[] slots) {
        throw new UnsupportedOperationException("此通道不支持显存直通");
    }

    /**
     * 取当前活跃引擎的槽位所有权共享块（64 字节，原生端序 DirectBuffer）
     *
     * <p>
     * 布局：{@code [0..32) = busy[8] int 位}（1 = Java 已认领且未归还）、
     * {@code [32..40) = heartbeat long}。访问走 acquire/release 语义，
     * 块随引擎实例换代——每次构建显存管线时取一次，不得缓存跨引擎复用。
     *
     * @return DirectBuffer；引擎未运行或不支持显存直通时为 null
     */
    default ByteBuffer slotStateBlock() {
        return null;
    }
}

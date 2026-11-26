package kairo.vistrel.streaming.natives;

/**
 * JNI 数据面线协议的 Java 侧唯一定义处。
 *
 * <p>Rust 端对应 {@code encode::hardware::slots}（槽位扁平协议）、
 * {@code jni::wire::pack_slot_state} / {@code pack_status}（回复打包）与
 * {@code encode::control::ControlStatus}（结果码表）。项目当前没有自动跨语言 gate 或启动
 * 握手：修改本类必须同步 Rust 端；正常安装下内嵌原生库有 SHA-256 校验兜底。
 *
 * <p>{@code vramSlotState} 返回 packed {@code long}（原生端序无关，按位解释）：
 * <ul>
 *   <li>{@code [7:0]} —— 在途掩码，一位一槽</li>
 *   <li>{@code [39:8]} —— 编码 Actor 心跳（单调递增 u32）</li>
 *   <li>{@code [55:40]} —— 保留，0</li>
 *   <li>{@code [63:56]} —— {@code status + 1}；0 是 JNI 异常的默认值，表示未知/故障</li>
 * </ul>
 *
 * <p>{@code pushVramSlot} 只回答「帧收没收到」：返回 {@code int} 的 {@code status + 1}，
 * 掩码与心跳只出现在 {@code vramSlotState} 快照里。
 *
 * <p>没有共享内存：Java 只持有一个不透明绑定句柄，所有状态经 JNI 往返。
 */
final class NativeWireContract {
    /**
     * 数据面协议版本。当前不做运行时握手；该值仅用于日志/诊断与后续演进占位。
     */
    public static final int PROTOCOL_VERSION = 4;
    /**
     * 显存槽位扁平协议每槽位的 long 数。
     */
    public static final int VRAM_SLOT_STRIDE = 7;
    /**
     * 掩码容量：与 Rust {@code MAX_VRAM_SLOTS} 同一条约定。
     */
    public static final int MAX_VRAM_SLOTS = 8;
    /**
     * 在途掩码占用的低位数。
     */
    public static final int MASK_BITS = 8;
    /**
     * 心跳字段的位偏移。
     */
    public static final int HEARTBEAT_SHIFT = 8;
    /**
     * 状态字节的位偏移。
     */
    public static final int STATUS_SHIFT = 56;

    private NativeWireContract() {
    }
}

package kairo.vistrel.streaming;

/**
 * Native 控制面结果码的 Java 侧类型表。
 *
 * <p>数值与 Rust {@code encode::control::ControlStatus} 一一对应；JNI 边界只传 {@code int}。
 * 契约变更必须两端同步，版本见 {@code kairo.vistrel.streaming.natives.NativeWireContract}。
 *
 * <p>每个码只表达一种调用方需要区分的恢复类别。{@link #description()} 是给日志与异常
 * 消息的简短原因，不参与判断；同一类别内的具体成因（哪条槽位协议被违例、导入为何回滚）
 * 由 Native 在产生点记录，不占用独立码位。
 */
public enum ControlStatus {
    /**
     * 命令已执行
     */
    OK(0, "成功"),
    /**
     * 本机没有可用硬件编码器：未探测到硬编，或候选后端全部建成失败
     */
    ENCODER_UNAVAILABLE(1, "无可用硬件编码器"),
    /**
     * 编码管线不可用：Actor 控制通道已关闭，或应答无法解析
     */
    PIPELINE_DEAD(2, "编码管线不可用"),
    /**
     * 控制面未受理或未应答：队列已满或 ACK 超时，命令可能已经执行
     */
    CONTROL_BUSY(3, "控制面未受理或未应答"),
    /**
     * 显存直通故障：导入整批回滚、注销保留导入引用，或槽位协议违例
     */
    VRAM_FAULT(4, "显存直通故障"),
    /**
     * 显存绑定已失效：句柄不属于当前引擎，或绑定已被换档/注销摘除
     */
    STALE_BINDING(5, "显存绑定已失效"),
    /**
     * 显存绑定已毒化：提交等待超时，在途 GPU 工作可能仍在读槽位纹理，拒绝后续推送
     */
    POISONED(6, "显存绑定已毒化");
    private final int code;
    private final String description;

    ControlStatus(int code, String description) {
        this.code = code;
        this.description = description;
    }

    /**
     * 线上判别值
     *
     * @return 与 Rust 枚举 discriminant 相同的整数
     */
    public int code() {
        return code;
    }

    /**
     * 简短原因
     *
     * @return 面向日志与异常消息的描述
     */
    public String description() {
        return description;
    }

    /**
     * 数值转枚举
     *
     * @param code Native 返回的判别值
     * @return 对应枚举；表外数值返回 {@code null}（调用方保留原始码）
     */
    public static ControlStatus fromCode(int code) {
        for (ControlStatus status : values()) {
            if (status.code == code) {
                return status;
            }
        }
        return null;
    }

    /**
     * 数值转简短原因，表外数值原样展示
     *
     * @param code Native 返回的判别值
     * @return 枚举描述或 {@code 未知错误码 <n>}
     */
    public static String describe(int code) {
        ControlStatus status = fromCode(code);
        return status != null ? status.description : "未知错误码 " + code;
    }
}

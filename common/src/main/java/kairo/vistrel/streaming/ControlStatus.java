package kairo.vistrel.streaming;

/**
 * Native 控制面结果码的 Java 侧类型表。
 *
 * <p>数值与 Rust {@code encode::control::ControlStatus} 一一对应；JNI 边界只传 {@code int}。
 * 契约变更必须两端同步，版本见 {@code kairo.vistrel.streaming.natives.NativeWireContract}。
 */
public enum ControlStatus {
    OK(0, "成功"),
    MALFORMED_BATCH(1, "槽位批量协议错位 (数组长度不是每槽位 long 数的整数倍)"),
    NO_HARDWARE_ENCODER(2, "本机无可用硬件编码器"),
    IMPORT_FAILED(3, "外部显存或时间线信号量导入失败 (整批已回滚)"),
    PIPELINE_DEAD(4, "编码管线已死 (fail-closed)"),
    ACK_TIMEOUT(5, "Actor ACK 超时"),
    RECONFIGURE_FAILED(6, "档位重配被编码线程拒绝 (编码器重配失败，档位保持不变)"),
    QUEUE_FULL(7, "控制队列已满 (编码线程已卡住不消费)"),
    RELEASE_FAILED(8, "注销前排空失败 (导入引用与槽位表已保留)"),
    ENCODER_BUILD_FAILED(9, "编码器建成失败 (候选后端全部创建失败，详见日志)"),
    STALE_BINDING(10, "显存绑定已过期 (旧轮次的槽位导入被拒绝)");
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
     * 可读原因
     *
     * @return 面向日志 / 异常消息的中文描述
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
     * 数值转可读原因，表外数值原样展示
     *
     * @param code Native 返回的判别值
     * @return 枚举描述或 {@code 未知错误码 <n>}
     */
    public static String describe(int code) {
        ControlStatus status = fromCode(code);
        return status != null ? status.description : "未知错误码 " + code;
    }
}

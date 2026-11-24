package kairo.vistrel.streaming.natives;

/**
 * JNI 数据面线协议的 Java 侧唯一定义处。
 *
 * <p>Rust 端对应 {@code encode::hardware::slots}（槽位扁平协议、共享块布局、阶段码）
 * 与 {@code encode::control::ControlStatus}（结果码表）。项目当前没有自动跨语言 gate
 * 或启动握手：修改本类必须同步 Rust 端；正常安装下内嵌原生库有 SHA-256 校验兜底。
 *
 * <p>共享块布局（64 字节单缓存行，原生端序）：
 * <ul>
 *   <li>{@code [0..32)} —— busy[8]，每槽位一个 u32，1 = 渲染线程已认领</li>
 *   <li>{@code [32..40)} —— heartbeat，编码线程每轮递增</li>
 *   <li>{@code [40..48)} —— stage，编码线程当前阶段码</li>
 *   <li>{@code [48..56)} —— bindingEpoch，显存绑定世代</li>
 *   <li>{@code [56..64)} —— 保留</li>
 * </ul>
 */
final class NativeWireContract {
    /**
     * 数据面协议版本。当前不做运行时握手；该值仅用于日志/诊断与后续演进占位。
     */
    public static final int PROTOCOL_VERSION = 1;
    /**
     * 显存槽位扁平协议每槽位的 long 数。
     */
    public static final int VRAM_SLOT_STRIDE = 7;
    /**
     * 共享块字节数。
     */
    public static final int SLOT_STATE_BLOCK_BYTES = 64;
    /**
     * 共享块 busy 位容量（与掩码容量同一条约定）。
     */
    public static final int SLOT_STATE_SLOTS = 8;
    /**
     * busy[8] 起始偏移。
     */
    public static final int BUSY_OFFSET = 0;
    /**
     * heartbeat 偏移。
     */
    public static final int HEARTBEAT_OFFSET = 32;
    /**
     * stage 偏移。
     */
    public static final int STAGE_OFFSET = 40;
    /**
     * 显存绑定世代偏移：Native Actor 在重建/失效重配时递增，导入/推送/注销都按它准入。
     */
    public static final int BINDING_EPOCH_OFFSET = 48;
    /**
     * 保留段起始偏移。
     */
    public static final int RESERVED_OFFSET = 56;

    private NativeWireContract() {
    }

    /**
     * 编码线程阶段码，与 Rust {@code ActorStage} 判别值一一对应。
     */
    public enum Stage {
        IDLE(0, "空闲等待"),
        RECONFIGURE(1, "重配编码器"),
        DRAIN(2, "排空编码器"),
        IMPORT_SLOTS(3, "导入显存槽位"),
        CLEAR_SLOTS(4, "注销显存槽位"),
        SHUTDOWN(5, "编码器关停"),
        WAIT_TIMELINE(6, "等待时间线信号量"),
        MAP_INPUT(7, "映射输入资源"),
        ENCODE_SUBMIT(8, "提交编码"),
        LOCK_BITSTREAM(9, "锁定码流"),
        ENCODE_FRAME(10, "帧编码");
        private final int code;
        private final String label;

        Stage(int code, String label) {
            this.code = code;
            this.label = label;
        }

        /**
         * 阶段判别值
         *
         * @return 与 Rust 枚举相同的整数
         */
        public int code() {
            return code;
        }

        /**
         * 阶段显示名
         *
         * @return 中文短标签
         */
        public String label() {
            return label;
        }

        /**
         * 数值转阶段
         *
         * @param code 阶段判别值
         * @return 对应阶段；表外数值返回 {@code null}
         */
        public static Stage fromCode(int code) {
            for (Stage stage : values()) {
                if (stage.code == code) {
                    return stage;
                }
            }
            return null;
        }

        /**
         * 数值转可读阶段名，表外数值原样展示
         *
         * @param code 阶段判别值
         * @return 阶段中文名或 {@code 未知阶段(<n>)}
         */
        public static String describe(int code) {
            Stage stage = fromCode(code);
            return stage != null ? stage.label : "未知阶段(" + code + ")";
        }
    }
}

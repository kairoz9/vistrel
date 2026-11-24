package kairo.vistrel.streaming;

import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import java.util.ArrayList;
import java.util.List;
import java.util.Objects;

/**
 * 本机编码能力快照，由 {@code NativeBridge.queryEncoderCaps()} 的 JSON 解析而来
 *
 * <p>
 * 会话实际在跑什么由 {@link SessionServer.EncoderGeneration#backend()} 承载。
 *
 * <p>
 * {@link State} 的三态不能合并：未探测是调用顺序问题，探测过无硬件是本机能力缺失。
 *
 * @param state    探测状态
 * @param backends 探测到的硬件后端，按厂商优先级序；不含软编
 */
public record EncoderCaps(State state, List<Backend> backends) {
    /**
     * 探测状态，与 Native 侧 {@code ProbeOutcome} 一一对应
     */
    public enum State {
        /**
         * 引擎尚未启动，探测没跑过
         */
        NOT_PROBED("notProbed"),
        /**
         * 探测过，本机没有硬件编码器
         */
        NO_HARDWARE("noHardware"),
        /**
         * 探测到至少一个硬件后端
         */
        CAPABILITIES("capabilities");
        private final String wireName;

        State(String wireName) {
            this.wireName = wireName;
        }

        /**
         * 线上词汇转状态，识别不出按未探测处理
         *
         * <p>
         * 未知词汇说明两边版本不一致。此时报「没有硬编」是假话，报「还没探测」至少
         * 不会误导。
         *
         * @param token Native 应答的 {@code state} 字段
         * @return 对应状态；{@code null} 或未知词汇返回 {@link #NOT_PROBED}
         */
        public static State fromWire(String token) {
            for (State s : values()) {
                if (s.wireName.equals(token)) {
                    return s;
                }
            }
            return NOT_PROBED;
        }

        public String wireName() {
            return wireName;
        }
    }

    /**
     * 单个后端的线上条目，与 Rust 侧 {@code BackendCapability::to_wire_json} 同形状
     *
     * @param kind   线上后端词汇（{@code nvenc} / {@code vulkan_video} / {@code software}）
     * @param codecs 支持的 codec，线上拼写为大写 {@code H264} / {@code H265} / {@code AV1}
     * @param gpu    显卡名；空串表示未知，软编恒为空串
     */
    public record Backend(String kind, List<String> codecs, String gpu) {
        /**
         * 软编以外都是硬件后端
         */
        public boolean hardware() {
            return !"software".equals(kind);
        }

        /**
         * 后端身份的显示名；词表外令牌原样返回
         *
         * @return {@code NVENC} / {@code Vulkan Video} / {@code 软件编码}，或未识别的 {@code kind}
         */
        public String label() {
            return switch (kind) {
                case "nvenc" -> "NVENC";
                case "vulkan_video" -> "Vulkan Video";
                case "software" -> "软件编码";
                default -> kind;
            };
        }

        /**
         * codec 的显示名，线上拼写加小数点
         *
         * @return {@code H.264} / {@code H.265} / {@code AV1} 的列表，顺序与 {@link #codecs()} 一致
         */
        public List<String> codecLabels() {
            return codecs.stream().map(Backend::codecLabel).toList();
        }

        private static String codecLabel(String codec) {
            return switch (codec) {
                case "H264" -> "H.264";
                case "H265" -> "H.265";
                default -> codec;
            };
        }

        public Backend {
            Objects.requireNonNull(kind, "后端身份不能为空");
            codecs = List.copyOf(codecs);
            gpu = gpu == null ? "" : gpu;
        }
    }

    public static final EncoderCaps EMPTY = new EncoderCaps(State.NOT_PROBED, List.of());

    public EncoderCaps {
        Objects.requireNonNull(state, "探测状态不能为空");
        backends = List.copyOf(backends);
    }

    /**
     * 解析 json：{@code {"state":..., "backends":[{kind,codecs,gpu}]}}
     *
     * <p>
     * 畸形应答返 {@link #EMPTY}，不抛异常。
     *
     * @param json Native 应答原文；{@code null} 或空串视为未探测
     * @return 解析结果，不为 {@code null}
     */
    public static EncoderCaps parse(String json) {
        if (json == null || json.isBlank()) {
            return EMPTY;
        }
        try {
            JsonObject obj = JsonParser.parseString(json).getAsJsonObject();
            State state = State.fromWire(obj.get("state").getAsString());
            List<Backend> parsed = new ArrayList<>();
            JsonArray raw = obj.getAsJsonArray("backends");
            if (raw != null) {
                for (JsonElement element : raw) {
                    JsonObject entry = element.getAsJsonObject();
                    List<String> codecs = new ArrayList<>();
                    JsonArray rawCodecs = entry.getAsJsonArray("codecs");
                    if (rawCodecs != null) {
                        for (JsonElement codec : rawCodecs) {
                            codecs.add(codec.getAsString());
                        }
                    }
                    String gpu = entry.get("gpu") == null ? "" : entry.get("gpu").getAsString();
                    parsed.add(new Backend(entry.get("kind").getAsString(), codecs, gpu));
                }
            }
            return new EncoderCaps(state, parsed);
        } catch (Exception e) {
            return EMPTY;
        }
    }

    /**
     * 探测是否跑过，无论有没有硬编
     */
    public boolean probed() {
        return state != State.NOT_PROBED;
    }

    /**
     * 本机是否至少有一个硬件编码后端，显存直通门控据此判定
     */
    public boolean hasHardware() {
        return backends.stream().anyMatch(Backend::hardware);
    }

    /**
     * 按线上词汇查条目
     *
     * <p>
     * 软编不在 {@link #backends} 里，按静态可行域现造。清单外的硬件后端返回
     * {@code null}。
     *
     * @param kind 线上后端词汇
     * @return 该后端的条目；词汇非法或硬件后端不在探测清单内返回 {@code null}
     */
    public Backend find(String kind) {
        if (kind == null || kind.isEmpty()) {
            return null;
        }
        for (Backend b : backends) {
            if (b.kind().equals(kind)) {
                return b;
            }
        }
        if ("software".equals(kind)) {
            return new Backend("software", List.of("H264"), "");
        }
        return null;
    }
}

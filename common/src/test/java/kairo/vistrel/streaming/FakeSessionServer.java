package kairo.vistrel.streaming;

import kairo.vistrel.input.InputEventSource;
import java.nio.ByteBuffer;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;

/**
 * 纯 Java 内存测试替身 (Test Double)：0 JNI、0 端口占用、秒级模拟推流会话与输入事件
 */
public class FakeSessionServer implements SessionServer {
    public static class FakeEncoderChannel implements EncoderChannel, InputEventSource {
        public int systemFramesPushed = 0;
        public int audioFramesPushed = 0;
        public boolean vramDirectSupported = false;
        /**
         * 覆盖 caps() 的应答：null 表示按 {@link #vramDirectSupported} 推导，
         * 非 null 时由用例直接给定能力快照
         */
        public EncoderCaps capsOverride = null;

        @Override
        public void pushSystemFrame(ByteBuffer buffer, int width, int height) {
            systemFramesPushed++;
        }

        @Override
        public void pushAudioPcm(ByteBuffer buffer, int samplesPerChannel) {
            audioFramesPushed++;
        }

        @Override
        public EncoderCaps caps() {
            if (capsOverride != null) {
                return capsOverride;
            }
            if (vramDirectSupported) {
                return new EncoderCaps(
                        EncoderCaps.State.CAPABILITIES,
                        List.of(new EncoderCaps.Backend("nvenc", List.of("H264", "H265"), "NVIDIA GeForce RTX 5060"))
                );
            }
            return new EncoderCaps(EncoderCaps.State.NO_HARDWARE, List.of());
        }
    }

    private final List<ClientEvent> events = new CopyOnWriteArrayList<>();
    private volatile java.util.function.Consumer<String> textInputSink = null;
    private volatile boolean running = false;
    private volatile String password = "";
    private volatile int audioBitrate = 0;
    /**
     * start 调用计数：重启用例用它证明自愈真的拆装过（而非冷却期空转）
     */
    public final java.util.concurrent.atomic.AtomicInteger startCount = new java.util.concurrent.atomic.AtomicInteger();

    @Override
    public synchronized void start(String address, int port) {
        this.running = true;
        startCount.incrementAndGet();
    }

    @Override
    public synchronized void stop() {
        this.running = false;
    }

    @Override
    public boolean isRunning() {
        return running;
    }

    @Override
    public void setPassword(String password) {
        this.password = password;
    }

    public String getPassword() {
        return password;
    }

    @Override
    public void setAudioBitrate(int bitrateKbps) {
        this.audioBitrate = bitrateKbps;
    }

    @Override
    public void pushEvent(ClientEvent event) {
        events.add(event);
    }

    /**
     * 测试辅助：检索已推送的下行控制事件（record 等值比较可直接断言）
     */
    public List<ClientEvent> getEvents() {
        return List.copyOf(events);
    }

    @Override
    public void setTextInputSink(java.util.function.Consumer<String> sink) {
        this.textInputSink = sink;
    }

    /**
     * 测试辅助：模拟浏览器输入文本经已注册 sink 送达，返回 sink 是否已接线
     */
    public boolean emitTextInput(String text) {
        java.util.function.Consumer<String> sink = textInputSink;
        if (sink != null) {
            sink.accept(text);
            return true;
        }
        return false;
    }

    public int getAudioBitrate() {
        return audioBitrate;
    }

    /**
     * 注入的档位协调器：VistrelService 经接口注入，与原生信令面同一条链路
     */
    private volatile ProfileCoordinator profileCoordinator;

    @Override
    public void setProfileCoordinator(ProfileCoordinator worker) {
        profileCoordinator = worker;
    }

    /**
     * 模拟 offer 协商成功：与生产同链路——Answer JSON 交档位协调器落账
     * （基线种入 + listener 广播初始档位），不经会话状态管道
     */
    public void simulateClientConnected(int width, int height, int fps) {
        simulateClientConnected(width, height, fps, "nvenc");
    }

    /**
     * 模拟 offer 协商成功并指定落账的后端身份
     *
     * @param backend 协商应答的线上后端词汇（nvenc / vulkan_video / software）
     */
    public void simulateClientConnected(int width, int height, int fps, String backend) {
        ProfileCoordinator worker = profileCoordinator;
        if (worker != null) {
            worker.noteOfferAnswer(
                    "{\"width\":"
                    + width
                    + ",\"height\":"
                    + height
                    + ",\"fps\":"
                    + fps
                    + ",\"bpp\":0.1,\"bitrate\":4000000,\"codec\":\"H264\",\"backend\":\""
                    + backend
                    + "\"}",
                    0.1
            );
        }
    }

    public void simulateClientDisconnected() {
        ProfileCoordinator worker = profileCoordinator;
        if (worker != null) {
            // 与生产同链路：信令层把关闭事件直接投给协调器
            worker.onSessionClosed();
        }
    }

    @Override
    public String applyProfile(int width, int height, int fps, double bpp) {
        // 测试替身直接接受请求档位（真实 Native 会回实际生效档位）；
        // 应答只含档位字段——codec 与后端身份是会话常量，不随档位应答走
        return "{\"width\":"
                + width
                + ",\"height\":"
                + height
                + ",\"fps\":"
                + fps
                + ",\"bpp\":"
                + bpp
                + ",\"bitrate\":4000000}";
    }
}

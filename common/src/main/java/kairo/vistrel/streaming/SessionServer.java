package kairo.vistrel.streaming;

/**
 * 推流控制面服务 (Control Plane)：负责网络服务生命周期、访问鉴权与推流会话感知。
 */
public interface SessionServer {
    /**
     * 启动推流服务
     *
     * @param address 监听地址
     * @param port 监听端口号
     */
    void start(String address, int port);

    /**
     * 停止服务并清理
     */
    void stop();

    /**
     * 停止接收新连接（幂等）：供采集等写者先退出；默认空实现
     */
    default void stopAccepting() {}

    /**
     * 服务当前是否正在运行
     */
    boolean isRunning();

    /**
     * 设置访问密码
     */
    void setPassword(String password);

    /**
     * 设置音频推流编码码率 (kbps)
     */
    void setAudioBitrate(int bitrateKbps);

    /**
     * 推送下行控制事件
     *
     * <p>调用方线程不限，实现必须异步入队；无活跃连接时静默丢弃。
     * 只收小型可 JSON 化控制事件，二进制载荷不在此通道。
     */
    void pushEvent(ClientEvent event);

    /**
     * 注册文本输入接收器：浏览器经信令 WS 发来的 GUI 文本（聊天/文本框输入），
     * 实现在信令线程回调 sink，消费方自行跳线程；传 null 解除注册
     */
    void setTextInputSink(java.util.function.Consumer<String> sink);

    /**
     * 阻塞式应用编码档位（控制面 → Native 会话重配）：成功回生效档位 JSON，
     * 无会话 / 引擎未启动 / 请求被拒回空串。调用方只能是后台 ask 线程。
     */
    String applyProfile(int width, int height, int fps, double bpp);

    /**
     * 注入档位协调器：信令面在 offer 协商成功时回调落账、会话关闭时投递复位。
     * 默认空实现，无需协调链路的实现（如测试替身）不必覆写
     */
    default void setProfileCoordinator(ProfileCoordinator worker) {}

    /**
     * 内容相位：STREAMING 内容在流动；PAUSED 会话在线但内容丢失
     */
    enum Phase {
        STREAMING,
        PAUSED
    }

    /**
     * 档位：显示几何、帧率与画质（bpp）
     *
     * <p>宽高为**显示尺寸**（码流声明的 Content Resolution，1080 显示高度即 1080）。
     */
    record Tier(int width, int height, int fps, double bpp) {}

    /**
     * 一个已协商建成的编码器代次：backend + codec，对象身份即控制面会话代次。
     *
     * <p>每次 offer 落账新建；{@link SessionState#withPhase} / {@link SessionState#withTier}
     * 原样携带。故意不覆写 {@code equals} / {@code hashCode}：backend/codec 相同也是不同代次，
     * 在途 ask 与采集侧按引用区分。Native 绑定是否失效由导入返回的绑定句柄回答。
     */
    final class EncoderGeneration {
        private final String backend;
        private final String codec;

        /**
         * @param backend 协商应答的线上后端词汇（nvenc / vulkan_video / software）
         * @param codec   协商应答的 codec 线上令牌（h264 / h265 / av1）；缺失时为空串
         */
        public EncoderGeneration(String backend, String codec) {
            this.backend = backend;
            this.codec = codec;
        }

        /**
         * 协商后端词汇
         *
         * @return 线上后端令牌；应答缺失时为 {@code null}
         */
        public String backend() {
            return backend;
        }

        /**
         * 协商 codec 令牌
         *
         * @return 线上 codec 令牌；缺失时为空串
         */
        public String codec() {
            return codec;
        }

        /**
         * 是否为硬件编码（nvenc / vulkan_video）：显存直通槽位只有硬件编码器能消费
         *
         * @return 协商落到硬编时为 true
         */
        public boolean hardware() {
            return "nvenc".equals(backend) || "vulkan_video".equals(backend);
        }

        @Override
        public String toString() {
            return backend + "/" + codec;
        }
    }

    /**
     * 会话控制面状态：编码器代次、内容相位与生效档位；{@code null} = 无会话
     *
     * <p>编码器代次只在协商落账时写入，此后只经 {@link #withPhase} / {@link #withTier}
     * 原样携带。消费者在动作点经 {@code ProfileCoordinator#current()} 读当下状态。
     *
     * <p>显存绑定的失效由 Native 返回的绑定句柄决定，不看本对象；
     * 几何由 {@link Tier} 承载，fps/bpp 变化不重建显存绑定。
     *
     * @param generation 编码器代次（backend + codec）
     * @param phase      内容相位
     * @param tier       生效档位
     */
    record SessionState(EncoderGeneration generation, Phase phase, Tier tier) {
        /**
         * 同会话的相位替换，编码器代次与档位原样保留
         *
         * @param next 目标相位
         * @return 新实例
         */
        public SessionState withPhase(Phase next) {
            return new SessionState(generation, next, tier);
        }

        /**
         * 同会话的档位替换，编码器代次与相位原样保留
         *
         * @param next 目标档位
         * @return 新实例
         */
        public SessionState withTier(Tier next) {
            return new SessionState(generation, phase, next);
        }

        /**
         * 是否为硬件编码
         *
         * @return 编码器代次落到硬编时为 true
         */
        public boolean hardware() {
            return generation.hardware();
        }
    }
}

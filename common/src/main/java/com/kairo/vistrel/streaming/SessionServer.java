package com.kairo.vistrel.streaming;

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
     * 是否为硬件编码（nvenc / vulkan_video）。
     *
     * @param backend 协商应答携带的线上后端词汇
     */
    public static boolean isHardwareBackend(String backend) {
        return "nvenc".equals(backend) || "vulkan_video".equals(backend);
    }

    /**
     * 档位：显示几何、帧率与画质（bpp）
     *
     * <p>宽高为**显示尺寸**（码流声明的 Content Resolution，1080 显示高度即 1080）。
     */
    record Tier(int width, int height, int fps, double bpp) {}

    /**
     * 会话控制面广播事件：事件不可变，消费者按类型分发，跨会话边界无撕裂窗口
     */
    sealed interface SessionEvent {
        /**
         * 会话已建立（协商落账）：后端身份与初始档位原子携带
         */
        record Established(String backend, Tier tier) implements SessionEvent {}

        /**
         * 档位或内容相位变化。相位只表达内容在不在流动，会话本身仍在线
         */
        record TierChanged(Phase phase, Tier tier) implements SessionEvent {}

        /**
         * 会话已关闭：停止采集、停投递，等待下一次协商
         */
        record Closed() implements SessionEvent {}

        /**
         * 内容相位：STREAMING 内容在流动；PAUSED 会话在线但内容丢失（仅停视频采集）
         */
        enum Phase {
            STREAMING,
            PAUSED
        }
    }
}

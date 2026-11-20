package com.kairo.vistrel.streaming.signaling;

/**
 * WebRTC 信令协议委托接口 (Decoupled Signaling Delegate)
 *
 * <p>信令协议与底层多媒体实现的边界：offer 协商、ICE 转发与会话终结。
 * 档位协调不经此接口——信令层直接持有 {@code ProfileCoordinator} 做快照获取、
 * 视口与 BPP 分发，委托只负责与 Native 的协议动作。
 */
public interface SignalingDelegate {
    /**
     * 收到客户端 WebRTC Offer SDP：交给 Native 协商并生成本地 Answer
     *
     * <p>目标几何与帧率已由信令层规划好，编码偏好原样透传，能否满足由 Native 按本机
     * 能力决定；Answer JSON 携带实际生效的档位与能力清单。
     *
     * @param offerSdpJson WebRTC Offer SDP JSON
     * @param width        目标宽度（内容驱动，非客户端选择）
     * @param height       目标高度
     * @param fps          目标帧率（游戏配置的最大帧率，可能被视口/VSync 夹低）
     * @param bpp          客户端画质档位
     * @param codec        客户端 Codec 偏好（线上令牌 {@code h264}/{@code h265}/{@code av1}；
     *                     {@code null} 表示无偏好；无法满足时协商被拒绝）
     * @param backend      客户端编码后端偏好（线上词汇 {@code nvenc}/{@code vulkan_video}/{@code software}；
     *                     {@code null} 表示无偏好；无法满足时协商被拒绝）
     * @return Answer JSON 字符串 (包含实际生效的分辨率、帧率、bpp、能力清单与 Answer SDP)，失败时返回 null
     */
    String onOffer(String offerSdpJson, int width, int height, int fps, double bpp, String codec, String backend);

    /**
     * 收到客户端追加的 ICE Candidate
     *
     * @param candidateJson ICE Candidate JSON
     */
    void onIceCandidate(String candidateJson);

    /**
     * 客户端信令连接断开或会话终结通知
     */
    void onSessionClosed();
}

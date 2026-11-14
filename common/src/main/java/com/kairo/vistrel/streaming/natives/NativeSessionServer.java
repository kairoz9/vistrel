package com.kairo.vistrel.streaming.natives;

import com.kairo.vistrel.Constants;
import com.kairo.vistrel.streaming.ClientEvent;
import com.kairo.vistrel.streaming.SessionServer;
import com.kairo.vistrel.streaming.ProfileCoordinator;
import com.kairo.vistrel.util.Addresses;
import java.util.function.Consumer;

/**
 * 基于底层 JNI NativeBridge 的 WebRTC 网络与信令服务实现
 */
public final class NativeSessionServer implements SessionServer {
    private final com.kairo.vistrel.streaming.signaling.SignalingServer signalingServer = new com.kairo.vistrel.streaming.signaling.SignalingServer(
            new com.kairo.vistrel.streaming.signaling.SignalingDelegate() {
                @Override
                public String onOffer(String offerSdpJson, int width, int height, int fps, double bpp) {
                    return NativeBridge.negotiateOffer(offerSdpJson, width, height, fps, bpp);
                }

                @Override
                public void onIceCandidate(String candidateJson) {
                    NativeBridge.addIceCandidate(candidateJson);
                }

                @Override
                public void onSessionClosed() {
                    NativeBridge.closeSession();
                }
            }
    );
    private volatile String currentPassword = null;
    private volatile boolean running = false;

    @Override
    public synchronized void start(String address, int port) {
        if (running) {
            return;
        }

        NativeBridge.load();
        if (!NativeBridge.isLoaded()) {
            Constants.LOG.error("Native 核心库加载失败，无法启动 WebRTC 服务");
            return;
        }

        try {
            // 服务端 trickle 的候选经信令层推给当前活跃连接
            NativeBridge.setLocalIceCandidateSink(signalingServer::sendToActiveClient);
            // 软编库运行期获取：阻塞式确保缓存/下载就绪后启动引擎
            // 媒体面与信令共用监听地址与端口号：固定端口，防火墙一条 TCP+UDP 规则就能放行
            NativeBridge.startEngine(OpenH264Loader.ensureLibrary(), mediaBindAddress(address, port));
            signalingServer.start(address, port, currentPassword);
            running = true;
        } catch (Throwable t) {
            Constants.LOG.error("启动 NativeSessionServer 失败: {}", t.getMessage(), t);
            NativeBridge.stopEngine();
        }
    }

    @Override
    public synchronized void stop() {
        if (!running) {
            return;
        }
        try {
            signalingServer.stop();
            NativeBridge.stopEngine();
        } catch (Throwable t) {
            Constants.LOG.warn("停止 NativeSessionServer 时发生异常: {}", t.getMessage());
        } finally {
            running = false;
        }
    }

    @Override
    public boolean isRunning() {
        return running;
    }

    /**
     * 关入口：停止接客与关闭复位广播，Native 引擎保持存活，供采集等写者先退出
     */
    @Override
    public void stopAccepting() {
        if (running) {
            signalingServer.stopAccepting();
        }
    }

    /**
     * 媒体面 UDP 绑定地址：复用信令的监听地址与端口号；IPv6 字面量按 SocketAddr 语法加方括号
     * （格式化交给 {@link Addresses#socketAddress}）
     *
     * <p>解析失败直接抛出：信令面随后无论如何都会因同一地址失败，不先启动引擎可让
     * 失败路径停在 Java 侧，不必跨 JNI 再回滚
     */
    private static String mediaBindAddress(String address, int port) {
        java.net.InetAddress resolved;
        try {
            resolved = java.net.InetAddress.getByName(address);
        } catch (java.net.UnknownHostException e) {
            throw new IllegalArgumentException("媒体面绑定地址无法解析: " + address, e);
        }
        return Addresses.socketAddress(resolved, port);
    }

    @Override
    public void setPassword(String password) {
        this.currentPassword = password;
        signalingServer.setPassword(password);
    }

    @Override
    public void setAudioBitrate(int bitrateKbps) {
        NativeBridge.load();
        if (NativeBridge.isLoaded()) {
            NativeBridge.setAudioBitrate(bitrateKbps);
        }
    }

    @Override
    public void pushEvent(ClientEvent event) {
        signalingServer.pushEvent(event);
    }

    @Override
    public void setTextInputSink(Consumer<String> sink) {
        signalingServer.setTextInputSink(sink);
    }

    /**
     * 注入档位协调器：offer 快照/视口/BPP 与 Answer 落账转发给信令层直用。
     *
     * @param worker 档位协调器；信令面回调经它直达，须在信令开张前注入
     */
    public void setProfileCoordinator(ProfileCoordinator worker) {
        signalingServer.setProfileCoordinator(worker);
    }

    @Override
    public String applyProfile(int width, int height, int fps, double bpp, String codec) {
        return NativeBridge.applyProfile(width, height, fps, bpp, codec);
    }
}

package kairo.vistrel.streaming.signaling;

import kairo.vistrel.streaming.ProfileCoordinator;
import kairo.vistrel.streaming.SessionServer;
import io.netty.buffer.ByteBuf;
import io.netty.buffer.Unpooled;
import io.netty.channel.embedded.EmbeddedChannel;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

/**
 * SignalingServer 全栈行为测试：socket 级跨包重组 + EmbeddedChannel 直驱的
 * 鉴权失败 / 4001 抢占 / 403 静态防护路径（无需端口与 Native 库）
 */
class SignalingServerTest {
    private static final byte[] MASK = {0x12, 0x34, 0x56, 0x78};

    /**
     * 手搓带掩码的客户端 text 帧（对撞 oracle，协议知识不与被测代码共享）
     */
    private static byte[] maskedTextFrame(String payload) {
        byte[] payloadBytes = payload.getBytes(StandardCharsets.UTF_8);
        ByteArrayOutputStream frame = new ByteArrayOutputStream();
        frame.write(0x81);
        if (payloadBytes.length <= 125) {
            frame.write(0x80 | payloadBytes.length);
        } else if (payloadBytes.length <= 65535) {
            frame.write(0x80 | 126);
            frame.write((payloadBytes.length >> 8) & 0xFF);
            frame.write(payloadBytes.length & 0xFF);
        } else {
            frame.write(0x80 | 127);
            for (int i = 7; i >= 0; i--) {
                frame.write((payloadBytes.length >> (8 * i)) & 0xFF);
            }
        }
        frame.write(MASK[0]);
        frame.write(MASK[1]);
        frame.write(MASK[2]);
        frame.write(MASK[3]);
        for (int i = 0; i < payloadBytes.length; i++) {
            frame.write(payloadBytes[i] ^ MASK[i % 4]);
        }
        return frame.toByteArray();
    }

    /**
     * 记录型假委托：纯协议三方法——offer 的规划结果与 Answer JSON、连接关闭可断言。
     * 视口 / BPP / Answer 落账的效果经注入协调器观察
     */
    private static final class RecordingDelegate implements SignalingDelegate {
        final AtomicReference<String> offerSdp = new AtomicReference<>(null);
        final AtomicReference<int[]> offerTarget = new AtomicReference<>(null);
        final AtomicReference<Double> offerBpp = new AtomicReference<>(null);
        final AtomicReference<String> offerCodec = new AtomicReference<>(null);
        final AtomicReference<String> offerBackend = new AtomicReference<>(null);
        final List<int[]> offerTargets = new CopyOnWriteArrayList<>();
        final CountDownLatch offerLatch = new CountDownLatch(1);
        final java.util.concurrent.atomic.AtomicInteger closeCount = new java.util.concurrent.atomic.AtomicInteger();
        /**
         * 置位后下一次 onOffer 返回 null（协商失败但认证已通过）
         */
        volatile boolean failNextOffer;

        @Override
        public String onOffer(
                String offerSdpJson,
                int width,
                int height,
                int fps,
                double bpp,
                String codec,
                String backend
        ) {
            offerSdp.set(offerSdpJson);
            offerTarget.set(new int[] {width, height, fps});
            offerTargets.add(new int[] {width, height, fps});
            offerBpp.set(bpp);
            offerCodec.set(codec);
            offerBackend.set(backend);
            offerLatch.countDown();
            if (failNextOffer) {
                failNextOffer = false;
                return null;
            }
            // 合法 JSON：信令层会把它交给协调器落账（预拆分拼接防格式化器拆坏转义）
            return "{\"width\":1920,\"height\":1080,\"fps\":60,\"bpp\":0.1,"
                    + "\"bitrate\":4000000,\"codec\":\"H264\",\"backend\":\"nvenc\"}";
        }

        @Override
        public void onIceCandidate(String candidateJson) {}

        @Override
        public void onSessionClosed() {
            closeCount.incrementAndGet();
        }
    }

    /**
     * 唤醒时读当下状态（生效档位）
     */
    private static final class RecordingWorkerListener implements Runnable {
        final List<SessionServer.Tier> applied = new CopyOnWriteArrayList<>();
        private volatile ProfileCoordinator worker;

        void attach(ProfileCoordinator worker) {
            this.worker = worker;
        }

        @Override
        public void run() {
            SessionServer.SessionState state = worker.current();
            if (state != null && state.phase() == SessionServer.Phase.STREAMING) {
                applied.add(state.tier());
            }
        }
    }

    /**
     * 经 EmbeddedChannel 完成 WebSocket 升级握手，返回已就绪的通道
     */
    private static EmbeddedChannel openWebSocket(SignalingServer server) {
        EmbeddedChannel ch = new EmbeddedChannel(server.new HttpAndWebSocketHandler());
        String handshake =
                "GET /ws HTTP/1.1\r\n"
                + "Host: embedded\r\n"
                + "Upgrade: websocket\r\n"
                + "Connection: Upgrade\r\n"
                + "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                + "Sec-WebSocket-Version: 13\r\n\r\n";
        ch.writeInbound(Unpooled.wrappedBuffer(handshake.getBytes(StandardCharsets.ISO_8859_1)));
        ByteBuf resp = ch.readOutbound();
        Assertions.assertNotNull(resp, "握手必须产生 101 响应");
        Assertions.assertTrue(resp
            .toString(StandardCharsets.ISO_8859_1)
            .startsWith("HTTP/1.1 101 Switching Protocols"));
        resp.release();
        return ch;
    }

    private static void sendOffer(EmbeddedChannel ch, String password) {
        sendOffer(ch, password, null);
    }

    private static void sendOffer(EmbeddedChannel ch, String password, String backend) {
        String offer = "{\"type\":\"offer\",\"password\":\""
                + password
                + "\",\"bpp\":0.15,\"sdp\":{}"
                + (backend != null ? ",\"backend\":\"" + backend + "\"" : "")
                + "}";
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame(offer)));
    }

    @Test
    void testWrongPasswordGetsAuthFailedThenDelayedClose() throws Exception {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        server.setPassword("pass123");

        EmbeddedChannel ch = openWebSocket(server);
        sendOffer(ch, "WRONG");

        ByteBuf frame = ch.readOutbound();
        Assertions.assertNotNull(frame, "错密码应收到 auth_failed 文本帧");
        Assertions.assertTrue(frame.toString(StandardCharsets.UTF_8).contains("auth_failed"), "应回 auth_failed 而非静默断连");
        frame.release();

        Assertions.assertTrue(ch.isActive(), "防爆破回退：auth_failed 后 200ms 内连接应保持");
        // 轮询泵送嵌入式事件循环直到 200ms 延迟关闭生效（上限 2 秒）
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
        while (ch.isActive() && System.nanoTime() < deadline) {
            ch.runPendingTasks();
            Thread.sleep(20);
        }
        Assertions.assertFalse(ch.isActive(), "200ms 回退到期后应断连");
    }

    @Test
    void testSecondAuthenticatedOfferPreemptsFirstSessionWith4001() {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        server.setPassword("pass123");

        EmbeddedChannel first = openWebSocket(server);
        sendOffer(first, "pass123");
        ByteBuf answer = first.readOutbound();
        Assertions.assertNotNull(answer, "首会话合法 offer 应收到 answer");
        answer.release();

        EmbeddedChannel second = openWebSocket(server);
        sendOffer(second, "pass123");
        ByteBuf secondAnswer = second.readOutbound();
        Assertions.assertNotNull(secondAnswer, "第二会话同样应协商成功");
        secondAnswer.release();

        ByteBuf preempted = first.readOutbound();
        Assertions.assertNotNull(preempted, "旧会话应收到抢占关闭帧");
        Assertions.assertEquals(0x88, preempted.readUnsignedByte(), "应为 FIN+Close opcode");
        // 长度字节
        preempted.readUnsignedByte();
        Assertions.assertEquals(4001, preempted.readUnsignedShort(), "抢占关闭状态码必须是 4001");
        preempted.release();
        Assertions.assertFalse(first.isActive(), "被抢占的旧会话应已断连");
    }

    /**
     * 以自定义 Host / Origin 头做一次握手，返回响应首行文本
     */
    private static String handshakeWith(SignalingServer server, String hostHeader, String originHeader) {
        EmbeddedChannel ch = new EmbeddedChannel(server.new HttpAndWebSocketHandler());
        String handshake = "GET /ws HTTP/1.1\r\n"
                + "Host: "
                + hostHeader
                + "\r\n"
                + (originHeader == null ? "" : "Origin: " + originHeader + "\r\n")
                + "Upgrade: websocket\r\n"
                + "Connection: Upgrade\r\n"
                + "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                + "Sec-WebSocket-Version: 13\r\n\r\n";
        ch.writeInbound(Unpooled.wrappedBuffer(handshake.getBytes(StandardCharsets.ISO_8859_1)));
        ByteBuf resp = ch.readOutbound();
        Assertions.assertNotNull(resp, "握手必须产生响应");
        String response = resp.toString(StandardCharsets.ISO_8859_1);
        resp.release();
        ch.finishAndReleaseAll();
        return response;
    }

    @Test
    void testCrossOriginWebSocketRejected() {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        String response = handshakeWith(server, "127.0.0.1:30808", "http://evil.example");
        Assertions.assertTrue(response.startsWith("HTTP/1.1 403"), "跨站页面发起的 WebSocket 必须被拒: " + response);
    }

    @Test
    void testSameOriginWebSocketAccepted() {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        String response = handshakeWith(server, "192.168.1.9:30808", "http://192.168.1.9:30808");
        Assertions.assertTrue(response.startsWith("HTTP/1.1 101"), "同源握手必须放行: " + response);
    }

    /**
     * 以普通 GET 访问指定路径，返回完整响应文本
     */
    private static String httpGet(SignalingServer server, String uri) {
        EmbeddedChannel ch = new EmbeddedChannel(server.new HttpAndWebSocketHandler());
        ch.writeInbound(Unpooled.wrappedBuffer(("GET " + uri + " HTTP/1.1\r\nHost: embedded\r\n\r\n")
            .getBytes(StandardCharsets.ISO_8859_1)
        ));
        ByteBuf resp = ch.readOutbound();
        Assertions.assertNotNull(resp, "GET 必须产生响应");
        String response = resp.toString(StandardCharsets.ISO_8859_1);
        resp.release();
        ch.finishAndReleaseAll();
        return response;
    }

    @Test
    void testAuthEndpointReflectsPasswordRequirement() {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        server.setPassword("pass123");
        String enabled = httpGet(server, "/api/auth");
        Assertions.assertTrue(enabled.startsWith("HTTP/1.1 200"), "认证查询必须 200: " + enabled);
        Assertions.assertTrue(enabled.contains("\"passwordRequired\":true"), "启用口令时应报告需要认证: " + enabled);
        // 空口令 = 免认证模式，页面据此跳过密码框
        server.setPassword("   ");
        String disabled = httpGet(server, "/api/auth");
        Assertions.assertTrue(disabled.contains("\"passwordRequired\":false"), "空口令即免认证: " + disabled);
    }

    /**
     * 开好 WebSocket 并完成认证，返回已就绪的通道（Answer 已消费）
     */
    private static EmbeddedChannel authenticatedWebSocket(SignalingServer server) {
        EmbeddedChannel ch = openWebSocket(server);
        sendOffer(ch, "pass123");
        ByteBuf answer = ch.readOutbound();
        Assertions.assertNotNull(answer);
        answer.release();
        return ch;
    }

    @Test
    void testTextInputForwardsToRegisteredSinkWhenAuthenticated() {
        List<String> received = new CopyOnWriteArrayList<>();
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        server.setPassword("pass123");
        server.setTextInputSink(received::add);

        EmbeddedChannel ch = authenticatedWebSocket(server);
        String payload = "{\"type\":\"text_input\",\"text\":\"你好\"}";
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame(payload)));
        Assertions.assertEquals(List.of("你好"), received, "已认证会话的输入文本必须送达注册的接收器");
        ch.finishAndReleaseAll();
    }

    @Test
    void testTextInputDroppedWithoutAuthentication() {
        List<String> received = new CopyOnWriteArrayList<>();
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        server.setPassword("pass123");
        server.setTextInputSink(received::add);

        EmbeddedChannel ch = openWebSocket(server);
        String payload = "{\"type\":\"text_input\",\"text\":\"越权\"}";
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame(payload)));
        Assertions.assertTrue(received.isEmpty(), "未认证会话的输入文本必须丢弃");
        ch.finishAndReleaseAll();
    }

    /**
     * 换档推送：Answer 携带显示几何初值，只有几何实际变化的唤醒才下发
     */
    @Test
    void testTierChangePushesDisplayGeometry() throws Exception {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        server.setPassword("pass123");
        java.util.concurrent.CountDownLatch askLatch = new java.util.concurrent.CountDownLatch(1);
        ProfileCoordinator worker = newWorker(new ProfileCoordinator.ContentSnapshot(3840, 2160, 60, false, 0), new RecordingWorkerListener(), (
                                                                                                                                                       width,
                                                                                                                                                       height,
                                                                                                                                                       fps,
                                                                                                                                                       bpp
                                                                                                                                               ) -> {
            askLatch.countDown();
            return "{\"width\":1280,\"height\":720,\"fps\":60,\"bpp\":0.1,\"bitrate\":2000000}";
        });
        worker.start();
        server.setProfileCoordinator(worker);

        EmbeddedChannel ch = openWebSocket(server);
        sendOffer(ch, "pass123");
        ByteBuf answer = ch.readOutbound();
        Assertions.assertNotNull(answer, "合法 offer 应收到 answer");
        answer.release();
        // 初值随 Answer 下发：会话建立的唤醒与初值等值，不得重复推送
        ch.runPendingTasks();
        Assertions.assertNull(ch.readOutbound(), "Answer 已携带初值，会话建立不得重复推送");

        String settings = "{\"type\":\"settings\",\"viewport\":{\"width\":1280,\"height\":720}}";
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame(settings)));
        Assertions.assertTrue(askLatch.await(2, TimeUnit.SECONDS), "视口变更必须触达后台 ask");

        String json = awaitDisplayGeometryPush(ch);
        Assertions.assertNotNull(json, "换档落账后必须主动推送显示几何");
        Assertions.assertTrue(
                json.contains("\"width\":1280") && json.contains("\"height\":720"),
                "推送必须携带新显示几何，实际 " + json
        );
        ch.finishAndReleaseAll();
        worker.shutdown();
    }

    /**
     * 轮询泵送嵌入式事件循环直到收到 display_geometry 推送帧（上限 2 秒）
     */
    private static String awaitDisplayGeometryPush(EmbeddedChannel ch) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
        while (System.nanoTime() < deadline) {
            ch.runPendingTasks();
            ByteBuf frame;
            while ((frame = ch.readOutbound()) != null) {
                String json = frame.toString(StandardCharsets.UTF_8);
                frame.release();
                if (json.contains("\"type\":\"display_geometry\"")) {
                    return json;
                }
            }
            Thread.sleep(10);
        }
        return null;
    }

    @Test
    void testTextInputTruncatedToTransportLimit() {
        List<String> received = new CopyOnWriteArrayList<>();
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        server.setPassword("pass123");
        server.setTextInputSink(received::add);

        EmbeddedChannel ch = authenticatedWebSocket(server);
        // 反向剪贴板：常规粘贴长度（300 字符，低于 8192 上限）必须原样送达
        String paste = "字".repeat(300);
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame("{\"type\":\"text_input\",\"text\":\"" + paste + "\"}")));
        String longText = "字".repeat(9000);
        ch.writeInbound(Unpooled.wrappedBuffer(
                maskedTextFrame("{\"type\":\"text_input\",\"text\":\"" + longText + "\"}")
        ));
        Assertions.assertEquals(2, received.size(), "两条输入文本都应送达");
        Assertions.assertEquals(300, received.get(0).length(), "常规粘贴必须原样送达");
        Assertions.assertEquals(8192, received.get(1).length(), "超长文本必须截断到传输上限");
        ch.finishAndReleaseAll();
    }

    @Test
    void testUnresolvableBindAddressFailsClosed() {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        Assertions.assertThrows(
                Exception.class,
                () -> server.start("no-such-host.invalid", 0, "pass123"),
                "无法解析的地址必须拒绝启动"
        );
        Assertions.assertFalse(server.isRunning(), "绑定失败后不得处于运行态");
    }

    @Test
    void testLoopbackBindAddressIsHonored() throws Exception {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        try {
            server.start("127.0.0.1", 0, "pass123");
            Assertions.assertTrue(server.boundPort() > 0, "监听端口必须可用");
            try (Socket socket = new Socket("127.0.0.1", server.boundPort())) {
                Assertions.assertTrue(socket.isConnected(), "回环地址必须可连");
            }
        } finally {
            server.stop();
        }
    }

    @Test
    void testStopAcceptingReleasesPortButKeepsEngineRunning() throws Exception {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        try {
            server.start("127.0.0.1", 0, "pass123");
            int port = server.boundPort();
            try (Socket socket = new Socket("127.0.0.1", port)) {
                Assertions.assertTrue(socket.isConnected(), "关入口前必须可连");
            }
            server.stopAccepting();
            // 幂等
            server.stopAccepting();
            Assertions.assertTrue(server.isRunning(), "关入口不得停引擎");
            Assertions.assertEquals(-1, server.boundPort(), "关入口必须释放监听端口");
            // 端口刚释放时内核仍可能替客户端完成 TCP 握手（随后 RST），
            // 契约是「不再被服务」而不是「connect 必然抛异常」
            Assertions.assertFalse(servedAfterShutdown(port), "关入口后不得再被服务");
        } finally {
            server.stop();
        }
    }

    /**
     * 探测该端口是否仍会被服务：连上就发一个 HTTP 请求，读不到任何字节（含被拒 / 立即断开）即视为已关
     *
     * @param port 目标端口
     * @return 收到任何响应字节时为 {@code true}
     */
    private static boolean servedAfterShutdown(int port) throws IOException {
        try (Socket probe = new Socket()) {
            probe.connect(new InetSocketAddress("127.0.0.1", port), 200);
            probe.setSoTimeout(500);
            probe.getOutputStream().write("GET / HTTP/1.1\r\nHost: x\r\n\r\n".getBytes(StandardCharsets.US_ASCII));
            probe.getOutputStream().flush();
            return probe.getInputStream().read() >= 0;
        } catch (IOException refusedOrReset) {
            return false;
        }
    }

    @Test
    void testConnectionCounterReleasedWhenChannelCloses() {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        // 逐个开关超过上限数量的连接：销账失效则第 65 个会被并发上限直接关掉
        for (int i = 0; i < 65; i++) {
            EmbeddedChannel ch = new EmbeddedChannel(server.new HttpAndWebSocketHandler());
            Assertions.assertTrue(ch.isActive(), "第 " + (i + 1) + " 个连接不应被并发上限拒绝，前序连接均已关闭");
            ch.finishAndReleaseAll();
        }
    }

    @Test
    void testStaticFileDirectoryTraversalBlocked() {
        SignalingServer server = new SignalingServer(new RecordingDelegate());
        EmbeddedChannel ch = new EmbeddedChannel(server.new HttpAndWebSocketHandler());
        ch.writeInbound(Unpooled.wrappedBuffer("GET /../secret.txt HTTP/1.1\r\nHost: embedded\r\n\r\n"
            .getBytes(StandardCharsets.ISO_8859_1)
        ));
        ByteBuf resp = ch.readOutbound();
        Assertions.assertNotNull(resp);
        Assertions.assertTrue(
                resp.toString(StandardCharsets.ISO_8859_1).startsWith("HTTP/1.1 403"),
                "目录穿越请求必须 403 Forbidden"
        );
        resp.release();
    }

    @Test
    void testWebSocketFragmentationAndDefensiveJson() throws Exception {
        RecordingDelegate rec = new RecordingDelegate();
        SignalingServer server = new SignalingServer(rec);
        // offer 处理要求协调器已注入（未就绪直接拒绝），注入空快照走回退档位
        server.setProfileCoordinator(newWorker(null, null, SignalingServerTest::nullApplier));
        // 用 0 让系统分配空闲端口：并行的测试任务（多版本）不会互相抢端口
        server.start("0.0.0.0", 0, "pass123");
        int port = server.boundPort();

        try (Socket socket = new Socket("127.0.0.1", port)) {
            socket.setSoTimeout(3000);
            java.io.OutputStream out = socket.getOutputStream();
            java.io.InputStream in = socket.getInputStream();
            // 1. WebSocket 升级握手
            String handshake = "GET /ws HTTP/1.1\r\n"
                    + "Host: 127.0.0.1:"
                    + port
                    + "\r\n"
                    + "Upgrade: websocket\r\n"
                    + "Connection: Upgrade\r\n"
                    + "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                    + "Sec-WebSocket-Version: 13\r\n\r\n";
            out.write(handshake.getBytes(StandardCharsets.ISO_8859_1));
            out.flush();

            byte[] respBuf = new byte[1024];
            int read = in.read(respBuf);
            String respStr = new String(respBuf, 0, read, StandardCharsets.ISO_8859_1);
            Assertions.assertTrue(
                    respStr.startsWith("HTTP/1.1 101 Switching Protocols"),
                    "必须响应 101 Switching Protocols"
            );
            // 2. 构造 3KB+ 巨型 Offer 模拟真实 SDP 体量
            StringBuilder bigSdp = new StringBuilder("v=0\\r\\no=- 123456 2 IN IP4 127.0.0.1\\r\\n");
            for (int i = 0; i < 60; i++) {
                bigSdp
                    .append("a=candidate:")
                    .append(i)
                    .append(" 1 UDP 2130706431 192.168.1.")
                    .append(i)
                    .append(" 5000 typ host\\r\\n");
            }
            String offerPayload = "{\"type\":\"offer\",\"password\":\"pass123\",\"bpp\":0.2,"
                    + "\"sdp\":{\"type\":\"offer\",\"sdp\":\""
                    + bigSdp
                    + "\"}}";
            // 3. 模拟跨 TCP 包分片到达 (拆成 3 段依次写入)
            byte[] fullFrame = maskedTextFrame(offerPayload);
            out.write(fullFrame, 0, 500);
            out.flush();
            Thread.sleep(30);
            out.write(fullFrame, 500, 1200);
            out.flush();
            Thread.sleep(30);
            out.write(fullFrame, 1700, fullFrame.length - 1700);
            out.flush();

            Assertions.assertTrue(
                    rec.offerLatch.await(3, TimeUnit.SECONDS),
                    "服务端应通过 ByteToMessageDecoder 成功拼接跨 TCP 包的大帧 Offer"
            );
            Assertions.assertNotNull(rec.offerSdp.get());
            Assertions.assertTrue(rec.offerSdp.get().contains("candidate:59"), "重组后的 SDP 应完整无缺");
            // 4. 发送非 JSON 对象消息，连接应防御性存活
            out.write(maskedTextFrame("\"zf\""));
            out.flush();
            Thread.sleep(50);
            Assertions.assertTrue(socket.isConnected() && !socket.isClosed(), "非法非 JSON 消息应防御性忽略，连接保持存活");
        } finally {
            server.stop();
        }
    }

    /**
     * 构造已喂入内容快照的协调器：offer 规划与 settings 分发的观察点（不 start——
     * offer 落账在信令线程同步完成，BPP ask 场景自行 start）
     */
    private static ProfileCoordinator newWorker(
            ProfileCoordinator.ContentSnapshot snapshot,
            RecordingWorkerListener listener,
            ProfileCoordinator.Applier applier
    ) {
        ProfileCoordinator worker = new ProfileCoordinator(applier, listener);
        if (listener != null) {
            listener.attach(worker);
        }
        if (snapshot != null) {
            worker.onContentSnapshot(snapshot, 0L);
        }
        return worker;
    }

    /**
     * 恒返 null 的假 Applier：只观察规划与落账路径，不经真实编码 ask
     */
    private static String nullApplier(int width, int height, int fps, double bpp) {
        return null;
    }

    /**
     * 内容驱动的 offer 规划：客户端不发几何——目标 = 内容快照 × 视口上限 × 配置帧率（VSync 夹低）；
     * Answer 由协调器落账（基线种入 + 初始档位广播）
     */
    @Test
    void testOfferWithoutGeometryPlansFromContentSnapshot() {
        RecordingDelegate rec = new RecordingDelegate();
        SignalingServer server = new SignalingServer(rec);
        server.setPassword("pass123");
        RecordingWorkerListener listener = new RecordingWorkerListener();
        ProfileCoordinator.ContentSnapshot snapshot = new ProfileCoordinator.ContentSnapshot(2560, 1440, 260, true, 165);
        server.setProfileCoordinator(newWorker(snapshot, listener, SignalingServerTest::nullApplier));

        EmbeddedChannel ch = openWebSocket(server);
        String offer =
                "{\"type\":\"offer\",\"password\":\"pass123\",\"bpp\":0.15,"
                + "\"viewport\":{\"width\":2560,\"height\":1440},\"codec\":\"av1\",\"backend\":\"vulkan_video\",\"sdp\":{}}";
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame(offer)));
        ByteBuf answer = ch.readOutbound();
        Assertions.assertNotNull(answer, "合法 offer 应收到 answer");
        answer.release();

        Assertions.assertNotNull(rec.offerTarget.get(), "委托必须收到服务端规划的目标档位");
        int[] target = rec.offerTarget.get();
        // 快照 Unlimited(260)→240 再被 VSync 夹到 165；视口物理像素 2560×1440 与内容一致不缩放
        Assertions.assertArrayEquals(new int[] {2560, 1440, 165}, target, "几何与帧率必须来自内容侧而非客户端");
        Assertions.assertEquals(0.15, rec.offerBpp.get(), 1e-9, "bpp 原样进入协商");
        Assertions.assertEquals("av1", rec.offerCodec.get(), "codec 偏好原样进入协商");
        Assertions.assertEquals("vulkan_video", rec.offerBackend.get(), "后端偏好原样进入协商");
        // Answer 落账：委托返回的 fake answer（1920×1080@60，nvenc）成为生效基线并广播
        Assertions.assertEquals(
                new SessionServer.Tier(1920, 1080, 60, 0.1),
                listener.applied.get(0),
                "Answer 必须经协调器落账广播初始档位（含后端身份透传）"
        );
        ch.finishAndReleaseAll();
    }

    /**
     * settings 状态同步：视口字段入协调器作自动上限输入，不产生任何回执帧；
     * 上限效果以随后的 offer 规划结果观察
     */
    @Test
    void testSettingsViewportForwardsWithoutReply() {
        RecordingDelegate rec = new RecordingDelegate();
        SignalingServer server = new SignalingServer(rec);
        server.setPassword("pass123");
        ProfileCoordinator.ContentSnapshot snapshot = new ProfileCoordinator.ContentSnapshot(3840, 2160, 60, false, 0);
        server.setProfileCoordinator(
                newWorker(snapshot, new RecordingWorkerListener(), SignalingServerTest::nullApplier)
        );

        EmbeddedChannel ch = authenticatedWebSocket(server);
        // 预拆分拼接：长字符串携带转义引号时格式化器再拆分会破坏转义，故手动拆成两段字面量
        String payload = "{\"type\":\"settings\",\"bpp\":0.1,\"viewport\":{\"width\":1440," + "\"height\":810}}";
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame(payload)));
        Assertions.assertNull(ch.readOutbound(), "视口同步不得产生回执帧");
        // 首个 offer 未带视口：4K 全量；settings 同步视口后：重规划被夹到 1440×810
        String secondOffer = "{\"type\":\"offer\",\"password\":\"pass123\",\"bpp\":0.1,\"sdp\":{}}";
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame(secondOffer)));
        ByteBuf answer = ch.readOutbound();
        Assertions.assertNotNull(answer, "重协商应收到 answer");
        answer.release();
        Assertions.assertEquals(2, rec.offerTargets.size(), "两次 offer 都应到达委托方");
        Assertions.assertArrayEquals(new int[] {3840, 2160, 60}, rec.offerTargets.get(0), "视口未上报时按内容全量");
        Assertions.assertArrayEquals(new int[] {1440, 810, 60}, rec.offerTargets.get(1), "视口上限必须参与重规划");
        ch.finishAndReleaseAll();
    }

    /**
     * settings 的 BPP 字段在已协商会话上直达协调器做后台阻塞 ask；信令不直接回帧
     */
    @Test
    void testSettingsBppForwardsWithoutInlineReply() throws Exception {
        RecordingDelegate rec = new RecordingDelegate();
        SignalingServer server = new SignalingServer(rec);
        server.setPassword("pass123");
        java.util.concurrent.CountDownLatch askLatch = new java.util.concurrent.CountDownLatch(1);
        AtomicReference<String> seenRequest = new AtomicReference<>();
        ProfileCoordinator worker = newWorker(null, new RecordingWorkerListener(), (w, h, f, b) -> {
            seenRequest.set(w + "x" + h + "@" + f + " bpp=" + b);
            askLatch.countDown();
            return "{\"width\":1920,\"height\":1080,\"fps\":60,\"bpp\":0.25,\"bitrate\":4000000}";
        });
        worker.start();
        server.setProfileCoordinator(worker);

        EmbeddedChannel ch = authenticatedWebSocket(server);
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame("{\"type\":\"settings\",\"bpp\":0.25}")));
        Assertions.assertNull(ch.readOutbound(), "BPP 变更不产生内联回帧");
        Assertions.assertTrue(askLatch.await(2, TimeUnit.SECONDS), "BPP 变更必须触达协调器的阻塞 ask");
        Assertions.assertEquals("1920x1080@60 bpp=0.25", seenRequest.get(), "BPP 直达协调器");
        ch.finishAndReleaseAll();
        worker.shutdown();
    }

    /**
     * 未协商时 settings 只同步视口，BPP 字段忽略不转发——offer 自带 bpp，
     * 会话建立即生效；既不拒绝也不产生回执
     */
    @Test
    void testSettingsBppIgnoredBeforeNegotiation() {
        RecordingDelegate rec = new RecordingDelegate();
        rec.failNextOffer = true;
        SignalingServer server = new SignalingServer(rec);
        server.setPassword("pass123");
        AtomicReference<String> seenRequest = new AtomicReference<>();
        server.setProfileCoordinator(newWorker(null, new RecordingWorkerListener(), (w, h, f, b) -> {
            seenRequest.set(w + "x" + h + "@" + f + " bpp=" + b);
            return null;
        }));

        EmbeddedChannel ch = openWebSocket(server);
        // 认证随 offer 完成：先发一次注定失败的 offer（认证已过、协商未成）
        ch.writeInbound(Unpooled.wrappedBuffer(
                maskedTextFrame("{\"type\":\"offer\",\"password\":\"pass123\",\"sdp\":{}}")
        ));
        ByteBuf failed = ch.readOutbound();
        Assertions.assertNotNull(failed, "协商失败应回错误帧");
        failed.release();

        String payload = "{\"type\":\"settings\",\"bpp\":0.25,\"viewport\":{\"width\":1280," + "\"height\":720}}";
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame(payload)));
        Assertions.assertNull(ch.readOutbound(), "忽略不产生回执帧");
        // 视口仍已入档：随后的 offer 规划被夹到 1280×720（无快照回退 1080p60 为底）
        String secondOffer = "{\"type\":\"offer\",\"password\":\"pass123\",\"bpp\":0.1,\"sdp\":{}}";
        ch.writeInbound(Unpooled.wrappedBuffer(maskedTextFrame(secondOffer)));
        ByteBuf answer = ch.readOutbound();
        Assertions.assertNotNull(answer, "重协商应收到 answer");
        answer.release();
        Assertions.assertArrayEquals(new int[] {1280, 720, 60}, rec.offerTarget.get(), "未协商时视口仍须同步");
        Assertions.assertNull(seenRequest.get(), "未协商时 BPP 不得触达 ask");
        ch.finishAndReleaseAll();
    }

    /**
     * 会话启停由信令层驱动：WS 关闭必须通知委托方（Java 侧据此停采集/音频/输入）
     */
    @Test
    void testChannelCloseNotifiesDelegateOnce() {
        RecordingDelegate rec = new RecordingDelegate();
        SignalingServer server = new SignalingServer(rec);
        server.setPassword("pass123");

        EmbeddedChannel ch = authenticatedWebSocket(server);
        ch.finishAndReleaseAll();
        Assertions.assertEquals(1, rec.closeCount.get(), "WS 关闭必须恰好通知一次会话终结");
    }
}

package kairo.vistrel.streaming.signaling;

import com.google.gson.Gson;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import kairo.vistrel.Constants;
import kairo.vistrel.streaming.ProfileCoordinator;
import kairo.vistrel.streaming.ClientEvent;
import kairo.vistrel.streaming.SessionServer;
import kairo.vistrel.util.Addresses;
import io.netty.bootstrap.ServerBootstrap;
import io.netty.buffer.ByteBuf;
import io.netty.buffer.Unpooled;
import io.netty.channel.*;
import io.netty.channel.nio.NioEventLoopGroup;
import io.netty.channel.socket.SocketChannel;
import io.netty.channel.socket.nio.NioServerSocketChannel;
import java.io.IOException;
import java.io.InputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.UnknownHostException;
import java.net.SocketAddress;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.Base64;
import java.util.Locale;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;

/**
 * 基于 Minecraft 原生 Netty 构建的轻量嵌入式 HTTP 与 WebSocket 信令服务
 */
public class SignalingServer {
    private static final Gson GSON = new Gson();
    private static final String WS_MAGIC_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    // 单条输入文本的传输上限：与正向剪贴板转发的 8192 对称，容纳反向粘贴
    private static final int MAX_TEXT_INPUT_LENGTH = 8192;
    // 粗粒度资源闸：只限制单实例同时存在的 TCP 连接数
    private static final int MAX_CONNECTIONS = 64;
    // 显示几何推送去重键的未下发值
    private static final long GEOMETRY_UNSENT = -1L;
    // 信令委托
    private final SignalingDelegate delegate;
    // 档位协调器
    private volatile ProfileCoordinator profileCoordinator;
    // 最近一次已下发（Answer 或推送）的显示几何；等值唤醒不重发
    private final AtomicLong sentGeometry = new AtomicLong(GEOMETRY_UNSENT);
    private volatile java.util.function.Consumer<String> textInputSink;
    private final AtomicReference<String> password = new AtomicReference<>(null);
    private final AtomicReference<Channel> activeChannel = new AtomicReference<>(null);
    // 认证节流器
    private final AuthThrottle throttle = new AuthThrottle();
    private final AtomicInteger connectionCount = new AtomicInteger();
    private EventLoopGroup bossGroup;
    private EventLoopGroup workerGroup;
    private Channel serverChannel;
    private volatile boolean running = false;
    private volatile boolean accepting = false;

    public SignalingServer(SignalingDelegate delegate) {
        this.delegate = delegate;
    }

    /**
     * 注入档位协调器（信令开张前调用）：offer 快照/视口/BPP 与 settings 分发的直达通道；
     * 同时登记唤醒，把显示几何的变化推给浏览器（Answer 已带初值，等值不重发）
     *
     * @param worker 档位协调器；offer 规划 / Answer 落账 / 关闭复位均经它直达
     */
    public void setProfileCoordinator(ProfileCoordinator worker) {
        this.profileCoordinator = worker;
        if (worker != null) {
            worker.addListener(this::onSessionStateChanged);
        }
    }

    /**
     * 启动信令与静态页面服务（幂等：已运行时直接返回）
     *
     * @param bindAddress     绑定地址（主机名或 IP 字面量）；解析不出或归属不为本机时
     *                        直接报错，不静默回退
     * @param port            监听端口
     * @param initialPassword 访问口令（空串 = 免认证）
     * @throws java.io.IOException bindAddress 无法解析时抛出；端口绑定等其余启动异常在
     *                            实现内部记日志吞掉，调用方须以 {@link #isRunning()} 判定成败
     */
    public synchronized void start(String bindAddress, int port, String initialPassword) throws Exception {
        if (running) {
            return;
        }
        setPassword(initialPassword);
        // 主机名走本机 DNS 解析：解析不出或解析出的地址不归属本机时由 bind 直接报错，不静默回退
        InetAddress bindTo;
        try {
            bindTo = InetAddress.getByName(bindAddress);
        } catch (UnknownHostException e) {
            throw new IOException("配置项 bindAddress 无法解析: " + bindAddress, e);
        }
        bossGroup = new NioEventLoopGroup(1);
        workerGroup = new NioEventLoopGroup(2);
        ServerBootstrap b = new ServerBootstrap();
        b
            .group(bossGroup, workerGroup)
            .channel(NioServerSocketChannel.class)
            .childOption(ChannelOption.TCP_NODELAY, true)
            .childOption(ChannelOption.SO_KEEPALIVE, true)
            .childHandler(new ChannelInitializer<SocketChannel>() {
                @Override
                protected void initChannel(SocketChannel ch) {
                    ch.pipeline().addLast(new HttpAndWebSocketHandler());
                }
            });

        serverChannel = b.bind(new InetSocketAddress(bindTo, port)).sync().channel();
        running = true;
        accepting = true;
    }

    /**
     * 实际监听端口：传入 0 时由系统分配空闲端口，调用方靠此取回真实端口
     *
     * @return 实际监听端口；服务未启动（无监听通道）时返回 {@code -1}
     */
    public int boundPort() {
        Channel channel = serverChannel;
        return channel != null ? ((InetSocketAddress) channel.localAddress()).getPort() : -1;
    }

    /**
     * 停止接客：关监听端口与当前连接，引擎与 Netty 线程组保持存活供 {@link #stop()} 收尾；幂等
     */
    public synchronized void stopAccepting() {
        if (!accepting) {
            return;
        }
        accepting = false;

        Channel cur = activeChannel.getAndSet(null);
        if (cur != null && cur.isActive()) {
            cur.writeAndFlush(WebSocketFrameCodec.closeFrame(1001, "Server shutting down"));
            cur.close();
        }

        if (serverChannel != null) {
            // 有界等待端口释放：紧随的重启才能确定性 bind 成功
            serverChannel.close().awaitUninterruptibly(1, TimeUnit.SECONDS);
            serverChannel = null;
        }
    }

    /**
     * 完整停机：先按 {@link #stopAccepting()} 关闭监听端口与活跃连接，
     * 再依序关闭 boss / worker 线程组（各自有界等待退出）；未运行时直接返回，幂等
     */
    public synchronized void stop() {
        if (!running) {
            return;
        }
        stopAccepting();
        running = false;

        if (bossGroup != null) {
            // quietPeriod=0：无在途任务即完成，不引入默认 2s 安静期；await 兜底防悬挂
            bossGroup.shutdownGracefully(0, 2, TimeUnit.SECONDS).awaitUninterruptibly(2, TimeUnit.SECONDS);
            bossGroup = null;
        }
        if (workerGroup != null) {
            workerGroup.shutdownGracefully(0, 2, TimeUnit.SECONDS).awaitUninterruptibly(2, TimeUnit.SECONDS);
            workerGroup = null;
        }
    }

    /**
     * 设置访问密码；null 或空串表示禁用密码认证，任何连接免密直连
     *
     * @param newPassword 新访问口令（先 trim 后生效）
     */
    public void setPassword(String newPassword) {
        if (newPassword == null || newPassword.trim().isEmpty()) {
            password.set(null);
        } else {
            password.set(newPassword.trim());
        }
    }

    public boolean isRunning() {
        return running;
    }

    /**
     * 向当前活跃连接推送一条信令 JSON（服务端 ICE 候选 trickle 用）
     *
     * 由 Rust Native 线程调用，Netty 的 writeAndFlush 本身线程安全；无活跃连接时静默丢弃。
     *
     * @param json 信令 JSON 文本，原样下发不加工
     */
    public void sendToActiveClient(String json) {
        Channel cur = activeChannel.get();
        if (cur != null && cur.isActive()) {
            cur.writeAndFlush(WebSocketFrameCodec.textFrame(json));
        }
    }

    /**
     * 推送下行控制事件统一入口：Gson 按事件 record 组件名序列化，线格式与网页端对齐
     *
     * 渲染线程经服务层调用，writeAndFlush 异步入队；无活跃连接时静默丢弃。
     *
     * @param event 下行控制事件 record
     */
    public void pushEvent(ClientEvent event) {
        sendToActiveClient(GSON.toJson(event));
    }

    /**
     * 档位协调器唤醒：把显示几何同步排到通道事件循环上。
     *
     * <p>唤醒可能来自渲染线程、后台 ask 线程或信令线程；收敛到事件循环后与信令帧写入串行，
     * 也不必在唤醒方线程上序列化信令帧。
     */
    private void onSessionStateChanged() {
        Channel channel = activeChannel.get();
        if (channel == null) {
            return;
        }
        channel.eventLoop().execute(this::pushDisplayGeometry);
    }

    /**
     * 下发当前显示几何：状态在推送时现读；等值唤醒（Answer 初值、相位翻转、纯码率重配）不重发，
     * 迟到的唤醒由现值幂等吸收——浏览器只镜像服务端状态，不参与时序判断。
     */
    private void pushDisplayGeometry() {
        ProfileCoordinator worker = profileCoordinator;
        SessionServer.SessionState state = worker == null ? null : worker.current();
        if (state == null) {
            return;
        }
        Channel channel = activeChannel.get();
        if (channel == null || !channel.isActive()) {
            return;
        }
        SessionServer.Tier tier = state.tier();
        long geometry = geometryKey(tier.width(), tier.height());
        if (sentGeometry.getAndSet(geometry) == geometry) {
            return;
        }
        sendToActiveClient(GSON.toJson(ClientEvent.DisplayGeometry.of(tier.width(), tier.height())));
    }

    /**
     * 显示几何去重键：宽高各占 32 位
     */
    private static long geometryKey(int width, int height) {
        return ((long) width << 32) | (height & 0xFFFFFFFFL);
    }

    public void setTextInputSink(java.util.function.Consumer<String> textInputSink) {
        this.textInputSink = textInputSink;
    }

    /**
     * 取对端 IP；嵌入式通道没有 InetSocketAddress，统一归入同一桶
     */
    private static String clientIp(Channel channel) {
        SocketAddress address = channel.remoteAddress();
        if (address instanceof InetSocketAddress inet && inet.getAddress() != null) {
            return Addresses.host(inet.getAddress());
        }
        return "unknown";
    }

    /**
     * 校验 WS 升级的 Origin：浏览器一定携带 Origin，跨站连接必须拒绝；
     * 非浏览器客户端不带 Origin 时放行
     */
    private static boolean isSameOrigin(String origin, String host) {
        if (origin == null || origin.isEmpty()) {
            return true;
        }
        if (host == null || host.isEmpty() || "null".equals(origin)) {
            return false;
        }
        String authority = origin;
        int schemeEnd = authority.indexOf("://");
        if (schemeEnd >= 0) {
            authority = authority.substring(schemeEnd + 3);
        }
        return authority.equalsIgnoreCase(host);
    }

    /**
     * HTTP 报文拼装常识：状态行与 "Name: value" 头行，各自自带 CRLF
     */
    private static String httpStatusLine(int code, String phrase) {
        return "HTTP/1.1 " + code + " " + phrase + "\r\n";
    }

    private static String httpHeader(String name, String value) {
        return name + ": " + value + "\r\n";
    }

    /**
     * RFC 6455 §4.2.2 的 101 Switching Protocols 握手应答：
     * Sec-WebSocket-Accept = Base64(SHA-1(客户端 Key + 固定 GUID))
     */
    private static String webSocketHandshakeResponse(String wsKey) throws NoSuchAlgorithmException {
        String acceptKey = Base64
            .getEncoder()
            .encodeToString(MessageDigest
                .getInstance("SHA-1")
                .digest((wsKey + WS_MAGIC_GUID).getBytes(StandardCharsets.ISO_8859_1))
            );
        return httpStatusLine(101, "Switching Protocols")
                + httpHeader("Upgrade", "websocket")
                + httpHeader("Connection", "Upgrade")
                + httpHeader("Sec-WebSocket-Accept", acceptKey)
                // 空行终结头部区
        + "\r\n";
    }

    /**
     * 密码未启用时放行所有连接（免认证模式）；启用后常量时间比对 trim 后的候选值，
     * 短路比较会泄露前缀匹配进度
     */
    private boolean verifyPassword(String candidate) {
        String expected = password.get();
        if (expected == null || expected.isEmpty()) {
            return true;
        }
        if (candidate == null) {
            return false;
        }
        return MessageDigest.isEqual(
                expected.getBytes(StandardCharsets.UTF_8),
                candidate.trim().getBytes(StandardCharsets.UTF_8)
        );
    }

    private static String guessContentType(String path) {
        int dot = path.lastIndexOf('.');
        if (dot < 0) {
            return "application/octet-stream";
        }
        String ext = path.substring(dot + 1).toLowerCase(Locale.ROOT);
        return switch (ext) {
            case "html", "htm" -> "text/html; charset=utf-8";
            case "js", "mjs" -> "application/javascript; charset=utf-8";
            case "css" -> "text/css; charset=utf-8";
            case "json" -> "application/json; charset=utf-8";
            case "svg" -> "image/svg+xml";
            case "png" -> "image/png";
            case "jpg", "jpeg" -> "image/jpeg";
            case "ico" -> "image/x-icon";
            case "wasm" -> "application/wasm";
            default -> "application/octet-stream";
        };
    }

    /**
     * 文本提示帧：认证失败与协商错误共用同一形状
     */
    private record TextSignal(String type, String message) {
        static TextSignal authFailed(String message) {
            return new TextSignal("auth_failed", message);
        }

        static TextSignal error(String message) {
            return new TextSignal("error", message);
        }
    }

    /**
     * 信令入站消息：按 type 在解析处单点分型，各型只带自己用得到的字段；
     * 非法字段组合与未知类型在 parseInbound 处拒绝。
     * sdp 与 candidate 保留为 JsonElement 原样透传，不做结构解包。
     */
    private sealed interface Inbound permits Offer, Candidate, TextInput, Settings {
    }

    /**
     * Offer 载荷：密码随 offer 认证；sdp 原样透传；bpp 与 viewport 是串流偏好；
     * codec 与 backend 是编码偏好（线上词汇，缺省表示无偏好，能否满足由 Native 决定）
     */
    private record Offer(String password, JsonElement sdp, Double bpp, Viewport viewport, String codec, String backend)
            implements Inbound
    {}

    private record Candidate(JsonElement candidate) implements Inbound {}

    private record TextInput(String text) implements Inbound {}

    private record Settings(Double bpp, Viewport viewport) implements Inbound {}

    /**
     * 视口上限：网页端已按视频显示区 CSS 尺寸 × devicePixelRatio 算好的物理像素，
     * 服务端零换算；字段缺失或非正数视为无效（保持既有上限）
     */
    private record Viewport(Integer width, Integer height) {
        boolean valid() {
            return width != null && height != null && width > 0 && height > 0;
        }
    }

    /**
     * 解析入站信令：非对象、缺 type、未知 type 一律返回 null
     */
    private static Inbound parseInbound(String text) {
        JsonElement parsed = JsonParser.parseString(text);
        if (!parsed.isJsonObject()) {
            return null;
        }
        JsonObject root = parsed.getAsJsonObject();
        JsonElement typeField = root.get("type");
        if (typeField == null || typeField.isJsonNull()) {
            return null;
        }
        return switch (typeField.getAsString()) {
            case "offer" -> GSON.fromJson(root, Offer.class);
            case "candidate" -> GSON.fromJson(root, Candidate.class);
            case "text_input" -> GSON.fromJson(root, TextInput.class);
            case "settings" -> GSON.fromJson(root, Settings.class);
            default -> null;
        };
    }

    /**
     * HTTP 错误应答常量表
     */
    private record HttpError(int code, String phrase) {
        static final HttpError FORBIDDEN = new HttpError(403, "Forbidden");
        static final HttpError NOT_FOUND = new HttpError(404, "Not Found");
        static final HttpError METHOD_NOT_ALLOWED = new HttpError(405, "Method Not Allowed");
        static final HttpError TOO_MANY_REQUESTS = new HttpError(429, "Too Many Requests");
        static final HttpError INTERNAL_ERROR = new HttpError(500, "Internal Server Error");
    }

    /**
     * 单连接生命周期处理 Handler：继承 ByteToMessageDecoder 复用 Netty 自动 Cumulation 缓冲，
     * 处理 TCP 跨包拆包与大帧粘包的解析错位。
     *
     * <p>package-private 以便同包测试经 EmbeddedChannel 直驱（无需端口与真实网络）。
     */
    class HttpAndWebSocketHandler extends io.netty.handler.codec.ByteToMessageDecoder {
        private boolean isWebSocket = false;
        private boolean authenticated = false;
        /**
         * 已占用的未认证槽位句柄，null 表示未占用
         */
        private AuthThrottle.Attempt authAttempt = null;
        private final WebSocketFrameCodec frameCodec = new WebSocketFrameCodec();

        @Override
        public void channelActive(ChannelHandlerContext ctx) throws Exception {
            super.channelActive(ctx);
            ctx
                .channel()
                .closeFuture()
                .addListener(ignored -> connectionCount.decrementAndGet());
            if (connectionCount.incrementAndGet() > MAX_CONNECTIONS) {
                Constants.LOG.warn("并发连接数已达上限 {}，拒绝新连接", MAX_CONNECTIONS);
                ctx.close();
            }
        }

        /**
         * 释放未认证槽位；成功、失败与断连三条路径共用，句柄自身幂等
         */
        private void releaseAuthSlot() {
            AuthThrottle.Attempt attempt = authAttempt;
            authAttempt = null;
            if (attempt != null) {
                attempt.close();
            }
        }

        @Override
        protected void decode(ChannelHandlerContext ctx, ByteBuf in, java.util.List<Object> out) {
            if (!isWebSocket) {
                // 在累计缓冲区中查找 HTTP 头部结束符 \r\n\r\n
                int readerIndex = in.readerIndex();
                int readableBytes = in.readableBytes();
                int headerEnd = -1;
                for (int i = readerIndex; i <= readerIndex + readableBytes - 4; i++) {
                    if (in.getByte(i) == '\r'
                            && in.getByte(i + 1) == '\n'
                            && in.getByte(i + 2) == '\r'
                            && in.getByte(i + 3) == '\n') {
                        headerEnd = i;
                        break;
                    }
                }

                if (headerEnd < 0) {
                    // HTTP Header 尚未收全，保留缓冲区等待后续 TCP 包拼接
                    return;
                }

                int headerLen = headerEnd - readerIndex;
                byte[] headerBytes = new byte[headerLen];
                in.readBytes(headerBytes);
                // 跳过 \r\n\r\n
                in.skipBytes(4);

                String headerText = new String(headerBytes, StandardCharsets.ISO_8859_1);
                handleHttp(ctx, headerText);

                if (!isWebSocket) {
                    // 普通静态资源请求由 sendHttpResponse 响应后直接关闭连接
                    return;
                }
            }
            // 升级为 WebSocket 协议后，流式解码 WebSocket 帧 (支持 TCP 跨包重组)
            dispatchFrames(ctx, in);
        }

        private void handleHttp(ChannelHandlerContext ctx, String headerText) {
            String[] lines = headerText.split("\r\n");
            if (lines.length == 0) {
                ctx.close();
                return;
            }

            String[] reqParts = lines[0].split(" ");
            if (reqParts.length < 2) {
                ctx.close();
                return;
            }
            String method = reqParts[0];
            String uri = reqParts[1];

            boolean isUpgrade = false;
            String wsKey = null;
            String hostHeader = null;
            String originHeader = null;

            for (int i = 1; i < lines.length; i++) {
                String line = lines[i];
                int colon = line.indexOf(':');
                if (colon > 0) {
                    String name = line.substring(0, colon).trim().toLowerCase(Locale.ROOT);
                    String val = line.substring(colon + 1).trim();
                    if ("upgrade".equals(name) && "websocket".equalsIgnoreCase(val)) {
                        isUpgrade = true;
                    } else if ("sec-websocket-key".equals(name)) {
                        wsKey = val;
                    } else if ("host".equals(name)) {
                        hostHeader = val;
                    } else if ("origin".equals(name)) {
                        originHeader = val;
                    }
                }
            }

            if (isUpgrade && wsKey != null && uri.startsWith("/ws")) {
                // 跨站页面能对本地端口发起 WS 连接，Origin 与 Host 不一致即拒绝
                if (!isSameOrigin(originHeader, hostHeader)) {
                    Constants.LOG.warn("拒绝跨站 WebSocket 连接: Origin={} Host={}", originHeader, hostHeader);
                    sendError(ctx, HttpError.FORBIDDEN);
                    return;
                }
                authAttempt = throttle.tryBegin(clientIp(ctx.channel()));
                if (authAttempt == null) {
                    Constants.LOG.warn("认证请求被节流拒绝: {}", clientIp(ctx.channel()));
                    sendError(ctx, HttpError.TOO_MANY_REQUESTS);
                    return;
                }
                // 执行 WebSocket 101 Switching Protocols 握手
                try {
                    ctx.writeAndFlush(Unpooled.wrappedBuffer(webSocketHandshakeResponse(wsKey)
                        .getBytes(StandardCharsets.ISO_8859_1)
                    ));
                    isWebSocket = true;
                } catch (Exception e) {
                    Constants.LOG.error("WebSocket 握手异常", e);
                    ctx.close();
                }
            } else if ("GET".equalsIgnoreCase(method)) {
                String cleanUri = uri.indexOf('?') >= 0 ? uri.substring(0, uri.indexOf('?')) : uri;
                if ("/api/auth".equals(cleanUri)) {
                    // 网页客户端据此决定弹密码框还是直接进会话；只暴露是否启用，不涉及密码本体
                    String expected = password.get();
                    boolean required = expected != null && !expected.isEmpty();
                    JsonObject authInfo = new JsonObject();
                    authInfo.addProperty("passwordRequired", required);
                    sendHttpResponse(
                            ctx,
                            200,
                            "OK",
                            "application/json; charset=utf-8",
                            GSON.toJson(authInfo).getBytes(StandardCharsets.UTF_8)
                    );
                    return;
                }
                // 静态文件分发
                handleStaticFile(ctx, uri);
            } else {
                sendError(ctx, HttpError.METHOD_NOT_ALLOWED);
            }
        }

        private void handleStaticFile(ChannelHandlerContext ctx, String uri) {
            String path = uri;
            int q = path.indexOf('?');
            if (q >= 0) {
                path = path.substring(0, q);
            }
            path = path.replaceAll("^/+", "");
            if (path.isEmpty()) {
                path = "index.html";
            }
            // 目录穿越安全防护
            if (path.contains("..") || path.contains("\\")) {
                sendError(ctx, HttpError.FORBIDDEN);
                return;
            }

            String resourcePath = "/web/" + path;
            try (InputStream in = getClass().getResourceAsStream(resourcePath)) {
                if (in == null) {
                    sendError(ctx, HttpError.NOT_FOUND);
                    return;
                }
                byte[] data = in.readAllBytes();
                String contentType = guessContentType(path);
                sendHttpResponse(ctx, 200, "OK", contentType, data);
            } catch (Exception e) {
                Constants.LOG.error("读取静态资源异常: {}", resourcePath, e);
                sendError(ctx, HttpError.INTERNAL_ERROR);
            }
        }

        /**
         * 纯文本错误应答：body 由码与短语自动拼装，配对由 HttpError 常量表保证
         */
        private void sendError(ChannelHandlerContext ctx, HttpError error) {
            sendHttpResponse(
                    ctx,
                    error.code(),
                    error.phrase(),
                    "text/plain; charset=utf-8",
                    (error.code() + " " + error.phrase()).getBytes(StandardCharsets.UTF_8)
            );
        }

        private void sendHttpResponse(
                ChannelHandlerContext ctx,
                int code,
                String phrase,
                String contentType,
                byte[] body
        ) {
            String headers = httpStatusLine(code, phrase)
                    + httpHeader("Content-Type", contentType)
                    + httpHeader("Content-Length", Integer.toString(body.length))
                    + httpHeader("Connection", "close")
                    // 空行终结头部区
            + "\r\n";
            ByteBuf out = Unpooled.buffer(headers.length() + body.length);
            out.writeBytes(headers.getBytes(StandardCharsets.ISO_8859_1));
            out.writeBytes(body);
            ctx.writeAndFlush(out).addListener(ChannelFutureListener.CLOSE);
        }

        /**
         * 帧派发：纯字节协议知识住在 {@link WebSocketFrameCodec}，这里只按入站单元类型
         * 执行对应行为（文本入信令路由、Close 回帧断连、Ping 回 Pong）。
         * 协议违规异常沿 ByteToMessageDecoder 上抛，由 exceptionCaught 统一断连。
         */
        private void dispatchFrames(ChannelHandlerContext ctx, ByteBuf in) {
            WebSocketFrameCodec.Inbound inbound;
            while ((inbound = frameCodec.decode(in)) != null) {
                switch (inbound.kind()) {
                    case TEXT -> onWsMessage(ctx, new String(inbound.payload(), StandardCharsets.UTF_8));
                    case CLOSE -> {
                        ctx.writeAndFlush(WebSocketFrameCodec.closeFrame(1000, "Normal Closure"));
                        ctx.close();
                        return;
                    }
                    case PING -> ctx.writeAndFlush(WebSocketFrameCodec.pongFrame(inbound.payload()));
                    case PONG -> {
                    }
                }
            }
        }

        private void onWsMessage(ChannelHandlerContext ctx, String text) {
            try {
                Inbound msg = parseInbound(text);
                if (msg == null) {
                    Constants.LOG.warn("收到非法或无 type 的信令消息，忽略: {}", text);
                    return;
                }
                if (msg instanceof Offer offer) {
                    handleOffer(ctx, offer);
                } else if (msg instanceof Candidate candidate) {
                    if (authenticated && candidate.candidate() != null && delegate != null) {
                        delegate.onIceCandidate(candidate.candidate().toString());
                    }
                } else if (msg instanceof TextInput textInput) {
                    handleTextInput(ctx, textInput);
                } else if (msg instanceof Settings settings) {
                    handleSettings(settings);
                }
            } catch (Throwable t) {
                Constants.LOG.error("处理 WebSocket 信令消息异常: {}", text, t);
            }
        }

        /**
         * 处理浏览器输入文本：仅已认证会话，超长按传输上限截断后交注册的接收器
         */
        private void handleTextInput(ChannelHandlerContext ctx, TextInput msg) {
            if (!authenticated) {
                return;
            }
            String text = msg.text();
            if (text == null || text.isEmpty()) {
                return;
            }
            if (text.length() > MAX_TEXT_INPUT_LENGTH) {
                text = text.substring(0, MAX_TEXT_INPUT_LENGTH);
            }
            java.util.function.Consumer<String> sink = textInputSink;
            if (sink != null) {
                sink.accept(text);
            }
        }

        /**
         * 处理已认证协商的 offer 消息：认证、抢占旧会话，并按内容驱动档位委托 SDP 协商
         */
        private void handleOffer(ChannelHandlerContext ctx, Offer msg) {
            String candidatePassword = msg.password() != null ? msg.password() : "";
            if (!authenticated) {
                String ip = clientIp(ctx.channel());
                if (!verifyPassword(candidatePassword)) {
                    throttle.onFailure(ip);
                    releaseAuthSlot();
                    ctx.writeAndFlush(WebSocketFrameCodec.textFrame(GSON.toJson(TextSignal.authFailed("密码错误，请重新输入"))));
                    // 防爆破回退：auth_failed 立即回帧，断连延迟 200ms 拖慢重试节奏
                    ctx.executor().schedule((Runnable) ctx::close, 200, TimeUnit.MILLISECONDS);
                    return;
                }
                throttle.onSuccess(ip);
                releaseAuthSlot();
                authenticated = true;
            }
            // 抢占机制：若已有旧连接，向旧连接发送 4001 Preempted 帧并断开
            Channel prev = activeChannel.getAndSet(ctx.channel());
            if (prev != null && prev != ctx.channel() && prev.isActive()) {
                prev.writeAndFlush(WebSocketFrameCodec.closeFrame(4001, "Preempted by new session"));
                prev.close();
            }
            // 信令面未就绪（引擎探测窗口内协调器尚未注入）直接拒绝
            if (delegate == null || profileCoordinator == null) {
                ctx.writeAndFlush(WebSocketFrameCodec.textFrame(GSON.toJson(TextSignal.error(
                        "Signaling delegate negotiate offer failed"
                ))));
                return;
            }

            Viewport viewportMsg = msg.viewport();
            int viewportWidth = 0;
            int viewportHeight = 0;
            if (viewportMsg != null && viewportMsg.valid()) {
                // offer 携带的视口优先：入档后立即用于本次规划（bpp 不经此入口，走 Answer 落账）
                viewportWidth = viewportMsg.width();
                viewportHeight = viewportMsg.height();
                profileCoordinator.onClientSettings(viewportWidth, viewportHeight, null);
            } else {
                // 未携带则沿用 settings 已同步的上限
                viewportWidth = profileCoordinator.viewportWidth();
                viewportHeight = profileCoordinator.viewportHeight();
            }
            // BPP 与编码偏好均为原始请求值透传：缺省与判定都在协调器 / Native 内完成，信令不加工
            double bpp = msg.bpp() != null ? msg.bpp() : ProfileCoordinator.MIN_BPP;
            String sdpJson = msg.sdp() != null ? msg.sdp().toString() : "{}";
            // 快照与内部状态同源：worker 内部持有渲染线程喂入的最新快照
            SessionServer.Tier target = profileCoordinator.plan(viewportWidth, viewportHeight, bpp);
            String answerJson =
                    delegate.onOffer(
                            sdpJson,
                            target.width(),
                            target.height(),
                            target.fps(),
                            bpp,
                            msg.codec(),
                            msg.backend()
            );
            if (answerJson != null && !answerJson.isEmpty()) {
                // 已被新连接抢占：旧会话迟到的 Answer 不得落账
                if (activeChannel.get() != ctx.channel() || !ctx.channel().isActive()) {
                    return;
                }
                // Answer 落账并广播初始档位；其实际档位即显示几何初值，记账后等值唤醒不再推送
                SessionServer.Tier settled = profileCoordinator.noteOfferAnswer(answerJson, bpp);
                if (settled != null) {
                    sentGeometry.set(geometryKey(settled.width(), settled.height()));
                }
                ctx.writeAndFlush(WebSocketFrameCodec.textFrame(answerJson));
            } else {
                ctx.writeAndFlush(WebSocketFrameCodec.textFrame(GSON.toJson(TextSignal.error(
                        "Signaling delegate negotiate offer failed"
                ))));
            }
        }

        /**
         * 处理客户端串流偏好同步：幂等状态快照，每次携带完整 bpp + viewport 物理像素，
         * 整体交协调器落位（视口非法值与未协商 BPP 的门控都在协调器内）。
         */
        private void handleSettings(Settings msg) {
            if (!authenticated) {
                return;
            }
            ProfileCoordinator worker = profileCoordinator;
            if (worker == null) {
                return;
            }
            Viewport viewportMsg = msg.viewport();
            Integer width = null;
            Integer height = null;
            if (viewportMsg != null && viewportMsg.valid()) {
                width = viewportMsg.width();
                height = viewportMsg.height();
            }
            worker.onClientSettings(width, height, msg.bpp());
        }

        @Override
        public void channelInactive(ChannelHandlerContext ctx) throws Exception {
            super.channelInactive(ctx);
            releaseAuthSlot();
            if (activeChannel.compareAndSet(ctx.channel(), null)) {
                if (delegate != null) {
                    delegate.onSessionClosed();
                }
                // 会话状态只有一个生产者：关闭事件由观察者（本层）直接投给协调器
                ProfileCoordinator worker = profileCoordinator;
                if (worker != null) {
                    worker.onSessionClosed();
                }
            }
        }

        @Override
        public void exceptionCaught(ChannelHandlerContext ctx, Throwable cause) throws Exception {
            Constants.LOG.debug("Channel 异常: {}", cause.getMessage());
            ctx.close();
            super.exceptionCaught(ctx, cause);
        }
    }
}

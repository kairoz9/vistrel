package kairo.vistrel.streaming.natives;

import kairo.vistrel.Constants;
import kairo.vistrel.platform.Services;
import java.io.IOException;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.security.MessageDigest;
import java.util.Arrays;

public class NativeBridge {
    private static volatile boolean loaded = false;

    /**
     * 引擎库加载唯一入口：显式指定 {@code -Dvistrel.nativeLib} 时直接加载该路径，
     * 否则从 jar 内解压平台原生库到加载器配置目录下的 {@code vistrel/natives/<平台>/}
     *
     * <p>
     * 幂等：已加载时直接返回。缺库或加载失败时抛 {@link RuntimeException} 阻断启动
     * （fail-fast：严禁带着缺失的引擎继续运行）。
     */
    @SuppressWarnings("restricted")
    public static synchronized void load() {
        if (loaded) {
            return;
        }
        try {
            String libName = (Constants.IS_WINDOWS ? "" : "lib")
                    + Constants.MOD_ID
                    + (Constants.IS_WINDOWS ? ".dll" : (Constants.IS_LINUX ? ".so" : ".dylib"));
            Path mainLib = Constants.NATIVE_LIB != null ? Path.of(Constants.NATIVE_LIB) : ensureNativeLib(libName);
            System.load(mainLib.toAbsolutePath().toString());
            loaded = true;
        } catch (Throwable t) {
            Constants.LOG.error("Failed to load Vistrel native library: {}", t.getMessage());
            throw new RuntimeException("Vistrel 原生核心库加载失败，阻断启动", t);
        }
    }

    /**
     * 把 jar 内嵌原生库落到实例目录。已有文件与内嵌资源 SHA-256 一致时跳过写入；
     *
     * @param libName 平台原生库文件名
     * @return 可交给 {@link System#load(String)} 的库文件路径
     * @throws Exception 内嵌资源缺失、目录不可写或替换失败
     */
    private static Path ensureNativeLib(String libName) throws Exception {
        Path dir = Services.PLATFORM
            .getConfigDir()
            .resolve(Constants.MOD_ID)
            .resolve("natives")
            .resolve(Constants.IS_WINDOWS ? "windows" : (Constants.IS_LINUX ? "linux" : "macos"));
        Files.createDirectories(dir);
        Path target = dir.resolve(libName);
        if (Files.isRegularFile(target) && hasSameContent(target, libName)) {
            return target;
        }
        try (InputStream in = openEmbedded(libName)) {
            Path tempFile = Files.createTempFile(dir, "." + libName + "-", ".tmp");
            try {
                Files.copy(in, tempFile, StandardCopyOption.REPLACE_EXISTING);
                try {
                    Files.move(tempFile, target, StandardCopyOption.REPLACE_EXISTING);
                } catch (IOException e) {
                    // 并发启动时另一实例可能刚把同内容落位并加载（Windows 会锁住目标文件），命中即复用
                    if (Files.isRegularFile(target) && hasSameContent(target, libName)) {
                        return target;
                    }
                    throw e;
                }
            } finally {
                Files.deleteIfExists(tempFile);
            }
        }
        return target;
    }

    private static boolean hasSameContent(Path file, String libName) throws Exception {
        try (InputStream embedded = openEmbedded(libName);
                InputStream existing = Files.newInputStream(file)
        ) {
            return Arrays.equals(sha256(embedded), sha256(existing));
        }
    }

    private static InputStream openEmbedded(String libName) {
        InputStream in = NativeBridge.class.getResourceAsStream("/natives/" + libName);
        if (in == null) {
            throw new IllegalStateException("嵌入式原生库缺失: /natives/" + libName);
        }
        return in;
    }

    private static byte[] sha256(InputStream in) throws Exception {
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        byte[] buffer = new byte[8192];
        int read;
        while ((read = in.read(buffer)) != -1) {
            digest.update(buffer, 0, read);
        }
        return digest.digest();
    }

    /**
     * 引擎库是否已成功加载；{@link #load()} 失败抛出后本方法恒为 {@code false}
     */
    public static boolean isLoaded() {
        return loaded;
    }

    // 纯分发器：会话启停由 Java 信令层自持（offer 成功 / WS 关闭），native 不回调状态
    // 服务端 ICE 候选上抛通道，由 NativeSessionServer 在启动时接入信令推送
    private static volatile java.util.function.Consumer<String> localIceCandidateSink = null;

    /**
     * 接入服务端 ICE 候选的推送通道（通常指向 SignalingServer::sendToActiveClient）
     */
    public static void setLocalIceCandidateSink(java.util.function.Consumer<String> sink) {
        localIceCandidateSink = sink;
    }

    /**
     * 由 Rust Native Core 跨线程推送本端 ICE 候选
     *
     * 入参是 {@code RTCIceCandidateInit} 的 JSON，这里按信令协议包成 candidate 帧
     */
    public static void onLocalIceCandidate(String candidateJson) {
        java.util.function.Consumer<String> sink = localIceCandidateSink;
        if (sink != null) {
            sink.accept("{\"type\":\"candidate\",\"candidate\":" + candidateJson + "}");
        }
    }

    /**
     * 由 Rust Native Core 日志桥逐条推送的 native 日志，转写进 log4j（latest.log 可见）
     *
     * 入参级别序数与 log 门面判别式一致：1=error 2=warn 3=info 4=debug 5=trace
     */
    public static void nativeLog(int level, String message) {
        switch (level) {
            case 1 -> Constants.LOG.error("{}", message);
            case 2 -> Constants.LOG.warn("{}", message);
            case 3 -> Constants.LOG.info("{}", message);
            case 4 -> Constants.LOG.debug("{}", message);
            default -> Constants.LOG.trace("{}", message);
        }
    }

    /**
     * 初始化 Rust Native 多媒体编解码与 WebRTC 核心引擎
     *
     * @param openh264LibPath 软件编码库路径：缓存命中的最终库路径、
     *                        刚下载的 Cisco .bz2 归档路径（Native 解压安装），或空串
     *                        （禁用/下载失败，软编降级、硬件路径照常）
     * @param udpBindAddress  媒体面 UDP 绑定地址（`ip:port`）：复用信令的监听地址与端口号；
     *                        缺失或空白抛 {@link IllegalArgumentException}
     */
    public static native void startEngine(String openh264LibPath, String udpBindAddress);

    /**
     * 平滑停止 Rust Native 引擎
     */
    public static native void stopEngine();

    /**
     * 处理 WebRTC Offer SDP 并按目标档位协商
     *
     * @param offerSdpJson WebRTC Offer SDP JSON
     * @param width        目标宽度（内容驱动，非客户端选择）
     * @param height       目标高度
     * @param fps          目标帧率
     * @param bpp          客户端画质档位
     * @param codec        客户端 Codec 偏好（线上令牌 {@code h264}/{@code h265}/{@code av1}；
     *                     {@code null} 表示无偏好；无法识别或浏览器未提供时协商被拒绝）
     * @param backend      客户端编码后端偏好（线上词汇 {@code nvenc}/{@code vulkan_video}/{@code software}；
     *                     {@code null} 表示无偏好；无法识别或不在本机探测清单内时协商被拒绝）
     * @return Answer JSON 字符串 (包含实际生效的分辨率、帧率、bpp、码率、codec、后端、
     *         可选值清单 {@code backends} 与 Answer SDP)
     */
    public static native String negotiateOffer(
            String offerSdpJson,
            int width,
            int height,
            int fps,
            float bpp,
            String codec,
            String backend
    );

    /**
     * 阻塞应用新的画质档位
     *
     * <p>这是档位的唯一运行期写路径, 成功应答返回**实际生效**的档位 JSON
     * ({@code width/height/fps/bpp/bitrate})；请求被拒绝或无活跃会话返回空串，
     * 调用方保持自己上次的生效基线。
     *
     * <p>0 几何 / 0 帧率 Native 按拒绝处理。
     *
     * @param width  目标宽度（内容驱动，非客户端选择）
     * @param height 目标高度
     * @param fps    目标帧率
     * @param bpp    客户端画质档位 (BPP)
     * @return 实际生效档位 JSON；被拒绝或无活跃会话返回空串
     */
    public static native String applyProfile(int width, int height, int fps, float bpp);

    /**
     * 注入客户端 ICE Candidate
     */
    public static native void addIceCandidate(String candidateJson);

    /**
     * 主动关闭当前处于活跃状态的 WebRTC 会话
     */
    public static native void closeSession();

    /**
     * 从活跃会话的输入环批量提取事件（渲染线程单消费者，每帧头部经 {@code pollAndDispatch} 调用一次）
     *
     * <p>
     * 线格式为若干条 opcode 打头的变长事件连续排列，多字节字段一律大端：
     * {@code 0x01 MouseMove [dx:i16, dy:i16]}、{@code 0x02 MouseButton [btn:u8, action:u8]}、
     * {@code 0x03 Key [code:i32, action:u8]}、{@code 0x04 MouseScroll [sy:i8]}、
     * {@code 0x05 AbsoluteMouseMove [x:u16, y:u16]}。与
     * {@link kairo.vistrel.input.InputEventDecoder} 是同一约定的两端。
     *
     * <p>
     * Native 在本次调用内写完即返回：缓冲区只需在调用期间有效，返回后内容即本批事件
     * （调用方须先 {@code clear()}，事件从缓冲区起点写入）。缓冲容量不足或达到
     * {@code maxEvents} 时按事件边界截断，半条事件不入缓冲。引擎未运行、无活跃会话
     * 或缓冲非 Direct 时返回 0，不视为错误。
     *
     * @param buffer    接收事件字节的堆外 DirectBuffer；Native 只按起始地址写入，
     *                  不读取也不推进 Java 侧 position/limit
     * @param maxEvents 本批最多提取的事件数
     * @return 实际提取的事件数（0 = 无事件或引擎未就绪）
     */
    public static native int pollInputEvents(java.nio.ByteBuffer buffer, int maxEvents);

    /**
     * 查询本机编码能力快照
     *
     * <p>
     * 返回全部可用后端及其 codec 与显卡名，由
     * {@link kairo.vistrel.streaming.EncoderCaps#parse} 解析后缓存。
     *
     * <p>
     * 会话实际在跑什么不在这里，那是协商建成的结果，落账在
     * {@link kairo.vistrel.streaming.SessionServer.EncoderGeneration#backend()}。
     *
     * @return JSON：{@code {"state":"capabilities"|"noHardware"|"notProbed",
     *         "backends":[{"kind":"nvenc","codecs":["H264","H265"],"gpu":"显卡名"}]}}。
     *         {@code state} 区分未探测与探测过无硬件，二者诊断意义相反，不得合并
     */
    public static native String queryEncoderCaps();

    /**
     * 批量注册全部显存槽位与同步原语 (JNI 原语层)。
     *
     * <p>协议与结果码（7 元组字段序、{@link kairo.vistrel.streaming.ControlStatus}）
     * 只在 {@link NativeWireContract} 定义一次，与 Native 侧
     * {@code encode::hardware::slots::FLAT_STRIDE} 是同一条约定的两端。
     *
     * @param bindingEpoch 显存绑定世代（共享块中的当前值）
     * @param slots 扁平 long[]：逐槽位依序排列的 7 元组，字段序见
     *              {@link NativeWireContract}
     * @return 控制结果码：{@link kairo.vistrel.streaming.ControlStatus#OK} 为成功，非零为失败原因
     *         (协议错位 / 无硬编 / 导入失败 / 管线已死 / ACK 超时 / 绑定世代过期)
     */
    public static native int importVramSlots(long bindingEpoch, long[] slots);

    /**
     * 安全注销并释放 Native 引擎内部缓存的所有外部显存槽位与同步原语
     *
     * @param bindingEpoch 显存绑定世代；旧世代的注销按空操作成功（导入已随实例销毁）
     * @return 控制结果码：仅 {@link kairo.vistrel.streaming.ControlStatus#OK} 表示
     *         原生侧已确认注销完毕；未确认时调用方不得销毁导出资源——后果与处置在
     *         {@link kairo.vistrel.streaming.VramBinding#close()} 与
     *         {@link kairo.vistrel.streaming.VramBinding.ReleaseStatus} 定义一次
     */
    public static native int clearVramSlots(long bindingEpoch);

    /**
     * 向 Native 引擎直通投递显存槽位（单向投递，不等待编码结果）
     *
     * <p>
     * {@code slotIdx} 为显存槽位索引 (0 合法)。绑定世代不匹配的帧在入口直接丢弃，
     * 检查是共享块上的原子读，不做 JNI 往返。
     *
     * @param bindingEpoch 显存绑定世代
     * @param slotIdx      已注册的显存槽位索引
     * @param syncValue    时间线信号量信号值 (Vulkan) 或 0L (OpenGL / 其它无时间线信号量后端)
     */
    public static native void pushVramSlot(long bindingEpoch, int slotIdx, long syncValue);

    /**
     * 取当前活跃引擎的槽位所有权共享块；布局与换代语义在
     * {@link NativeWireContract} 定义一次（跨语言契约的 Java 端唯一定义处），
     * 由 {@link kairo.vistrel.streaming.VramBinding} 包装消费
     *
     * @return 64 字节 DirectBuffer；引擎未运行时为 null
     */
    public static native ByteBuffer slotStateBlock();

    /**
     * 向 Native 引擎投递系统内存帧 (PBO 读回 / 软编回退通道)
     *
     * <p>
     * 缓冲区须为 RGBA（byte0=R）DirectBuffer；
     * Native 在本次调用内就拷贝完，返回后缓冲区可立即复用。
     *
     * @param buffer RGBA（byte0=R）像素的堆外 DirectBuffer
     * @param width  可见区宽度（像素）
     * @param height 可见区高度（像素）
     */
    public static native void pushSystemFrame(java.nio.ByteBuffer buffer, int width, int height);

    /**
     * 设置音频推流比特率 (kbps)
     *
     * @param bitrateKbps 目标比特率 (如 96, 128, 256)
     */
    public static native void setAudioBitrate(int bitrateKbps);

    /**
     * 向 Native 引擎投递一帧音频 PCM 数据 (48kHz Stereo 16-bit PCM)
     *
     * @param buffer            包含 16-bit PCM 的堆外 DirectBuffer (单帧通常为 3840 字节)
     * @param samplesPerChannel 单声道采样点数 (通常为 960，对应 20ms)
     */
    public static native void pushAudioPcm(java.nio.ByteBuffer buffer, int samplesPerChannel);
}

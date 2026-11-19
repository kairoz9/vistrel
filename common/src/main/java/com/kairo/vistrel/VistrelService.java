package com.kairo.vistrel;

import com.kairo.vistrel.capture.AudioCaptureScheduler;
import com.kairo.vistrel.capture.VideoCaptureScheduler;
import com.kairo.vistrel.config.VistrelConfig;
import com.kairo.vistrel.streaming.ProfileCoordinator;
import com.kairo.vistrel.streaming.ProfileCoordinator.ContentSnapshot;
//? if >=26.3 {
import com.kairo.vistrel.input.Sdl3InputAdapter;
//? } else {
/*import com.kairo.vistrel.input.GlfwInputAdapter;*/
//? }
import com.kairo.vistrel.input.InputEventSource;
import com.kairo.vistrel.input.InputInjector;
import com.kairo.vistrel.streaming.natives.NativeEncoderChannel;
import com.kairo.vistrel.streaming.natives.NativeSessionServer;
import com.kairo.vistrel.platform.Services;
import com.kairo.vistrel.streaming.EncoderChannel;
import com.kairo.vistrel.streaming.ClientEvent;
import com.kairo.vistrel.streaming.SessionServer;
import com.kairo.vistrel.util.Addresses;
import java.net.URI;
import java.util.Locale;
import java.util.Objects;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.LongSupplier;
import java.util.function.Supplier;
import net.minecraft.client.Minecraft;

/**
 * Vistrel 统一应用服务总控
 *
 * <p>
 * 对外是 Mixin 与系统接入层使用的静态门面，内部编排推流服务 (SessionServer)、
 * 视频捕获 (VideoCaptureScheduler)、音频采集 (AudioCaptureScheduler) 与输入注入 (InputInjector)。
 */
public final class VistrelService {
    private static volatile boolean running = false;
    /**
     * 启动时注入的配置视图、server 与数据面两面：编码停滞自愈按同一套视图与实例重启。
     */
    private static volatile ServiceContext activeContext = null;
    private static volatile VideoCaptureScheduler activeVideoScheduler = null;
    /**
     * 本 tick 是否已取得采集时机（渲染线程独占）
     *
     * 帧限频器前采集与 runTick 尾部兜底同属一个 tick：限频器被调用时前者先到，
     * 长睡眠后尾部再跑一次会把同一帧采集两遍，故以此标志二选一
     */
    private static boolean captureTakenThisTick = false;
    private static volatile AudioCaptureScheduler activeAudioScheduler = null;
    private static volatile InputInjector activeInputInjector = null;
    static volatile ProfileCoordinator activeProfileCoordinator = null;
    // 会话活跃标志
    private static volatile boolean sessionActive = false;
    // 环回设备绑定正本在 AudioCaptureScheduler.LoopbackBinding：随 MC 音频库存亡，服务启停不触碰
    /**
     * 浏览器访问地址缓存：启动期在启动线程解析一次（解析含 DNS 查询与网卡枚举）
     */
    private static volatile String accessUrl = null;
    // 剪贴板长度上限：超长复制静默丢弃
    private static final int MAX_CLIPBOARD_FORWARD_LENGTH = 8192;
    /**
     * 两次自动重启的最小间隔：编码线程若反复停滞，避免重启风暴
     */
    private static final long STALL_RESTART_MIN_INTERVAL_NS = 30_000_000_000L;
    /**
     * 拆除落地等待上限：含会话关闭（写者排空 + 端口放手，原生侧 1s 预算）、
     * 信令线程池收尾与最长 5s 的编码注销确认
     */
    private static final long TEARDOWN_AWAIT_SECONDS = 10;
    /**
     * 控制面：手动启停与自愈重启共用的串行队列 + 自愈冷却闸；冷却只约束自愈，手动不受限
     */
    private static final RestartGate CONTROL = new RestartGate(STALL_RESTART_MIN_INTERVAL_NS, System::nanoTime);

    private VistrelService() {
    }

    /**
     * 显存直通只由硬编后端提供：NVENC 与 Vulkan Video 都能直接消费显存槽位，软编只能编码主机帧
     *
     * @param caps 编码能力快照，线格式见 {@link EncoderChannel#caps()}
     */
    private static boolean supportsVramDirect(String caps) {
        return caps.startsWith("nvenc|") || caps.startsWith("vulkan_video|");
    }

    /**
     * 标准启动入口：以全局配置视图 ({@code VistrelConfig::current}) 与默认 Native 实例启动
     */
    public static synchronized void start() {
        NativeEncoderChannel dataPlane = new NativeEncoderChannel();
        start(VistrelConfig::current, new NativeSessionServer(), dataPlane, dataPlane);
    }

    /**
     * 依赖注入启动入口：允许测试与高级场景注入自定义配置视图、控制面与数据面通道。
     *
     * @param config  配置视图
     * @param server  控制面服务实现
     * @param encoder     数据面编码通道实现
     * @param inputEvents 输入事件源实现
     */
    public static synchronized void start(
            Supplier<VistrelConfig> config,
            SessionServer server,
            EncoderChannel encoder,
            InputEventSource inputEvents
    ) {
        if (running) {
            Constants.LOG.warn("VistrelService 已处于运行状态，忽略重复启动请求");
            return;
        }
        Objects.requireNonNull(config, "配置视图不能为空");
        Objects.requireNonNull(server, "SessionServer 不能为空");
        Objects.requireNonNull(encoder, "EncoderChannel 不能为空");
        Objects.requireNonNull(inputEvents, "InputEventSource 不能为空");

        activeContext = new ServiceContext(config, server, encoder, inputEvents);
        sessionActive = false;

        try {
            VistrelConfig cfg = Objects.requireNonNull(config.get(), "配置视图求值结果不能为空");
            // 1. 组装采集调度器：显存直通能力延迟到会话建立时求值
            VideoCaptureScheduler videoScheduler =
                    new VideoCaptureScheduler(encoder, () -> cfg.vramDirect() && supportsVramDirect(encoder.caps()));
            // 编码线程停滞时由显存管线心跳超时触发，整套推流服务自动重启自愈
            videoScheduler.setEncoderStallListener(() -> autoRestartEngine(videoScheduler));
            activeVideoScheduler = videoScheduler;
            // 2. 唤醒音频环回会话
            AudioCaptureScheduler audioScheduler = new AudioCaptureScheduler(encoder);
            activeAudioScheduler = audioScheduler;
            // 3. 构造输入注入器（平台适配器的版本差异在此单点选择）；光标内缝在此转统一事件
            //? if >=26.3 {
            InputInjector injector = new InputInjector(
                    inputEvents,
                    new Sdl3InputAdapter(),
                    VistrelService::isStreaming,
                    VistrelService::pushCursorEvent
            );
            //? } else {
            /*InputInjector injector = new InputInjector(
                    inputEvents, new GlfwInputAdapter(), VistrelService::isStreaming, VistrelService::pushCursorEvent);*/
            //? }
            activeInputInjector = injector;
            // 4. 内容档位协调器：内容/视口/BPP 三路输入的阻塞 ask 汇聚点（ask 只在工作线程）。
            // 会话状态只有一个生产者（协调器）：采集、音频与接入标记都直接订阅它的广播，
            // 信令层只负责把关闭事件投给它
            ProfileCoordinator profileCoordinator = new ProfileCoordinator(server::applyProfile);
            profileCoordinator.addListener(videoScheduler::onSessionChanged);
            profileCoordinator.addListener(audioScheduler::onSessionChanged);
            profileCoordinator.addListener(VistrelService::onSessionChanged);
            // 协调器注入信令面：offer 规划/落账与 WS 关闭复位都由信令面回调触发
            server.setProfileCoordinator(profileCoordinator);
            profileCoordinator.start();
            activeProfileCoordinator = profileCoordinator;
            // 5. 配置并启动 WebRTC 网络信令服务
            server.setPassword(cfg.password());
            server.setAudioBitrate(cfg.audioBitrate());
            server.setTextInputSink(injector::enqueueText);
            server.start(cfg.bindAddress(), cfg.port());
            if (!server.isRunning()) {
                // 实现内部吞掉了启动异常（端口被占 / 引擎拒启等）：在此显式失败，
                // 避免顶着未启动的网络服务宣称运行中
                throw new IllegalStateException("网络服务未能启动");
            }
            // 解析只在启动线程做一次，status 展示只读缓存
            accessUrl = Addresses.accessUrl(cfg.bindAddress(), cfg.port());
            running = true;
            if (Services.PLATFORM.isDevelopmentEnvironment()) {
                Constants.LOG.info("================================================================================");
                Constants.LOG.info("  Vistrel 服务已启动");
                Constants.LOG.info("  访问地址: {}", accessUrl);
                Constants.LOG.info("  访问密码: {}", cfg.password().isEmpty() ? "未启用（免认证）" : "[" + cfg.password() + "]");
                Constants.LOG.info("  硬件加速状态: {}", encoder.caps());
                Constants.LOG.info("================================================================================");
            }
        } catch (Throwable t) {
            Constants.LOG.error("VistrelService 启动失败: {}", t.getMessage(), t);
            stop();
        }
    }

    /**
     * 统一停机入口：按视频采集 → 音频采集 → 网络服务的顺序依次停止
     *
     * <p>
     * 拆除动作会释放 {@code capture()} 正在使用的 GPU 资源，两者必须同处渲染线程才不会交错，
     * 因此在非渲染线程调用时投递到渲染线程执行。本方法等待拆除落地才返回：
     * 端口、native 引擎与 GPU 资源在返回时已彻底释放，紧随的启动不会与本次拆除相撞。
     * 渲染线程调用或无实例时就地拆除，等待即刻完成。
     */
    public static void stop() {
        CountDownLatch tornDown = stopInternal();
        if (tornDown != null && !awaitTeardown(tornDown)) {
            Constants.LOG.warn("停机拆除 {}s 内未完成，端口与引擎可能尚未释放", TEARDOWN_AWAIT_SECONDS);
        }
    }

    /**
     * 投递控制面操作（/vistrel 命令层经此派发，与启停、自愈同一条串行队列）
     *
     * @param action 控制面动作，在控制面单线程串行执行
     * @throws NullPointerException action 为 {@code null} 时抛出
     */
    public static void submitControl(Runnable action) {
        Objects.requireNonNull(action, "控制面动作不能为空");
        CONTROL.submit(action);
    }

    /**
     * 停机并返回拆除完成信号
     *
     * @return 拆除落地后归零的闩锁；服务本就未运行时返回 null，无需等待
     */
    private static synchronized CountDownLatch stopInternal() {
        if (!running && activeContext == null) {
            return null;
        }
        running = false;
        sessionActive = false;
        VideoCaptureScheduler videoScheduler = activeVideoScheduler;
        AudioCaptureScheduler audioScheduler = activeAudioScheduler;
        ProfileCoordinator profileCoordinator = activeProfileCoordinator;
        InputInjector inputInjector = activeInputInjector;
        // 服务实例同样在停机当刻摘取：拆除可能推迟到渲染线程，届时 activeContext 已空
        SessionServer server = getServer();
        // 先关入口：避免拆除窗口内建出半死活会话
        if (server != null) {
            try {
                server.stopAccepting();
            } catch (Throwable t) {
                Constants.LOG.warn("停止入口异常: {}", t.getMessage());
            }
            // 文本出口随入口一并摘除
            server.setTextInputSink(null);
        }
        activeVideoScheduler = null;
        activeAudioScheduler = null;
        activeInputInjector = null;
        activeProfileCoordinator = null;
        activeContext = null;
        accessUrl = null;
        if (profileCoordinator != null) {
            profileCoordinator.shutdown();
        }

        CountDownLatch teardownDone = new CountDownLatch(1);
        Runnable teardownTask =
                () -> {
            try {
                teardown(videoScheduler, audioScheduler, inputInjector, server, true);
            } finally {
                teardownDone.countDown();
            }
        };
        Minecraft mc = Minecraft.getInstance();
        if (mc == null || mc.isSameThread()) {
            // 无 Minecraft 实例或已在渲染线程则就地执行
            teardownTask.run();
            return teardownDone;
        }
        try {
            mc.execute(teardownTask);
        } catch (Throwable t) {
            // 投递失败说明渲染循环已不可靠：就地只跑与渲染线程无关的部分。
            // GPU 拆除跳过——撞上仍活着的渲染循环就是并发拆 GL/Vulkan 资源，
            // 宁可留资源随引擎关停回收，也不赌一帧中段的并发拆除
            Constants.LOG.warn("停机任务投递渲染线程失败，GPU 拆除跳过: {}", t.getMessage());
            try {
                teardown(videoScheduler, audioScheduler, inputInjector, server, false);
            } finally {
                teardownDone.countDown();
            }
        }
        return teardownDone;
    }

    /**
     * 等待拆除落地：超时或中断都返回 false（拆除可能仍在进行）
     */
    private static boolean awaitTeardown(CountDownLatch latch) {
        try {
            return latch.await(TEARDOWN_AWAIT_SECONDS, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return false;
        }
    }

    /**
     * 执行实际拆除：入参是 stop() 当场捕获的实例，与并发的 start() 互不干扰。
     * {@code includeGpu} 为 false 时跳过视频调度器（GL/Vulkan 资源只能渲染线程拆，
     * 投递渲染线程失败的兜底路径就地带音频、输入与网络收尾）
     */
    private static void teardown(
            VideoCaptureScheduler videoScheduler,
            AudioCaptureScheduler audioScheduler,
            InputInjector inputInjector,
            SessionServer server,
            boolean includeGpu
    ) {
        try {
            // 0. 先释放远端按住的按键：injector 已摘除，帧头的 STREAMING_EXIT 不会再触发
            if (inputInjector != null) {
                inputInjector.releaseAll();
            }
        } catch (Throwable t) {
            Constants.LOG.warn("释放输入状态异常: {}", t.getMessage());
        }
        try {
            // 1. 关闭视频捕获调度器（GL/Vulkan 资源，必须渲染线程）
            if (includeGpu && videoScheduler != null) {
                videoScheduler.cleanup();
            }
        } catch (Throwable t) {
            Constants.LOG.warn("停止 VideoCaptureScheduler 异常: {}", t.getMessage());
        }

        try {
            // 2. 关闭音频环回会话 (内部会 join 等待采集线程退出)
            if (audioScheduler != null) {
                audioScheduler.cleanup();
            }
        } catch (Throwable t) {
            Constants.LOG.warn("停止 AudioCaptureScheduler 异常: {}", t.getMessage());
        }

        try {
            // 3. 最终停止 WebRTC 网络服务与活跃对端连接
            if (server != null) {
                server.stop();
            }
        } catch (Throwable t) {
            Constants.LOG.warn("停止 SessionServer 异常: {}", t.getMessage());
        }
        Constants.LOG.info("Vistrel 服务已停止");
    }

    private static void onSessionChanged(SessionServer.SessionState state) {
        switch (state.phase()) {
            case STREAMING -> {
                boolean wasActive = sessionActive;
                sessionActive = true;
                // 内容路径同样可能迎来首个真实接入（offer 成功走信令层，这里兜恢复/重协商）
                if (!wasActive) {
                    com.kairo.vistrel.command.VistrelCommands.feedback("§e远程会话已接入");
                }
            }
            case INACTIVE -> sessionActive = false;
            // 内容暂停：会话仍在线
            case PAUSED -> {
            }
        }
    }

    /**
     * 渲染线程调用：编码停滞触发入口，移交控制面队列；闭包携带来源实例供过期识别
     */
    private static void autoRestartEngine(VideoCaptureScheduler origin) {
        CONTROL.submit(() -> restartEngine(origin));
    }

    /**
     * 仅测试使用：清空自愈冷却账目
     */
    static void resetRestartCooldownForTest() {
        CONTROL.resetRestartCooldownForTest();
    }

    /**
     * 编码停滞自愈：卡死线程弃置重建整套服务（换新编码线程、CUDA 上下文与槽位），带冷却防风暴；
     * origin 已换代即信号过期跳过。按启动时选定的配置视图与同一套注入实例重建，
     * 生产视图跟随全局 current，运行期改密在重启时自然生效
     */
    static void restartEngine(VideoCaptureScheduler origin) {
        if (activeVideoScheduler != origin) {
            Constants.LOG.info("编码停滞信号已过期（实例已换代），跳过自愈重启");
            return;
        }
        if (!CONTROL.tryRestart()) {
            Constants.LOG.warn("编码停滞自愈冷却中，跳过本次自动重启");
            return;
        }
        Constants.LOG.error("编码线程停滞，自动重启推流引擎以重建编码管线");
        // 停机会清空注入件，这里先持有引用
        ServiceContext ctx = activeContext;
        CountDownLatch tornDown = stopInternal();
        if (tornDown == null) {
            // 服务已不在运行：无需自愈
            return;
        }
        // 拆除在渲染线程落地（含 1s 会话关闭与最长 5s 的编码注销确认）；等它结束后再重启，
        // 否则新旧服务会在端口与 GPU 资源上相撞
        if (!awaitTeardown(tornDown)) {
            Constants.LOG.error("引擎拆除未在期限内完成，放弃自动重启");
            return;
        }
        if (ctx != null) {
            start(ctx.config(), ctx.server(), ctx.encoder(), ctx.inputEvents());
        } else {
            start();
        }
    }

    /**
     * 主渲染循环头部回调（由 MixinMinecraft 触发）
     */
    public static void onRenderFrameHead() {
        captureTakenThisTick = false;
        InputInjector injector = activeInputInjector;
        if (injector != null) {
            injector.pollAndDispatch();
        }
    }

    /**
     * 客户端 tick 回调（由各加载器的客户端 tick 事件触发，20Hz）
     */
    public static void onClientTick() {
        ProfileCoordinator profileCoordinator = activeProfileCoordinator;
        if (profileCoordinator == null) {
            return;
        }
        Minecraft mc = Minecraft.getInstance();
        if (mc == null || mc.getWindow() == null) {
            return;
        }
        int width = mc.getWindow().getWidth();
        int height = mc.getWindow().getHeight();
        int configFps = mc.options.framerateLimit().get();
        boolean vsync = mc.options.enableVsync().get();
        //? if >=26.3 {
        com.mojang.blaze3d.platform.VideoMode videoMode = mc.getWindow().getActiveVideoMode();
        int refreshHz = videoMode != null ? Math.round(videoMode.getRefreshRate()) : 0;
        //? } else {
        /*int refreshHz = mc.getWindow().getRefreshRate();*/
        //? }
        ContentSnapshot snapshot = new ContentSnapshot(width, height, configFps, vsync, refreshHz);
        if (isStreaming()) {
            profileCoordinator.onContentSnapshot(snapshot, System.nanoTime());
        } else {
            // 无会话只种子快照：纯 Java 读取，不碰采集/输入/JNI，让 offer 按真实内容规划
            profileCoordinator.seedContentSnapshot(snapshot);
        }
    }

    /**
     * 帧限频器入睡前回调（由 FramerateLimiter / RenderSystem Mixin 触发）
     *
     * 限频器在 present 之后调用：此处提交采集，编码与限频睡眠并行，省掉睡眠造成的整段延迟；
     * framerateLimit >= 260 时限频器不被调用，由 {@link #onRenderFrameTail()} 兜底
     */
    public static void onBeforeFrameLimitSleep() {
        captureTakenThisTick = true;
        VideoCaptureScheduler scheduler = activeVideoScheduler;
        if (scheduler != null) {
            scheduler.capture();
        }
    }

    /**
     * 主渲染循环尾部回调（由 MixinMinecraft 触发）
     *
     * 仅作限频器未被调用时的兜底；本 tick 已在限频器前采集过则不重复采集
     */
    public static void onRenderFrameTail() {
        if (captureTakenThisTick) {
            return;
        }
        captureTakenThisTick = true;
        VideoCaptureScheduler scheduler = activeVideoScheduler;
        if (scheduler != null) {
            scheduler.capture();
        }
    }

    /**
     * 获取当前活跃的视频调度器实例
     *
     * @return 当前活跃实例；服务未启动、启动失败或已停机时返回 {@code null}。
     *         实例随时可能被 start/stop 置空：先存到局部变量再判空使用
     */
    public static VideoCaptureScheduler getVideoCaptureScheduler() {
        return activeVideoScheduler;
    }

    /**
     * 是否存在已建立的远程推流玩家会话 (纯内存原子读取，Mixin 优先查询)
     *
     * @return true 表示服务运行中且远程会话已接入；false 表示两者任一不成立
     */
    public static boolean isStreaming() {
        return running && sessionActive;
    }

    /**
     * InputInjector 光标的统一下行事件出口：读当前活跃服务推送，无服务时静默丢弃
     */
    private static void pushCursorEvent(boolean visible, int x, int y) {
        SessionServer server = getServer();
        if (server != null) {
            server.pushEvent(ClientEvent.CursorState.of(visible, x, y));
        }
    }

    /**
     * 串流中把游戏内打开网址动作传到远程浏览器
     *
     * @param uri 待打开的网址
     * @return true 表示已接管并推送远程浏览器，调用方必须取消本地打开；false 表示
     *         未接管（未推流、非 http/https 或 uri 为 {@code null}），本地照常打开
     */
    public static boolean forwardOpenUrl(URI uri) {
        if (!isStreaming() || uri == null) {
            return false;
        }
        String scheme = uri.getScheme() == null ? "" : uri.getScheme().toLowerCase(Locale.ROOT);
        if (!"http".equals(scheme) && !"https".equals(scheme)) {
            return false;
        }
        SessionServer server = getServer();
        if (server == null) {
            return false;
        }
        server.pushEvent(ClientEvent.OpenUrl.of(uri.toString()));
        return true;
    }

    /**
     * 旁听 vanilla 剪贴板写入：串流中同步转发远程浏览器
     *
     * <p>空串与超长文本静默丢弃。
     *
     * @param text 剪贴板文本；超上限整体静默丢弃
     */
    public static void onClipboardWritten(String text) {
        if (!isStreaming() || text == null || text.isEmpty() || text.length() > MAX_CLIPBOARD_FORWARD_LENGTH) {
            return;
        }
        SessionServer server = getServer();
        if (server == null) {
            return;
        }
        server.pushEvent(ClientEvent.ClipboardWrite.of(text));
    }

    /**
     * 当前音频采集调度器；服务未启动、启动失败或已停机时返回 {@code null}
     *
     * <p>实例随时可能被 start/stop 置空：先存到局部变量再判空使用。
     * Mixin 回调与 start/stop 在不同线程并发，字段只能读一次。
     * 设备句柄与 context 就绪位的正本在 {@link AudioCaptureScheduler.LoopbackBinding}，
     * Mixin 钩子直达；本方法仅供 init 尾部钩子取调度器唤醒采集线程。
     */
    public static AudioCaptureScheduler getAudioScheduler() {
        return activeAudioScheduler;
    }

    /**
     * 服务集群是否正在运行
     *
     * @return true 表示服务处于运行状态；不代表存在远程会话（会话活跃见 {@link #isStreaming()}）
     */
    public static boolean isRunning() {
        return running;
    }

    /**
     * 获取当前活跃的控制面服务实例
     *
     * @return 当前活跃实例；服务未启动或已停机时返回 {@code null}，引用随时可能被
     *         start/stop 置空，先存局部变量再判空使用
     */
    public static SessionServer getServer() {
        ServiceContext ctx = activeContext;
        return ctx == null ? null : ctx.server();
    }

    /**
     * 获取当前活跃的数据面推流通道
     *
     * @return 当前活跃通道；服务未启动或已停机时返回 {@code null}，引用随时可能被
     *         start/stop 置空，先存局部变量再判空使用
     */
    public static EncoderChannel getChannel() {
        ServiceContext ctx = activeContext;
        return ctx == null ? null : ctx.encoder();
    }

    /**
     * 浏览器访问地址：读启动期缓存，缓存未就绪（未启动/已停机）时回落回环地址。
     *
     * @return 形如 {@code http://<host>:<port>} 的完整访问地址，永不返回 {@code null}
     */
    public static String getAccessUrl() {
        String cached = accessUrl;
        if (cached != null) {
            return cached;
        }
        return "http://127.0.0.1:" + VistrelConfig.current().port();
    }

    /**
     * 启动时注入的三件套：配置、控制面服务、数据面通道
     */
    private record ServiceContext(
            Supplier<VistrelConfig> config,
            SessionServer server,
            EncoderChannel encoder,
            InputEventSource inputEvents
    ) {}

    /**
     * 控制面内件：串行队列 + 自愈冷却闸（{@code 0 = 从未放行}）
     */
    static final class RestartGate {
        private final long restartCooldownNanos;
        private final LongSupplier clockNanos;
        private final AtomicLong lastRestartNanos = new AtomicLong(0L);
        private final ExecutorService executor = Executors.newSingleThreadExecutor(r -> {
            Thread thread = new Thread(r, "Vistrel-Control");
            thread.setDaemon(true);
            return thread;
        });

        RestartGate(long restartCooldownNanos, LongSupplier clockNanos) {
            this.restartCooldownNanos = restartCooldownNanos;
            this.clockNanos = clockNanos;
        }

        void submit(Runnable action) {
            executor.execute(action);
        }

        /**
         * 自愈冷却检查：距上次放行不足冷却期返回 false；放行并记账返回 true
         */
        boolean tryRestart() {
            long now = clockNanos.getAsLong();
            long last = lastRestartNanos.get();
            if (last != 0L && now - last < restartCooldownNanos) {
                return false;
            }
            return lastRestartNanos.compareAndSet(last, now);
        }

        /**
         * 仅测试使用：清空自愈冷却账目
         */
        void resetRestartCooldownForTest() {
            lastRestartNanos.set(0L);
        }
    }
}

package kairo.vistrel.streaming;

import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import kairo.vistrel.Constants;
import java.util.List;
import java.util.Objects;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.LockSupport;

/**
 * 档位变更后台协调器：内容轮询 + 信令偏好汇成阻塞 ask，渲染线程零等待。
 *
 * <p>渲染线程只喂内容快照（无变更回调，只能按帧节拍轮询）；视口与 BPP 是信令事件，
 * 变化立即生效，防抖归客户端。内容须经 1 秒稳定窗：同一快照持续 1 秒才合成目标，
 * 与上次已尝试目标一致则跳过，拒绝后的重试风暴由 lastAttempted 记账；0×0 立即翻
 * PAUSED，恢复立即翻回 STREAMING（采集按在位档位重建），ask 只微调几何。
 *
 * <p>会话状态是唯一一份不可变记录（{@link SessionServer.SessionState}）：新代次 / 新相位 /
 * 新档位都整体替换；在途 ask 以发布快照的编码器代次引用结账。监听器只收唤醒，消费者在
 * 动作点经 {@link #current()} 现读，投递顺序不参与正确性。
 *
 * <p>线程模型：客户端线程每 tick 喂内容快照（{@link #onContentSnapshot}）并就地翻相位；
 * 信令线程经 {@link #noteOfferAnswer} / {@link #onClientSettings} / {@link #onSessionClosed}
 * 落账；工作线程串行执行阻塞 ask（{@link Applier}），应答 JSON 是档位的唯一事实源。
 *
 * <p>拒绝不自动重试：旧档继续生效，失败只落日志。
 */
public final class ProfileCoordinator {
    private static final long STABILITY_WINDOW_NS = 1_000_000_000L;
    /**
     * Minecraft 帧率滑杆的 Unlimited 哨兵值：滑杆范围 10..260（步进 10），260 显示为「无限」
     */
    private static final int UNLIMITED_FPS_SENTINEL = 260;
    /**
     * Unlimited 映射到的流帧率上限，与 Native 侧 MAX_FPS 一致
     */
    static final int STREAM_MAX_FPS = 240;
    /**
     * 无内容快照时的回退档位（1080p60）
     */
    private static final int FALLBACK_WIDTH = 1920;
    private static final int FALLBACK_HEIGHT = 1080;
    private static final int FALLBACK_FPS = 60;
    /**
     * BPP 合法区间，与 Native clamp_bpp 及网页端滑杆一致
     */
    public static final double MIN_BPP = 0.10;
    private static final double MAX_BPP = 0.30;

    /**
     * 游戏实例内容的不可变快照：客户端线程每次 tick 采样一次
     *
     * <p>Content Resolution = framebuffer 的物理像素尺寸；Streaming Framerate 的三个输入
     * （配置帧率上限、VSync、显示器刷新率）一并采集，由 {@link ProfileCoordinator#plan}
     * 合并成目标档位。宽高为 0 表示最小化 / 无 framebuffer，语义是暂停而非修正。
     */
    public record ContentSnapshot(int frameWidth, int frameHeight, int configFps, boolean vsync, int refreshHz) {
        /**
         * 空快照：客户端线程尚未产出任何采样（启动窗口），规划器据此回退 1080p60
         */
        public static final ContentSnapshot EMPTY = new ContentSnapshot(0, 0, 0, false, 0);

        public boolean hasFramebuffer() {
            return frameWidth > 0 && frameHeight > 0;
        }
    }

    /**
     * 阻塞 ask 出口：生产是 NativeBridge::applyProfile，测试注入假实现。
     * 返回应答 JSON（成功时含实际生效档位）；无会话 / 引擎未启动 / 请求被拒返回空串或 null
     */
    public interface Applier {
        String apply(int width, int height, int fps, double bpp);
    }

    private final Applier applier;
    // 唤醒监听器：写少读多，回调各自跳线程
    private final List<Runnable> listeners = new CopyOnWriteArrayList<>();
    // latest-wins 槽位直接存待下发档位
    private final AtomicReference<SessionServer.Tier> pending = new AtomicReference<>(null);
    private final Thread worker;
    private volatile boolean stopped = false;
    // 视口物理像素上限（网页端算好 CSS × DPR），0 = 无上限；两字段独立写，
    // 撕裂最坏是单帧内一维短暂滞后，稳定窗口会吸收，不值得为此上打包编码
    private volatile int viewportWidth = 0;
    private volatile int viewportHeight = 0;
    /**
     * 当前会话状态；null = 无会话。任何变化都整体替换实例
     */
    private final AtomicReference<SessionServer.SessionState> published = new AtomicReference<>(null);
    /**
     * 结算锁：快照发布与相位翻转必须串行化。信令层可能有多个 event loop 并发落账；
     * 若不串行化，旧快照可能后发布覆盖新快照。会话代次由 {@link SessionServer.EncoderGeneration}
     * 的对象身份承载，不需要额外的计数器。
     */
    private final Object settlementLock = new Object();
    // ---- 内容稳定计时：lastSeenAtNanos/seenDirty 只由渲染线程读写；lastAttempted 另有信令线程记账 ----
    private volatile ContentSnapshot latestSnapshot = ContentSnapshot.EMPTY;
    private long lastSeenAtNanos = 0L;
    private boolean seenDirty = false;
    private SessionServer.Tier lastAttempted = null;
    private volatile boolean paused = false;

    /**
     * 主构造：创建后台工作线程但不启动（启动须调用 {@link #start()}）
     *
     * @param applier 阻塞 ask 出口；传 {@code null} 等同恒返回 {@code null} 的空实现
     *                （ask 恒无应答，旧档继续生效）
     */
    public ProfileCoordinator(Applier applier) {
        this.applier = applier != null ? applier : (width, height, fps, bpp) -> null;
        this.worker = new Thread(this::runLoop, "vistrel-profile-worker");
        this.worker.setDaemon(true);
    }

    /**
     * 单监听器便捷构造：等同构造后立即 {@link #addListener}，测试与简单装配用
     *
     * @param applier 阻塞 ask 出口，语义同主构造（{@code null} 等同空实现）
     * @param listener 初始唤醒监听器；{@code null} 时不注册
     */
    public ProfileCoordinator(Applier applier, Runnable listener) {
        this(applier);
        addListener(listener);
    }

    /**
     * 注册唤醒监听器：回调里不得缓存状态，要读就调 {@link #current()}
     *
     * @param listener 唤醒回调；{@code null} 忽略不注册，重复注册重复收
     */
    public void addListener(Runnable listener) {
        if (listener != null) {
            listeners.add(listener);
        }
    }

    /**
     * 当前会话状态：调用方在自己的动作点现读，不要缓存唤醒时刻的值
     *
     * @return 不可变状态；无会话时为 {@code null}
     */
    public SessionServer.SessionState current() {
        return published.get();
    }

    /**
     * 启动工作线程；服务启动时调用一次
     *
     * @throws IllegalThreadStateException 工作线程已启动过时抛出
     */
    public void start() {
        worker.start();
    }

    /**
     * 停止工作线程；服务停机时调用，未处理的待办一并作废；shutdown 后所有入口幂等空转
     */
    public void shutdown() {
        stopped = true;
        worker.interrupt();
    }

    /**
     * 客户端线程每次 tick 喂入内容采样（稳定窗判定与下发时机的机制总纲见类文档）；
     * framebuffer 0×0 是暂停信号，就地（客户端线程）分发暂停事件
     */
    public void onContentSnapshot(ContentSnapshot snapshot, long nowNanos) {
        if (stopped) {
            return;
        }
        Objects.requireNonNull(snapshot, "snapshot 不能为空");
        // 与上一帧比较（latestSnapshot 此刻还是旧值）：变了即重启 1 秒稳定计时
        boolean contentChanged = !snapshot.equals(latestSnapshot);
        latestSnapshot = snapshot;
        if (!snapshot.hasFramebuffer()) {
            // 内容丢失：未暂停则立即翻相位（不等稳定窗），计时作废——
            // latestSnapshot 为 0×0，恢复帧与之比较必然判「变化」，自然重启计时
            seenDirty = false;
            if (!paused) {
                paused = true;
                publishPhase(SessionServer.Phase.PAUSED);
            }
            return;
        }
        if (contentChanged) {
            lastSeenAtNanos = nowNanos;
            seenDirty = true;
            return;
        }
        if (!seenDirty || nowNanos - lastSeenAtNanos < STABILITY_WINDOW_NS) {
            return;
        }
        // 内容稳定满 1 秒：此刻才合成目标（视口 / BPP 用当前值，其变化走信令事件路径）。
        // bpp 取当前生效值（纯码率重配不会被拒，请求值与生效值无分叉）。
        // lastAttempted 不在此记账——成功 / 拒绝 / 无应答由 applyTarget 统一落账，
        // 预写会让 applyTarget 的等值去重读到刚写的自己，永远放行不了
        seenDirty = false;
        if (paused) {
            // 恢复即刻翻回 STREAMING：采集侧先按在位档位重建，下面的 ask 只微调几何
            paused = false;
            publishPhase(SessionServer.Phase.STREAMING);
        }
        // latestSnapshot 此刻已是本次快照，plan 直读内部状态
        SessionServer.Tier target = plan(viewportWidth, viewportHeight, currentBpp());
        if (target.equals(lastAttempted)) {
            return;
        }
        submit(target);
    }

    /**
     * 无会话期种子：只更新最新内容快照，不规划、不下发、不广播。
     *
     * <p>offer 规划发生在会话建立之前，此时 {@link #onContentSnapshot} 的内容来源尚未开启，
     * 兜底几何会让接入后按第一份稳定快照再重建一次采集管线。
     *
     * @param snapshot 当前内容快照；不得为空
     */
    public void seedContentSnapshot(ContentSnapshot snapshot) {
        if (stopped) {
            return;
        }
        latestSnapshot = Objects.requireNonNull(snapshot, "snapshot 不能为空");
    }

    /**
     * 信令线程入口：外部偏好变更（settings 消息同步，offer 携带视口也走这里，无变化字段传 null）。
     * 防抖归客户端（resize 已 250ms 防抖），视口 / BPP 任一变化都立即用当前内容快照合成目标
     * 落槽位 ask（latest-wins 自动合并风暴）；offer 未协商时忽略——offer 自带 bpp 与视口，
     * 会话建立即生效。视口非法值忽略（保持既有上限）；BPP 越界按拒绝记日志保持旧档
     *
     * @param viewportWidth  视口物理像素宽度；null 表示本字段无变化
     * @param viewportHeight 视口物理像素高度；null 表示本字段无变化
     * @param bpp            请求画质档位 (BPP)；null 表示本字段无变化
     */
    public void onClientSettings(Integer viewportWidth, Integer viewportHeight, Double bpp) {
        if (stopped) {
            return;
        }
        boolean viewportChanged = false;
        if (viewportWidth != null
                && viewportHeight != null
                && viewportWidth > 0
                && viewportHeight > 0
                && (viewportWidth != this.viewportWidth || viewportHeight != this.viewportHeight)) {
            this.viewportWidth = viewportWidth;
            this.viewportHeight = viewportHeight;
            viewportChanged = true;
        }
        boolean bppChanged = false;
        double requestedBpp = MIN_BPP;
        SessionServer.SessionState state = published.get();
        if (bpp != null) {
            double requested = clampBpp(bpp);
            if (!isBppInRange(requested)) {
                Constants.LOG.warn("BPP 请求越界 ({})，保持旧档 {}", requested, state == null ? "无会话" : state.tier());
            } else if (requested != currentBpp() && state != null) {
                requestedBpp = requested;
                bppChanged = true;
            }
        }
        // 未协商（offer 未落账）时不主动 ask：内容由 offer 的初始规划生效。
        // 会话活跃即基线已种入，此处读 lastAttempted 恒非空
        if ((viewportChanged || bppChanged) && state != null) {
            submit(planForSettings(requestedBpp));
        }
    }

    /**
     * 当前生效的 BPP：取自最近一次生效档位——纯码率重配不会被编码器拒绝，请求值与生效值
     * 无分叉；未协商时取下限
     */
    private double currentBpp() {
        SessionServer.SessionState state = published.get();
        return state != null ? state.tier().bpp() : MIN_BPP;
    }

    /**
     * 当前视口物理像素上限（settings 同步的最新值），供 offer 规划在消息未携带视口时回退；0 = 无上限
     */
    public int viewportWidth() {
        return viewportWidth;
    }

    public int viewportHeight() {
        return viewportHeight;
    }

    /**
     * 用当前内容快照规划目标：快照由渲染线程持续喂入（latestSnapshot），调用方无需传递。
     * 输出是「请求」而非最终档位——4K 包络、偶数对齐与 level 预算由 Native sanitize 最终裁定
     *
     * @param viewportWidth  视口物理像素上限，0 = 无上限
     * @param viewportHeight 视口物理像素上限，0 = 无上限
     * @param bpp            请求画质档位 (BPP)
     * @return 档位请求，只作为 ask 目标下发，并非最终生效档位
     */
    public SessionServer.Tier plan(int viewportWidth, int viewportHeight, double bpp) {
        ContentSnapshot snapshot = latestSnapshot;
        int width;
        int height;
        int fps;
        if (snapshot.hasFramebuffer()) {
            width = snapshot.frameWidth();
            height = snapshot.frameHeight();
            int configured = snapshot.configFps() == UNLIMITED_FPS_SENTINEL ? STREAM_MAX_FPS : snapshot.configFps();
            fps = configured;
            if (snapshot.vsync() && snapshot.refreshHz() > 0) {
                fps = Math.min(fps, snapshot.refreshHz());
            }
        } else {
            width = FALLBACK_WIDTH;
            height = FALLBACK_HEIGHT;
            fps = FALLBACK_FPS;
        }
        fps = Math.max(1, Math.min(STREAM_MAX_FPS, fps));

        if (viewportWidth > 0 && viewportHeight > 0) {
            if (width > viewportWidth || height > viewportHeight) {
                // 等比只缩：取视口宽高两边中更紧的约束，交叉乘比较避免除法误差
                if ((long) width * viewportHeight > (long) viewportWidth * height) {
                    int scaledHeight = (int) Math.max(1L, (long) height * viewportWidth / width);
                    width = viewportWidth;
                    height = scaledHeight;
                } else {
                    int scaledWidth = (int) Math.max(1L, (long) width * viewportHeight / height);
                    width = scaledWidth;
                    height = viewportHeight;
                }
            }
        }
        return new SessionServer.Tier(width, height, fps, clampBpp(bpp));
    }

    /**
     * 信令线程入口：offer 协商成功的 Answer 落账。应答携带协商定死的后端身份与初始档位，
     * 此后内容 / BPP 请求以该基线为去重锚
     *
     * @param answerJson   协商应答 JSON（与下发浏览器的 Answer 同源）
     * @param requestedBpp 请求的画质档位，用于种入去重锚
     * @return 落账的实际档位（Answer 的显示几何）；不可解析或字段缺失时为 {@code null}，状态保持不变
     */
    public SessionServer.Tier noteOfferAnswer(String answerJson, double requestedBpp) {
        if (stopped) {
            return null;
        }
        JsonObject obj;
        try {
            obj = JsonParser.parseString(answerJson).getAsJsonObject();
        } catch (Exception e) {
            Constants.LOG.warn("Answer 解析失败，档位基线保持未种: {}", e.getMessage());
            return null;
        }
        SessionServer.Tier applied = tierFrom(obj);
        if (applied == null) {
            Constants.LOG.warn("Answer 档位字段缺失，基线保持未种");
            return null;
        }
        String backend =
                obj.has("backend") && !obj.get("backend").isJsonNull() ? obj.get("backend").getAsString() : null;
        String codec = obj.has("codec") && !obj.get("codec").isJsonNull() ? obj.get("codec").getAsString() : "";
        // 落账即新代次：新 EncoderGeneration 对象身份即控制面会话代次，与快照发布在同一临界区。
        // 落账相位取当下内容（最小化中直接 PAUSED）
        synchronized (settlementLock) {
            published.set(
                    new SessionServer.SessionState(
                            new SessionServer.EncoderGeneration(backend, codec),
                            phaseNow(),
                            applied
                    )
            );
            lastAttempted = new SessionServer.Tier(
                    applied.width(),
                    applied.height(),
                    applied.fps(),
                    clampBpp(requestedBpp)
            );
        }
        wakeListeners();
        return applied;
    }

    /**
     * 信令线程入口：会话关闭（WS 断开）复位基线并清空待办，
     * 使迟到的内容事件按「无活跃会话」处理而不是沿用已失效的值
     *
     * <p>关闭无条件唤醒：内容暂停时消费者已处于暂停相位，去重会让复位丢失
     */
    public void onSessionClosed() {
        if (stopped) {
            return;
        }
        // 复位即作废：状态清空（在途 ask 读 null 即作废）。与落账共用结算锁，
        // 避免关闭与并发落账交错出「已关闭但快照复活」
        synchronized (settlementLock) {
            published.set(null);
        }
        pending.set(null);
        wakeListeners();
    }

    /**
     * 唤醒监听器：只喊一声，不带状态——消费者在动作点读 {@link #current()}
     */
    private void wakeListeners() {
        for (Runnable listener : listeners) {
            try {
                listener.run();
            } catch (Throwable t) {
                // 单消费者异常不得截断广播（关闭 / 暂停必须到达其余消费者）
                Constants.LOG.error("会话监听器异常", t);
            }
        }
    }

    /**
     * 就地翻相位：同会话换实例（在途 ask 的应答只写档位）；无会话或已一致时空转。
     * 与结算共用锁：相位发布必须落在当下快照上，不能被并发落账的旧相位覆盖。
     */
    private void publishPhase(SessionServer.Phase phase) {
        synchronized (settlementLock) {
            SessionServer.SessionState state;
            do {
                state = published.get();
                if (state == null || state.phase() == phase) {
                    return;
                }
            } while (!published.compareAndSet(state, state.withPhase(phase)));
        }
        wakeListeners();
    }

    /**
     * 落账相位取当下内容：最小化中建立的会话直接 PAUSED
     */
    private SessionServer.Phase phaseNow() {
        return paused ? SessionServer.Phase.PAUSED : SessionServer.Phase.STREAMING;
    }

    private void submit(SessionServer.Tier tier) {
        pending.set(tier);
        LockSupport.unpark(worker);
    }

    private void runLoop() {
        while (!stopped) {
            SessionServer.Tier apply = pending.getAndSet(null);
            if (apply == null) {
                // park 前的投递由 unpark 许可兜住：不会漏任务也不会永久睡眠
                LockSupport.park(this);
                continue;
            }
            try {
                applyTarget(apply);
            } catch (Throwable t) {
                Constants.LOG.error("档位任务处理异常: {}", t.getMessage(), t);
            }
        }
    }

    /**
     * 信令线程：外部偏好（视口 / BPP）变更的目标几何。有新鲜内容快照就整体重规划
     * （bpp 下降可经 level 预算把被压低的帧率拿回来，视口收缩立即降采样）；
     * 暂停中 / 无快照时保持实际生效的几何与帧率，绝不借回退档位抬几何
     */
    private SessionServer.Tier planForSettings(double bpp) {
        ContentSnapshot snapshot = latestSnapshot;
        if (!snapshot.hasFramebuffer()) {
            SessionServer.Tier applied = published.get().tier();
            return new SessionServer.Tier(applied.width(), applied.height(), applied.fps(), bpp);
        }
        return plan(viewportWidth, viewportHeight, bpp);
    }

    private void applyTarget(SessionServer.Tier target) {
        SessionServer.SessionState before = published.get();
        if (before == null) {
            lastAttempted = target;
            return;
        }
        // 与上次请求一致则不 ask：被内容吸收的外部变化不打扰编码器
        if (target.equals(lastAttempted)) {
            return;
        }
        SessionServer.EncoderGeneration beforeGeneration = before.generation();
        String json = applier.apply(target.width(), target.height(), target.fps(), target.bpp());
        SessionServer.SessionState now = published.get();
        if (now == null || now.generation() != beforeGeneration) {
            // ask 期间跨代次 / 关闭：应答属于旧会话，不得写回
            return;
        }
        SessionServer.Tier parsed = json == null || json.isEmpty() ? null : parseTierReply(json);
        if (parsed == null) {
            // 无会话 / 应答不可解析：旧档继续生效，基线保持不动
            lastAttempted = target;
            Constants.LOG.warn("档位 ask 无应答（{}），保持旧档 {}", target, now.tier());
            return;
        }
        // 只写档位：相位可能已翻过，以当下实例为 CAS 目标重试；跨代次则放弃
        while (true) {
            now = published.get();
            if (now == null || now.generation() != beforeGeneration) {
                return;
            }
            if (published.compareAndSet(now, now.withTier(parsed))) {
                break;
            }
        }
        lastAttempted = target;
        wakeListeners();
    }

    /**
     * 应答 JSON 的公共档位字段（协商应答与档位 ask 应答同形）：缺任一字段即 null
     */
    private static SessionServer.Tier tierFrom(JsonObject obj) {
        try {
            return new SessionServer.Tier(
                    obj.get("width").getAsInt(),
                    obj.get("height").getAsInt(),
                    obj.get("fps").getAsInt(),
                    obj.has("bpp") ? obj.get("bpp").getAsDouble() : MIN_BPP
            );
        } catch (Exception e) {
            return null;
        }
    }

    /**
     * 解析 JNI 档位 ask 应答：字段恒为「仍在生效」的实际档位——拒绝 / 无会话由 Native
     * 回空串，解析失败即 null。
     */
    private SessionServer.Tier parseTierReply(String json) {
        try {
            SessionServer.Tier tier = tierFrom(JsonParser.parseString(json).getAsJsonObject());
            if (tier == null) {
                Constants.LOG.warn("档位应答 JSON 字段缺失");
            }
            return tier;
        } catch (Exception e) {
            Constants.LOG.warn("档位应答 JSON 解析失败: {}", e.getMessage());
            return null;
        }
    }

    /**
     * BPP 钳制：非有限值回落最低档，区间外夹端点，并量化到 0.01 步长（与网页端滑杆一致）。
     * 只在规划与落账内部生效——信令透传原始请求值，Native 侧 clamp_bpp 同规则兜底
     */
    static double clampBpp(double bpp) {
        if (!Double.isFinite(bpp)) {
            return MIN_BPP;
        }
        double clamped = Math.min(MAX_BPP, Math.max(MIN_BPP, bpp));
        return Math.round(clamped * 100.0) / 100.0;
    }

    static boolean isBppInRange(double bpp) {
        return Double.isFinite(bpp) && bpp >= MIN_BPP && bpp <= MAX_BPP;
    }
}

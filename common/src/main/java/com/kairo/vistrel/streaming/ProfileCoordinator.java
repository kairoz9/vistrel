package com.kairo.vistrel.streaming;

import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import com.kairo.vistrel.Constants;
import java.util.List;
import java.util.Objects;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Consumer;
import java.util.concurrent.locks.LockSupport;

/**
 * 档位变更后台协调器：本地内容轮询 + 外部偏好事件，汇成阻塞 ask，渲染线程零等待
 *
 * <p>职责分界：渲染线程只管游戏内容（snapshot 无变更回调，只能帧节拍轮询）；视口与 BPP
 * 是信令推的外部事件，变化立即生效——防抖归客户端（resize 已 250ms 防抖），服务端不做二次稳定。
 *
 * <p>内容变化须经稳定窗：plan 是纯函数，快照不变则目标必不变——客户端 tick（20Hz）只做一次
 * 快照值比较，同一快照持续 1 秒才合成目标放行；窗口内反复变化只有最终稳定者被放行；
 * 与上次已尝试目标一致则跳过——拒绝后的重试风暴由 lastAttempted 统一记账；0×0 → 立即广播
 * PAUSED 并清去重锚，恢复后同一目标照常重发。
 *
 * <p>线程模型：
 * <ul>
 *   <li>客户端线程每次 tick 喂内容快照：会话活跃时经 {@link #onContentSnapshot}（规划 + 稳定判定，
 *       纯内存算术，无锁无阻塞），事件落 latest-wins 槽位并唤醒工作线程；暂停事件在客户端
 *       线程就地分发。无会话时经 {@link #seedContentSnapshot} 仅更新快照，供 offer 规划使用。
 *       稳定窗状态（lastSeenAtNanos / seenDirty / paused）只由客户端线程读写</li>
 *   <li>信令线程经三个入口落位会话事件：{@link #noteOfferAnswer}（offer 落账）、
 *       {@link #onClientSettings}（外部偏好变更：视口 / BPP 任一变化立即落槽位）、
 *       {@link #onSessionClosed}（关闭复位）。lastAttempted 的跨线程写只发生在
 *       offer 落账与 ask 记账——Java 引用写原子，最坏滞后一帧重放行一次</li>
 *   <li>工作线程串行执行阻塞 ask（{@link Applier}，即 JNI {@code applyProfile}），
 *       应答 JSON 是档位的唯一事实源，结果广播给全部已注册监听器</li>
 * </ul>
 *
 * <p>拒绝不自动重试：目标统一记为已尝试，避免编码器持续拒绝时的风暴；
 * 旧档继续生效（流没有变化），失败痕迹只落日志，浏览器不感知。
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
        String apply(int width, int height, int fps, double bpp, String codec);
    }

    private final Applier applier;
    // 状态监听器：写少读多，广播来自工作线程 / 渲染线程 / 信令线程，各自跳线程
    private final List<Consumer<SessionServer.SessionState>> listeners = new CopyOnWriteArrayList<>();
    // latest-wins 槽位直接存待下发状态（内容变更与 BPP 变更同型，拖动风暴自动合并）
    private final AtomicReference<SessionServer.SessionState> pending = new AtomicReference<>(null);
    private final Thread worker;
    private volatile boolean stopped = false;
    // 视口物理像素上限（网页端算好 CSS × DPR），0 = 无上限；两字段独立写，
    // 撕裂最坏是单帧内一维短暂滞后，稳定窗口会吸收，不值得为此上打包编码
    private volatile int viewportWidth = 0;
    private volatile int viewportHeight = 0;
    /**
     * 最近一次实际生效的档位
     * 换代必换实例——在途 ask 结账按引用同一性作废，值相等不代表同一会话
     */
    private final AtomicReference<SessionServer.SessionState> lastApplied =
            new AtomicReference<>(SessionServer.SessionState.INACTIVE);
    // ---- 内容稳定计时：lastSeenAtNanos/seenDirty 只由渲染线程读写；lastAttempted 另有信令线程记账 ----
    private volatile ContentSnapshot latestSnapshot = ContentSnapshot.EMPTY;
    private long lastSeenAtNanos = 0L;
    private boolean seenDirty = false;
    private SessionServer.SessionState lastAttempted = null;
    private volatile boolean paused = false;

    /**
     * 主构造：创建后台工作线程但不启动（启动须调用 {@link #start()}）
     *
     * @param applier 阻塞 ask 出口；传 {@code null} 等同恒返回 {@code null} 的空实现
     *                （ask 恒无应答，旧档继续生效）
     */
    public ProfileCoordinator(Applier applier) {
        this.applier = applier != null ? applier : (width, height, fps, bpp, codec) -> null;
        this.worker = new Thread(this::runLoop, "vistrel-profile-worker");
        this.worker.setDaemon(true);
    }

    /**
     * 单监听器便捷构造：等同构造后立即 {@link #addListener}，测试与简单装配用
     *
     * @param applier  阻塞 ask 出口，语义同主构造（{@code null} 等同空实现）
     * @param listener 初始状态监听器；{@code null} 时不注册
     */
    public ProfileCoordinator(Applier applier, Consumer<SessionServer.SessionState> listener) {
        this(applier);
        addListener(listener);
    }

    /**
     * 注册状态消费者：只收生效状态（STREAMING 落账 / PAUSED 内容丢失 / INACTIVE 关闭），
     * 拒绝不外报——旧档继续生效，流本身没有变化，浏览器以视频元素与 getStats 自观测
     *
     * @param listener 状态消费者；{@code null} 忽略不注册，重复注册重复收
     */
    public void addListener(Consumer<SessionServer.SessionState> listener) {
        if (listener != null) {
            listeners.add(listener);
        }
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
            // 内容丢失：未暂停则立即下发暂停态（不等稳定窗），计时作废——
            // latestSnapshot 为 0×0，恢复帧与之比较必然判「变化」，自然重启计时。
            // 同时清去重锚：恢复时目标常与暂停前一字不差，必须照常下发以重建采集
            seenDirty = false;
            lastAttempted = null;
            if (!paused) {
                paused = true;
                broadcast(lastApplied.get().paused());
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
        paused = false;
        // latestSnapshot 此刻已是本次快照，plan 直读内部状态
        SessionServer.SessionState target = plan(viewportWidth, viewportHeight, currentBpp());
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
        if (bpp != null) {
            double requested = clampBpp(bpp);
            if (!isBppInRange(requested)) {
                Constants.LOG.warn("BPP 请求越界 ({})，保持旧档 {}", requested, lastApplied.get());
            } else if (requested != currentBpp() && lastApplied.get().active()) {
                requestedBpp = requested;
                bppChanged = true;
            }
        }
        // 未协商（offer 未落账）时不主动 ask：内容由 offer 的初始规划生效。
        // 会话活跃即基线已种入，此处读 lastAttempted 恒非空
        if ((viewportChanged || bppChanged) && lastApplied.get().active()) {
            submit(planForSettings(requestedBpp));
        }
    }

    /**
     * 当前生效的 BPP：取自最近一次生效快照——纯码率重配不会被编码器拒绝，请求值与生效值
     * 无分叉；未协商时 INACTIVE 自带滑杆下限
     */
    private double currentBpp() {
        return lastApplied.get().bpp();
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
     * @return 档位请求（active 位恒 true，只作为 ask 请求下发），并非最终生效档位
     */
    public SessionServer.SessionState plan(int viewportWidth, int viewportHeight, double bpp) {
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
        // 请求态：active 位固定 true（规划结果只作为 ask 请求），codec 与 backend 由
        // offer 应答决定，会话前为占位值，不会被 ask 消费
        return new SessionServer.SessionState(
                SessionServer.SessionState.Phase.STREAMING,
                width,
                height,
                fps,
                clampBpp(bpp),
                lastApplied.get().codec(),
                lastApplied.get().backend()
        );
    }

    /**
     * 信令线程入口：offer 协商成功的 Answer 落账。Answer JSON 自行解析（与阻塞 ask
     * 应答同形状，解析器只有一份），种入 lastApplied / bpp 基线并经 listener 广播
     * 初始档位——渲染侧管线落地、音频会话同步与会话接入提示都从这条链走；
     * 此后内容 / BPP 请求以该基线为去重锚
     */
    public void noteOfferAnswer(String answerJson, double requestedBpp) {
        if (stopped) {
            return;
        }
        SessionServer.SessionState applied = parseApplied(answerJson, SessionServer.SessionState.BACKEND_UNKNOWN);
        if (applied == null) {
            Constants.LOG.warn("Answer 解析失败，档位基线保持未种");
            return;
        }
        // 落账即换代：parseApplied 恒产新实例，在途 ask 结账按引用同一性作废
        lastApplied.set(applied);
        lastAttempted = new SessionServer.SessionState(
                SessionServer.SessionState.Phase.STREAMING,
                applied.width(),
                applied.height(),
                applied.fps(),
                clampBpp(requestedBpp),
                applied.codec(),
                applied.backend()
        );
        broadcast(applied);
    }

    /**
     * 信令线程入口：会话关闭（WS 断开）复位基线并清空待办，
     * 使迟到的内容事件按「无活跃会话」处理而不是沿用已失效的值
     *
     * <p>关闭必须无条件广播 INACTIVE：内容已暂停时 listener 已处于暂停相位，
     * 去重会让这次复位丢失（音频不停、sessionActive 不复位）
     */
    public void onSessionClosed() {
        if (stopped) {
            return;
        }
        // 复位即换代：基线换回 INACTIVE 常量，与在途 ask 捕获的活跃实例引用必不相同
        lastApplied.set(SessionServer.SessionState.INACTIVE);
        pending.set(null);
        broadcast(SessionServer.SessionState.INACTIVE);
    }

    private void broadcast(SessionServer.SessionState state) {
        for (Consumer<SessionServer.SessionState> listener : listeners) {
            try {
                listener.accept(state);
            } catch (Throwable t) {
                // 单消费者异常不得截断广播（关闭 / 暂停必须到达其余消费者）
                Constants.LOG.error("会话状态监听器异常", t);
            }
        }
    }

    private void submit(SessionServer.SessionState state) {
        pending.set(state);
        LockSupport.unpark(worker);
    }

    private void runLoop() {
        while (!stopped) {
            SessionServer.SessionState apply = pending.getAndSet(null);
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
    private SessionServer.SessionState planForSettings(double bpp) {
        ContentSnapshot snapshot = latestSnapshot;
        if (!snapshot.hasFramebuffer()) {
            SessionServer.SessionState applied = lastApplied.get();
            return new SessionServer.SessionState(
                    SessionServer.SessionState.Phase.STREAMING,
                    applied.width(),
                    applied.height(),
                    applied.fps(),
                    bpp,
                    applied.codec(),
                    applied.backend()
            );
        }
        return plan(viewportWidth, viewportHeight, bpp);
    }

    private void applyTarget(SessionServer.SessionState target) {
        SessionServer.SessionState before = lastApplied.get();
        if (!before.active()) {
            lastAttempted = target;
            return;
        }
        // 与上次请求一致则不 ask：外部变化（如视口收缩）被内容完全吸收时，目标一个字不变，
        // 编码器无需重配；暂停会让 lastAttempted 归零，恢复时同一目标照常下发
        if (target.equals(lastAttempted)) {
            return;
        }
        String json = applier.apply(target.width(), target.height(), target.fps(), target.bpp(), target.codec());
        if (lastApplied.get() != before) {
            // ask 期间换代（关闭复位 / 新 offer 落账）
            return;
        }
        SessionServer.SessionState parsed = json == null || json.isEmpty() ? null : parseApplied(json, before.backend());
        if (parsed == null) {
            // 无会话 / 应答不可解析：旧档继续生效，基线保持不动
            lastAttempted = target;
            Constants.LOG.warn("档位 ask 无应答（{}），保持旧档 {}", target, lastApplied.get());
            return;
        }
        // CAS 失败说明应答属于旧会话，连同广播一并作废。
        if (!lastApplied.compareAndSet(before, parsed)) {
            return;
        }
        lastAttempted = target;
        broadcast(parsed);
    }

    /**
     * 解析 JNI 应答：字段恒为「仍在生效」的实际档位——拒绝 / 无会话由 Native 回空串，
     * 解析失败即 null。backend 为可选字段：缺失时按未知处理
     */
    /**
     * 解析 JNI 应答：字段恒为「仍在生效」的实际档位——拒绝 / 无会话由 Native 回空串，
     * 解析失败即 null。后端身份只在协商应答里携带（身份只在协商时确定并变更）：
     * 协商落账传未知值兜底，档位 ask 传基线的协商值
     *
     * @param fallbackBackend 应答未携带 backend 时的后端身份
     */
    private SessionServer.SessionState parseApplied(String json, String fallbackBackend) {
        try {
            JsonObject obj = JsonParser.parseString(json).getAsJsonObject();
            int width = obj.get("width").getAsInt();
            int height = obj.get("height").getAsInt();
            int fps = obj.get("fps").getAsInt();
            double bpp = obj.has("bpp") ? obj.get("bpp").getAsDouble() : MIN_BPP;
            String codec = obj.get("codec").getAsString();
            String backend =
                    obj.has("backend") && !obj.get("backend").isJsonNull()
                    ? obj.get("backend").getAsString()
                    : fallbackBackend;
            return new SessionServer.SessionState(
                    SessionServer.SessionState.Phase.STREAMING,
                    width,
                    height,
                    fps,
                    bpp,
                    codec,
                    backend
            );
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

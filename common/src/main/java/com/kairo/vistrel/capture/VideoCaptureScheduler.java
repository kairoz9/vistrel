package com.kairo.vistrel.capture;

import com.kairo.vistrel.Constants;
import com.kairo.vistrel.streaming.EncoderChannel;
import com.kairo.vistrel.streaming.SessionServer;
import com.kairo.vistrel.streaming.SessionServer.SessionEvent;
import com.kairo.vistrel.streaming.SessionServer.SessionEvent.Phase;
import com.kairo.vistrel.streaming.SessionServer.Tier;
import net.minecraft.client.Minecraft;
import java.util.Objects;
import java.util.function.BooleanSupplier;

/**
 * 视频渲染捕获总调度器
 * 负责主渲染线程的帧率节拍控制 (Pacing)、双管道 (VRAM Direct / RAM Readback) 的可用性裁定与断路器降级。
 *
 * 降级条件分两种：构造期失败立即降级；运行期只有**连续** {@code FAILURE_THRESHOLD} 帧
 * {@code Failed}（管线抛出异常同样折算为 {@code Failed}）才熔断，{@code Delivered} 与
 * {@code Dropped}（槽位全在途等主动丢帧）一律清零、不计入。
 *
 * 实例自身不持有任何静态状态，生命周期由 VistrelService 统一管理。
 */
public class VideoCaptureScheduler {
    private final EncoderChannel channel;
    /**
     * 显存直通能力判定：每次会话变更时求值一次
     */
    private final BooleanSupplier vramDirectAllowed;
    private final FramePacingThrottler throttler;
    /**
     * 采集状态（仅渲染线程读写，与 capture 同线程串行）：IDLE ⇄ CAPTURING 由会话广播
     * 驱动，TORN_DOWN 为终态（迟到的会话广播在此丢弃）。单字段单写者，无需同步原语
     */
    private CaptureState state = CaptureState.IDLE;
    private VideoPipeline ramPipeline = null;
    private VideoPipeline vramPipeline = null;
    /**
     * 会话生命周期快照（事件溯源）
     */
    private volatile boolean sessionLive = false;
    /**
     * 会话的协商后端是否硬件：建立事件落账后**会话内恒定**
     */
    private volatile boolean sessionHardware = false;
    /**
     * 当前管线采集尺寸；宽高 0 表示尚无可用管线，几何变化必须重建
     * codec 会话期内固定（SDP 只协商一次），不参与重建判定。
     */
    private int pipelineWidth = 0;
    private int pipelineHeight = 0;
    /**
     * 编码停滞自愈回调：显存管线心跳超时或导入报「编码器不可用」时在渲染线程调用，
     * 实现方必须立即移交后台线程。测试与未装配场景用空实现占位
     */
    private static final Runnable NOOP_STALL_LISTENER = () -> {};
    private Runnable encoderStallListener = NOOP_STALL_LISTENER;

    /**
     * 注入编码停滞自愈回调（服务启动时装配）
     *
     * @param listener 停滞自愈回调；显存管线心跳超时或导入报「编码器不可用」判定后
     *                 在渲染线程触发一次，实现方必须立即移交后台线程，不得在回调内
     *                 触碰任何 GPU 资源；{@code null} 回落空实现
     */
    public void setEncoderStallListener(Runnable listener) {
        this.encoderStallListener = listener != null ? listener : NOOP_STALL_LISTENER;
    }

    /**
     * 固定开关构造：以 60fps 节拍器初始化
     *
     * @param channel           数据面通道，采集帧经它投递
     * @param vramDirectEnabled 是否允许显存直通；false 时恒定走 RAM 回读管线，
     *                          true 时显存管线构建失败仍会在运行期永久降级到 RAM
     */
    public VideoCaptureScheduler(EncoderChannel channel, boolean vramDirectEnabled) {
        this(channel, () -> vramDirectEnabled, new FramePacingThrottler(60));
    }

    /**
     * 固定开关构造：节流器注入（经它编排虚拟时间）
     */
    public VideoCaptureScheduler(EncoderChannel channel, boolean vramDirectEnabled, FramePacingThrottler throttler) {
        this(channel, () -> vramDirectEnabled, throttler);
    }

    /**
     * 生产构造：以 60fps 节拍器初始化，显存直通能力延迟求值
     *
     * @param channel           数据面通道，采集帧经它投递
     * @param vramDirectAllowed 显存直通能力判定；每次会话变更时求值一次。调用方须保证
     *                          Native 编码能力探测已完成（首次启动的构造期 caps 仍为空）
     */
    public VideoCaptureScheduler(EncoderChannel channel, BooleanSupplier vramDirectAllowed) {
        this(channel, vramDirectAllowed, new FramePacingThrottler(60));
    }

    /**
     * 测试装配构造：能力判定与节流器均注入
     */
    public VideoCaptureScheduler(
            EncoderChannel channel,
            BooleanSupplier vramDirectAllowed,
            FramePacingThrottler throttler
    ) {
        this.channel = channel;
        this.vramDirectAllowed = Objects.requireNonNull(vramDirectAllowed, "显存直通能力判定不能为空");
        this.throttler = Objects.requireNonNull(throttler, "节流器不能为空");
    }

    protected VideoPipeline createRamPipeline(int width, int height) {
        //? if gpu_device {
        return new RenderpearlRamPipeline(this.channel, width, height);
        //? } else {
        /*return new OpenGlRamPipeline(this.channel, width, height);*/
        //? }
    }

    protected VideoPipeline createVramPipeline(int width, int height) {
        //? if vulkan_backend {
        return new RenderpearlVramPipeline(this.channel, width, height, this.encoderStallListener);
        //? } else {
        /*// 纯 OpenGL 后端（1.20/1.21 线与 26.1）明确不支持显存直通，统一走双 PBO 内存回读管线
        return null;*/
        //? }
    }

    /**
     * 会话事件主入口（经 {@code ProfileCoordinator} 的监听回调触发）：建立事件落账
     * 会话身份并建首份管线，档位变化重建采集管线，关闭与内容丢失停采释放
     */
    public void onSessionEvent(SessionEvent event) {
        if (event instanceof SessionEvent.Established established) {
            this.sessionLive = true;
            this.sessionHardware = SessionServer.isHardwareBackend(established.backend());
            dispatchTier(established.tier(), this.sessionHardware);
            return;
        }
        if (event instanceof SessionEvent.TierChanged tierChanged) {
            if (!this.sessionLive) {
                // 迟到的旧会话档位：会话已关，不得为死会话重建管线
                return;
            }
            if (tierChanged.phase() == Phase.PAUSED) {
                // 内容丢失：会话仍在线，停采释放管线（音频照常，仅视频停采）
                dispatchIdle();
                return;
            }
            dispatchTier(tierChanged.tier(), this.sessionHardware);
            return;
        }
        // 会话关闭：生命周期位清零并停采
        this.sessionLive = false;
        this.sessionHardware = false;
        dispatchIdle();
    }

    /**
     * 生效档位落到节拍器与渲染线程管线
     */
    private void dispatchTier(Tier tier, boolean hardwareBackend) {
        if (tier.fps() > 0) {
            this.throttler.setFps(tier.fps());
        }
        int width = tier.width();
        int height = tier.height();
        runOnRenderThread(() -> {
            if (this.state == CaptureState.TORN_DOWN) {
                // teardown 之后才落地的重建任务：管线必须保持已释放，泄漏防线
                return;
            }
            if (width <= 0 || height <= 0) {
                // 防御兜底：合法档位不会有 0 几何（Native 拒绝暂停信号），坏值按停采处理
                this.state = CaptureState.IDLE;
                cleanupPipelines();
                return;
            }
            this.state = CaptureState.CAPTURING;
            boolean vramDirectEnabled = resolveVramDirect(hardwareBackend);
            // 只变了帧率：尺寸没变且管线还在，直接复用（暂停后管线已释放，自然不会命中）；
            // 能力从 false 变 true 时必须重建
            boolean vramSettled =
                    vramDirectEnabled ? vramPipeline != null || vramBreaker.isTripped() : vramPipeline == null;
            if (width == pipelineWidth && height == pipelineHeight && ramPipeline != null && vramSettled) {
                return;
            }

            cleanupPipelines();
            // 换了尺寸 = 换了管线：断路器状态与连续失败数不能带过来，
            // 否则重建后可能被上次的失败记录立刻误判成故障
            this.vramBreaker.reset();
            this.ramFailureGate.reset();
            // 1. 优先构建兜底保障的 RAM 回读管道
            try {
                this.ramPipeline = createRamPipeline(width, height);
            } catch (Throwable t) {
                Constants.LOG.error("构建 RAM 回读管道异常: {}", t.getMessage(), t);
            }
            // 2. 构造期就定死显存直通 (VRAM Direct) 能不能用：结构性失败即永久降级，
            //    本次会话不再重试
            if (vramDirectEnabled && !this.vramBreaker.isTripped()) {
                try {
                    this.vramPipeline = createVramPipeline(width, height);
                    if (this.vramPipeline == null) {
                        demoteVram("当前版本不支持显存直通");
                    }
                } catch (EncoderChannel.ControlException t) {
                    this.vramPipeline = null;
                    if (t.code == EncoderChannel.CONTROL_NO_HARDWARE_ENCODER) {
                        // 编码器不在了：降级采集救不了没有编码器的流，唯一恢复路径
                        // 是整引擎自愈重启（重新探测硬件并重新协商）
                        Constants.LOG.error("显存直通构建失败：{}，触发编码停滞自愈", t.getMessage());
                        this.encoderStallListener.run();
                    } else {
                        // 槽位注册失败等故障：编码器仍在，降级 RAM 照常续流
                        demoteVram("构造期未就绪: " + t.getMessage());
                    }
                } catch (Throwable t) {
                    this.vramPipeline = null;
                    demoteVram("构造期未就绪: " + t.getMessage());
                }
            }

            this.pipelineWidth = width;
            this.pipelineHeight = height;
        });
    }

    /**
     * 停采并释放管线：会话关闭与内容丢失共用
     */
    private void dispatchIdle() {
        runOnRenderThread(() -> {
            if (this.state == CaptureState.TORN_DOWN) {
                return;
            }
            this.state = CaptureState.IDLE;
            cleanupPipelines();
        });
    }

    /**
     * 读取显存直通能力：失败按禁用处理，绝不让 JNI 能力查询异常掀翻渲染线程
     */
    private boolean resolveVramDirect(boolean hardwareBackend) {
        try {
            return this.vramDirectAllowed.getAsBoolean() && hardwareBackend;
        } catch (Throwable t) {
            Constants.LOG.error("读取显存直通能力失败，按禁用处理: {}", t.getMessage());
            return false;
        }
    }

    /**
     * 渲染线程投递：无实例或已在渲染线程就地执行，否则排入 Minecraft 任务队列
     */
    private void runOnRenderThread(Runnable task) {
        Minecraft mc = Minecraft.getInstance();
        if (mc == null || mc.isSameThread()) {
            task.run();
            return;
        }
        try {
            mc.execute(task);
        } catch (Throwable t) {
            Constants.LOG.error("无法将采集任务投递至渲染线程: {}", t.getMessage());
        }
    }

    /**
     * 连续失败多少帧才判定为该级管线故障并熔断
     */
    private static final int FAILURE_THRESHOLD = 30;
    private static final String VRAM_LABEL = "显存直通 (VRAM Direct)";
    private static final String RAM_LABEL = "内存回读 (RAM Readback)";
    /**
     * 显存直通的断路器：熔断即永久降级到 RAM（仅渲染线程读写）
     */
    private final CircuitBreaker vramBreaker = new CircuitBreaker(VRAM_LABEL, FAILURE_THRESHOLD);
    /**
     * RAM 首次采集异常只报一次（仅渲染线程读写）
     *
     * RAM 是最后一级，熔断无处可去，所以它只要「异常不逃逸 + 不刷屏」：
     * 第一条异常报一次，此后继续采，管线重建时归位。
     */
    private final ReportGate ramFailureGate = ReportGate.once();

    /**
     * 采集一帧并直接推流
     *
     * 三种结果各有明确处理：{@code Delivered} 已投递、{@code Dropped} 主动丢帧（刻意不回落 RAM）、
     * {@code Failed} 回落下一级并累计连续失败。异常一律算作 {@code Failed}，绝不允许逃逸到渲染线程。
     *
     * @return 本帧是否已投递；false 表示无推流会话、被节流拦截、主动丢帧
     */
    public boolean capture() {
        if (this.state != CaptureState.CAPTURING) {
            CaptureProfiler.count(CaptureProfiler.Counter.SCHEDULER_IDLE);
            return false;
        }

        if (!throttler.shouldCapture()) {
            CaptureProfiler.count(CaptureProfiler.Counter.SCHEDULER_THROTTLED);
            return false;
        }
        long startNanos = CaptureProfiler.begin();
        try {
            return dispatchCapture();
        } finally {
            CaptureProfiler.end(CaptureProfiler.Stage.SCHEDULER_CAPTURE, startNanos);
        }
    }

    private boolean dispatchCapture() {
        // Level 1: 显存直通 (VRAM Direct)
        if (vramPipeline != null) {
            VideoPipeline.CaptureOutcome outcome;
            try {
                outcome = vramPipeline.capture();
            } catch (Throwable t) {
                Constants.LOG.error("{} 采集抛出异常: {}", VRAM_LABEL, t.getMessage());
                outcome = VideoPipeline.CaptureOutcome.Failed;
            }
            if (vramBreaker.record(outcome)) {
                demoteVram(vramBreaker.streak() + " 帧连续采集失败");
            }
            switch (outcome) {
                case Delivered:
                    CaptureProfiler.count(CaptureProfiler.Counter.VRAM_DELIVERED);
                    return true;
                case Dropped:
                    // 主动丢帧（槽位全在途 / 首帧预热）：管线健康，本帧直接跳过
                    CaptureProfiler.count(CaptureProfiler.Counter.VRAM_DROPPED);
                    return false;
                case Failed:
                    CaptureProfiler.count(CaptureProfiler.Counter.VRAM_FAILED);
                    break;
            }
        }
        // Level 2: 内存回读 (RAM Readback)
        if (ramPipeline != null) {
            VideoPipeline.CaptureOutcome outcome = captureRamSafely();
            if (outcome == VideoPipeline.CaptureOutcome.Delivered) {
                CaptureProfiler.count(CaptureProfiler.Counter.RAM_DELIVERED);
                return true;
            }
            CaptureProfiler.count(
                    outcome == VideoPipeline.CaptureOutcome.Dropped
                    ? CaptureProfiler.Counter.RAM_DROPPED
                    : CaptureProfiler.Counter.RAM_FAILED
            );
            return false;
        }

        CaptureProfiler.count(CaptureProfiler.Counter.SCHEDULER_NO_PIPELINE);
        return false;
    }

    /**
     * 采一帧内存回读：异常折算为 {@code Failed} 且不逃逸，首条异常报一次
     */
    private VideoPipeline.CaptureOutcome captureRamSafely() {
        try {
            return ramPipeline.capture();
        } catch (Throwable t) {
            if (ramFailureGate.tryFire()) {
                Constants.LOG.error("{} 采集抛出异常: {}", RAM_LABEL, t.getMessage());
            }
            return VideoPipeline.CaptureOutcome.Failed;
        }
    }

    /**
     * 永久降级：标记熔断、释放显存管线，本次会话只走 RAM（重建管线才复位）
     */
    private void demoteVram(String reason) {
        this.vramBreaker.trip();
        CaptureProfiler.count(CaptureProfiler.Counter.VRAM_DEMOTED);
        Constants.LOG.warn("显存直通 (VRAM Direct) 永久降级到 RAM 回读: {}", reason);
        if (this.vramPipeline != null) {
            try {
                this.vramPipeline.close();
            } catch (Throwable ignored) {
            }
            this.vramPipeline = null;
        }
    }

    private void cleanupPipelines() {
        if (this.vramPipeline != null) {
            try {
                this.vramPipeline.close();
            } catch (Throwable ignored) {
            }
            this.vramPipeline = null;
        }
        if (this.ramPipeline != null) {
            try {
                this.ramPipeline.close();
            } catch (Throwable ignored) {
            }
            this.ramPipeline = null;
        }
        // 管线已销毁，几何随之失效：下一次推送必须重建，不能走复用分支
        this.pipelineWidth = 0;
        this.pipelineHeight = 0;
    }

    /**
     * 拆除采集（幂等）：置入 TORN_DOWN 终态并释放全部管线与节拍基准；
     * 终态下到达的会话重建任务一律拒绝，管线保持已释放（泄漏防线）
     */
    public synchronized void cleanup() {
        this.state = CaptureState.TORN_DOWN;
        cleanupPipelines();
        throttler.reset();
    }

    /**
     * 采集状态：本调度器视角的采集开关
     */
    enum CaptureState {
        /**
         * 无会话或内容丢失：门关、管线已释放
         */
        IDLE,
        /**
         * 会话活跃且几何已落地：capture 正常放行
         */
        CAPTURING,
        /**
         * 实例已拆除（终态）
         */
        TORN_DOWN
    }

    /**
     * 断路器：连续 {@code Failed} 累加到阈值即熔断，一次成功或主动丢帧即清零
     *
     * 熔断后不会自行恢复，只有管线重建时的 {@link #reset()} 才归位
     * （经典断路器的半开试探在这里没有意义：降级目标早已选定）。
     */
    static final class CircuitBreaker {
        private final String label;
        private final int threshold;
        /**
         * 连续失败帧数：任一 {@code Delivered} / {@code Dropped} 即清零
         */
        private int streak = 0;
        private boolean tripped = false;

        CircuitBreaker(String label, int threshold) {
            this.label = label;
            this.threshold = threshold;
        }

        String label() {
            return label;
        }

        int streak() {
            return streak;
        }

        boolean isTripped() {
            return tripped;
        }

        /**
         * 直接熔断（构造期用）：不计数、不看阈值
         */
        void trip() {
            this.tripped = true;
        }

        /**
         * 重建管线时归位：断路状态与连续失败数都不能跨管线带进来
         */
        void reset() {
            this.streak = 0;
            this.tripped = false;
        }

        /**
         * 记一次采集结局
         *
         * @return 本次是否恰好熔断；已熔断的实例此后一直返回 false（熔断只需告警一次）
         */
        boolean record(VideoPipeline.CaptureOutcome outcome) {
            if (this.tripped) {
                return false;
            }
            if (outcome != VideoPipeline.CaptureOutcome.Failed) {
                this.streak = 0;
                return false;
            }
            this.streak++;
            if (this.streak >= this.threshold) {
                this.tripped = true;
                return true;
            }
            return false;
        }
    }
}

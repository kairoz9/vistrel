package com.kairo.vistrel.capture;

import java.util.function.LongSupplier;

/**
 * 采集限速器：把采集次数压到平均不超过目标帧率，防止高刷渲染时浪费 GPU DMA 与编码算力。
 * 判定用时间预算而不是绝对节拍点，渲染帧率不是目标整数倍时也能产满目标帧率；停滞只补一帧，不会连发补帧。
 * 时钟为构造注入：生产走系统单调时钟，测试注入可编程虚拟时间逐帧钉边界
 */
public class FramePacingThrottler {
    /**
     * 允许早到的预算：帧间隔抖动会让帧落在节拍点前，严格比较会整拍丢帧；
     * 1ms 在 240fps 下最多多采约 24%，60fps 下约 6%
     */
    private static final long EARLY_TOLERANCE_NANOS = 1_000_000L;
    private final LongSupplier clockNanos;
    private volatile int fps;
    /**
     * 攒下的时间预算：每帧累加流逝时间，放行时扣掉一个间隔（主渲染线程独占）
     */
    private long budgetNanos = 0;
    /**
     * 上一帧的判定时刻：预算按帧间流逝时间累加（主渲染线程独占）
     */
    private long lastNanos = 0;

    /**
     * 生产便捷构造：时钟取系统墙钟
     *
     * @param fps 初始推流帧率上限；非正值回落 60
     */
    public FramePacingThrottler(int fps) {
        this(fps, System::nanoTime);
    }

    /**
     * 全量注入构造：clockNanos 供测试编排虚拟时间，生产路径走便捷构造
     *
     * @param fps       初始推流帧率上限；非正值回落 60
     * @param clockNanos 纳秒时钟源；{@code null} 回落系统墙钟
     */
    public FramePacingThrottler(int fps, LongSupplier clockNanos) {
        this.clockNanos = clockNanos != null ? clockNanos : System::nanoTime;
        setFps1(fps);
    }

    public int getFps() {
        return fps;
    }

    /**
     * 接收新推流帧率（控制面推送，可安全由任意线程异步调用）
     *
     * @param fps 新推流帧率上限；非正值回落 60
     */
    public void setFps(int fps) {
        setFps1(fps);
    }

    private void setFps1(int fps) {
        this.fps = fps > 0 ? fps : 60;
    }

    /**
     * 限速判定
     *
     * @return true 表示本帧放行并扣除一个间隔的预算；false 表示提前量超过容差被拦截。
     *         判定须在主渲染线程调用，内部预算与基准时刻由该线程独占
     */
    public boolean shouldCapture() {
        long now = clockNanos.getAsLong();
        if (lastNanos == 0) {
            // 首帧立即放行并锚定基准
            lastNanos = now;
            return true;
        }
        long interval = 1_000_000_000L / this.fps;
        budgetNanos += now - lastNanos;
        lastNanos = now;
        if (budgetNanos < interval - EARLY_TOLERANCE_NANOS) {
            return false;
        }
        // 可为负：下一帧先把透支的补回来，长期平均正好落在目标帧率；
        // 剩余预算超过一整个间隔说明期间有停滞，直接丢弃——停滞落下的帧已不可补，追发只会连发重复帧
        budgetNanos -= interval;
        if (budgetNanos >= interval) {
            budgetNanos = 0;
        }
        return true;
    }

    /**
     * 清空预算与判定基准：下一次 {@link #shouldCapture()} 立即放行；帧率上限保持不变
     */
    public void reset() {
        budgetNanos = 0;
        lastNanos = 0;
    }
}

package com.kairo.vistrel.capture;

import java.util.function.LongSupplier;

/**
 * 高精度帧节拍拦截器
 * 用于将 Minecraft 主渲染循环与协商的推流帧率对齐，防止高刷渲染时浪费 GPU DMA 与编码算力。
 * 时钟为构造注入：生产走系统单调时钟，测试注入可编程虚拟时间逐帧钉边界
 */
public class FramePacingThrottler {
    private final LongSupplier clockNanos;
    private volatile int fps;
    /**
     * 最近一次实际放行的采集时刻：切档时以此为基准重排节拍（主渲染线程独占）
     */
    private long lastCaptureNanos = 0;
    /**
     * 下一个节拍点：到点才放行；跨过整个节拍间隔时以当前时刻重新对齐，
     * 不补发落下的帧（主渲染线程独占）
     */
    private long dueNanos = 0;
    /**
     * 主渲染线程已生效的帧率：用来检测切档并在主线程内重排节拍，避免跨线程读写竞态
     */
    private int appliedFps = 60;

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
        this.appliedFps = this.fps;
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
     * 节拍判定：时钟在本类内部取，调用方不传时刻
     *
     * @return true 表示到点放行（本帧继续采集，节拍点推进）；false 表示未到节拍点被拦截。
     *         判定须在主渲染线程调用，内部节拍状态由该线程独占
     */
    public boolean shouldCapture() {
        long currentNanos = clockNanos.getAsLong();
        int current = this.fps;
        // 间隔由 fps 现场派生，不存第二份状态：双 volatile 写不成原子对，切档首帧会用旧间隔重排
        long interval = 1_000_000_000L / current;
        // 检测切档：在主渲染线程安全重排节拍
        if (current != appliedFps) {
            appliedFps = current;
            if (dueNanos != 0) {
                dueNanos = lastCaptureNanos + interval;
            }
        }

        if (dueNanos == 0) {
            // 首帧立即放行并锚定节拍
            dueNanos = currentNanos + interval;
            lastCaptureNanos = currentNanos;
            return true;
        }
        if (currentNanos < dueNanos) {
            return false;
        }
        // 正常按节拍推进；若本次放行已跨过至少一个完整节拍（渲染停滞/掉帧），
        // 就以当前时刻重新对齐——停滞期间落下的帧补不回来，沿旧节拍追赶只会连发补帧，
        // 重新对齐后本帧放行、之后回到正常间隔
        dueNanos = (currentNanos >= dueNanos + interval) ? currentNanos + interval : dueNanos + interval;
        lastCaptureNanos = currentNanos;
        return true;
    }

    /**
     * 清空节拍基准：最近放行时刻与下一个节拍点归零，下一次 {@link #shouldCapture()}
     * 立即放行并重新锚定节拍；帧率上限保持不变
     */
    public void reset() {
        lastCaptureNanos = 0;
        dueNanos = 0;
        appliedFps = this.fps;
    }
}

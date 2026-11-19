package com.kairo.vistrel.capture;

import com.kairo.vistrel.Constants;
import com.kairo.vistrel.platform.Services;
import java.util.Arrays;
import java.util.Locale;
import java.util.concurrent.atomic.AtomicLongArray;

/**
 * Java 侧采集阶段计时（仅开发环境）。
 *
 * <p>在渲染线程上记录各采集阶段的墙钟耗时，后台守护线程每 5 秒汇总一次
 * p50/p95/p99/max 与交付/丢帧/失败计数。每个窗口只打印本窗口有活动的一路
 * 管线（VRAM 或 RAM），管线切换窗口两路都会出现。
 * 统计口径是 <b>CPU 提交耗时</b>：
 * GPU 异步执行与编码器消费不在内；{@code submit} 的 2 深背压等待计入该阶段，
 * {@code map} 只计映射调用本身（Vulkan 后端为纯映射，无隐式等待）。
 *
 * <p>各阶段样本独立计算分位数，不保证跨阶段来自同一帧；分位数不可直接相加做
 * 严格分解。未走到阶段结束点的异常帧只计入 total 与失败计数，不会补造阶段样本。
 *
 * <p>开发环境默认启用，可用 {@code -Dvistrel.capture.profile=false} 关闭；
 * 生产环境恒定关闭。关闭时 {@link #begin()} 返回 0，其余记录方法立即返回。
 *
 * <p>线程模型：{@code end}/{@code count} 只在渲染线程调用，每个阶段单写者；
 * 上报线程只读环形缓冲与原子计数，不与渲染线程同步。
 */
final class CaptureProfiler {
    static final boolean ENABLED = Services.PLATFORM.isDevelopmentEnvironment()
            && !"false".equalsIgnoreCase(System.getProperty("vistrel.capture.profile"));
    /**
     * 每阶段保留的最近样本数：5 秒窗口在 240fps 下约 1200 帧，4096 足够覆盖
     */
    private static final int CAPACITY = 4096;
    private static final int MASK = CAPACITY - 1;
    private static final long REPORT_INTERVAL_MILLIS = 5_000L;

    enum Stage {
        SCHEDULER_CAPTURE("capture"),
        VRAM_TOTAL("total"),
        VRAM_PREP("prep"),
        VRAM_BLIT("blit"),
        VRAM_BARRIER("barrier"),
        VRAM_SIGNAL("signal"),
        VRAM_SUBMIT("submit"),
        VRAM_PUSH("push"),
        RAM_TOTAL("total"),
        RAM_PREP("prep"),
        RAM_STATE_SAVE("state.save"),
        RAM_BLIT_Y("blit.y"),
        RAM_BLIT_UV("blit.uv"),
        RAM_COPY("copy"),
        RAM_SUBMIT("submit"),
        RAM_READBACK("readback"),
        RAM_FENCE("fence"),
        RAM_MAP("map"),
        RAM_PUSH("push"),
        RAM_UNMAP("unmap"),
        RAM_STATE_RESTORE("state.restore");
        final String label;

        Stage(String label) {
            this.label = label;
        }
    }

    enum Counter {
        SCHEDULER_IDLE,
        SCHEDULER_THROTTLED,
        SCHEDULER_NO_PIPELINE,
        VRAM_DELIVERED,
        VRAM_DROPPED,
        VRAM_FAILED,
        VRAM_DEMOTED,
        RAM_DELIVERED,
        RAM_DROPPED,
        RAM_FAILED
    }

    private static final SampleWindow[] WINDOWS;
    private static final AtomicLongArray COUNTERS;
    /**
     * 仅上报线程读写：上一窗口结束时的累计计数
     */
    private static final long[] LAST_COUNTERS;

    static {
        if (ENABLED) {
            Stage[] stages = Stage.values();
            WINDOWS = new SampleWindow[stages.length];
            for (int i = 0; i < stages.length; i++) {
                WINDOWS[i] = new SampleWindow();
            }
            COUNTERS = new AtomicLongArray(Counter.values().length);
            LAST_COUNTERS = new long[Counter.values().length];
            Thread reporter = new Thread(CaptureProfiler::reportLoop, "Vistrel Capture Profiler");
            reporter.setDaemon(true);
            reporter.start();
        } else {
            WINDOWS = null;
            COUNTERS = null;
            LAST_COUNTERS = null;
        }
    }

    private CaptureProfiler() {
    }

    /**
     * 开始一段阶段计时
     *
     * @return 起始纳秒时间戳；关闭时返回 0
     */
    static long begin() {
        return ENABLED ? System.nanoTime() : 0L;
    }

    /**
     * 结束一段阶段计时并写入样本
     *
     * @param stage      阶段
     * @param startNanos {@link #begin()} 的返回值
     */
    static void end(Stage stage, long startNanos) {
        if (ENABLED) {
            WINDOWS[stage.ordinal()].record(System.nanoTime() - startNanos);
        }
    }

    /**
     * 累计一次结局/调度计数
     */
    static void count(Counter counter) {
        if (ENABLED) {
            COUNTERS.incrementAndGet(counter.ordinal());
        }
    }

    private static void reportLoop() {
        Constants.LOG.info(
                "[capture-profile] 开发环境已启用：每 {}s 汇总 Java 侧采集阶段耗时（单位 ms；仅 CPU 提交耗时，不含 GPU 异步执行）",
                REPORT_INTERVAL_MILLIS / 1000L
        );
        long lastReportNanos = System.nanoTime();
        while (true) {
            try {
                Thread.sleep(REPORT_INTERVAL_MILLIS);
            } catch (InterruptedException e) {
                return;
            }
            long nowNanos = System.nanoTime();
            try {
                report(nowNanos - lastReportNanos);
            } catch (Throwable t) {
                Constants.LOG.warn("[capture-profile] 汇总失败: {}", t.getMessage());
            }
            lastReportNanos = nowNanos;
        }
    }

    private static void report(long windowNanos) {
        Stage[] stages = Stage.values();
        Stats[] stats = new Stats[stages.length];
        for (Stage stage : stages) {
            stats[stage.ordinal()] = WINDOWS[stage.ordinal()].snapshot();
        }
        long[] counters = new long[Counter.values().length];
        for (Counter counter : Counter.values()) {
            counters[counter.ordinal()] = nextCounterDelta(counter.ordinal());
        }
        // 无串流会话时调度器每帧只累计 idle，不打印空窗口
        boolean hasCapture = stats[Stage.SCHEDULER_CAPTURE.ordinal()].samples > 0
                || stats[Stage.VRAM_TOTAL.ordinal()].samples > 0
                || stats[Stage.RAM_TOTAL.ordinal()].samples > 0;
        if (!hasCapture) {
            return;
        }
        // 每窗口只打印有活动的一路；切换窗口两路都有活动时同时打印
        boolean vramActive = stats[Stage.VRAM_TOTAL.ordinal()].samples > 0
                || counters[Counter.VRAM_DELIVERED.ordinal()] > 0
                || counters[Counter.VRAM_DROPPED.ordinal()] > 0
                || counters[Counter.VRAM_FAILED.ordinal()] > 0
                || counters[Counter.VRAM_DEMOTED.ordinal()] > 0;
        boolean ramActive = stats[Stage.RAM_TOTAL.ordinal()].samples > 0
                || counters[Counter.RAM_DELIVERED.ordinal()] > 0
                || counters[Counter.RAM_DROPPED.ordinal()] > 0
                || counters[Counter.RAM_FAILED.ordinal()] > 0;
        String window = String.format(Locale.ROOT, "%.2fs", windowNanos / 1_000_000_000.0);

        StringBuilder line = new StringBuilder(256);
        line
            .append("[capture-profile] window=")
            .append(window)
            .append(" (ms) | sched idle=")
            .append(counters[Counter.SCHEDULER_IDLE.ordinal()])
            .append(" throttled=")
            .append(counters[Counter.SCHEDULER_THROTTLED.ordinal()])
            .append(" noPipeline=")
            .append(counters[Counter.SCHEDULER_NO_PIPELINE.ordinal()]);
        appendStages(line, stats, Stage.SCHEDULER_CAPTURE);
        Constants.LOG.info(line.toString());

        if (vramActive) {
            line = new StringBuilder(256);
            line
                .append("[capture-profile] window=")
                .append(window)
                .append(" (ms) | vram delivered=")
                .append(counters[Counter.VRAM_DELIVERED.ordinal()])
                .append(" dropped=")
                .append(counters[Counter.VRAM_DROPPED.ordinal()])
                .append(" failed=")
                .append(counters[Counter.VRAM_FAILED.ordinal()])
                .append(" demoted=")
                .append(counters[Counter.VRAM_DEMOTED.ordinal()]);
            appendStages(
                    line,
                    stats,
                    Stage.VRAM_TOTAL,
                    Stage.VRAM_PREP,
                    Stage.VRAM_BLIT,
                    Stage.VRAM_BARRIER,
                    Stage.VRAM_SIGNAL,
                    Stage.VRAM_SUBMIT,
                    Stage.VRAM_PUSH
            );
            Constants.LOG.info(line.toString());
        }

        if (ramActive) {
            line = new StringBuilder(256);
            line
                .append("[capture-profile] window=")
                .append(window)
                .append(" (ms) | ram delivered=")
                .append(counters[Counter.RAM_DELIVERED.ordinal()])
                .append(" dropped=")
                .append(counters[Counter.RAM_DROPPED.ordinal()])
                .append(" failed=")
                .append(counters[Counter.RAM_FAILED.ordinal()]);
            appendStages(
                    line,
                    stats,
                    Stage.RAM_TOTAL,
                    Stage.RAM_PREP,
                    Stage.RAM_STATE_SAVE,
                    Stage.RAM_BLIT_Y,
                    Stage.RAM_BLIT_UV,
                    Stage.RAM_COPY,
                    Stage.RAM_SUBMIT,
                    Stage.RAM_READBACK,
                    Stage.RAM_FENCE,
                    Stage.RAM_MAP,
                    Stage.RAM_PUSH,
                    Stage.RAM_UNMAP,
                    Stage.RAM_STATE_RESTORE
            );
            Constants.LOG.info(line.toString());
        }
    }

    private static void appendStages(StringBuilder line, Stats[] stats, Stage... stages) {
        for (Stage stage : stages) {
            Stats s = stats[stage.ordinal()];
            if (s.samples == 0) {
                continue;
            }
            line
                .append(" | ")
                .append(stage.label)
                .append(" n=")
                .append(s.samples)
                .append(" p50=")
                .append(ms(s.p50))
                .append(" p95=")
                .append(ms(s.p95))
                .append(" p99=")
                .append(ms(s.p99))
                .append(" max=")
                .append(ms(s.max));
        }
    }

    private static String ms(long nanos) {
        return String.format(Locale.ROOT, "%.3f", nanos / 1_000_000.0);
    }

    private static long nextCounterDelta(int index) {
        long current = COUNTERS.get(index);
        long delta = current - LAST_COUNTERS[index];
        LAST_COUNTERS[index] = current;
        return delta;
    }

    /**
     * 单阶段定长环形样本：渲染线程单写者，上报线程按累计写入位置取最近样本
     */
    private static final class SampleWindow {
        private final AtomicLongArray samples = new AtomicLongArray(CAPACITY);
        /**
         * 仅渲染线程写
         */
        private long nextWrite;
        /**
         * 样本写入后递增发布：上报线程看到计数时样本必已可见
         */
        private volatile long publishedWrites;
        /**
         * 仅上报线程读写：上一窗口结束时的累计写入数
         */
        private long lastWrite;

        void record(long nanos) {
            long index = nextWrite++;
            samples.set((int) (index & MASK), nanos);
            publishedWrites = index + 1;
        }

        Stats snapshot() {
            long end = publishedWrites;
            long count = end - lastWrite;
            lastWrite = end;
            int n = (int) Math.min(count, CAPACITY);
            if (n <= 0) {
                return Stats.EMPTY;
            }
            long[] values = new long[n];
            for (int i = 0; i < n; i++) {
                values[i] = samples.get((int) ((end - n + i) & MASK));
            }
            Arrays.sort(values);
            return new Stats(
                    n,
                    values[percentileIndex(n, 0.50)],
                    values[percentileIndex(n, 0.95)],
                    values[percentileIndex(n, 0.99)],
                    values[n - 1]
            );
        }
    }

    private static int percentileIndex(int samples, double percentile) {
        int index = (int) Math.ceil(percentile * samples) - 1;
        return Math.max(0, Math.min(samples - 1, index));
    }

    private static final class Stats {
        static final Stats EMPTY = new Stats(0, 0L, 0L, 0L, 0L);
        final int samples;
        final long p50;
        final long p95;
        final long p99;
        final long max;

        Stats(int samples, long p50, long p95, long p99, long max) {
            this.samples = samples;
            this.p50 = p50;
            this.p95 = p95;
            this.p99 = p99;
            this.max = max;
        }
    }
}

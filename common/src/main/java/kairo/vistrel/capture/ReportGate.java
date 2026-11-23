package kairo.vistrel.capture;

/**
 * 重复告警的节流：只决定「现在该不该上报」，上报内容由调用方自己组织。
 *
 * <p>
 * 两种形态：一次性（首次放行后记住，{@code reset()} 复位，适合"首条异常只报一次"）
 * 与窗口汇总（计数累加，窗口期满且有量才放行，适合周期性汇总）。
 */
final class ReportGate {
    private final long windowNanos;
    // 窗口形态：下一窗口的起始时刻；0 = 还没开过窗（首笔即视为期满）
    private long nextWindowNanos = 0L;
    private long count = 0L;
    private boolean fired = false;

    private ReportGate(long windowNanos) {
        this.windowNanos = windowNanos;
    }

    /**
     * 一次性形态
     */
    static ReportGate once() {
        return new ReportGate(0L);
    }

    /**
     * 窗口汇总形态：每 {@code windowNanos} 一个窗口
     */
    static ReportGate windowed(long windowNanos) {
        return new ReportGate(windowNanos);
    }

    /**
     * 一次性形态：首次 true，此后 false，直到 {@link #reset()}
     */
    boolean tryFire() {
        if (fired) {
            return false;
        }
        fired = true;
        return true;
    }

    boolean isFired() {
        return fired;
    }

    /**
     * 一次性形态：复位（管线重建等恢复路径）
     */
    void reset() {
        fired = false;
    }

    /**
     * 窗口形态：累计本窗口计数
     */
    void accumulate(long n) {
        count += n;
    }

    /**
     * 窗口形态：窗口未到期返回 -1；到期则开新窗并返回本窗计数
     * （0 表示窗口内没有量，调用方就不用记日志）
     */
    long drainIfDue(long nowNanos) {
        if (nowNanos < nextWindowNanos) {
            return -1L;
        }
        long drained = count;
        count = 0L;
        nextWindowNanos = nowNanos + windowNanos;
        return drained;
    }
}

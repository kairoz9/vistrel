package kairo.vistrel.streaming.signaling;

import java.util.HashMap;
import java.util.Map;
import java.util.function.LongSupplier;

/**
 * 认证节流：per-IP 失败冷却 + 全局未认证并发与频率闸。
 *
 * <p>全部方法同步：认证尝试频率低，不存在竞争压力。
 */
final class AuthThrottle {
    /**
     * 同时允许存在的未认证 WebSocket 连接数
     */
    static final int MAX_PENDING = 4;
    /**
     * 每分钟允许的认证尝试总次数
     */
    static final int MAX_ATTEMPTS_PER_MINUTE = 10;
    /**
     * 连续失败多少次进入冷却
     */
    static final int FAILURES_BEFORE_BAN = 5;
    /**
     * 冷却阶梯：5 分钟 → 15 分钟 → 60 分钟封顶
     */
    private static final long[] BAN_LADDER_MILLIS = {5 * 60_000L, 15 * 60_000L, 60 * 60_000L};
    /**
     * 追踪的 IP 上限，超过即清理过期记录，防止伪造源地址撑爆内存
     */
    private static final int MAX_TRACKED_IPS = 256;
    private static final long WINDOW_MILLIS = 60_000L;
    private final LongSupplier clock;
    private final Map<String, IpRecord> records = new HashMap<>();
    private int pending = 0;
    private long windowStartMillis = 0L;
    private int windowAttempts = 0;

    AuthThrottle() {
        this(System::currentTimeMillis);
    }

    /**
     * 注入时钟以便测试使用；生产路径使用系统毫秒
     */
    AuthThrottle(LongSupplier clock) {
        this.clock = clock;
    }

    /**
     * 占用一个未认证槽位。
     *
     * @param ip 客户端地址，用于冷却判定与失败计数
     * @return 槽位句柄；{@code null} 表示该 IP 正在冷却、并发已满或频率超限，调用方应立即断开。
     *     句柄必须 close()，且重复 close 安全（不会扣掉别的连接占的槽位）
     */
    synchronized Attempt tryBegin(String ip) {
        long now = clock.getAsLong();
        IpRecord record = records.get(ip);
        if (record != null && record.banUntilMillis > now) {
            return null;
        }
        if (pending >= MAX_PENDING) {
            return null;
        }
        refreshWindow(now);
        if (windowAttempts >= MAX_ATTEMPTS_PER_MINUTE) {
            return null;
        }
        windowAttempts++;
        pending++;
        return new Attempt(this);
    }

    /**
     * 已占用的未认证槽位句柄：幂等释放把自己的归属交给对象，调用方不必手工配对
     */
    static final class Attempt implements AutoCloseable {
        private final AuthThrottle throttle;
        // 只在 throttle 监视器下读写
        private boolean closed;

        private Attempt(AuthThrottle throttle) {
            this.throttle = throttle;
        }

        @Override
        public void close() {
            synchronized (throttle) {
                if (closed) {
                    return;
                }
                closed = true;
                if (throttle.pending > 0) {
                    throttle.pending--;
                }
            }
        }
    }

    synchronized void onFailure(String ip) {
        long now = clock.getAsLong();
        IpRecord record = records.computeIfAbsent(ip, key -> new IpRecord());
        record.failures++;
        if (record.failures >= FAILURES_BEFORE_BAN) {
            int step = Math.min(record.failures - FAILURES_BEFORE_BAN, BAN_LADDER_MILLIS.length - 1);
            record.banUntilMillis = now + BAN_LADDER_MILLIS[step];
        }
        if (records.size() > MAX_TRACKED_IPS) {
            // 先清过期冷却；仍超限说明正被大量伪造源地址冲击，此时整表清空优先于保内存
            records
                .entrySet()
                .removeIf(e -> e.getValue().banUntilMillis <= now);
            if (records.size() > MAX_TRACKED_IPS) {
                records.clear();
            }
        }
    }

    synchronized void onSuccess(String ip) {
        records.remove(ip);
    }

    private void refreshWindow(long now) {
        if (now - windowStartMillis >= WINDOW_MILLIS) {
            windowStartMillis = now;
            windowAttempts = 0;
        }
    }

    private static final class IpRecord {
        private int failures;
        private long banUntilMillis;
    }
}

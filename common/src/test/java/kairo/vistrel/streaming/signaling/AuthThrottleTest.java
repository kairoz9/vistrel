package kairo.vistrel.streaming.signaling;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import java.util.concurrent.atomic.AtomicLong;

/**
 * 认证节流测试：per-IP 冷却阶梯、并发槽位与全局频率闸；
 * 时钟注入，无需真等冷却时间
 */
class AuthThrottleTest {
    private final AtomicLong now = new AtomicLong(1_000_000L);
    private final AuthThrottle throttle = new AuthThrottle(now::get);

    private AuthThrottle.Attempt begin(String ip) {
        AuthThrottle.Attempt attempt = throttle.tryBegin(ip);
        Assertions.assertNotNull(attempt, "应允许认证尝试: " + ip);
        return attempt;
    }

    /**
     * 一次「占用 → 释放 → 记失败」
     */
    private void failOnce(String ip) {
        begin(ip).close();
        throttle.onFailure(ip);
    }

    @Test
    void testConsecutiveFailuresBanSameIp() {
        String ip = "10.0.0.5";
        for (int i = 0; i < AuthThrottle.FAILURES_BEFORE_BAN; i++) {
            failOnce(ip);
        }
        Assertions.assertNull(throttle.tryBegin(ip), "连续失败达阈值后该 IP 必须进入冷却");
        Assertions.assertNotNull(throttle.tryBegin("10.0.0.55"), "冷却只作用于失败的那个 IP");
    }

    @Test
    void testBanExpiresThenEscalates() {
        String ip = "10.0.0.6";
        for (int i = 0; i < AuthThrottle.FAILURES_BEFORE_BAN; i++) {
            failOnce(ip);
        }
        now.addAndGet(5 * 60_000L + 1);
        Assertions.assertNotNull(throttle.tryBegin(ip), "首轮冷却到期后必须放行");
        failOnce(ip);
        Assertions.assertNull(throttle.tryBegin(ip), "再次失败必须重新进入冷却");
        now.addAndGet(5 * 60_000L);
        Assertions.assertNull(throttle.tryBegin(ip), "第二轮冷却应升级到 15 分钟，5 分钟时仍拒绝");
        now.addAndGet(10 * 60_000L + 1);
        Assertions.assertNotNull(throttle.tryBegin(ip), "升级后的冷却到期应放行");
    }

    @Test
    void testPendingSlotCapRejectsExtraUnauthenticatedConnections() {
        AuthThrottle.Attempt[] attempts = new AuthThrottle.Attempt[AuthThrottle.MAX_PENDING];
        for (int i = 0; i < attempts.length; i++) {
            attempts[i] = begin("10.0." + i + ".1");
        }
        Assertions.assertNull(throttle.tryBegin("10.1.0.9"), "未认证并发槽位满时必须拒绝");
        attempts[0].close();
        Assertions.assertNotNull(throttle.tryBegin("10.1.0.9"), "释放槽位后应重新允许");
    }

    @Test
    void testDoubleCloseDoesNotFreeAnotherSlot() {
        AuthThrottle.Attempt[] attempts = new AuthThrottle.Attempt[AuthThrottle.MAX_PENDING];
        for (int i = 0; i < attempts.length; i++) {
            attempts[i] = begin("10.2." + i + ".1");
        }
        AuthThrottle.Attempt first = attempts[0];
        first.close();
        // 幂等：不得再扣一格
        first.close();
        AuthThrottle.Attempt extra = begin("10.3.0.1");
        Assertions.assertNull(throttle.tryBegin("10.3.0.2"), "重复 close 若多扣一格，这里会被错误放行");
        extra.close();
    }

    @Test
    void testGlobalAttemptWindowBounds() {
        String ip = "10.0.0.7";
        for (int i = 0; i < AuthThrottle.MAX_ATTEMPTS_PER_MINUTE; i++) {
            begin(ip).close();
        }
        Assertions.assertNull(throttle.tryBegin(ip), "一分钟内超过总尝试次数必须拒绝");
        now.addAndGet(60_001L);
        Assertions.assertNotNull(throttle.tryBegin(ip), "窗口滚动后应重新放行");
    }

    @Test
    void testSuccessClearsFailureCount() {
        String ip = "10.0.0.8";
        for (int i = 0; i < AuthThrottle.FAILURES_BEFORE_BAN - 1; i++) {
            failOnce(ip);
        }
        throttle.onSuccess(ip);
        for (int i = 0; i < AuthThrottle.FAILURES_BEFORE_BAN - 1; i++) {
            failOnce(ip);
        }
        Assertions.assertNotNull(throttle.tryBegin(ip), "认证成功必须重置失败计数，未达阈值不得冷却");
    }
}

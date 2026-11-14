package com.kairo.vistrel;

import com.kairo.vistrel.VistrelService.RestartGate;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * 控制面用例：自愈冷却窗口内的放行与拒绝、「从未放行记为 0」的约定，
 * 以及移交的动作确实在后台队列线程执行
 */
class RestartGateTest {
    @Test
    void testTryRestartRespectsCooldownWindow() {
        long[] now = {1_000L};
        RestartGate plane = new RestartGate(30L, () -> now[0]);
        Assertions.assertTrue(plane.tryRestart(), "从未放行：首触发放行");
        Assertions.assertFalse(plane.tryRestart(), "冷却期内拒绝");
        now[0] += 29L;
        Assertions.assertFalse(plane.tryRestart(), "差 1ns 满冷却期仍拒绝");
        now[0] += 1L;
        Assertions.assertTrue(plane.tryRestart(), "满冷却期再次放行并刷新时刻");
        now[0] += 29L;
        Assertions.assertFalse(plane.tryRestart(), "放行时刻按上次刷新重算冷却");
    }

    @Test
    void testFirstFireAtZeroClockNotBlockedBySentinel() {
        long[] now = {0L};
        RestartGate plane = new RestartGate(30L, () -> now[0]);
        Assertions.assertTrue(plane.tryRestart(), "时钟从 0 起步：不得把首次误判为冷却中");
    }

    @Test
    void testSubmitRunsActionOffCallerThread() throws Exception {
        RestartGate plane = new RestartGate(30L, () -> 0L);
        CountDownLatch ran = new CountDownLatch(1);
        Assertions.assertDoesNotThrow(() -> plane.submit(ran::countDown));
        Assertions.assertTrue(ran.await(5, TimeUnit.SECONDS), "移交的动作必须在后台执行到位");
    }
}

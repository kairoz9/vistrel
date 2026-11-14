package com.kairo.vistrel.capture;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * ReportGate 用例：一次性形态的首次放行与复位，窗口形态的
 * 首笔即期满、未到期不放行、到期合并放行、空窗不记日志
 */
class ReportGateTest {
    @Test
    void testOnceFiresExactlyOnceUntilReset() {
        ReportGate gate = ReportGate.once();
        Assertions.assertTrue(gate.tryFire(), "首次放行");
        Assertions.assertFalse(gate.tryFire(), "闩锁期内不得再放行");
        Assertions.assertTrue(gate.isFired());
        gate.reset();
        Assertions.assertFalse(gate.isFired(), "解闩后恢复未触发态");
        Assertions.assertTrue(gate.tryFire(), "解闩后可再次放行");
    }

    @Test
    void testWindowedFirstWindowCountsFromZero() {
        ReportGate gate = ReportGate.windowed(100L);
        gate.accumulate(2L);
        Assertions.assertEquals(2L, gate.drainIfDue(0L), "首窗从 0 起算：首笔即期满，不等第一个完整窗口");
    }

    @Test
    void testWindowedHoldsUntilDueThenDrainsMergedCount() {
        ReportGate gate = ReportGate.windowed(100L);
        // 首窗立即放行一次，从此窗口按放行时刻滚动
        gate.accumulate(3L);
        Assertions.assertEquals(3L, gate.drainIfDue(100L), "首笔即期满");
        gate.accumulate(1L);
        Assertions.assertEquals(-1L, gate.drainIfDue(150L), "次窗未期满不放行（150 < 200）");
        Assertions.assertEquals(-1L, gate.drainIfDue(199L), "差 1ns 仍不放行");
        gate.accumulate(2L);
        Assertions.assertEquals(3L, gate.drainIfDue(200L), "期满放行并合并本窗多次累计");
        Assertions.assertEquals(-1L, gate.drainIfDue(200L), "同时刻二次查询：新窗（至 300）未期满");
        Assertions.assertEquals(0L, gate.drainIfDue(300L), "空窗期满返回 0，调用方免日志");
    }
}

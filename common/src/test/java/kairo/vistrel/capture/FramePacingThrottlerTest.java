package kairo.vistrel.capture;

import java.util.concurrent.atomic.AtomicLong;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

public class FramePacingThrottlerTest {
    /**
     * 可编程虚拟时钟：每个用例独立实例，逐帧显式推进，边界值精确可钉
     */
    private final AtomicLong now = new AtomicLong(0L);

    private FramePacingThrottler newThrottler(int fps) {
        return new FramePacingThrottler(fps, now::get);
    }

    @Test
    void testThrottlingAtHighFramerate() {
        // 60 FPS 配置 (周期 ~16.66ms = 16_666_666 纳秒)
        FramePacingThrottler throttler = newThrottler(60);

        long startNanos = 1_000_000_000L;
        now.set(startNanos);
        // 首帧必须被放行捕获
        Assertions.assertTrue(throttler.shouldCapture());
        // 模拟 200 FPS 高频渲染（5ms 后来帧）：应当被节拍器拦截
        now.set(startNanos + 5_000_000L);
        Assertions.assertFalse(throttler.shouldCapture());
        // 10ms 后来帧：依然在 16.6ms 周期内，继续拦截
        now.set(startNanos + 10_000_000L);
        Assertions.assertFalse(throttler.shouldCapture());
        // 16.7ms 后来帧：已到节拍点，放行捕获
        now.set(startNanos + 16_700_000L);
        Assertions.assertTrue(throttler.shouldCapture());
    }

    @Test
    void testVariableFramerateLowFpsNeverStalled() {
        // 游戏掉帧至 20 FPS（50ms 渲染周期）时，每帧都落在已过期的节拍点上，
        // 故每渲染一帧即时放行，不引入额外丢帧
        FramePacingThrottler throttler = newThrottler(60);

        now.set(1_000_000_000L);
        Assertions.assertTrue(throttler.shouldCapture());
        // 50ms 后来帧 (20 FPS)
        now.set(1_050_000_000L);
        Assertions.assertTrue(throttler.shouldCapture());

        now.set(1_100_000_000L);
        Assertions.assertTrue(throttler.shouldCapture());
    }

    @Test
    void testNonMultipleRenderRateReachesFullProfileRate() {
        // 节拍制：R=90fps 渲染（11.1ms 周期）配 60fps 档。失效模式：按帧边界量化
        // 在 R 非 60 整数倍时只能产出 R/⌈R/60⌉（90fps→45fps）；节拍制必须产满 60fps
        FramePacingThrottler throttler = newThrottler(60);
        // 90fps 渲染周期
        long framePeriod = 11_111_111L;
        long t0 = 1_000_000_000L;
        int captures = 0;
        // 模拟 2 秒渲染循环（180 帧）
        for (long i = 0; i < 180; i++) {
            now.set(t0 + i * framePeriod);
            if (throttler.shouldCapture()) {
                captures++;
            }
        }
        // 2 秒 @60fps = 120 次放行（边界取整抖动容差 ±3）
        Assertions.assertTrue(Math.abs(captures - 120) <= 3, "2 秒实际放行 " + captures + " 次，未产满 60fps");
    }

    @Test
    void testPostStallRealignmentWithoutCatchUpBurst() {
        // 渲染停滞 300ms 后恢复：恢复首帧立即放行并把节拍重锚到 due = now + interval，
        // 随后紧邻帧必须被拦——不得追赶连发（停滞期帧已不可补，追发即重复劳动）
        FramePacingThrottler throttler = newThrottler(60);
        now.set(1_000_000_000L);
        Assertions.assertTrue(throttler.shouldCapture());

        now.set(1_300_000_000L);
        Assertions.assertTrue(throttler.shouldCapture());
        // 恢复后 5ms / 10ms 的高刷帧必须拦截（对齐后的节拍未到）
        now.set(1_305_000_000L);
        Assertions.assertFalse(throttler.shouldCapture());
        now.set(1_310_000_000L);
        Assertions.assertFalse(throttler.shouldCapture());
        // 一个节拍周期后恢复常规放行
        now.set(1_316_666_667L);
        Assertions.assertTrue(throttler.shouldCapture());
    }

    @Test
    void testSetFpsReanchorsBeat() {
        // 运行时切换帧率必须重锚节拍
        FramePacingThrottler throttler = newThrottler(60);
        now.set(1_000_000_000L);
        Assertions.assertTrue(throttler.shouldCapture());

        throttler.setFps(30);
        Assertions.assertEquals(30, throttler.getFps());
        now.set(1_020_000_000L);
        Assertions.assertFalse(throttler.shouldCapture());
        now.set(1_034_000_000L);
        Assertions.assertTrue(throttler.shouldCapture());
    }
}

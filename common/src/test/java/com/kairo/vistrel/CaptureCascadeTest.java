package com.kairo.vistrel;

import com.kairo.vistrel.capture.FramePacingThrottler;
import com.kairo.vistrel.capture.VideoCaptureScheduler;
import com.kairo.vistrel.capture.VideoPipeline;
import com.kairo.vistrel.streaming.EncoderChannel;
import com.kairo.vistrel.streaming.SessionServer.SessionState;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;
import java.util.function.LongSupplier;

/**
 * 经 VideoCaptureScheduler 驱动的正交双管道完整级联与断路器测试
 * 显存直通优先；构造期失败立即降级，运行期需**连续 30 帧** {@code Failed} 才断路，
 * 主动丢帧（{@code Dropped}）永不计入
 */
public class CaptureCascadeTest {
    private static final long FRAME_INTERVAL_NANOS = 1_000_000_000L / 60;

    /**
     * 可编排的 VideoPipeline 替身
     */
    private static final class FakePipeline implements VideoPipeline {
        int captureCalls = 0;
        int closeCalls = 0;
        /**
         * 采集结果：默认投递成功，可按用例改写为 Dropped / Failed
         */
        VideoPipeline.CaptureOutcome outcome = VideoPipeline.CaptureOutcome.Delivered;
        RuntimeException runtimeException = null;
        final int width;
        final int height;
        final Runnable onCapture;

        FakePipeline(int width, int height, boolean shouldSucceed, Runnable onCapture) {
            this.width = width;
            this.height = height;
            this.outcome = shouldSucceed ? VideoPipeline.CaptureOutcome.Delivered : VideoPipeline.CaptureOutcome.Failed;
            this.onCapture = onCapture;
        }

        @Override
        public VideoPipeline.CaptureOutcome capture() {
            captureCalls++;
            if (runtimeException != null) {
                throw runtimeException;
            }
            if (onCapture != null) {
                onCapture.run();
            }
            return outcome;
        }

        @Override
        public void close() {
            closeCalls++;
        }
    }

    /**
     * 可编排的推流通道替身
     */
    private static final class FakeEncoderChannel implements EncoderChannel {
        boolean streaming = true;
        int width = 1920;
        int height = 1080;
        int fps = 60;
        int pushes = 0;
        int vramPushes = 0;
        int lastVramSlotIdx = -1;
        long lastVramSignalVal = -1L;

        @Override
        public void pushSystemFrame(ByteBuffer buffer) {
            pushes++;
        }

        /**
         * 记录推流帧的槽位租约替身：真实管线由 importVramSlots 取得并独占 push
         */
        final EncoderChannel.SlotLease lease = new EncoderChannel.SlotLease() {
            @Override
            public void push(int slotIdx, long signalVal) {
                vramPushes++;
                lastVramSlotIdx = slotIdx;
                lastVramSignalVal = signalVal;
            }

            @Override
            public void close() {}
        };

        @Override
        public EncoderChannel.SlotLease importVramSlots(long[] slots) {
            return lease;
        }
    }

    /**
     * 可控时钟替身
     */
    private static final class FakeClock implements LongSupplier {
        long nanos = 0;

        @Override
        public long getAsLong() {
            return nanos;
        }

        void advanceFrame() {
            nanos += FRAME_INTERVAL_NANOS;
        }
    }

    private static VideoCaptureScheduler scheduler(
            FakePipeline vram,
            FakePipeline ram,
            FakeEncoderChannel channel,
            FakeClock clock,
            boolean vramDirectEnabled
    ) {
        VideoCaptureScheduler sched =
                new VideoCaptureScheduler(channel, vramDirectEnabled, new FramePacingThrottler(60, clock)) {
            @Override
            protected VideoPipeline createVramPipeline(int width, int height) {
                if (vram == null) {
                    throw new IllegalStateException("测试：VRAM 直通构造期不支持");
                }
                return vram;
            }

            @Override
            protected VideoPipeline createRamPipeline(int width, int height) {
                return ram;
            }
        };
        if (channel.streaming) {
            sched.onSessionChanged(
                    new SessionState(
                            SessionState.Phase.STREAMING,
                            channel.width,
                            channel.height,
                            channel.fps,
                            0.1,
                            "H264",
                            "unknown"
                    )
            );
        }
        return sched;
    }

    @Test
    void testVramDirectAcceptedSkipsRamReadback() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        FakePipeline vram = new FakePipeline(1920, 1080, true, () -> channel.lease.push(0, 1L));
        FakePipeline ram = new FakePipeline(1920, 1080, true, null);

        Assertions.assertTrue(scheduler(vram, ram, channel, new FakeClock(), true).capture(), "显存直通被接受时本帧应视为已投递");
        Assertions.assertEquals(1, vram.captureCalls, "应当优先调度 Level 1 显存直通路径");
        Assertions.assertEquals(1, channel.vramPushes, "应当经槽位租约投递显存帧");
        Assertions.assertEquals(0, channel.lastVramSlotIdx);
        Assertions.assertEquals(1L, channel.lastVramSignalVal);
        Assertions.assertEquals(0, ram.captureCalls, "显存直通成功时不应当触发 Level 2 RAM 回读");
        Assertions.assertEquals(0, channel.pushes, "显存直通成功时不应当走系统内存投递");
    }

    @Test
    void testGlVramPushAcceptedSkipsRamReadback() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        // GL 形状帧无时间线信号量，signalVal 恒为 0（协议合法值，见 NativeBridge.pushVramSlot）
        FakePipeline vram = new FakePipeline(1920, 1080, true, () -> channel.lease.push(2, 0L));
        FakePipeline ram = new FakePipeline(1920, 1080, true, null);

        Assertions.assertTrue(
                scheduler(vram, ram, channel, new FakeClock(), true).capture(),
                "OpenGL 两阶段 VRAM 帧被接受时本帧应视为已投递"
        );
        Assertions.assertEquals(1, vram.captureCalls, "应当优先调度 Level 1 显存直通路径");
        Assertions.assertEquals(1, channel.vramPushes, "应当经槽位租约投递显存帧");
        Assertions.assertEquals(2, channel.lastVramSlotIdx);
        Assertions.assertEquals(0L, channel.lastVramSignalVal);
        Assertions.assertEquals(0, ram.captureCalls, "显存直通成功时不应当触发 Level 2 RAM 回读");
        Assertions.assertEquals(0, channel.pushes, "显存直通成功时不应当走系统内存投递");
    }

    @Test
    void testVramConstructionFailureDemotesPermanentlyToRam() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        ByteBuffer ramBuf = ByteBuffer.allocateDirect(16);
        FakePipeline ram = new FakePipeline(1920, 1080, true, () -> channel.pushSystemFrame(ramBuf));
        FakeClock clock = new FakeClock();
        // vram 传入 null，会触发 createVramPipeline 抛出异常
        VideoCaptureScheduler capture = scheduler(null, ram, channel, clock, true);

        Assertions.assertTrue(capture.capture(), "显存直通构造期失败时应直接由 RAM 回读投递");
        Assertions.assertEquals(1, ram.captureCalls);
        Assertions.assertEquals(1, channel.pushes);
        Assertions.assertEquals(0, channel.vramPushes);

        for (int i = 0; i < 3; i++) {
            clock.advanceFrame();
            Assertions.assertTrue(capture.capture());
        }

        Assertions.assertEquals(4, ram.captureCalls, "降级后每帧都应走 RAM 回读");
        Assertions.assertEquals(4, channel.pushes);
        Assertions.assertEquals(0, channel.vramPushes, "降级后不得再触发 VRAM 投递");
    }

    /**
     * 主动丢帧（槽位全在途）绝不能触发断路器
     *
     * 这是 1440p 切帧率时「永久降级到 RAM 回读」的回归用例：槽位压力是设计内的背压，
     * 若被当成故障，一次丢帧就会把整个会话推上最贵的回读路径。
     */
    @Test
    void testVramDroppedNeverTriggersCircuitBreaker() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        FakePipeline vram = new FakePipeline(1920, 1080, true, null);
        vram.outcome = VideoPipeline.CaptureOutcome.Dropped;
        FakePipeline ram = new FakePipeline(1920, 1080, true, null);
        FakeClock clock = new FakeClock();

        VideoCaptureScheduler capture = scheduler(vram, ram, channel, clock, true);

        for (int i = 0; i < 100; i++) {
            clock.advanceFrame();
            Assertions.assertFalse(capture.capture(), "主动丢帧不产生投递");
        }

        Assertions.assertEquals(100, vram.captureCalls, "管线健康，每帧仍应交给显存直通");
        Assertions.assertEquals(0, vram.closeCalls, "主动丢帧无论多少次都不得降级");
        Assertions.assertEquals(0, ram.captureCalls, "主动丢帧不回落 RAM——回读正是要避免的路径");
    }

    /**
     * 真正的故障要连续 N 帧才降级，单帧失败只回落 RAM 投递
     */
    @Test
    void testVramRuntimeFailureDemotesOnlyAfterConsecutiveThreshold() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        // 运行期恒返回 Failed
        FakePipeline vram = new FakePipeline(1920, 1080, false, null);
        ByteBuffer ramBuf = ByteBuffer.allocateDirect(16);
        FakePipeline ram = new FakePipeline(1920, 1080, true, () -> channel.pushSystemFrame(ramBuf));
        FakeClock clock = new FakeClock();

        VideoCaptureScheduler capture = scheduler(vram, ram, channel, clock, true);
        // 首帧失败：本帧回落 RAM 投递，但断路器不动
        Assertions.assertTrue(capture.capture(), "单帧失败应回落 RAM 并投递成功");
        Assertions.assertEquals(1, vram.captureCalls);
        Assertions.assertEquals(0, vram.closeCalls, "单帧失败不得触发降级");
        Assertions.assertEquals(1, ram.captureCalls);
        // 连续失败累计到阈值才降级
        for (int i = 1; i < 30; i++) {
            clock.advanceFrame();
            capture.capture();
        }
        Assertions.assertEquals(30, vram.captureCalls);
        Assertions.assertEquals(1, vram.closeCalls, "连续 30 帧失败后才降级并释放显存资源");
        Assertions.assertEquals(30, ram.captureCalls);
        // 降级后永久走 RAM，不再触碰 VRAM
        for (int i = 0; i < 3; i++) {
            clock.advanceFrame();
            Assertions.assertTrue(capture.capture());
        }
        Assertions.assertEquals(30, vram.captureCalls, "降级后不得再调用 VRAM");
        Assertions.assertEquals(33, ram.captureCalls);
        Assertions.assertEquals(33, channel.pushes);
    }

    @Test
    void testVramDirectNeverAttemptedWhenDisabled() {
        FakeEncoderChannel channelOff = new FakeEncoderChannel();
        FakePipeline vramOff = new FakePipeline(1920, 1080, true, null);
        FakePipeline ramOff = new FakePipeline(1920, 1080, true, null);
        // 开关关闭
        scheduler(vramOff, ramOff, channelOff, new FakeClock(), false).capture();
        Assertions.assertEquals(0, vramOff.captureCalls, "开关关闭时不得触碰显存直通路径");
        Assertions.assertEquals(1, ramOff.captureCalls);
    }

    @Test
    void testVramCapabilityEvaluatedOnSessionChangeNotConstruction() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        FakeClock clock = new FakeClock();
        boolean[] capsReady = {false};
        int[] evaluations = {0};
        List<FakePipeline> createdVrams = new ArrayList<>();
        List<FakePipeline> createdRams = new ArrayList<>();

        VideoCaptureScheduler capture =
                new VideoCaptureScheduler(channel, () -> {
            evaluations[0]++;
            return capsReady[0];
        }, new FramePacingThrottler(60, clock)) {
            @Override
            protected VideoPipeline createVramPipeline(int width, int height) {
                FakePipeline p = new FakePipeline(width, height, true, null);
                createdVrams.add(p);
                return p;
            }

            @Override
            protected VideoPipeline createRamPipeline(int width, int height) {
                FakePipeline p = new FakePipeline(width, height, true, null);
                createdRams.add(p);
                return p;
            }
        };
        Assertions.assertEquals(0, evaluations[0], "显存直通能力不得在构造期求值");

        SessionState session = new SessionState(SessionState.Phase.STREAMING, 1920, 1080, 60, 0.1, "H264", "unknown");
        capture.onSessionChanged(session);
        Assertions.assertEquals(1, evaluations[0], "首次会话变更时求值一次");
        Assertions.assertEquals(0, createdVrams.size(), "能力未就绪时不得尝试显存管线");
        Assertions.assertEquals(1, createdRams.size(), "应保留 RAM 兜底管线");

        capsReady[0] = true;
        capture.onSessionChanged(session);
        Assertions.assertEquals(2, evaluations[0], "后续会话变更必须重新求值");
        Assertions.assertEquals(1, createdVrams.size(), "能力就绪后必须启用显存直通");
        Assertions.assertEquals(1, createdRams.get(0).closeCalls, "旧 RAM 管线应在重建时释放");
        Assertions.assertEquals(2, createdRams.size(), "启用显存直通后仍保留 RAM 兜底");

        Assertions.assertTrue(capture.capture(), "显存管线启用后应投递");
        Assertions.assertEquals(1, createdVrams.get(0).captureCalls);
        Assertions.assertEquals(0, createdRams.get(1).captureCalls);
    }

    @Test
    void testNoSessionSkipsEntireCapture() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        channel.streaming = false;
        FakePipeline vram = new FakePipeline(1920, 1080, true, null);
        FakePipeline ram = new FakePipeline(1920, 1080, true, null);

        Assertions.assertFalse(scheduler(vram, ram, channel, new FakeClock(), true).capture(), "无推流会话时不应投递");
        Assertions.assertEquals(0, vram.captureCalls);
        Assertions.assertEquals(0, ram.captureCalls);
    }

    @Test
    void testThrottleDropsFramesWithinInterval() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        FakePipeline vram = new FakePipeline(1920, 1080, true, null);
        FakePipeline ram = new FakePipeline(1920, 1080, true, null);
        FakeClock clock = new FakeClock();

        VideoCaptureScheduler capture = scheduler(vram, ram, channel, clock, true);

        Assertions.assertTrue(capture.capture(), "首帧应当投递");
        Assertions.assertFalse(capture.capture(), "同一时刻的第二帧应被帧节拍拦截");
        Assertions.assertEquals(1, vram.captureCalls, "被拦截的帧不得触碰管道");

        clock.advanceFrame();
        Assertions.assertTrue(capture.capture(), "跨过一个帧周期后应恢复采集");
        Assertions.assertEquals(2, vram.captureCalls);
    }

    @Test
    void testProfileDynamicallyUpdatedInstantly() {
        List<FakePipeline> createdVrams = new ArrayList<>();
        List<FakePipeline> createdRams = new ArrayList<>();
        FakeEncoderChannel channel = new FakeEncoderChannel();
        FakeClock clock = new FakeClock();

        VideoCaptureScheduler capture =
                new VideoCaptureScheduler(channel, true, new FramePacingThrottler(60, clock)) {
            @Override
            protected VideoPipeline createVramPipeline(int width, int height) {
                FakePipeline p = new FakePipeline(width, height, true, null);
                createdVrams.add(p);
                return p;
            }

            @Override
            protected VideoPipeline createRamPipeline(int width, int height) {
                FakePipeline p = new FakePipeline(width, height, true, null);
                createdRams.add(p);
                return p;
            }
        };

        capture.onSessionChanged(new SessionState(SessionState.Phase.STREAMING, 1920, 1080, 60, 0.1, "H264", "unknown"));
        Assertions.assertEquals(1, createdVrams.size());
        Assertions.assertEquals(1, createdRams.size());
        Assertions.assertEquals(1920, createdVrams.get(0).width);
        Assertions.assertEquals(1080, createdVrams.get(0).height);
        // 首帧采集（1080p）
        capture.capture();
        Assertions.assertEquals(1, createdVrams.get(0).captureCalls);
        // 控制面通知变更推送（720p@30fps，Push 驱动即时生效）
        capture.onSessionChanged(new SessionState(SessionState.Phase.STREAMING, 1280, 720, 30, 0.1, "H264", "unknown"));
        Assertions.assertEquals(2, createdVrams.size(), "会话规格变更由推送驱动直接构造新规格 Pipeline 替换");
        Assertions.assertEquals(1, createdVrams.get(0).closeCalls, "旧 Vram Pipeline 立即被清理释放");
        Assertions.assertEquals(1, createdRams.get(0).closeCalls, "旧 Ram Pipeline 立即被清理释放");
        Assertions.assertEquals(1280, createdVrams.get(1).width);
        Assertions.assertEquals(720, createdVrams.get(1).height);
        // 推进到下一个 30fps 节拍（约 33.3ms）
        clock.nanos += 35_000_000L;
        capture.capture();
        Assertions.assertEquals(1, createdVrams.get(1).captureCalls);
    }

    /**
     * 帧率变化不得重建管线
     *
     * 帧率不改变纹理尺寸，重建会走 `waitIdle` + 两次阻塞 JNI（注销/注册显存槽位）+ 纹理重分配，
     * 那正是切帧率时偶发卡死的窗口。
     */
    @Test
    void testFpsOnlyChangeReusesPipelines() {
        List<FakePipeline> createdVrams = new ArrayList<>();
        List<FakePipeline> createdRams = new ArrayList<>();
        FakeEncoderChannel channel = new FakeEncoderChannel();

        VideoCaptureScheduler capture =
                new VideoCaptureScheduler(channel, true, new FramePacingThrottler(60, new FakeClock())) {
            @Override
            protected VideoPipeline createVramPipeline(int width, int height) {
                FakePipeline p = new FakePipeline(width, height, true, null);
                createdVrams.add(p);
                return p;
            }

            @Override
            protected VideoPipeline createRamPipeline(int width, int height) {
                FakePipeline p = new FakePipeline(width, height, true, null);
                createdRams.add(p);
                return p;
            }
        };

        capture.onSessionChanged(new SessionState(SessionState.Phase.STREAMING, 1920, 1080, 60, 0.1, "H264", "unknown"));
        Assertions.assertEquals(1, createdVrams.size());
        Assertions.assertEquals(1, createdRams.size());
        // 仅帧率变化：复用现成管线，不销毁也不重建
        capture.onSessionChanged(
                new SessionState(SessionState.Phase.STREAMING, 1920, 1080, 240, 0.1, "H264", "unknown")
        );
        Assertions.assertEquals(1, createdVrams.size(), "帧率变化不应重建显存管线");
        Assertions.assertEquals(1, createdRams.size(), "帧率变化不应重建内存管线");
        Assertions.assertEquals(0, createdVrams.get(0).closeCalls, "复用期间不得销毁管线");
        // 分辨率变化仍必须重建
        capture.onSessionChanged(
                new SessionState(SessionState.Phase.STREAMING, 2560, 1440, 240, 0.1, "H264", "unknown")
        );
        Assertions.assertEquals(2, createdVrams.size(), "分辨率变化必须重建显存管线");
        Assertions.assertEquals(1, createdVrams.get(0).closeCalls, "旧管线在重建时释放");
    }

    @Test
    void testSessionLifecycleReplacesPipelinesOnSessionChange() {
        List<FakePipeline> createdVrams = new ArrayList<>();
        List<FakePipeline> createdRams = new ArrayList<>();
        FakeEncoderChannel channel = new FakeEncoderChannel();

        VideoCaptureScheduler mgr =
                new VideoCaptureScheduler(channel, true, new FramePacingThrottler(60, new FakeClock())) {
            @Override
            protected VideoPipeline createVramPipeline(int width, int height) {
                FakePipeline p = new FakePipeline(width, height, true, null);
                createdVrams.add(p);
                return p;
            }

            @Override
            protected VideoPipeline createRamPipeline(int width, int height) {
                FakePipeline p = new FakePipeline(width, height, true, null);
                createdRams.add(p);
                return p;
            }
        };
        // 未连接时无 pipeline
        Assertions.assertFalse(mgr.capture(), "无会话时 capture 返回 false");
        Assertions.assertEquals(0, createdVrams.size());
        // 会话建立 (1080p)
        mgr.onSessionChanged(new SessionState(SessionState.Phase.STREAMING, 1920, 1080, 60, 0.1, "H264", "unknown"));
        Assertions.assertEquals(1, createdVrams.size());
        Assertions.assertEquals(1, createdRams.size());
        FakePipeline firstVram = createdVrams.get(0);
        Assertions.assertEquals(0, firstVram.closeCalls);
        // 会话断开：立即清理释放
        mgr.onSessionChanged(SessionState.INACTIVE);
        Assertions.assertEquals(1, firstVram.closeCalls, "会话断开时 Pipeline 必须被清理");
        Assertions.assertFalse(mgr.capture(), "会话断开后 capture 应当返回 false");
    }

    /**
     * RAM 回读是最后一级，异常绝不能逃逸到渲染线程
     *
     * 回归陷阱：两级捕获缺一不可——任何一级漏包 try/catch，RAM 管线内部的
     * Throwable 都会掀翻主渲染循环（本用例专门钉住 Level 2）。
     */
    @Test
    void testRamExceptionNeverEscapesToRenderThread() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        FakePipeline ram = new FakePipeline(1920, 1080, true, null);
        ram.runtimeException = new IllegalStateException("测试：RAM 回读内部异常");
        FakeClock clock = new FakeClock();
        // 显存直通关闭 → 只剩 RAM 一级
        VideoCaptureScheduler capture = scheduler(null, ram, channel, clock, false);

        boolean delivered = Assertions.assertDoesNotThrow(capture::capture, "RAM 采集异常必须被调度器吞下并折算为 Failed");
        Assertions.assertFalse(delivered, "异常帧不得被当作已投递");

        for (int i = 0; i < 30; i++) {
            clock.advanceFrame();
            Assertions.assertDoesNotThrow(capture::capture);
        }

        Assertions.assertEquals(31, ram.captureCalls, "RAM 没有下一级可降级，熔断后仍必须继续采集");
        Assertions.assertEquals(0, ram.closeCalls, "RAM 管线不得因熔断被销毁");
    }

    /**
     * RAM 连续返回 Failed 也不得关闭管线
     *
     * VRAM 熔断后尚有 RAM 兜底，RAM 熔断后无路可走：关闭它等于把会话打成黑屏，
     * 而黑屏比「继续尝试」更糟 —— 故障必须可见，但会话不能自杀。
     */
    @Test
    void testRamFailureNeverClosesTheOnlyRemainingPipeline() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        // 恒返回 Failed
        FakePipeline ram = new FakePipeline(1920, 1080, false, null);
        FakeClock clock = new FakeClock();

        VideoCaptureScheduler capture = scheduler(null, ram, channel, clock, false);

        for (int i = 0; i < 40; i++) {
            if (i > 0) {
                clock.advanceFrame();
            }
            Assertions.assertFalse(capture.capture(), "失败的帧不产生投递");
        }

        Assertions.assertEquals(40, ram.captureCalls, "越过阈值后每一帧仍应交给 RAM 回读");
        Assertions.assertEquals(0, ram.closeCalls, "最后一根管线不得被断路动作关闭");
    }

    /**
     * 显存直通抛异常等同于返回 Failed：折算后仍要按连续帧数熔断
     */
    @Test
    void testVramExceptionCountsAsFailureAndEventuallyDemotes() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        ByteBuffer ramBuf = ByteBuffer.allocateDirect(16);
        FakePipeline vram = new FakePipeline(1920, 1080, true, null);
        vram.runtimeException = new IllegalStateException("测试：显存直通内部异常");
        FakePipeline ram = new FakePipeline(1920, 1080, true, () -> channel.pushSystemFrame(ramBuf));
        FakeClock clock = new FakeClock();

        VideoCaptureScheduler capture = scheduler(vram, ram, channel, clock, true);

        for (int i = 0; i < 30; i++) {
            if (i > 0) {
                clock.advanceFrame();
            }
            Assertions.assertDoesNotThrow(capture::capture, "显存直通异常必须折算为 Failed，不得逃逸");
        }

        Assertions.assertEquals(30, vram.captureCalls);
        Assertions.assertEquals(1, vram.closeCalls, "连续 30 帧异常应与连续 30 帧 Failed 一致地熔断");
        Assertions.assertEquals(30, ram.captureCalls, "熔断前后每帧都应回落 RAM 投递");

        clock.advanceFrame();
        Assertions.assertTrue(capture.capture());
        Assertions.assertEquals(30, vram.captureCalls, "熔断后不得再触碰显存直通");
    }

    /**
     * 一次成功必须清零连续失败数
     *
     * 否则「29 失败 + 1 成功 + 29 失败」会被算成连续 58 次而误熔断，
     * 把本来健康的显存直通永久推上最贵的回读路径。
     */
    @Test
    void testDeliveredResetsFailureStreak() {
        FakeEncoderChannel channel = new FakeEncoderChannel();
        ByteBuffer ramBuf = ByteBuffer.allocateDirect(16);
        FakePipeline vram = new FakePipeline(1920, 1080, true, null);
        FakePipeline ram = new FakePipeline(1920, 1080, true, () -> channel.pushSystemFrame(ramBuf));
        FakeClock clock = new FakeClock();

        VideoCaptureScheduler capture = scheduler(vram, ram, channel, clock, true);

        vram.outcome = VideoPipeline.CaptureOutcome.Failed;
        for (int i = 0; i < 29; i++) {
            if (i > 0) {
                clock.advanceFrame();
            }
            capture.capture();
        }

        vram.outcome = VideoPipeline.CaptureOutcome.Delivered;
        clock.advanceFrame();
        Assertions.assertTrue(capture.capture(), "成功的这一帧应由显存直通投递");

        vram.outcome = VideoPipeline.CaptureOutcome.Failed;
        for (int i = 0; i < 29; i++) {
            clock.advanceFrame();
            capture.capture();
        }

        Assertions.assertEquals(0, vram.closeCalls, "一次成功即清零连续失败数，累计 58 帧失败不得熔断");
    }

    //? if vulkan_backend {
    @Test
    void testRenderpearlVramPipelineSafety() {
        Assertions.assertThrows(
                IllegalArgumentException.class,
                () -> new com.kairo.vistrel.capture.RenderpearlVramPipeline(null, 1920, 1080, () -> {}),
                "空 Channel 创建管线应抛出异常"
        );
        Assertions.assertThrows(
                IllegalArgumentException.class,
                () -> new com.kairo.vistrel.capture.RenderpearlVramPipeline(null, 1920, 1080, null),
                "空停滞回调创建管线应抛出异常"
        );
    }
    //? }
}

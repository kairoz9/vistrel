package com.kairo.vistrel.capture;

import com.kairo.vistrel.capture.AudioCaptureScheduler.LoopbackBinding;
import com.kairo.vistrel.streaming.EncoderChannel;
import com.kairo.vistrel.streaming.SessionServer.SessionEvent;
import com.kairo.vistrel.streaming.SessionServer.Tier;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import java.nio.ByteBuffer;

/**
 * AudioCaptureScheduler 单元测试 (基于 EncoderChannel 替身，0 JNI 依赖)
 */
public class AudioCaptureSchedulerTest {
    private static final class TestEncoderChannel implements EncoderChannel {
        int audioPushes = 0;
        int lastSamples = 0;

        @Override
        public void pushSystemFrame(ByteBuffer buffer) {}

        @Override
        public void pushAudioPcm(ByteBuffer buffer, int samplesPerChannel) {
            audioPushes++;
            lastSamples = samplesPerChannel;
        }
    }

    @BeforeEach
    @AfterEach
    void resetBinding() {
        // 绑定正本是进程级静态：测试间按 MC 销毁音频库的语义复位
        LoopbackBinding.clear();
    }

    @Test
    void testBindingPairSwapSemantics() {
        LoopbackBinding.open(0xAABB0011L);
        Assertions.assertTrue(LoopbackBinding.current().isBound());
        Assertions.assertFalse(LoopbackBinding.current().contextReady(), "新开设备的 context 必然未就绪");
        LoopbackBinding.markContextReady();
        Assertions.assertTrue(LoopbackBinding.current().contextReady());
        Assertions.assertEquals(0xAABB0011L, LoopbackBinding.current().device(), "就绪位翻转不得丢句柄");
        LoopbackBinding.open(0xCCDD0022L);
        Assertions.assertFalse(LoopbackBinding.current().contextReady(), "换设备即换 context，就绪位不得残留");
        LoopbackBinding.clear();
        Assertions.assertFalse(LoopbackBinding.current().isBound());
        Assertions.assertFalse(LoopbackBinding.current().contextReady());
    }

    @Test
    void testAudioCaptureSchedulerSessionBindingAndLifecycle() {
        TestEncoderChannel channel = new TestEncoderChannel();
        AudioCaptureScheduler scheduler = new AudioCaptureScheduler(channel, (dev, buf, samples) -> {});

        Assertions.assertFalse(LoopbackBinding.current().isBound());
        // 模拟外部控制面通知会话接入（采集线程在首个接入会话时启动）
        scheduler.onSessionEvent(new SessionEvent.Established("nvenc", new Tier(1920, 1080, 60, 0.1)));
        // 测试模拟绑定环回设备 (句柄 0x12345678L)
        LoopbackBinding.open(0x12345678L);
        Assertions.assertTrue(LoopbackBinding.current().isBound());
        // 测试终止与清理：调度器销毁不触碰绑定正本（那是 MC 音频库的资产）
        scheduler.cleanup();
        Assertions.assertTrue(LoopbackBinding.current().isBound(), "调度器销毁不得清空绑定正本");
    }

    @Test
    void testSynchronousCleanupJoinsThread() throws Exception {
        TestEncoderChannel channel = new TestEncoderChannel();
        AudioCaptureScheduler scheduler = new AudioCaptureScheduler(channel, (dev, buf, samples) -> {});
        scheduler.onSessionEvent(new SessionEvent.Established("nvenc", new Tier(1920, 1080, 60, 0.1)));

        LoopbackBinding.open(0x11223344L);
        Assertions.assertTrue(LoopbackBinding.current().isBound());

        java.lang.reflect.Field field = AudioCaptureScheduler.class.getDeclaredField("captureThread");
        field.setAccessible(true);
        Thread workerThread = (Thread) field.get(scheduler);
        Assertions.assertNotNull(workerThread, "启动后采集守护线程不应为空");
        Assertions.assertTrue(workerThread.isAlive(), "采集守护线程应处于运行状态");
        // 执行同步清理
        scheduler.cleanup();
        // 核心不变量断言：cleanup 返回后线程必须已被 join 终结
        Assertions.assertFalse(workerThread.isAlive(), "cleanup 返回时采集线程必须已彻底终止");
    }

    @Test
    void testContextReadyGateAndSafeBufferLimit() throws Exception {
        TestEncoderChannel channel = new TestEncoderChannel();
        java.util.concurrent.atomic.AtomicInteger renderCount = new java.util.concurrent.atomic.AtomicInteger(0);

        AudioCaptureScheduler scheduler = new AudioCaptureScheduler(channel, (dev, buf, samples) -> {
            renderCount.incrementAndGet();
        });
        // 仅开启推流和绑定设备，此时 context 未就绪
        scheduler.onSessionEvent(new SessionEvent.Established("nvenc", new Tier(1920, 1080, 60, 0.1)));
        LoopbackBinding.open(0x99887766L);
        // 等待 60ms (3 个时钟周期)，断言在 context 未 ready 时绝不能有任何渲染与投递
        Thread.sleep(60);
        Assertions.assertEquals(0, renderCount.get(), "上下文未就绪前绝对不可调用 render");
        Assertions.assertEquals(0, channel.audioPushes, "上下文未就绪前绝对不可投递音频数据");
        // 激活上下文就绪门禁并唤醒采集线程（同 init 尾部钩子的两步）
        LoopbackBinding.markContextReady();
        scheduler.onContextReady();
        Assertions.assertTrue(LoopbackBinding.current().contextReady());
        // 等待至少一帧采样完成
        long deadline = System.currentTimeMillis() + 1000;
        while (channel.audioPushes == 0 && System.currentTimeMillis() < deadline) {
            Thread.sleep(10);
        }
        Assertions.assertTrue(channel.audioPushes > 0, "上下文就绪后应开始投递音频帧");
        Assertions.assertEquals(
                AudioCaptureScheduler.AUDIO_SAMPLES_PER_CHANNEL,
                channel.lastSamples,
                "投递采样数必须钉在 960/通道"
        );
        // 销毁上下文
        LoopbackBinding.clear();
        Assertions.assertFalse(LoopbackBinding.current().contextReady());
        Assertions.assertEquals(0L, LoopbackBinding.current().device());

        int pushesAfterDestroy = channel.audioPushes;
        Thread.sleep(60);
        Assertions.assertEquals(pushesAfterDestroy, channel.audioPushes, "上下文销毁后不应继续投递音频");

        scheduler.cleanup();
    }
}

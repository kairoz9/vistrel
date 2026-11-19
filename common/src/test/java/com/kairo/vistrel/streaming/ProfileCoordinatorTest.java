package com.kairo.vistrel.streaming;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;
import com.kairo.vistrel.streaming.ProfileCoordinator.ContentSnapshot;
import com.kairo.vistrel.streaming.SessionServer.SessionState;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;

/**
 * 档位协调器：BPP 旁路语义、内容变更 ask、拒绝保持旧档与暂停分发。
 * Applier 注入假 JSON 应答，监听器全量记录，经真实工作线程往返（闩锁等待）
 */
class ProfileCoordinatorTest {
    private static final long SECOND = 1_000_000_000L;

    /**
     * 全量记录型监听器：Apply 记录生效事件（target 即生效值），Pause 计数
     */
    private static final class RecordingListener implements java.util.function.Consumer<SessionState> {
        final List<SessionState> applied = new CopyOnWriteArrayList<>();
        final AtomicInteger paused = new AtomicInteger();

        @Override
        public void accept(SessionState state) {
            if (state.active()) {
                applied.add(state);
            } else {
                paused.incrementAndGet();
            }
        }
    }

    /**
     * 期望的生效状态：ask 应答 JSON 的 bpp 恒为 0.1
     */
    private static SessionState apply(int width, int height, int fps) {
        return new SessionState(SessionState.Phase.STREAMING, width, height, fps, 0.1, "H264");
    }

    /**
     * 假 JNI 应答 JSON：与 Answer / ask 应答同形状
     */
    private static String answerJson(int width, int height, int fps) {
        return "{\"width\":"
                + width
                + ",\"height\":"
                + height
                + ",\"fps\":"
                + fps
                + ",\"bpp\":0.1,\"bitrate\":5000000,\"codec\":\"H264\",\"rejected\":false}";
    }

    private static ProfileCoordinator.Applier applierAnswering(
            AtomicReference<String> seenRequest,
            CountDownLatch askLatch
    ) {
        // 假 JNI：原样回显请求几何并宣告生效；拒绝场景用 applierWithNoAnswer
        return (width, height, fps, bpp, codec) -> {
            if (seenRequest != null) {
                seenRequest.set(width + "x" + height + "@" + fps + " bpp=" + bpp);
            }
            if (askLatch != null) {
                askLatch.countDown();
            }
            return answerJson(width, height, fps);
        };
    }

    private static ProfileCoordinator.Applier applierWithNoAnswer() {
        // 拒绝 / 无会话：Native 回空串，Worker 走无应答路径
        return (width, height, fps, bpp, codec) -> "";
    }

    /**
     * 轮询等待列表长度到达期望值：applier 与 onApplied 分属 worker 线程前后脚，无先后闩锁
     */
    private static void awaitSize(List<?> list, int expected) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
        while (list.size() < expected && System.nanoTime() < deadline) {
            Thread.sleep(10);
        }
    }

    @Test
    void testContentChangeStableAppliesTarget() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicReference<String> seenRequest = new AtomicReference<>();
        CountDownLatch askLatch = new CountDownLatch(1);
        ProfileCoordinator worker = new ProfileCoordinator(applierAnswering(seenRequest, askLatch), listener);
        worker.start();
        try {
            // 生产时序：offer 协商成功先落账种入活跃基线（经 listener 广播初始档位），
            // 随后内容事件才可能触发 ask
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            assertEquals(apply(1920, 1080, 60), listener.applied.get(0), "Answer 落账广播初始档位");
            // 渲染线程两帧喂入同一目标：跨过 1 秒稳定窗
            worker.onContentSnapshot(new ContentSnapshot(2560, 1440, 260, false, 0), 0L);
            worker.onContentSnapshot(new ContentSnapshot(2560, 1440, 260, false, 0), SECOND);
            assertTrue(askLatch.await(2, TimeUnit.SECONDS), "稳定后必须发生一次 ask");

            assertEquals("2560x1440@240 bpp=0.1", seenRequest.get(), "Unlimited 映射为 240");
            awaitSize(listener.applied, 2);
            assertEquals(apply(2560, 1440, 240), listener.applied.get(1));
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testViewportChangeAppliesImmediatelyWithoutStabilityWindow() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicInteger askCount = new AtomicInteger();
        AtomicReference<String> seenRequest = new AtomicReference<>();
        CountDownLatch askLatch = new CountDownLatch(1);
        ProfileCoordinator worker = new ProfileCoordinator((width, height, fps, bpp, codec) -> {
            askCount.incrementAndGet();
            seenRequest.set(width + "x" + height + "@" + fps + " bpp=" + bpp);
            askLatch.countDown();
            return answerJson(width, height, fps);
        }, listener);
        worker.start();
        try {
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            // 外部偏好不走稳定窗：视口收缩立即用当前内容合成落 ask（防抖归客户端）
            worker.onContentSnapshot(new ContentSnapshot(1920, 1080, 60, false, 0), 0L);
            worker.onClientSettings(1280, 720, null);
            assertTrue(askLatch.await(2, TimeUnit.SECONDS), "视口变化必须立即触发 ask");
            assertEquals("1280x720@60 bpp=0.1", seenRequest.get(), "视口上限立即参与合成");
            // 等值视口重复上报不再触发（幂等状态同步）
            worker.onClientSettings(1280, 720, null);
            Thread.sleep(100);
            assertEquals(1, askCount.get(), "等值视口不得重复 ask");
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testViewportAbsorbedByContentSkipsAsk() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicInteger askCount = new AtomicInteger();
        ProfileCoordinator worker = new ProfileCoordinator((width, height, fps, bpp, codec) -> {
            askCount.incrementAndGet();
            return answerJson(width, height, fps);
        }, listener);
        worker.start();
        try {
            worker.noteOfferAnswer(answerJson(800, 600, 60), 0.1);
            worker.onContentSnapshot(new ContentSnapshot(800, 600, 60, false, 0), 0L);
            Thread.sleep(200);
            int baseline = askCount.get();
            // 视口从比内容大缩到仍然比内容大：目标档位一个字不变，不得打扰编码器
            worker.onClientSettings(1920, 1080, null);
            Thread.sleep(100);
            worker.onClientSettings(1280, 720, null);
            Thread.sleep(100);
            assertEquals(baseline, askCount.get(), "被内容吸收的视口变化不得触发重配");
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testSeededSnapshotDrivesOfferPlanWithoutAsk() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicInteger askCount = new AtomicInteger();
        ProfileCoordinator worker = new ProfileCoordinator((width, height, fps, bpp, codec) -> {
            askCount.incrementAndGet();
            return answerJson(width, height, fps);
        }, listener);
        worker.start();
        try {
            worker.seedContentSnapshot(new ContentSnapshot(856, 480, 120, false, 0));
            SessionState planned = worker.plan(1156, 650, 0.1);
            assertEquals(856, planned.width(), "offer 规划必须基于种子快照的宽度");
            assertEquals(480, planned.height(), "offer 规划必须基于种子快照的高度");
            assertEquals(120, planned.fps(), "offer 规划必须基于种子快照的帧率");
            Thread.sleep(100);
            assertEquals(0, askCount.get(), "种子只更新快照，不得触发 ask");
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testBppOnlyAskKeepsAppliedGeometryWhenNoFreshContent() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicReference<String> seenRequest = new AtomicReference<>();
        CountDownLatch askLatch = new CountDownLatch(1);
        ProfileCoordinator worker = new ProfileCoordinator(applierAnswering(seenRequest, askLatch), listener);
        worker.start();
        try {
            worker.noteOfferAnswer(answerJson(2560, 1440, 90), 0.1);
            worker.onClientSettings(null, null, 0.2);
            assertTrue(askLatch.await(2, TimeUnit.SECONDS), "BPP 请求必须触发 ask");
            // 无内容快照：保持实际生效的几何与帧率，只换 bpp
            assertEquals("2560x1440@90 bpp=0.2", seenRequest.get());
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testBppAskWithFreshSnapshotReplansGeometry() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicReference<String> seenRequest = new AtomicReference<>();
        CountDownLatch askLatch = new CountDownLatch(1);
        ProfileCoordinator worker = new ProfileCoordinator(applierAnswering(seenRequest, askLatch), listener);
        worker.start();
        try {
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            // 有新鲜内容：BPP 请求合并当前内容与视口上限（bpp 下降可拿回被预算压低的帧率）
            worker.onContentSnapshot(new ContentSnapshot(3840, 2160, 120, false, 0), 0L);
            worker.onClientSettings(1920, 1080, 0.3);
            assertTrue(askLatch.await(2, TimeUnit.SECONDS), "BPP 请求必须触发 ask");
            assertEquals("1920x1080@120 bpp=0.3", seenRequest.get(), "几何按内容×视口重规划");
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testEncoderRejectionKeepsPreviousProfileWithoutRetry() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicInteger askCount = new AtomicInteger();
        CountDownLatch askHappened = new CountDownLatch(1);
        ProfileCoordinator.Applier countingReject =
                (width, height, fps, bpp, codec) -> {
            askCount.incrementAndGet();
            askHappened.countDown();
            return applierWithNoAnswer().apply(width, height, fps, bpp, codec);
        };
        ProfileCoordinator worker = new ProfileCoordinator(countingReject, listener);
        worker.start();
        try {
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            worker.onContentSnapshot(new ContentSnapshot(2560, 1440, 120, false, 0), 0L);
            worker.onContentSnapshot(new ContentSnapshot(2560, 1440, 120, false, 0), SECOND);
            assertTrue(askHappened.await(2, TimeUnit.SECONDS), "稳定后必须发生一次 ask");
            // 拒绝不外报：onApplied 只有 Answer 落账一次，被拒目标不上屏
            Thread.sleep(100);
            assertEquals(1, listener.applied.size(), "被拒目标不得上屏生效");
            // 不自动重试：随后相同的稳定内容不再触发 ask
            worker.onContentSnapshot(new ContentSnapshot(2560, 1440, 120, false, 0), SECOND * 5);
            Thread.sleep(200);
            assertEquals(1, askCount.get(), "被拒目标不得自动重试");
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testOutOfRangeBppRejectedWithoutAsk() {
        RecordingListener listener = new RecordingListener();
        AtomicInteger askCount = new AtomicInteger();
        ProfileCoordinator.Applier counting =
                (width, height, fps, bpp, codec) -> {
            askCount.incrementAndGet();
            return "{}";
        };
        ProfileCoordinator worker = new ProfileCoordinator(counting, listener);
        // 不 start：越界拒绝在工作线程之外同步完成
        worker.onClientSettings(null, null, 0.95);
        assertEquals(0, askCount.get(), "越界 BPP 不得触达 Native");
    }

    @Test
    void testEqualBppRequestSkipsDuplicateAsk() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicInteger askCount = new AtomicInteger();
        ProfileCoordinator worker = new ProfileCoordinator((width, height, fps, bpp, codec) -> {
            askCount.incrementAndGet();
            return answerJson(width, height, fps);
        }, listener);
        worker.start();
        try {
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            // settings 是幂等状态同步，每次视口上报都重复携带 bpp：等值必须短路
            worker.onClientSettings(null, null, 0.1);
            assertEquals(0, askCount.get(), "等值 BPP 不得触发 ask");
            worker.onClientSettings(null, null, 0.2);
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
            while (askCount.get() < 1 && System.nanoTime() < deadline) {
                Thread.sleep(10);
            }
            assertEquals(1, askCount.get(), "只有真正的变更落到 Native");
            awaitSize(listener.applied, 2);
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testZeroGeometryPausesImmediately() {
        RecordingListener listener = new RecordingListener();
        ProfileCoordinator worker = new ProfileCoordinator((width, height, fps, bpp, codec) -> null, listener);
        // 0×0 在渲染线程就地暂停，不需要工作线程
        worker.onContentSnapshot(ContentSnapshot.EMPTY, 0L);
        assertEquals(1, listener.paused.get(), "内容丢失必须立即暂停");
        // 暂停期间不会产生任何 ask
        worker.onContentSnapshot(ContentSnapshot.EMPTY, SECOND);
        assertEquals(1, listener.paused.get(), "重复 0×0 不重复暂停");
    }

    // 停机契约：shutdown 后所有入口幂等空转，迟到事件不落账不广播
    @Test
    void testEntriesAreNoopsAfterShutdown() {
        RecordingListener listener = new RecordingListener();
        ProfileCoordinator worker = new ProfileCoordinator(applierWithNoAnswer(), listener);
        worker.start();
        worker.shutdown();

        worker.noteOfferAnswer(answerJson(1280, 720, 60), 0.1);
        worker.onSessionClosed();
        worker.onClientSettings(1280, 720, 0.2);
        worker.onContentSnapshot(ContentSnapshot.EMPTY, 0L);

        assertEquals(0, listener.applied.size(), "停机后不得落账广播");
        assertEquals(0, listener.paused.get(), "停机后不得分发暂停");
    }

    @Test
    void testPauseResumeForcesRebuild() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicReference<String> seenRequest = new AtomicReference<>();
        CountDownLatch askLatch = new CountDownLatch(1);
        ProfileCoordinator worker = new ProfileCoordinator(applierAnswering(seenRequest, askLatch), listener);
        worker.start();
        try {
            // 先建立基线再最小化（0×0 暂停），随后同一几何恢复：必须走一次 ask 且带 resume 标志
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            worker.onContentSnapshot(new ContentSnapshot(1920, 1080, 60, false, 0), 0L);
            worker.onContentSnapshot(ContentSnapshot.EMPTY, SECOND);
            assertEquals(1, listener.paused.get(), "最小化必须暂停");
            worker.onContentSnapshot(new ContentSnapshot(1920, 1080, 60, false, 0), SECOND * 2);
            worker.onContentSnapshot(new ContentSnapshot(1920, 1080, 60, false, 0), SECOND * 3);
            assertTrue(askLatch.await(2, TimeUnit.SECONDS), "恢复后的首个稳定目标必须 ask");
            awaitSize(listener.applied, 2);
            assertEquals(apply(1920, 1080, 60), listener.applied.get(1), "恢复必须重新下发同一档位");
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testChurningValuesOnlyFinalStableOneApplies() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicReference<String> seenRequest = new AtomicReference<>();
        CountDownLatch askLatch = new CountDownLatch(1);
        ProfileCoordinator worker = new ProfileCoordinator(applierAnswering(seenRequest, askLatch), listener);
        worker.start();
        try {
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            // 拖动窗口：窗口内几何反复变化各自重置计时，只有最终稳定者跨过 1 秒窗被放行
            worker.onContentSnapshot(new ContentSnapshot(1024, 768, 60, false, 0), 0L);
            worker.onContentSnapshot(new ContentSnapshot(1280, 720, 60, false, 0), SECOND / 2);
            worker.onContentSnapshot(new ContentSnapshot(1600, 900, 60, false, 0), SECOND / 2 + SECOND / 4);
            worker.onContentSnapshot(new ContentSnapshot(1600, 900, 60, false, 0), SECOND * 2);
            assertTrue(askLatch.await(2, TimeUnit.SECONDS), "最终稳定值必须放行");
            assertEquals("1600x900@60 bpp=0.1", seenRequest.get(), "只有最终稳定者生效，中间值全部丢弃");
            awaitSize(listener.applied, 2);
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testIdenticalValueProducesNoRequest() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicInteger askCount = new AtomicInteger();
        ProfileCoordinator worker = new ProfileCoordinator((width, height, fps, bpp, codec) -> {
            askCount.incrementAndGet();
            return answerJson(width, height, fps);
        }, listener);
        worker.start();
        try {
            // 基线与内容规划出的目标一致：跨过稳定窗也不得重复下发
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            worker.onContentSnapshot(new ContentSnapshot(1920, 1080, 60, false, 0), 0L);
            worker.onContentSnapshot(new ContentSnapshot(1920, 1080, 60, false, 0), SECOND * 5);
            Thread.sleep(200);
            assertEquals(0, askCount.get(), "与已尝试目标一致的内容不得重复 ask");
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testSessionCloseResetsPendingAndAppliedBaseline() throws Exception {
        RecordingListener listener = new RecordingListener();
        AtomicReference<String> seenRequest = new AtomicReference<>();
        ProfileCoordinator worker = new ProfileCoordinator(applierAnswering(seenRequest, null), listener);
        worker.start();
        try {
            // 生产前提：渲染帧只在会话活跃后采样（isStreaming 门控），先落账基线再验证关闭
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            worker.onContentSnapshot(new ContentSnapshot(2560, 1440, 120, false, 0), 0L);
            worker.onSessionClosed();
            // 会话关闭清空待办：即使随后稳定也不触发 ask（applier 会返回 null → 走拒绝）
            worker.onContentSnapshot(new ContentSnapshot(2560, 1440, 120, false, 0), SECOND * 10);
            Thread.sleep(200);
            assertNull(seenRequest.get(), "无会话时不应有 ask 到达 Native");
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testSessionClosedDuringAskDiscardsStaleResult() throws Exception {
        RecordingListener listener = new RecordingListener();
        CountDownLatch askEntered = new CountDownLatch(1);
        CountDownLatch releaseAsk = new CountDownLatch(1);
        ProfileCoordinator worker = new ProfileCoordinator((width, height, fps, bpp, codec) -> {
            askEntered.countDown();
            try {
                assertTrue(releaseAsk.await(2, TimeUnit.SECONDS), "测试必须放行在途 ask");
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            return answerJson(width, height, fps);
        }, listener);
        worker.start();
        try {
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            assertEquals(apply(1920, 1080, 60), listener.applied.get(0), "Answer 落账广播初始档位");
            // 喂一帧内容后视口变更立即 ask：目标与落账基线不同，工作线程进入慢 JNI 并阻塞
            worker.onContentSnapshot(new ContentSnapshot(1920, 1080, 60, false, 0), 0L);
            worker.onClientSettings(800, 600, null);
            assertTrue(askEntered.await(2, TimeUnit.SECONDS), "ask 必须已进入阻塞 JNI");
            // ask 在途时 WS 断开：关闭复位广播 INACTIVE，代数递增
            worker.onSessionClosed();
            releaseAsk.countDown();
            // 在途应答必须作废：旧会话档位不得写回基线再广播 STREAMING（断线复活）
            Thread.sleep(200);
            assertEquals(1, listener.applied.size(), "旧会话应答不得复活为 STREAMING");
            assertEquals(1, listener.paused.get(), "关闭复位恰好广播一次 INACTIVE");
            // 重连照常：新 offer 落账不受在途作废影响
            worker.noteOfferAnswer(answerJson(2560, 1440, 90), 0.1);
            assertEquals(apply(2560, 1440, 90), listener.applied.get(1), "重连落账照常广播");
        } finally {
            worker.shutdown();
        }
    }

    @Test
    void testReofferDuringAskDiscardsStaleResult() throws Exception {
        RecordingListener listener = new RecordingListener();
        CountDownLatch askEntered = new CountDownLatch(1);
        CountDownLatch releaseAsk = new CountDownLatch(1);
        ProfileCoordinator worker = new ProfileCoordinator((width, height, fps, bpp, codec) -> {
            askEntered.countDown();
            try {
                assertTrue(releaseAsk.await(2, TimeUnit.SECONDS), "测试必须放行在途 ask");
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            return answerJson(width, height, fps);
        }, listener);
        worker.start();
        try {
            worker.noteOfferAnswer(answerJson(1920, 1080, 60), 0.1);
            assertEquals(apply(1920, 1080, 60), listener.applied.get(0), "Answer 落账广播初始档位");
            // 喂一帧内容后视口变更立即 ask：工作线程进入慢 JNI 并阻塞
            worker.onContentSnapshot(new ContentSnapshot(1920, 1080, 60, false, 0), 0L);
            worker.onClientSettings(800, 600, null);
            assertTrue(askEntered.await(2, TimeUnit.SECONDS), "ask 必须已进入阻塞 JNI");
            // ask 在途时不间断重协商：新 offer 落账换基线实例，旧应答按引用同一性作废
            worker.noteOfferAnswer(answerJson(2560, 1440, 90), 0.1);
            assertEquals(apply(2560, 1440, 90), listener.applied.get(1), "新 offer 落账照常广播");
            releaseAsk.countDown();
            Thread.sleep(200);
            assertEquals(2, listener.applied.size(), "旧会话应答不得覆写新基线或再次广播");
        } finally {
            worker.shutdown();
        }
    }
}

package kairo.vistrel.input;

import com.mojang.blaze3d.platform.Window;
import net.minecraft.client.Minecraft;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import java.util.ArrayList;
import java.util.List;

/**
 * 输入注入器用例：重置检测器的状态变化边界——退出推流与打开/关闭 GUI 的转变
 * 恰好触发一次，进出场与抓取转变各报告一次供光标状态帧推送；以及类型化事件
 * 分发对适配器绑定类事件的路由接线
 */
class InputInjectorTest {
    private static final InputInjector.ResetDetector.Event NONE = InputInjector.ResetDetector.Event.NONE;
    private static final InputInjector.ResetDetector.Event EXIT = InputInjector.ResetDetector.Event.STREAMING_EXIT;
    private static final InputInjector.ResetDetector.Event ENTER = InputInjector.ResetDetector.Event.SESSION_ENTER;
    private static final InputInjector.ResetDetector.Event RELEASE = InputInjector.ResetDetector.Event.GRAB_RELEASE;
    private static final InputInjector.ResetDetector.Event ACQUIRE = InputInjector.ResetDetector.Event.GRAB_ACQUIRE;

    static class MockAdapter extends InputAdapter {
        final List<String> logs = new ArrayList<>();

        @Override
        public long windowHandle(Window window) {
            return 0L;
        }

        @Override
        protected void updateModifiers(int code, int action) {}

        @Override
        protected void doInjectKey(Minecraft mc, long windowHandle, int code, int rawAction) {
            logs.add("Key:" + code + ":" + rawAction);
        }

        @Override
        protected void doInjectMouseButton(Minecraft mc, long windowHandle, int jsBtn, int action) {
            logs.add("MouseBtn:" + jsBtn + ":" + action);
        }

        @Override
        public void injectMouseMove(Minecraft mc, long windowHandle, double newX, double newY, double dx, double dy) {
            logs.add("MouseMove:" + newX + ":" + newY + ":" + dx + ":" + dy);
        }

        @Override
        public void injectMouseScroll(Minecraft mc, long windowHandle, double xOffset, double yOffset) {
            logs.add("MouseScroll:" + xOffset + ":" + yOffset);
        }

        @Override
        protected void doInjectText(Minecraft mc, long windowHandle, String text) {
            logs.add("Text:" + text);
        }
    }

    private static InputInjector newInjector(InputAdapter adapter) {
        // 文本入队路径以推流中为前提（生产门控），其余用例不经过该门控
        return newInjector(adapter, true);
    }

    private static InputInjector newInjector(InputAdapter adapter, boolean streaming) {
        return new InputInjector(new InputEventSource() {
        }, adapter, () -> streaming, (visible, x, y) -> {});
    }

    @Test
    void testColdIdleStaysSilent() {
        InputInjector.ResetDetector detector = new InputInjector.ResetDetector();
        Assertions.assertEquals(NONE, detector.poll(false, false), "从未推流过的 idle 帧不应有状态事件");
        Assertions.assertEquals(NONE, detector.poll(false, false), "持续 idle 不应重复触发");
    }

    @Test
    void testFirstStreamingFrameReportsSessionEnter() {
        InputInjector.ResetDetector detector = new InputInjector.ResetDetector();
        Assertions.assertEquals(ENTER, detector.poll(true, true), "首帧建立基线并报告进入推流");
        Assertions.assertEquals(NONE, detector.poll(true, true), "锁定状态保持无事件");
    }

    @Test
    void testGrabReleaseFiresExactlyOnce() {
        InputInjector.ResetDetector detector = new InputInjector.ResetDetector();
        detector.poll(true, true);
        Assertions.assertEquals(RELEASE, detector.poll(true, false), "锁定→释放（GUI 打开）触发重置");
        Assertions.assertEquals(NONE, detector.poll(true, false), "持续释放不重复触发");
        Assertions.assertEquals(ACQUIRE, detector.poll(true, true), "释放→锁定（GUI 关闭）报告抓取恢复");
        Assertions.assertEquals(NONE, detector.poll(true, true), "持续锁定不重复触发");
    }

    @Test
    void testGrabAcquireFiresExactlyOnce() {
        InputInjector.ResetDetector detector = new InputInjector.ResetDetector();
        detector.poll(true, false);
        Assertions.assertEquals(ACQUIRE, detector.poll(true, true), "释放→锁定（GUI 关闭）报告抓取恢复");
        Assertions.assertEquals(NONE, detector.poll(true, true), "持续锁定不重复触发");
    }

    @Test
    void testStreamingExitFiresExactlyOnce() {
        InputInjector.ResetDetector detector = new InputInjector.ResetDetector();
        detector.poll(true, true);
        Assertions.assertEquals(EXIT, detector.poll(false, true), "退出推流触发离场重置");
        Assertions.assertEquals(NONE, detector.poll(false, true), "持续 idle 不重复触发");
    }

    @Test
    void testIdleFramesDoNotTouchGrabHistory() {
        InputInjector.ResetDetector detector = new InputInjector.ResetDetector();
        // 锁定状态进入推流
        detector.poll(true, true);
        // 退出（STREAMING_EXIT）
        detector.poll(false, true);
        // idle 期间本地松开鼠标——不应改写抓取历史
        detector.poll(false, false);
        Assertions.assertEquals(ENTER, detector.poll(true, true), "重入推流仍锁定：仅报告进入（idle 帧未污染历史）");
    }

    @Test
    void testGrabHistorySurvivesIdleAcrossStreamingCycles() {
        InputInjector.ResetDetector detector = new InputInjector.ResetDetector();
        // 上一轮推流：锁定
        detector.poll(true, true);
        // 退出
        detector.poll(false, true);
        // 重入时若已释放（上轮遗留锁定历史），应触发一次重置释放粘滞按键
        Assertions.assertEquals(RELEASE, detector.poll(true, false), "跨推流周期的锁定→释放残留转变必须重置");
    }

    @Test
    void testDispatchRoutesAdapterBoundEvents() {
        // move/absoluteMove 首行即解引用 mc/window，单测 JVM 无实例，不在覆盖范围
        MockAdapter adapter = new MockAdapter();
        InputInjector injector = newInjector(adapter);

        injector.dispatch(
                List.of(
                        new InputEventDecoder.Key(26, InputAdapter.ACTION_PRESS),
                        new InputEventDecoder.MouseButton(1, InputAdapter.ACTION_RELEASE),
                        new InputEventDecoder.MouseScroll((byte) -1)
                ),
                null,
                null,
                12345L
        );

        Assertions.assertTrue(adapter.logs.contains("Key:26:1"));
        Assertions.assertTrue(adapter.logs.contains("MouseBtn:1:0"));
        Assertions.assertTrue(adapter.logs.contains("MouseScroll:0.0:-1.0"));
        Assertions.assertEquals(3, adapter.logs.size(), "三个事件恰好三条注入记录");
    }

    @Test
    void testTextInputDrainedToAdapterInOrder() {
        MockAdapter adapter = new MockAdapter();
        InputInjector injector = newInjector(adapter);

        injector.enqueueText("你");
        injector.enqueueText("好a");
        injector.drainText(null, 0L);
        Assertions.assertEquals(List.of("Text:你", "Text:好a"), adapter.logs, "文本按序逐条注入");

        injector.enqueueText("后续");
        Assertions.assertEquals(2, adapter.logs.size(), "drain 后新入队文本留待下次");
    }

    @Test
    void testStreamingExitDiscardsPendingText() {
        MockAdapter adapter = new MockAdapter();
        InputInjector injector = newInjector(adapter);
        injector.enqueueText("残留");
        injector.discardPendingText();
        injector.drainText(null, 0L);
        Assertions.assertTrue(adapter.logs.isEmpty(), "退出推流应丢弃未消费文本");
    }

    @Test
    void testReleaseAllDiscardsPendingTextWithoutMinecraft() {
        MockAdapter adapter = new MockAdapter();
        InputInjector injector = newInjector(adapter);
        injector.enqueueText("停机前残留文本");
        // 无 Minecraft 实例：adapter.reset 安全跳过，文本必须照常丢弃
        injector.releaseAll();
        injector.drainText(null, 0L);
        Assertions.assertTrue(adapter.logs
            .stream()
            .noneMatch(log -> log.startsWith("Text:")), "releaseAll 必须丢弃未消费文本");
    }

    @Test
    void testEnqueueDroppedWhenNotStreaming() {
        MockAdapter adapter = new MockAdapter();
        InputInjector injector = newInjector(adapter, false);
        injector.enqueueText("未推流文本");
        injector.drainText(null, 0L);
        Assertions.assertTrue(adapter.logs.isEmpty(), "未推流时入队文本必须直接丢弃");
    }

    @Test
    void testPendingTextDropsNewestWhenFull() {
        MockAdapter adapter = new MockAdapter();
        InputInjector injector = newInjector(adapter);
        for (int i = 0; i <= InputInjector.MAX_PENDING_TEXTS; i++) {
            injector.enqueueText("t" + i);
        }
        injector.drainText(null, 0L);
        List<String> expected = new ArrayList<>();
        for (int i = 0; i < InputInjector.MAX_PENDING_TEXTS; i++) {
            expected.add("Text:t" + i);
        }
        Assertions.assertEquals(expected, adapter.logs, "饱和丢最新保最旧，前序文本顺序不变");
    }
}

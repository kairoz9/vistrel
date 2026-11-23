package kairo.vistrel.input;

import kairo.vistrel.Constants;
import kairo.vistrel.input.InputEventDecoder.AbsoluteMouseMove;
import kairo.vistrel.input.InputEventDecoder.Key;
import kairo.vistrel.input.InputEventDecoder.MouseButton;
import kairo.vistrel.input.InputEventDecoder.MouseMove;
import kairo.vistrel.input.InputEventDecoder.MouseScroll;
import com.mojang.blaze3d.platform.Window;
import net.minecraft.client.Minecraft;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.List;
import java.util.Queue;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.function.BooleanSupplier;

/**
 * 远程 WebRTC 输入注入器
 * 在渲染主循环每帧头部拉取、解码、分发并注入至 Minecraft 引擎
 *
 * <p>会话级实例由 VistrelService 构造持有；何时重置按住的按键（退出推流、
 * 打开 GUI）与何时推送光标状态帧由嵌套 {@link ResetDetector} 判断，
 * Minecraft 对接、事件分发与键鼠注入都在本类完成。
 */
public final class InputInjector {
    private static final int EVENT_BUFFER_BYTES = 4096;
    private static final int MAX_EVENTS_PER_BATCH = 128;
    /**
     * 未消费输入文本的队列上界（32 条 × 8192 字符 ≈ 512KB 堆上界），包私有供测试引用
     */
    static final int MAX_PENDING_TEXTS = 32;
    private static final double CURSOR_COORD_SCALE = 65535.0;
    private final ByteBuffer eventBuffer = ByteBuffer.allocateDirect(EVENT_BUFFER_BYTES).order(ByteOrder.BIG_ENDIAN);
    private final InputEventSource inputSource;
    private final InputAdapter adapter;
    private final BooleanSupplier streamingProbe;
    private final CursorStateSink cursorStateSink;
    private final ResetDetector resetDetector = new ResetDetector();
    // 浏览器输入文本：有界队列，饱和丢最新保最旧
    private final Queue<String> pendingText = new ArrayBlockingQueue<>(MAX_PENDING_TEXTS);

    /**
     * 装配注入器：事件缓冲、文本队列与重置检测器就地初始化
     *
     * @param inputSource    输入事件源（帧头 {@link #pollAndDispatch()} 经它取事件）
     * @param adapter         平台输入适配器（Minecraft 对接与键鼠注入的实际执行者）
     * @param streamingProbe  推流状态探针；false 时帧头只评估推流退出，不触碰引擎接口
     * @param cursorStateSink 光标状态帧推送出口（会话进入与抓取状态转变帧各推一次）
     */
    public InputInjector(
            InputEventSource inputSource,
            InputAdapter adapter,
            BooleanSupplier streamingProbe,
            CursorStateSink cursorStateSink
    ) {
        this.inputSource = inputSource;
        this.adapter = adapter;
        this.streamingProbe = streamingProbe;
        this.cursorStateSink = cursorStateSink;
    }

    /**
     * 每帧头部拉取并注入输入事件
     *
     * <p>无活跃 Remote Player 时连 JNI 都不碰；推流停止当帧即释放按住的按键，不留卡键。
     */
    public void pollAndDispatch() {
        boolean isStreaming = streamingProbe.getAsBoolean();
        Minecraft mc = Minecraft.getInstance();
        Window window = mc != null ? mc.getWindow() : null;

        ResetDetector.Event event = resetDetector.poll(isStreaming, mc != null && mc.mouseHandler.isMouseGrabbed());
        if (event == ResetDetector.Event.STREAMING_EXIT) {
            // 退出推流：释放全部按下的键并丢弃未消费文本（游戏实例缺席时安全跳过）
            discardPendingText();
            if (mc != null && window != null) {
                adapter.reset(mc, adapter.windowHandle(window));
            }
            return;
        }
        if (!isStreaming || mc == null || window == null) {
            return;
        }
        if (event == ResetDetector.Event.GRAB_RELEASE) {
            // 打开 GUI / 菜单释放光标锁定，主动重置输入状态释放已按下的按键
            adapter.reset(mc, adapter.windowHandle(window));
        }
        if (event != ResetDetector.Event.NONE) {
            // 会话进入与抓取状态转变各推一次光标状态帧
            pushCursorState(mc, window);
        }
        // 输入文本先行注入：同一帧内文本早于按键（打完字才回车发送）
        drainText(mc, adapter.windowHandle(window));

        eventBuffer.clear();
        int count = inputSource.pollInputEvents(eventBuffer, MAX_EVENTS_PER_BATCH);
        if (count <= 0) {
            return;
        }

        long windowHandle = adapter.windowHandle(window);
        dispatch(InputEventDecoder.decode(eventBuffer, count), mc, window, windowHandle);
    }

    /**
     * 浏览器输入文本入队：信令 WS 线程调用，长度上限由信令层截断保证。
     * 未推流直接丢弃——没有画面可注入，空闲期不积压；队列饱和按 Drop-Newest
     * 丢弃（保最旧即保打字顺序），与 native 输入环同策略
     *
     * @param text 待注入文本；null 或空串忽略
     */
    public void enqueueText(String text) {
        if (text == null || text.isEmpty()) {
            return;
        }
        if (!streamingProbe.getAsBoolean()) {
            return;
        }
        if (!pendingText.offer(text)) {
            Constants.LOG.warn("输入文本队列已满（{} 条），丢弃最新文本", MAX_PENDING_TEXTS);
        }
    }

    /**
     * 停机路径的输入释放：补发全部按住按键的 release 并丢弃未消费文本。
     * 与帧头 STREAMING_EXIT 同语义，但由 VistrelService 在渲染线程的 teardown 里显式调用——
     * injector 被摘除后帧头不会再进入本类，重置只能在这里兜底，否则按键粘滞到本地手动点按
     */
    public void releaseAll() {
        discardPendingText();
        Minecraft mc = Minecraft.getInstance();
        Window window = mc != null ? mc.getWindow() : null;
        if (mc != null && window != null) {
            adapter.reset(mc, adapter.windowHandle(window));
        }
    }

    /**
     * 排空输入文本队列逐条注入
     */
    void drainText(Minecraft mc, long windowHandle) {
        String text;
        while ((text = pendingText.poll()) != null) {
            adapter.injectText(mc, windowHandle, text);
        }
    }

    /**
     * 丢弃全部未消费输入文本（退出推流时调用）
     */
    void discardPendingText() {
        pendingText.clear();
    }

    /**
     * 按类型把解码出的事件路由到适配器
     *
     * <p>包私有只为测试：不涉及 Minecraft 实例的路由逻辑可以在单测里直接调用。
     */
    void dispatch(List<InputEventDecoder.Event> events, Minecraft mc, Window window, long windowHandle) {
        for (InputEventDecoder.Event event : events) {
            if (event instanceof MouseMove e) {
                move(e, mc, window, windowHandle);
            } else if (event instanceof MouseButton e) {
                adapter.injectMouseButton(mc, windowHandle, e.button(), e.action());
            } else if (event instanceof Key e) {
                adapter.injectKey(mc, windowHandle, e.code(), e.action());
            } else if (event instanceof MouseScroll e) {
                adapter.injectMouseScroll(mc, windowHandle, 0.0, (double) e.sy());
            } else if (event instanceof AbsoluteMouseMove e) {
                absoluteMove(e, mc, window, windowHandle);
            } else {
                Constants.LOG.warn("Input 事件类型未经分发路由: {}", event.getClass().getSimpleName());
            }
        }
    }

    /**
     * 相对位移统一走光标回调：未抓取时钳制在窗口内移动虚拟光标；抓取态坐标不钳制（视角虚拟
     * 坐标无界），灵敏度与 Y 反转由 vanilla 的 onMove→turnPlayer 路径自管，与本地硬件输入同路
     */
    private void move(MouseMove e, Minecraft mc, Window window, long windowHandle) {
        double newX = mc.mouseHandler.xpos() + e.dx();
        double newY = mc.mouseHandler.ypos() + e.dy();
        if (!mc.mouseHandler.isMouseGrabbed()) {
            newX = Math.max(0, Math.min(window.getScreenWidth(), newX));
            newY = Math.max(0, Math.min(window.getScreenHeight(), newY));
        }
        adapter.injectMouseMove(mc, windowHandle, newX, newY, e.dx(), e.dy());
    }

    /**
     * 绝对位置：归一化坐标换算窗口坐标后移动光标
     */
    private void absoluteMove(AbsoluteMouseMove e, Minecraft mc, Window window, long windowHandle) {
        double winX = ((double) e.x() / CURSOR_COORD_SCALE) * window.getScreenWidth();
        double winY = ((double) e.y() / CURSOR_COORD_SCALE) * window.getScreenHeight();
        double curX = mc.mouseHandler.xpos();
        double curY = mc.mouseHandler.ypos();
        adapter.injectMouseMove(mc, windowHandle, winX, winY, winX - curX, winY - curY);
    }

    /**
     * 推送光标状态帧：GUI 态客户端 overlay 光标的可见性与归一化位置
     */
    private void pushCursorState(Minecraft mc, Window window) {
        cursorStateSink.push(
                !mc.mouseHandler.isMouseGrabbed(),
                normalize(mc.mouseHandler.xpos(), window.getScreenWidth()),
                normalize(mc.mouseHandler.ypos(), window.getScreenHeight())
        );
    }

    /**
     * 窗口坐标 → 0-65535 归一化，与 {@link #absoluteMove} 的换算互为逆运算
     */
    private static int normalize(double pos, double size) {
        if (size <= 0.0) {
            return 0;
        }
        double ratio = Math.max(0.0, Math.min(1.0, pos / size));
        return (int) Math.round(ratio * CURSOR_COORD_SCALE);
    }

    /**
     * 光标状态帧推送出口：可见性 + 归一化坐标，由信令面发往浏览器
     */
    @FunctionalInterface
    public interface CursorStateSink {
        void push(boolean visible, int normalizedX, int normalizedY);
    }

    /**
     * 输入重置检测器：跟踪 streaming 与鼠标抓取的上一帧状态，
     * 在状态转变帧报告事件——重置类事件释放已按下按键，抓取与进出场转变
     * 同时驱动光标状态帧推送，重复帧不重复触发。
     */
    static final class ResetDetector {
        /**
         * 本帧状态转变事件
         */
        enum Event {
            /**
             * 本帧无状态转变
             */
            NONE,
            /**
             * 退出推流：释放全部按下的键，防止离场按键粘滞
             */
            STREAMING_EXIT,
            /**
             * 进入推流（含重入且抓取状态未变）：推送光标状态基线
             */
            SESSION_ENTER,
            /**
             * 鼠标抓取释放（打开 GUI/菜单）：释放按键交还本地输入，光标转可见
             */
            GRAB_RELEASE,
            /**
             * 鼠标抓取恢复（关闭 GUI/菜单）：光标转隐藏
             */
            GRAB_ACQUIRE
        }

        private boolean wasStreaming = false;
        private boolean stateInitialized = false;
        private boolean lastGrabbed = false;

        /**
         * 喂入本帧状态，返回本帧的状态转变事件。
         *
         * <p>非 streaming 帧只评估推流转变且不触碰抓取历史（idle 帧提前返回）；
         * 首帧建立抓取基线并报告进入推流；重入推流叠加抓取释放时按 GRAB_RELEASE
         * 报告（重置语义是两者的超集，光标推送由同一帧的任一非 NONE 事件触发）。
         */
        Event poll(boolean isStreaming, boolean isMouseGrabbed) {
            if (!isStreaming) {
                if (wasStreaming) {
                    wasStreaming = false;
                    return Event.STREAMING_EXIT;
                }
                return Event.NONE;
            }
            boolean sessionEntered = !wasStreaming;
            wasStreaming = true;

            if (!stateInitialized) {
                stateInitialized = true;
                lastGrabbed = isMouseGrabbed;
                return Event.SESSION_ENTER;
            }
            boolean grabReleased = lastGrabbed && !isMouseGrabbed;
            boolean grabAcquired = !lastGrabbed && isMouseGrabbed;
            lastGrabbed = isMouseGrabbed;
            if (grabReleased) {
                return Event.GRAB_RELEASE;
            }
            if (grabAcquired) {
                return Event.GRAB_ACQUIRE;
            }
            return sessionEntered ? Event.SESSION_ENTER : Event.NONE;
        }
    }
}

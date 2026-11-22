package com.kairo.vistrel.capture;

import com.kairo.vistrel.Constants;
import com.kairo.vistrel.streaming.EncoderChannel;
import com.kairo.vistrel.streaming.SessionServer;
import org.lwjgl.openal.ALC10;
import org.lwjgl.openal.SOFTLoopback;
import org.lwjgl.system.MemoryStack;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.IntBuffer;
import java.util.Objects;
import java.util.concurrent.locks.LockSupport;
import java.util.function.Supplier;

/**
 * OpenAL 内存环回音频采集调度器
 *
 * <p>
 * 负责驱动 OpenAL Soft 虚拟环回设备 (ALC_SOFT_loopback)，
 * 以 50Hz (20ms) 节拍渲染 48kHz Stereo 16-bit PCM (单帧 960 采样/通道，3840 字节)，
 * 并通过常驻双缓冲轮替 (Ping-Pong Buffer)。
 *
 * <p>
 * 无推流客户端时线程挂起，客户端接入后立即唤醒恢复推流。
 * 设备句柄与 context 就绪位的正本在 {@link LoopbackBinding}（随 MC 音频库存亡），
 * 本类只读快照，销毁不触碰绑定。
 */
public class AudioCaptureScheduler {
    public static final int AUDIO_SAMPLE_RATE = 48000;
    public static final int AUDIO_CHANNELS = 2;
    public static final int AUDIO_SAMPLES_PER_CHANNEL = 960;
    public static final int AUDIO_TOTAL_SAMPLES = AUDIO_SAMPLES_PER_CHANNEL * AUDIO_CHANNELS;
    // 3840 字节
    public static final int AUDIO_FRAME_BYTES = AUDIO_TOTAL_SAMPLES * 2;
    // 64KB 安全余量，防止驱动在通道/格式未定时写越界
    public static final int BUFFER_SAFETY_CAPACITY = 65536;
    // 20 ms
    public static final long TICK_INTERVAL_NS = 20_000_000L;

    @FunctionalInterface
    public interface SampleRenderer {
        /**
         * 渲染一帧环回 PCM：把环回设备混音结果写入缓冲首部（采集线程在 renderLock 内调用，
         * 实现不得长期持锁或阻塞）
         *
         * @param device  环回设备句柄（{@link LoopbackBinding#device()} 的快照值）
         * @param buffer  写入目标（堆外缓冲，容量含 {@link #BUFFER_SAFETY_CAPACITY} 安全余量；
         *                调用方随后按 {@link #AUDIO_FRAME_BYTES} 截取有效载荷投递）
         * @param samples 每通道采样数（{@link #AUDIO_SAMPLES_PER_CHANNEL}）
         */
        void render(long device, ByteBuffer buffer, int samples);
    }

    /**
     * 从原属性表中解析 HRTF 偏好 (6554 ALC_HRTF_SOFT / 6546 ALC_HRTF_SPECIFIER_SOFT)，未声明时默认开启
     *
     * @param originalAttrs OpenAL 原属性表（key/value 对，以 0 终止）；可为 null
     * @return HRTF 偏好值：1 = 开启（默认），0 = 关闭
     */
    public static int extractHrtfPreference(IntBuffer originalAttrs) {
        int hrtf = 1;
        if (originalAttrs != null) {
            while (originalAttrs.hasRemaining()) {
                int key = originalAttrs.get();
                if (key == 0) {
                    break;
                }
                int val = originalAttrs.hasRemaining() ? originalAttrs.get() : 0;
                if (key == 6554
                        /* ALC_HRTF_SOFT */
                || key == 6546) /* ALC_HRTF_SPECIFIER_SOFT */
                {
                    hrtf = val;
                }
            }
        }
        return hrtf;
    }

    /**
     * 组装环回设备专用 alcCreateContext 属性表：48kHz Stereo 16-bit PCM + HRTF 偏好
     *
     * @param stack 分配用 MemoryStack（属性表随栈帧释放，调用方须当场消费）
     * @param hrtf  HRTF 偏好值，取 {@link #extractHrtfPreference(IntBuffer)} 的返回约定
     *              （1 = 开启，0 = 关闭）
     * @return 以 0 终止的 alcCreateContext 属性表
     */
    public static IntBuffer loopbackContextAttributes(MemoryStack stack, int hrtf) {
        IntBuffer attrs = stack.callocInt(16);
        attrs.put(SOFTLoopback.ALC_FORMAT_CHANNELS_SOFT).put(SOFTLoopback.ALC_STEREO_SOFT);
        attrs.put(SOFTLoopback.ALC_FORMAT_TYPE_SOFT).put(SOFTLoopback.ALC_SHORT_SOFT);
        attrs.put(ALC10.ALC_FREQUENCY).put(AUDIO_SAMPLE_RATE);
        attrs.put(6554).put(hrtf);
        // 终止符
        attrs.put(0);
        attrs.flip();
        return attrs;
    }

    private final EncoderChannel channel;
    private final SampleRenderer sampleRenderer;
    private volatile boolean running = false;
    private Thread captureThread = null;
    private final Object stateLock = new Object();
    private final Object renderLock = new Object();
    // 双缓冲（堆外 DirectByteBuffer），启动时分配一次常驻复用；
    // 单个 64KB 安全余量，防止驱动在通道/格式未定时写越界
    private final ByteBuffer[] pingPongBuffers = new ByteBuffer[] {
            ByteBuffer.allocateDirect(BUFFER_SAFETY_CAPACITY).order(ByteOrder.nativeOrder()),
            ByteBuffer.allocateDirect(BUFFER_SAFETY_CAPACITY).order(ByteOrder.nativeOrder())
    };
    private int pingPongIndex = 0;
    private volatile boolean isStreaming = false;
    /**
     * 当前会话状态的读取口：唤醒入口即求值
     */
    private final Supplier<SessionServer.SessionState> sessionState;

    /**
     * 生产构造：采样渲染直接走 OpenAL Soft 环回入口 {@code alcRenderSamplesSOFT}
     *
     * @param channel      音频 PCM 投递通道
     * @param sessionState 当前会话状态的读取口（生产为 {@code coordinator::current}）
     */
    public AudioCaptureScheduler(EncoderChannel channel, Supplier<SessionServer.SessionState> sessionState) {
        this(channel, SOFTLoopback::alcRenderSamplesSOFT, sessionState);
    }

    /**
     * 注入构造：自定义采样渲染出口（测试替身或替代环回实现用）
     *
     * @param channel        音频 PCM 投递通道
     * @param sampleRenderer 环回采样渲染出口；{@code null} 回落 {@code alcRenderSamplesSOFT}
     * @param sessionState   当前会话状态的读取口
     */
    public AudioCaptureScheduler(
            EncoderChannel channel,
            SampleRenderer sampleRenderer,
            Supplier<SessionServer.SessionState> sessionState
    ) {
        this.channel = channel;
        this.sampleRenderer = sampleRenderer != null ? sampleRenderer : SOFTLoopback::alcRenderSamplesSOFT;
        this.sessionState = Objects.requireNonNull(sessionState, "会话状态读取口不能为空");
    }

    /**
     * MC 音频 context 创建完成（Library.init 尾部钩子）：唤醒可能在就绪位上挂起的采集线程。
     * 就绪位本体在 {@link LoopbackBinding}，本方法只负责唤醒
     */
    public void onContextReady() {
        synchronized (stateLock) {
            stateLock.notifyAll();
        }
    }

    /**
     * MC 销毁音频库前的会合点：等在途的环回采样退出 renderLock，
     * 防止 native 调用撞上随后的 context/设备销毁
     */
    public void awaitRenderIdle() {
        synchronized (renderLock) {
        }
    }

    /**
     * 会话状态变更入口（协调器唤醒）：无会话停止投递（线程挂起待命）；
     * 会话在线即置位启动，内容暂停只停视频、音频照常
     */
    public void applySessionState() {
        SessionServer.SessionState state = this.sessionState.get();
        if (state == null) {
            this.isStreaming = false;
            return;
        }
        // 内容相位只管视频：最小化 / 无 framebuffer 时会话仍在线，音频不中断
        this.isStreaming = true;
        start();
        if (LoopbackBinding.current().contextReady()) {
            synchronized (stateLock) {
                // 客户端接入且上下文就绪，即刻唤醒
                stateLock.notifyAll();
            }
        }
    }

    /**
     * 启动 50Hz 高精度等时音频捕获守护线程；首个接入会话触发，幂等
     */
    public synchronized void start() {
        if (running) {
            return;
        }
        running = true;

        captureThread = new Thread(this::runCaptureLoop, "vistrel-audio-capture");
        captureThread.setDaemon(true);
        captureThread.setPriority(Thread.MAX_PRIORITY);
        captureThread.start();
    }

    /**
     * 终止音频捕获线程 (同步等待线程退出)；绑定正本在 {@link LoopbackBinding}，
     * 不随本类销毁
     */
    public void cleanup() {
        Thread threadToJoin;
        synchronized (this) {
            if (!running && captureThread == null) {
                return;
            }
            running = false;
            synchronized (stateLock) {
                stateLock.notifyAll();
            }
            threadToJoin = captureThread;
            captureThread = null;
            if (threadToJoin != null) {
                threadToJoin.interrupt();
            }
        }

        if (threadToJoin != null && threadToJoin != Thread.currentThread()) {
            try {
                // 等待采集线程安全退出执行栈 (20ms 节拍，500ms 超时足够长)
                threadToJoin.join(500);
                if (threadToJoin.isAlive()) {
                    Constants.LOG.warn("音频采集线程在 500ms 内未正常退出");
                }
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                Constants.LOG.warn("等待音频采集线程退出被中断");
            }
        }
    }

    private void runCaptureLoop() {
        long nextTickTimeNs = System.nanoTime();

        while (running) {
            // 1. 无推流客户端或音频上下文未就绪时挂起线程，避免无谓的 CPU 占用与上下文调用
            if (!this.isStreaming || !LoopbackBinding.current().contextReady()) {
                synchronized (stateLock) {
                    while (running && (!this.isStreaming || !LoopbackBinding.current().contextReady())) {
                        try {
                            stateLock.wait();
                        } catch (InterruptedException e) {
                            if (!running) {
                                break;
                            }
                        }
                    }
                }
                if (!running) {
                    break;
                }
                // 唤醒后重置时钟
                nextTickTimeNs = System.nanoTime();
            }
            // 2. 双缓冲轮替采样与通道投递（绑定快照每 tick 取一次，句柄与就绪位同源）
            LoopbackBinding binding = LoopbackBinding.current();
            long device = binding.device();
            if (device != 0L && binding.contextReady() && running) {
                ByteBuffer buffer = pingPongBuffers[pingPongIndex];
                pingPongIndex ^= 1;
                buffer.clear();

                try {
                    synchronized (renderLock) {
                        LoopbackBinding live = LoopbackBinding.current();
                        if (live.contextReady() && live.device() != 0L) {
                            // OpenAL C 引擎裸指针直写 3840 字节 PCM (零内存复制)
                            this.sampleRenderer.render(device, buffer, AUDIO_SAMPLES_PER_CHANNEL);
                        }
                    }
                    if (LoopbackBinding.current().contextReady()) {
                        buffer.position(0);
                        buffer.limit(AUDIO_FRAME_BYTES);
                        // 经由 EncoderChannel 投递有效 3840 字节音频 PCM
                        this.channel.pushAudioPcm(buffer, AUDIO_SAMPLES_PER_CHANNEL);
                    }
                } catch (Throwable t) {
                    Constants.LOG.warn("音频帧渲染或投递异常: {}", t.getMessage());
                }
            }
            // 3. 高精度 nanoTime 漂移补偿休眠
            nextTickTimeNs += TICK_INTERVAL_NS;
            long delayNs = nextTickTimeNs - System.nanoTime();
            if (delayNs > 0) {
                LockSupport.parkNanos(delayNs);
            } else if (delayNs < -100_000_000L) {
                // GC 停顿或系统卡顿落后超过 100ms 时重置时钟，避免急速赶帧产生破音
                nextTickTimeNs = System.nanoTime();
            }
        }
    }

    /**
     * MC OpenAL 环回绑定的进程级正本：设备句柄与 context 就绪位成对传递，
     * 生命周期对齐 MC 音频库，与 Vistrel 服务的启停换代无关。
     *
     * <p>收拢在本类下仅为音频域命名空间，正本不属于调度器：写入点仅 Mixin 三钩子
     * （设备开启 / context 创建完成 / 音频库销毁），引用原子换新保证句柄与就绪位
     * 永不撕裂；调度器只读快照，重启换代无需搬运状态。
     */
    public record LoopbackBinding(long device, boolean contextReady) {
        private static final LoopbackBinding EMPTY = new LoopbackBinding(0L, false);
        private static volatile LoopbackBinding current = EMPTY;

        /**
         * 环回设备已开启：新句柄的 context 必然尚未创建，就绪位归零
         */
        public static void open(long device) {
            current = new LoopbackBinding(device, false);
        }

        /**
         * context 创建完成并已激活（Library.init 尾部）
         */
        public static void markContextReady() {
            LoopbackBinding snapshot = current;
            current = new LoopbackBinding(snapshot.device, true);
        }

        /**
         * MC 音频库销毁：句柄与就绪位一并作废
         */
        public static void clear() {
            current = EMPTY;
        }

        /**
         * 当前绑定快照：调用方持局部引用再取字段，避免多次读取跨代不一致
         */
        public static LoopbackBinding current() {
            return current;
        }

        /**
         * 是否已绑定环回设备
         *
         * @return true 表示 {@link #device()} 为有效句柄（非 0）
         */
        public boolean isBound() {
            return device != 0L;
        }
    }
}

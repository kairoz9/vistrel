package kairo.vistrel.streaming.natives;

import kairo.vistrel.streaming.ControlStatus;
import kairo.vistrel.streaming.EncoderChannel.ControlException;
import kairo.vistrel.streaming.NativeResult;
import kairo.vistrel.streaming.VramBinding;

/**
 * 基于 JNI 的显存绑定：槽位表、在途位与心跳全在 Native，本类只持句柄、导入掩码与心跳基准。
 *
 * <p>构造由 {@link NativeEncoderChannel#bindVram} 完成；本类不持有 Vulkan 纹理或导出池。
 */
final class NativeVramBinding implements VramBinding {
    private static final long IN_FLIGHT_MASK = (1L << NativeWireContract.MASK_BITS) - 1;
    private static final long MAX_SLOT_MASK = (1L << NativeWireContract.MAX_VRAM_SLOTS) - 1;
    /**
     * 导入返回的绑定句柄；所有 JNI 调用携带它，Native 据此拒绝旧绑定。
     */
    private final long handle;
    /**
     * 本绑定实际导入的槽位掩码：只有这些索引可以取用。
     */
    private final long importedMask;
    /**
     * close 可能在停机线程发生，acquire/push 在渲染线程读，volatile 保证可见。
     */
    private volatile boolean closed = false;
    private ReleaseStatus releaseStatus = null;
    /**
     * 上次观察到的编码心跳；-1 表示尚未成功拉过快照。
     */
    private long lastHeartbeat = -1L;
    /**
     * 上次心跳推进的时刻（构造时刻为初始基准）。
     */
    private long lastProgressNanos = System.nanoTime();

    /**
     * @param handle       导入成功返回的绑定句柄，必须为正
     * @param importedMask 本次实际导入的槽位掩码，非空且不超出协议容量
     */
    NativeVramBinding(long handle, long importedMask) {
        if (handle <= 0) {
            throw new IllegalArgumentException("显存绑定句柄非法: " + handle);
        }
        if (importedMask == 0L || (importedMask & ~MAX_SLOT_MASK) != 0L) {
            throw new IllegalArgumentException("槽位掩码非法: " + importedMask);
        }
        this.handle = handle;
        this.importedMask = importedMask;
    }

    @Override
    public int acquire(int preferredIdx) {
        if (closed) {
            throw new IllegalStateException("显存绑定已关闭，拒绝取槽");
        }
        long nowNanos = System.nanoTime();
        long reply = NativeBridge.vramSlotState(handle);
        ControlStatus status = statusOf(reply);
        if (status == null) {
            throw new ControlException("NativeBridge.vramSlotState 返回未知状态", ControlStatus.PIPELINE_DEAD);
        }
        if (status == ControlStatus.STALE_BINDING || status == ControlStatus.PIPELINE_DEAD) {
            // 旧绑定/引擎不在：不刷新心跳基准，让停滞判定在阈值后接管
            return BINDING_UNAVAILABLE;
        }
        if (status != ControlStatus.OK) {
            throw new ControlException("NativeBridge.vramSlotState 失败", status);
        }
        observe(heartbeatOf(reply), nowNanos);
        long free = ~inFlightOf(reply) & importedMask;
        return pickSlot(preferredIdx, free);
    }

    @Override
    public boolean push(int slot, long signalVal) {
        if (closed) {
            throw new IllegalStateException("显存绑定已关闭，拒绝投递");
        }
        int reply = NativeBridge.pushVramSlot(handle, slot, signalVal);
        ControlStatus status = statusOf(reply);
        if (status == null) {
            throw new ControlException("NativeBridge.pushVramSlot 返回未知状态", ControlStatus.PIPELINE_DEAD);
        }
        if (status == ControlStatus.OK) {
            return true;
        }
        if (status == ControlStatus.STALE_BINDING || status == ControlStatus.PIPELINE_DEAD) {
            // 帧被入口丢弃，Native 未置位；换代由调度器 rebind
            return false;
        }
        throw new ControlException("NativeBridge.pushVramSlot 失败", status);
    }

    @Override
    public boolean heartbeatTimedOut() {
        return heartbeatTimedOut(System.nanoTime());
    }

    /**
     * 心跳超时的显式时刻入口，供测试注入时钟。
     */
    boolean heartbeatTimedOut(long nowNanos) {
        return !closed && nowNanos - lastProgressNanos >= STALL_HEARTBEAT_TIMEOUT_NS;
    }

    @Override
    public String stallDiagnostics() {
        String label = NativeBridge.stallDiagnostics(handle);
        return label == null || label.isEmpty() ? "未知阶段" : label;
    }

    @Override
    public synchronized ReleaseStatus close() {
        if (closed) {
            return releaseStatus;
        }
        closed = true;
        if (!NativeBridge.isLoaded()) {
            // Native 库未加载就没有导入方，游戏侧资源可放心释放
            releaseStatus = ReleaseStatus.RELEASED;
            return releaseStatus;
        }
        NativeResult result;
        try {
            result = NativeResult.parse(NativeBridge.clearVramSlots(handle));
        } catch (Throwable t) {
            // JNI 异常同样意味着未获确认：资源必须保留，重复 close 也返回同一结算
            releaseStatus = ReleaseStatus.RETAINED;
            throw t;
        }
        boolean confirmed = result.ok()
                && result.data() != null
                && result.data().isJsonPrimitive()
                && result.data().getAsInt() == ControlStatus.OK.code();
        releaseStatus = confirmed ? ReleaseStatus.RELEASED : ReleaseStatus.RETAINED;
        return releaseStatus;
    }

    /**
     * 心跳推进即刷新停滞基准；同一心跳重复观察不刷新。
     */
    void observe(long heartbeat, long nowNanos) {
        if (heartbeat != lastHeartbeat) {
            lastHeartbeat = heartbeat;
            lastProgressNanos = nowNanos;
        }
    }

    /**
     * push 回包的结果码：0 是 JNI 异常默认值，返回 null。
     */
    static ControlStatus statusOf(int raw) {
        if (raw == 0) {
            return null;
        }
        return ControlStatus.fromCode(raw - 1);
    }

    /**
     * packed 快照里的结果码；0 返回 null。
     */
    static ControlStatus statusOf(long reply) {
        return statusOf((int) (reply >>> NativeWireContract.STATUS_SHIFT));
    }

    /**
     * packed 快照里的在途掩码。
     */
    static long inFlightOf(long reply) {
        return reply & IN_FLIGHT_MASK;
    }

    /**
     * packed 快照里的心跳计数。
     */
    static long heartbeatOf(long reply) {
        return (reply >>> NativeWireContract.HEARTBEAT_SHIFT) & 0xFFFF_FFFFL;
    }

    /**
     * 在导入槽位里选空闲位：首选可用则取首选，否则取最低空闲位。
     *
     * @param preferredIdx 首选索引；非法或忙时忽略
     * @param freeMask     空闲位图
     * @return 槽位索引；全忙返回 {@link VramBinding#NO_FREE_SLOT}
     */
    int pickSlot(int preferredIdx, long freeMask) {
        long free = freeMask & importedMask;
        if (free == 0) {
            return NO_FREE_SLOT;
        }
        if (preferredIdx >= 0
                && preferredIdx < NativeWireContract.MAX_VRAM_SLOTS
                && (free & (1L << preferredIdx)) != 0) {
            return preferredIdx;
        }
        return Long.numberOfTrailingZeros(free);
    }
}

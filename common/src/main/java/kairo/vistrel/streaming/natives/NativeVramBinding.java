package kairo.vistrel.streaming.natives;

import kairo.vistrel.streaming.ControlStatus;
import kairo.vistrel.streaming.EncoderChannel.ControlException;
import kairo.vistrel.streaming.NativeResult;
import kairo.vistrel.streaming.VramBinding;

/**
 * 基于 JNI 的显存绑定：槽位表与在途状态全在 Native，本类只持一个不透明句柄。
 *
 * <p>构造由 {@link NativeEncoderChannel#bindVram} 完成；本类不持有 Vulkan 纹理或导出池。
 * {@link #acquire(int, long)} 每次拉一次权威快照（在途掩码 + 心跳）；未投递的槽位在
 * Native 从未置位，因此不存在需要归还的本地账。
 */
final class NativeVramBinding implements VramBinding {
    private static final long IN_FLIGHT_MASK = (1L << NativeWireContract.MASK_BITS) - 1;
    /**
     * 导入时取得的绑定句柄：推取/投递/注销都带上它，Native 据此拒绝旧绑定。
     */
    private final long handle;
    /**
     * 本绑定实际导入的槽位掩码。
     */
    private final long slotMask;
    private final int slotCount;
    /**
     * 关闭标志：close 可能在停机线程发生，acquire/push 在渲染线程读，volatile 保证可见。
     * 注销结算本身由 close 的同步块保证幂等。
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
     * @param handle    导入成功返回的绑定句柄，必须为正
     * @param slotCount 本次导入的槽位数，1..{@link NativeWireContract#MAX_VRAM_SLOTS}
     */
    NativeVramBinding(long handle, int slotCount) {
        if (handle <= 0) {
            throw new IllegalArgumentException("显存绑定句柄非法: " + handle);
        }
        if (slotCount <= 0 || slotCount > NativeWireContract.MAX_VRAM_SLOTS) {
            throw new IllegalArgumentException("槽位数量越界: " + slotCount);
        }
        this.handle = handle;
        this.slotCount = slotCount;
        this.slotMask = (1L << slotCount) - 1;
    }

    @Override
    public int acquire(int preferredIdx, long nowNanos) {
        if (closed) {
            throw new IllegalStateException("显存绑定已关闭，拒绝取槽");
        }
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
        long free = ~inFlightOf(reply) & slotMask;
        return pickSlot(preferredIdx, free, slotCount);
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
    public boolean heartbeatTimedOut(long nowNanos) {
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
     *
     * @param heartbeat 本 tick 快照里的心跳
     * @param nowNanos  本帧开始时刻
     */
    void observe(long heartbeat, long nowNanos) {
        if (heartbeat != lastHeartbeat) {
            lastHeartbeat = heartbeat;
            lastProgressNanos = nowNanos;
        }
    }

    /**
     * push 回复里的结果码：Native 回送 {@code code + 1}。
     *
     * @param raw Native 原始返回值
     * @return 状态；0（JNI 异常默认值）返回 null，调用方必须按故障处理
     */
    static ControlStatus statusOf(int raw) {
        if (raw == 0) {
            return null;
        }
        return ControlStatus.fromCode(raw - 1);
    }

    /**
     * packed 快照回复里的结果码；0（JNI 异常默认值）返回 null。
     *
     * @param reply packed 回复
     * @return 状态；未知返回 null
     */
    static ControlStatus statusOf(long reply) {
        return statusOf((int) (reply >>> NativeWireContract.STATUS_SHIFT));
    }

    /**
     * packed 回复里的在途掩码。
     *
     * @param reply packed 回复
     * @return 在途位图
     */
    static long inFlightOf(long reply) {
        return reply & IN_FLIGHT_MASK;
    }

    /**
     * packed 回复里的心跳计数。
     *
     * @param reply packed 回复
     * @return 单调心跳
     */
    static long heartbeatOf(long reply) {
        return (reply >>> NativeWireContract.HEARTBEAT_SHIFT) & 0xFFFF_FFFFL;
    }

    /**
     * 在空闲掩码里选槽：首选优先，忙则按索引序降级。
     *
     * @param preferredIdx 首选槽位索引；非法值直接降级
     * @param freeMask     空闲位图
     * @param slotCount    本绑定槽位数
     * @return 槽位索引；全忙返回 {@link VramBinding#NO_FREE_SLOT}
     */
    static int pickSlot(int preferredIdx, long freeMask, int slotCount) {
        if (preferredIdx >= 0 && preferredIdx < slotCount && (freeMask & (1L << preferredIdx)) != 0) {
            return preferredIdx;
        }
        for (int i = 0; i < slotCount; i++) {
            if ((freeMask & (1L << i)) != 0) {
                return i;
            }
        }
        return NO_FREE_SLOT;
    }
}

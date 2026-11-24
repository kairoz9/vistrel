package kairo.vistrel.streaming.natives;

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import kairo.vistrel.streaming.VramBinding;

/**
 * 游戏侧渲染线程与编码 Actor 之间共享的槽位所有权块（Java 视图）。
 *
 * <p>布局与偏移见 {@link NativeWireContract}；访问全部走 acquire/release 与 CAS。
 * 块由 Native 引擎按实例泄漏分配，随引擎换代；本类不得缓存跨引擎复用。
 *
 * <p>用直接 {@link ByteBuffer} 而不是 {@code java.lang.foreign.MemorySegment}：本类
 * 在 1.20（Java 17）到 26.3（Java 25）的所有版本节点上都要编译，FFM 只在 22+ 可用。
 */
final class SlotStateBlock {
    private static final long BUSY_STRIDE = Integer.BYTES;
    private static final VarHandle BUSY_HANDLE =
            MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.nativeOrder());
    private static final VarHandle HEARTBEAT_HANDLE =
            MethodHandles.byteBufferViewVarHandle(long[].class, ByteOrder.nativeOrder());
    private static final VarHandle STAGE_HANDLE =
            MethodHandles.byteBufferViewVarHandle(long[].class, ByteOrder.nativeOrder());
    private static final VarHandle BINDING_EPOCH_HANDLE =
            MethodHandles.byteBufferViewVarHandle(long[].class, ByteOrder.nativeOrder());
    private final ByteBuffer block;
    private final int slotCount;
    /**
     * 心跳基准：push 成功时记录的心跳值与时刻（仅渲染线程写）
     */
    private long snapshotHeartbeat;
    private long snapshotNanos;

    /**
     * 包装 Native 共享块。
     *
     * @param block     直接内存块，容量不得小于 {@link NativeWireContract#SLOT_STATE_BLOCK_BYTES}
     * @param slotCount 本管线实际使用的槽位数，1..{@link NativeWireContract#SLOT_STATE_SLOTS}
     */
    public SlotStateBlock(ByteBuffer block, int slotCount) {
        if (block == null) {
            throw new IllegalArgumentException("槽位所有权块不能为空");
        }
        if (!block.isDirect()) {
            throw new IllegalArgumentException("槽位所有权块必须是直接内存");
        }
        if (block.capacity() < NativeWireContract.SLOT_STATE_BLOCK_BYTES) {
            throw new IllegalArgumentException(
                    "槽位所有权块尺寸不足: " + block.capacity() + " < " + NativeWireContract.SLOT_STATE_BLOCK_BYTES
            );
        }
        if (slotCount <= 0 || slotCount > NativeWireContract.SLOT_STATE_SLOTS) {
            throw new IllegalArgumentException("槽位数量越界: " + slotCount);
        }
        this.block = block;
        this.slotCount = slotCount;
        // 构造时记下当前心跳与时刻：心跳推进即比对不上，真卡死满 1s 触发
        this.snapshotHeartbeat = heartbeat();
        this.snapshotNanos = System.nanoTime();
    }

    /**
     * 认领一个空闲槽位：CAS 0→1 原子占位，轮转优先、索引序降级；全忙返回 -1。
     *
     * @param preferredIdx 首选槽位索引
     * @return 认领的槽位索引；全忙 -1 对应主动丢帧 (Dropped)，不是故障 (Failed)
     */
    public int claim(int preferredIdx) {
        if (preferredIdx >= 0 && preferredIdx < slotCount && tryClaim(preferredIdx)) {
            return preferredIdx;
        }
        for (int i = 0; i < slotCount; i++) {
            if (tryClaim(i)) {
                return i;
            }
        }
        return -1;
    }

    /**
     * 渲染线程异常路径归还槽位（正常归还由 Actor 在编码出口完成，幂等）。
     *
     * @param slotIdx 槽位索引
     */
    public void release(int slotIdx) {
        BUSY_HANDLE.setRelease(block, (int) (NativeWireContract.BUSY_OFFSET + slotIdx * BUSY_STRIDE), 0);
    }

    /**
     * push 成功后记录心跳基准；时刻用本帧开头那个。
     *
     * @param nowNanos 本帧开始时刻
     */
    public void markPushed(long nowNanos) {
        this.snapshotHeartbeat = heartbeat();
        this.snapshotNanos = nowNanos;
    }

    /**
     * 心跳超时判定（每次采集 tick 都执行，含全忙丢帧的 tick）。
     *
     * @param nowNanos 当前时刻
     * @return true 表示心跳停在基准值且超过时限
     */
    public boolean heartbeatTimedOut(long nowNanos) {
        return heartbeat() == snapshotHeartbeat
                && nowNanos - snapshotNanos >= VramBinding.STALL_HEARTBEAT_TIMEOUT_NS;
    }

    /**
     * 编码线程最后进入的阶段名。
     *
     * @return 阶段中文名；未知码原样展示
     */
    public String stageName() {
        long code = (long) STAGE_HANDLE.getAcquire(block, NativeWireContract.STAGE_OFFSET);
        return NativeWireContract.Stage.describe((int) code);
    }

    /**
     * 当前显存绑定轮次：导入、推送与注销的准入令牌。
     *
     * @return 共享块中的绑定轮次；0 表示尚未建立编码器
     */
    public long bindingEpoch() {
        return (long) BINDING_EPOCH_HANDLE.getAcquire(block, NativeWireContract.BINDING_EPOCH_OFFSET);
    }

    private boolean tryClaim(int slotIdx) {
        return (int) BUSY_HANDLE.compareAndExchange(
                block,
                (int) (NativeWireContract.BUSY_OFFSET + slotIdx * BUSY_STRIDE),
                0,
                1
        ) == 0;
    }

    private long heartbeat() {
        return (long) HEARTBEAT_HANDLE.getAcquire(block, NativeWireContract.HEARTBEAT_OFFSET);
    }
}

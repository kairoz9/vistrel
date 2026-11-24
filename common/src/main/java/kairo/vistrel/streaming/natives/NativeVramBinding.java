package kairo.vistrel.streaming.natives;

import kairo.vistrel.streaming.ControlStatus;
import kairo.vistrel.streaming.NativeResult;
import kairo.vistrel.streaming.VramBinding;

/**
 * 基于 JNI 的显存绑定：槽位表已整批导入，持有一份共享所有权块视图与导入时的绑定轮次。
 *
 * <p>构造由 {@link NativeEncoderChannel#bindVram} 完成；本类只做投递、认领与注销，
 * 不持有 Vulkan 纹理或导出池（那些归采集侧的导出集）。
 */
final class NativeVramBinding implements VramBinding {
    private final SlotStateBlock stateBlock;
    /**
     * 导入时的显存绑定轮次：推送与注销都带上它，Native 据此拒绝旧绑定的操作。
     */
    private final long bindingEpoch;
    /**
     * 关闭标志：close 可能在停机线程发生，push 在渲染线程读，volatile 保证关闭可见。
     * 注销结算本身由 close 的同步块保证幂等。
     */
    private volatile boolean closed = false;
    private ReleaseStatus releaseStatus = null;

    NativeVramBinding(SlotStateBlock stateBlock, long bindingEpoch) {
        this.stateBlock = stateBlock;
        this.bindingEpoch = bindingEpoch;
    }

    @Override
    public void push(int slotIdx, long signalVal) {
        if (closed) {
            throw new IllegalStateException("显存绑定已关闭，拒绝投递");
        }
        NativeBridge.pushVramSlot(bindingEpoch, slotIdx, signalVal);
    }

    @Override
    public int claim(int preferredIdx) {
        return stateBlock.claim(preferredIdx);
    }

    @Override
    public void release(int slotIdx) {
        stateBlock.release(slotIdx);
    }

    @Override
    public void markPushed(long nowNanos) {
        stateBlock.markPushed(nowNanos);
    }

    @Override
    public boolean heartbeatTimedOut(long nowNanos) {
        return stateBlock.heartbeatTimedOut(nowNanos);
    }

    @Override
    public String stageName() {
        return stateBlock.stageName();
    }

    @Override
    public long bindingEpoch() {
        return bindingEpoch;
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
            result = NativeResult.parse(NativeBridge.clearVramSlots(bindingEpoch));
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
}

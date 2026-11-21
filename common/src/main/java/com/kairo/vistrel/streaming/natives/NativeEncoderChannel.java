package com.kairo.vistrel.streaming.natives;

import com.kairo.vistrel.input.InputEventSource;
import com.kairo.vistrel.streaming.EncoderChannel;
import java.nio.ByteBuffer;

/**
 * 基于底层 JNI NativeBridge 的推流数据通道默认实现
 */
public final class NativeEncoderChannel implements EncoderChannel, InputEventSource {
    @Override
    public void pushSystemFrame(ByteBuffer buffer) {
        NativeBridge.pushSystemFrame(buffer);
    }

    @Override
    public void pushAudioPcm(ByteBuffer buffer, int samplesPerChannel) {
        NativeBridge.pushAudioPcm(buffer, samplesPerChannel);
    }

    @Override
    public int pollInputEvents(ByteBuffer buffer, int maxEvents) {
        if (!NativeBridge.isLoaded()) {
            return 0;
        }
        return NativeBridge.pollInputEvents(buffer, maxEvents);
    }

    @Override
    public String caps() {
        if (!NativeBridge.isLoaded()) {
            return "";
        }
        return NativeBridge.queryEncoderCaps();
    }

    @Override
    public SlotLease importVramSlots(long[] slots) {
        if (!NativeBridge.isLoaded()) {
            throw new IllegalStateException("Native 库未加载，无法注册显存槽位");
        }
        int code = NativeBridge.importVramSlots(slots);
        if (code != EncoderChannel.CONTROL_OK) {
            throw new EncoderChannel.ControlException(
                    "NativeBridge.importVramSlots 失败 (整批已回滚): " + EncoderChannel.describeControlResult(code) + " (" + code + ")",
                    code
            );
        }
        return new NativeSlotLease();
    }

    @Override
    public ByteBuffer slotStateBlock() {
        if (!NativeBridge.isLoaded()) {
            return null;
        }
        return NativeBridge.slotStateBlock();
    }

    /**
     * 持有投递与注销义务的原生槽位租约
     */
    private static final class NativeSlotLease implements SlotLease {
        private volatile boolean closed = false;
        /**
         * close() 之后有意义：注销是否得到原生确认（可否销毁导出资源的处置契约
         * 在 {@code SlotLease.isReleased} 定义一次）
         */
        private volatile boolean releaseAcked = false;

        @Override
        public void push(int slotIdx, long signalVal) {
            if (closed) {
                throw new IllegalStateException("显存槽位租约已关闭，拒绝投递");
            }
            NativeBridge.pushVramSlot(slotIdx, signalVal);
        }

        @Override
        public void close() {
            if (closed) {
                return;
            }
            closed = true;
            if (!NativeBridge.isLoaded()) {
                // Native 库未加载就没有导入方，游戏侧资源可放心释放
                releaseAcked = true;
                return;
            }
            releaseAcked = NativeBridge.clearVramSlots() == EncoderChannel.CONTROL_OK;
        }

        @Override
        public boolean isReleased() {
            return releaseAcked;
        }
    }
}

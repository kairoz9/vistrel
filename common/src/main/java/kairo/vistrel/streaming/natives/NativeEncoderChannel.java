package kairo.vistrel.streaming.natives;

import kairo.vistrel.input.InputEventSource;
import kairo.vistrel.streaming.ControlStatus;
import kairo.vistrel.streaming.EncoderCaps;
import kairo.vistrel.streaming.EncoderChannel;
import kairo.vistrel.streaming.VramBinding;
import java.nio.ByteBuffer;
import java.util.List;

/**
 * 基于底层 JNI NativeBridge 的推流数据通道默认实现。
 */
public final class NativeEncoderChannel implements EncoderChannel, InputEventSource {
    @Override
    public void pushSystemFrame(ByteBuffer buffer, int width, int height) {
        NativeBridge.pushSystemFrame(buffer, width, height);
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

    /**
     * 能力快照缓存：探测结果在进程生命周期内不变，解析一次长期复用
     *
     * <p>
     * 只在已探测时落地。未探测意味着引擎还没启动，缓存它会让后续调用永远读到未探测
     * ——显存直通门控在启动早期求值时就撞这个现场，必须下次再问。
     */
    private volatile EncoderCaps cachedCaps = null;

    @Override
    public EncoderCaps caps() {
        EncoderCaps cached = cachedCaps;
        if (cached != null) {
            return cached;
        }
        if (!NativeBridge.isLoaded()) {
            return EncoderCaps.EMPTY;
        }
        EncoderCaps parsed = EncoderCaps.parse(NativeBridge.queryEncoderCaps());
        if (parsed.probed()) {
            cachedCaps = parsed;
        }
        return parsed;
    }

    /**
     * 把类型化槽位描述编码成 JNI 的 7 元组扁平数组。
     *
     * <p>包私有供测试直接钉住字段序，不依赖 JNI。
     *
     * @param slots 槽位描述
     * @return 每槽位 {@link NativeWireContract#VRAM_SLOT_STRIDE} 个 long 的扁平数组
     */
    static long[] encodeSlots(List<VramSlotDescriptor> slots) {
        long[] flat = new long[slots.size() * NativeWireContract.VRAM_SLOT_STRIDE];
        int base = 0;
        for (VramSlotDescriptor slot : slots) {
            flat[base] = slot.slotIdx();
            flat[base + 1] = slot.memHandle();
            flat[base + 2] = slot.semHandle();
            flat[base + 3] = slot.width();
            flat[base + 4] = slot.height();
            flat[base + 5] = slot.sizeBytes();
            flat[base + 6] = slot.offset();
            base += NativeWireContract.VRAM_SLOT_STRIDE;
        }
        return flat;
    }

    @Override
    public VramBinding bindVram(List<VramSlotDescriptor> slots) {
        if (!NativeBridge.isLoaded()) {
            throw new IllegalStateException("Native 库未加载，无法注册显存槽位");
        }
        if (slots == null || slots.isEmpty()) {
            throw new IllegalArgumentException("显存槽位描述不能为空");
        }
        if (slots.size() > NativeWireContract.SLOT_STATE_SLOTS) {
            throw new IllegalArgumentException(
                    "显存槽位数量超过共享块容量: " + slots.size() + " > " + NativeWireContract.SLOT_STATE_SLOTS
            );
        }
        // 先取块再导入：块无效时在导入前失败，不会留下需要回滚的 Native 槽位表。
        // 绑定轮次也取自同一块；导入期间编码器重建时 Native 回 StaleBinding，重读一次再试
        long[] flat = encodeSlots(slots);
        for (int attempt = 0; ; attempt++) {
            ByteBuffer stateBlock = NativeBridge.slotStateBlock();
            if (stateBlock == null
                    || !stateBlock.isDirect()
                    || stateBlock.capacity() < NativeWireContract.SLOT_STATE_BLOCK_BYTES) {
                throw new IllegalStateException("Native 未提供有效的槽位所有权块，显存直通不可用");
            }
            SlotStateBlock blockView = new SlotStateBlock(stateBlock, slots.size());
            long bindingEpoch = blockView.bindingEpoch();
            int code = NativeBridge.importVramSlots(bindingEpoch, flat);
            if (code == ControlStatus.OK.code()) {
                return new NativeVramBinding(blockView, bindingEpoch);
            }
            if (code == ControlStatus.STALE_BINDING.code() && attempt == 0) {
                // 读取绑定轮次与导入之间编码器重建：重读当前轮次再导入一次
                continue;
            }
            throw new EncoderChannel.ControlException("NativeBridge.importVramSlots 失败", code);
        }
    }
}

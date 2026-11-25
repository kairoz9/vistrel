package kairo.vistrel.streaming.natives;

import kairo.vistrel.input.InputEventSource;
import kairo.vistrel.streaming.ControlStatus;
import kairo.vistrel.streaming.EncoderCaps;
import kairo.vistrel.streaming.EncoderChannel;
import kairo.vistrel.streaming.NativeResult;
import kairo.vistrel.streaming.VramBinding;
import com.google.gson.JsonObject;
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
        NativeResult result = NativeResult.parse(NativeBridge.queryEncoderCaps());
        if (!result.ok() || result.data() == null) {
            return EncoderCaps.EMPTY;
        }
        EncoderCaps parsed = EncoderCaps.parse(result.data().toString());
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
        if (slots.size() > NativeWireContract.MAX_VRAM_SLOTS) {
            throw new IllegalArgumentException(
                    "显存槽位数量超过掩码容量: " + slots.size() + " > " + NativeWireContract.MAX_VRAM_SLOTS
            );
        }
        // 导入与句柄由 Native 一次给出；失败即整条显存管线失败，原始句柄已被 Native 消费。
        long[] flat = encodeSlots(slots);
        NativeResult result = NativeResult.parse(NativeBridge.importVramSlots(flat));
        if (!result.ok() || result.data() == null || !result.data().isJsonObject()) {
            throw new EncoderChannel.ControlException(
                    result.message() != null ? result.message() : "显存槽位导入失败",
                    ControlStatus.PIPELINE_DEAD
            );
        }
        JsonObject data = result.data().getAsJsonObject();
        if (!data.has("code") || !data.get("code").isJsonPrimitive()) {
            throw new EncoderChannel.ControlException("显存槽位导入结果缺少结果码", ControlStatus.PIPELINE_DEAD);
        }
        int code = data.get("code").getAsInt();
        if (code != ControlStatus.OK.code()) {
            throw new EncoderChannel.ControlException("NativeBridge.importVramSlots 失败", code);
        }
        if (!data.has("handle") || !data.get("handle").isJsonPrimitive()) {
            throw new EncoderChannel.ControlException("显存槽位导入结果缺少句柄", ControlStatus.PIPELINE_DEAD);
        }
        long handle = data.get("handle").getAsLong();
        return new NativeVramBinding(handle, slots.size());
    }
}

package kairo.vistrel.streaming.natives;

import kairo.vistrel.streaming.ControlStatus;
import kairo.vistrel.streaming.VramBinding;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * 显存绑定 Java 侧的纯逻辑契约：packed 回复解析、选槽与心跳基准。
 * 不加载 Native 库，所有断言都只依赖线协议与服务端无关的本地状态。
 */
class NativeVramBindingTest {
    private static final long TIMEOUT = VramBinding.STALL_HEARTBEAT_TIMEOUT_NS;

    private static long reply(ControlStatus status, long inFlight, long heartbeat) {
        return (((long) status.code() + 1) << NativeWireContract.STATUS_SHIFT)
                | ((heartbeat & 0xFFFF_FFFFL) << NativeWireContract.HEARTBEAT_SHIFT)
                | (inFlight & 0xFFL);
    }

    @Test
    void testUnknownZeroReplyIsNotSuccess() {
        Assertions.assertNull(NativeVramBinding.statusOf(0), "JNI 异常默认值 0 必须按未知/故障处理，不能读成 Ok");
    }

    @Test
    void testPackedReplyRoundTrip() {
        long packed = reply(ControlStatus.OK, 0b101L, 42L);
        Assertions.assertEquals(ControlStatus.OK, NativeVramBinding.statusOf(packed));
        Assertions.assertEquals(0b101L, NativeVramBinding.inFlightOf(packed));
        Assertions.assertEquals(42L, NativeVramBinding.heartbeatOf(packed));

        long error = reply(ControlStatus.SLOT_NOT_FREE, 0, 0);
        Assertions.assertEquals(ControlStatus.SLOT_NOT_FREE, NativeVramBinding.statusOf(error));
    }

    @Test
    void testInFlightMaskKeepsOnlyLowBits() {
        long packed = reply(ControlStatus.OK, 0xFFL, 1L);
        Assertions.assertEquals(0xFFL, NativeVramBinding.inFlightOf(packed));
    }

    @Test
    void testPickSlotPrefersThenFallsBack() {
        Assertions.assertEquals(1, NativeVramBinding.pickSlot(1, 0b011L, 3), "首选空闲必须直接取用");
        Assertions.assertEquals(0, NativeVramBinding.pickSlot(2, 0b011L, 3), "首选忙时按索引序降级");
        Assertions.assertEquals(2, NativeVramBinding.pickSlot(-1, 0b100L, 3), "非法首选退化为索引序");
        Assertions.assertEquals(2, NativeVramBinding.pickSlot(9, 0b100L, 3), "越界首选同样退化");
        Assertions.assertEquals(-1, NativeVramBinding.pickSlot(0, 0L, 3), "全忙返回 -1");
    }

    @Test
    void testPickSlotNeverEscapesSlotCount() {
        Assertions.assertEquals(-1, NativeVramBinding.pickSlot(5, 1L << 5, 3), "槽位数外的高位不得被选中");
    }

    @Test
    void testHeartbeatBaselineAdvancesOnlyOnProgress() {
        NativeVramBinding binding = new NativeVramBinding(1, 3);
        long t0 = 1_000_000_000L;
        binding.observe(7, t0);
        Assertions.assertFalse(binding.heartbeatTimedOut(t0 + TIMEOUT - 1), "未达阈值不触发");
        Assertions.assertTrue(binding.heartbeatTimedOut(t0 + TIMEOUT), "冻结满阈值必须触发");

        binding.observe(7, t0 + TIMEOUT + 200_000_000L);
        Assertions.assertTrue(binding.heartbeatTimedOut(t0 + TIMEOUT + 900_000_000L), "同一心跳重复观察不得刷新基准");
        binding.observe(8, t0 + TIMEOUT + 900_000_000L);
        Assertions.assertFalse(binding.heartbeatTimedOut(t0 + TIMEOUT + 900_000_000L + TIMEOUT - 1), "心跳推进后重新计时");
    }

    @Test
    void testConstructorRejectsInvalidHandleOrSlotCount() {
        Assertions.assertThrows(IllegalArgumentException.class, () -> new NativeVramBinding(0, 3));
        Assertions.assertThrows(IllegalArgumentException.class, () -> new NativeVramBinding(1, 0));
        Assertions.assertThrows(IllegalArgumentException.class, () -> new NativeVramBinding(
                1,
                NativeWireContract.MAX_VRAM_SLOTS + 1
        ));
    }
}

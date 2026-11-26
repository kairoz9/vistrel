package kairo.vistrel.streaming.natives;

import kairo.vistrel.streaming.ControlStatus;
import kairo.vistrel.streaming.VramBinding;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * 显存绑定 Java 侧纯逻辑：packed 解析、选槽与心跳基准；不加载 Native 库。
 */
class NativeVramBindingTest {
    private static final long TIMEOUT = VramBinding.STALL_HEARTBEAT_TIMEOUT_NS;

    private static long reply(ControlStatus status, long inFlight, long heartbeat) {
        return (((long) status.code() + 1) << NativeWireContract.STATUS_SHIFT)
                | ((heartbeat & 0xFFFF_FFFFL) << NativeWireContract.HEARTBEAT_SHIFT)
                | (inFlight & 0xFFL);
    }

    @Test
    void testPushStatusIsCodePlusOne() {
        Assertions.assertNull(NativeVramBinding.statusOf(0), "JNI 异常默认值 0 不能读成 Ok");
        Assertions.assertEquals(ControlStatus.OK, NativeVramBinding.statusOf(ControlStatus.OK.code() + 1), "push 只回状态码");
        Assertions.assertEquals(ControlStatus.POISONED, NativeVramBinding.statusOf(ControlStatus.POISONED.code() + 1));
        Assertions.assertNull(NativeVramBinding.statusOf(200), "表外码不映射到枚举");
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

        long poisoned = reply(ControlStatus.POISONED, 0, 0);
        Assertions.assertEquals(ControlStatus.POISONED, NativeVramBinding.statusOf(poisoned));
    }

    @Test
    void testPackedReplyLayoutGolden() {
        Assertions.assertEquals(
                0x0100001234567805L,
                reply(ControlStatus.OK, 0b101L, 0x12345678L),
                "Java 与 Rust 必须共用同一固定位布局"
        );
    }

    @Test
    void testInFlightMaskKeepsOnlyLowBits() {
        long packed = reply(ControlStatus.OK, 0xFFL, 1L);
        Assertions.assertEquals(0xFFL, NativeVramBinding.inFlightOf(packed));
    }

    @Test
    void testPickSlotPrefersThenFallsBack() {
        NativeVramBinding binding = new NativeVramBinding(1, 0b111L);
        Assertions.assertEquals(1, binding.pickSlot(1, 0b011L), "首选空闲必须直接取用");
        Assertions.assertEquals(0, binding.pickSlot(2, 0b011L), "首选忙时按索引序降级");
        Assertions.assertEquals(2, binding.pickSlot(-1, 0b100L), "非法首选退化为索引序");
        Assertions.assertEquals(2, binding.pickSlot(9, 0b100L), "越界首选同样退化");
        Assertions.assertEquals(VramBinding.NO_FREE_SLOT, binding.pickSlot(0, 0L), "全忙返回无空闲哨兵");
    }

    @Test
    void testPickSlotNeverEscapesImportedMask() {
        NativeVramBinding binding = new NativeVramBinding(1, 0b111L);
        Assertions.assertEquals(VramBinding.NO_FREE_SLOT, binding.pickSlot(5, 1L << 5), "导入掩码外的高位不得被选中");
    }

    @Test
    void testUnavailableSentinelIsDistinctFromNoFreeSlot() {
        Assertions.assertNotEquals(VramBinding.NO_FREE_SLOT, VramBinding.BINDING_UNAVAILABLE);
    }

    @Test
    void testHeartbeatBaselineAdvancesOnlyOnProgress() {
        NativeVramBinding binding = new NativeVramBinding(1, 0b111L);
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
    void testImportedMaskRejectsUnimportedSlotBits() {
        NativeVramBinding binding = new NativeVramBinding(1, 0b101L);
        Assertions.assertEquals(0, binding.pickSlot(-1, 0b101L));
        Assertions.assertEquals(2, binding.pickSlot(-1, 0b100L));
        Assertions.assertEquals(VramBinding.NO_FREE_SLOT, binding.pickSlot(1, 0b010L), "未导入的槽位不得被选中");
    }

    @Test
    void testConstructorRejectsInvalidHandleOrMask() {
        Assertions.assertThrows(IllegalArgumentException.class, () -> new NativeVramBinding(0, 0b111L));
        Assertions.assertThrows(IllegalArgumentException.class, () -> new NativeVramBinding(1, 0L));
        Assertions.assertThrows(IllegalArgumentException.class, () -> new NativeVramBinding(
                1,
                1L << NativeWireContract.MAX_VRAM_SLOTS
        ));
    }
}

package kairo.vistrel.streaming;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * 控制结果码表与 Rust 侧 {@code ControlStatus} 的判别值一一对应。
 *
 * <p>项目不做跨语言自动 gate：数值在这里逐个钉死，供 Java 侧消费；
 * 修改 Rust 枚举时必须同步本表与 {@code NativeWireContract}。
 */
public class ControlCodeTableTest {
    @Test
    void testCodesMatchRustControlErrorDiscriminants() {
        Assertions.assertEquals(0, ControlStatus.OK.code());
        Assertions.assertEquals(1, ControlStatus.ENCODER_UNAVAILABLE.code());
        Assertions.assertEquals(2, ControlStatus.PIPELINE_DEAD.code());
        Assertions.assertEquals(3, ControlStatus.CONTROL_BUSY.code());
        Assertions.assertEquals(4, ControlStatus.VRAM_FAULT.code());
        Assertions.assertEquals(5, ControlStatus.STALE_BINDING.code());
        Assertions.assertEquals(6, ControlStatus.POISONED.code());
    }

    /**
     * 0..6 每个码都要有人话，不能落进 default 的「未知错误码」
     */
    @Test
    void testEveryKnownCodeHasAReason() {
        for (int code = 0; code <= 6; code++) {
            Assertions.assertNotNull(ControlStatus.fromCode(code), "码 " + code + " 缺少枚举");
            Assertions.assertFalse(ControlStatus.describe(code).startsWith("未知错误码"), "码 " + code + " 缺少可读原因");
        }
        Assertions.assertNotEquals(
                ControlStatus.describe(ControlStatus.ENCODER_UNAVAILABLE.code()),
                ControlStatus.describe(ControlStatus.PIPELINE_DEAD.code()),
                "编码器不可用与管线已死必须区分：前者是永久拒绝，后者可重连协商"
        );
        Assertions.assertTrue(ControlStatus.describe(7).startsWith("未知错误码"), "表外的码才允许落进未知");
        Assertions.assertNull(ControlStatus.fromCode(7), "表外的码不映射到枚举");
    }

    /**
     * 编码器不可用的判定必须包含「Actor 已死」：降级 RAM 无法恢复一条没有编码器的流。
     */
    @Test
    void testPipelineDeadCountsAsEncoderUnavailable() {
        Assertions.assertTrue(new EncoderChannel.ControlException("dead", ControlStatus.PIPELINE_DEAD)
            .encoderUnavailable()
        );
        Assertions.assertTrue(new EncoderChannel.ControlException("unavailable", ControlStatus.ENCODER_UNAVAILABLE)
            .encoderUnavailable()
        );
        Assertions.assertFalse(new EncoderChannel.ControlException("busy", ControlStatus.CONTROL_BUSY)
            .encoderUnavailable()
        );
        Assertions.assertFalse(new EncoderChannel.ControlException("vram", ControlStatus.VRAM_FAULT)
            .encoderUnavailable()
        );
    }
}

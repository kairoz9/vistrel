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
        Assertions.assertEquals(1, ControlStatus.MALFORMED_BATCH.code());
        Assertions.assertEquals(2, ControlStatus.NO_HARDWARE_ENCODER.code());
        Assertions.assertEquals(3, ControlStatus.IMPORT_FAILED.code());
        Assertions.assertEquals(4, ControlStatus.PIPELINE_DEAD.code());
        Assertions.assertEquals(5, ControlStatus.ACK_TIMEOUT.code());
        Assertions.assertEquals(6, ControlStatus.RECONFIGURE_FAILED.code());
        Assertions.assertEquals(7, ControlStatus.QUEUE_FULL.code());
        Assertions.assertEquals(8, ControlStatus.RELEASE_FAILED.code());
        Assertions.assertEquals(9, ControlStatus.ENCODER_BUILD_FAILED.code());
        Assertions.assertEquals(10, ControlStatus.STALE_BINDING.code());
        Assertions.assertEquals(11, ControlStatus.SLOT_NOT_FREE.code());
        Assertions.assertEquals(12, ControlStatus.WRONG_THREAD.code());
        Assertions.assertEquals(13, ControlStatus.POISONED.code());
    }

    /**
     * 0..13 每个码都要有人话，不能落进 default 的「未知错误码」
     */
    @Test
    void testEveryKnownCodeHasAReason() {
        for (int code = 0; code <= 13; code++) {
            Assertions.assertNotNull(ControlStatus.fromCode(code), "码 " + code + " 缺少枚举");
            Assertions.assertFalse(ControlStatus.describe(code).startsWith("未知错误码"), "码 " + code + " 缺少可读原因");
        }
        Assertions.assertNotEquals(
                ControlStatus.describe(ControlStatus.ACK_TIMEOUT.code()),
                ControlStatus.describe(ControlStatus.RECONFIGURE_FAILED.code()),
                "档位重配失败必须与 ACK 超时区分开"
        );
        Assertions.assertTrue(ControlStatus.describe(14).startsWith("未知错误码"), "表外的码才允许落进未知");
        Assertions.assertNull(ControlStatus.fromCode(14), "表外的码不映射到枚举");
    }
}

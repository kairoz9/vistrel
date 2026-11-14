package com.kairo.vistrel.streaming;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * 控制结果码表与 Rust 侧 {@code ControlError} 的判别值一一对应
 *
 * 数值即判别值（{@code rust-native/src/encode/control.rs}），这里逐个钉死，
 * 防止任何一端新增或重排变体后另一端静默漂移成「未知错误码」。
 */
public class ControlCodeTableTest {
    @Test
    void testCodesMatchRustControlErrorDiscriminants() {
        Assertions.assertEquals(0, EncoderChannel.CONTROL_OK);
        Assertions.assertEquals(1, EncoderChannel.CONTROL_MALFORMED_BATCH);
        Assertions.assertEquals(2, EncoderChannel.CONTROL_NO_HARDWARE_ENCODER);
        Assertions.assertEquals(3, EncoderChannel.CONTROL_IMPORT_FAILED);
        Assertions.assertEquals(4, EncoderChannel.CONTROL_PIPELINE_DEAD);
        Assertions.assertEquals(5, EncoderChannel.CONTROL_ACK_TIMEOUT);
        Assertions.assertEquals(6, EncoderChannel.CONTROL_RECONFIGURE_FAILED);
        Assertions.assertEquals(7, EncoderChannel.CONTROL_QUEUE_FULL);
        Assertions.assertEquals(8, EncoderChannel.CONTROL_RELEASE_FAILED);
    }

    /**
     * 0..8 每个码都要有人话，不能落进 default 的「未知错误码」
     */
    @Test
    void testEveryKnownCodeHasAReason() {
        for (int code = 0; code <= 8; code++) {
            Assertions.assertFalse(
                    EncoderChannel.describeControlResult(code).startsWith("未知错误码"),
                    "码 " + code + " 缺少可读原因"
            );
        }
        Assertions.assertNotEquals(
                EncoderChannel.describeControlResult(EncoderChannel.CONTROL_ACK_TIMEOUT),
                EncoderChannel.describeControlResult(EncoderChannel.CONTROL_RECONFIGURE_FAILED),
                "档位重配失败必须与 ACK 超时区分开"
        );
        Assertions.assertTrue(EncoderChannel.describeControlResult(9).startsWith("未知错误码"), "表外的码才允许落进未知");
    }
}

package kairo.vistrel.streaming.natives;

import java.util.List;
import kairo.vistrel.streaming.EncoderChannel.VramSlotDescriptor;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * 显存槽位扁平协议：字段序是与 Rust {@code FLAT_STRIDE} 消费端共享的契约，
 * 这里用纯内存编码钉死，不依赖 JNI 或 GPU。
 */
class VramSlotEncodingTest {
    @Test
    void testSlotTupleFieldOrder() {
        long[] flat = NativeEncoderChannel.encodeSlots(List.of(
                new VramSlotDescriptor(0, 11L, 12L, 1920, 1080, 13L, 14L),
                new VramSlotDescriptor(2, 21L, 0L, 1280, 720, 0L, 0L)
        ));

        Assertions.assertArrayEquals(
                new long[] {0L, 11L, 12L, 1920L, 1080L, 13L, 14L, 2L, 21L, 0L, 1280L, 720L, 0L, 0L},
                flat,
                "槽位元组必须按 [slotIdx, memHandle, semHandle, width, height, sizeBytes, offset] 排列"
        );
    }

    @Test
    void testDescriptorRejectsInvalidHandlesAndGeometry() {
        Assertions.assertThrows(
                IllegalArgumentException.class,
                () -> new VramSlotDescriptor(0, 0L, 0L, 1920, 1080, 0L, 0L),
                "缺少外部显存句柄必须拒绝"
        );
        Assertions.assertThrows(
                IllegalArgumentException.class,
                () -> new VramSlotDescriptor(0, 1L, 0L, 0, 1080, 0L, 0L),
                "零宽度必须拒绝"
        );
        Assertions.assertThrows(
                IllegalArgumentException.class,
                () -> new VramSlotDescriptor(-1, 1L, 0L, 1920, 1080, 0L, 0L),
                "负槽位索引必须拒绝"
        );
    }
}

package kairo.vistrel.streaming;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import java.util.List;

/**
 * {@link EncoderCaps} 的解析契约。Rust 侧 {@code test_capability_wire_shape} 固定
 * 产出，这里固定消费，任一侧改字段名或拼写都要同步改这两处。
 */
class EncoderCapsTest {
    /**
     * 完整应答：硬编条目按优先级序解析，codec 用线上大写拼写
     */
    @Test
    void testParseCapabilitiesSnapshot() {
        String json =
                """
                {"state":"capabilities","backends":[
                  {"kind":"nvenc","codecs":["H264","H265","AV1"],"gpu":"NVIDIA GeForce RTX 5060"},
                  {"kind":"vulkan_video","codecs":["H264"],"gpu":"AMD Radeon"}
                ]}""";
        EncoderCaps caps = EncoderCaps.parse(json);

        Assertions.assertEquals(EncoderCaps.State.CAPABILITIES, caps.state());
        Assertions.assertTrue(caps.probed());
        Assertions.assertTrue(caps.hasHardware());
        Assertions.assertEquals(2, caps.backends().size());
        Assertions.assertEquals("nvenc", caps.backends().get(0).kind());
        Assertions.assertEquals(List.of("H264", "H265", "AV1"), caps.backends().get(0).codecs());
        Assertions.assertEquals("NVIDIA GeForce RTX 5060", caps.backends().get(0).gpu());
    }

    /**
     * 未探测与探测过无硬件是两个状态：都会答「不能硬编」，但前者是调用顺序问题，
     * 后者是本机能力缺失
     */
    @Test
    void testParseDistinguishesNotProbedFromNoHardware() {
        EncoderCaps notProbed = EncoderCaps.parse("{\"state\":\"notProbed\",\"backends\":[]}");
        Assertions.assertEquals(EncoderCaps.State.NOT_PROBED, notProbed.state());
        Assertions.assertFalse(notProbed.probed());
        Assertions.assertFalse(notProbed.hasHardware());

        EncoderCaps noHardware = EncoderCaps.parse("{\"state\":\"noHardware\",\"backends\":[]}");
        Assertions.assertEquals(EncoderCaps.State.NO_HARDWARE, noHardware.state());
        Assertions.assertTrue(noHardware.probed(), "探测跑过就必须报告已探测");
        Assertions.assertFalse(noHardware.hasHardware());
    }

    /**
     * 畸形应答退化为未探测，不抛异常
     */
    @Test
    void testMalformedAnswerDegradesToNotProbed() {
        for (String bad : new String[] {null, "", "   ", "not json", "{}", "[]", "{\"state\":123}"}) {
            EncoderCaps caps = EncoderCaps.parse(bad);
            Assertions.assertEquals(EncoderCaps.State.NOT_PROBED, caps.state(), "畸形应答必须退化为未探测: " + bad);
            Assertions.assertTrue(caps.backends().isEmpty());
        }
    }

    /**
     * 软编不在探测清单里，但可行域是静态声明的，任何时候都查得到
     */
    @Test
    void testSoftwareIsAlwaysFindable() {
        EncoderCaps noHardware = new EncoderCaps(EncoderCaps.State.NO_HARDWARE, List.of());
        EncoderCaps.Backend software = noHardware.find("software");
        Assertions.assertNotNull(software, "软编可行域是静态声明，不依赖探测");
        Assertions.assertEquals(List.of("H264"), software.codecs());
        Assertions.assertEquals("", software.gpu());
        Assertions.assertFalse(software.hardware());
    }

    /**
     * 清单外后端返回 null，由调用方显式处理
     */
    @Test
    void testUnknownBackendReturnsNull() {
        EncoderCaps caps = new EncoderCaps(
                EncoderCaps.State.CAPABILITIES,
                List.of(new EncoderCaps.Backend("nvenc", List.of("H264"), "RTX"))
        );
        Assertions.assertNull(caps.find("vulkan_video"), "清单里没有的后端必须返回 null");
        Assertions.assertNull(caps.find("bogus"), "词表外令牌必须返回 null");
        Assertions.assertNull(caps.find(null));
        Assertions.assertNull(caps.find(""));
    }

    /**
     * 后端列表不可变：快照跨线程共享，改动它会让读到一半的调用方看到撕裂的值
     */
    @Test
    void testBackendsAreImmutable() {
        EncoderCaps caps = EncoderCaps.parse(
                "{\"state\":\"capabilities\",\"backends\":[{\"kind\":\"nvenc\",\"codecs\":[\"H264\"],\"gpu\":\"\"}]}"
        );
        Assertions.assertThrows(UnsupportedOperationException.class, () -> caps
            .backends()
            .add(new EncoderCaps.Backend("software", List.of("H264"), "")));
    }

    /**
     * 显示名：线上词汇翻译用户友好的名称，词表外的令牌原样透出
     */
    @Test
    void testLabels() {
        EncoderCaps.Backend nvenc = new EncoderCaps.Backend("nvenc", List.of("H264"), "RTX");
        Assertions.assertEquals("NVENC", nvenc.label());
        Assertions.assertEquals("Vulkan Video", new EncoderCaps.Backend("vulkan_video", List.of(), "").label());
        Assertions.assertEquals("软件编码", new EncoderCaps.Backend("software", List.of(), "").label());
        Assertions.assertEquals("bogus", new EncoderCaps.Backend("bogus", List.of(), "").label(), "词表外令牌原样返回");

        EncoderCaps.Backend multi = new EncoderCaps.Backend("nvenc", List.of("H264", "H265", "AV1"), "");
        Assertions.assertEquals(List.of("H.264", "H.265", "AV1"), multi.codecLabels());
    }
}

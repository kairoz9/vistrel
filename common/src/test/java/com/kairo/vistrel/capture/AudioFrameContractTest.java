package com.kairo.vistrel.capture;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import org.lwjgl.openal.ALC10;
import org.lwjgl.openal.SOFTLoopback;
import org.lwjgl.system.MemoryStack;
import java.nio.IntBuffer;

/**
 * 音频帧格式回归测试：手写字面量为独立基准，与 rust-native/src/encode/audio.rs 的常量组
 * 是同一条约定的两端——两侧值必须同时等于本测试的字面量。
 *
 * <p>运行期校验在 Rust 侧（长度不符丢帧报错），本测试保证 Java 侧改动必须有意为之。
 */
public class AudioFrameContractTest {
    @Test
    void testAudioFrameContractPinnedToWireValues() {
        // 48kHz Stereo 16-bit PCM，20ms 每帧 960 采样/通道 = 3840 字节
        Assertions.assertEquals(48000, AudioCaptureScheduler.AUDIO_SAMPLE_RATE);
        Assertions.assertEquals(2, AudioCaptureScheduler.AUDIO_CHANNELS);
        Assertions.assertEquals(960, AudioCaptureScheduler.AUDIO_SAMPLES_PER_CHANNEL);
        Assertions.assertEquals(1920, AudioCaptureScheduler.AUDIO_TOTAL_SAMPLES);
        Assertions.assertEquals(3840, AudioCaptureScheduler.AUDIO_FRAME_BYTES);
        Assertions.assertEquals(20_000_000L, AudioCaptureScheduler.TICK_INTERVAL_NS);
    }

    @Test
    void testLoopbackAttributesEncodeContract() {
        // 行为面：环回设备属性表必须真把这些约定值写进 alcCreateContext
        try (MemoryStack stack = MemoryStack.stackPush()) {
            IntBuffer attrs = AudioCaptureScheduler.loopbackContextAttributes(stack, 1);
            // [channels 键值对, type 键值对, freq 键值对, hrtf 键值对, 终止符]
            Assertions.assertEquals(SOFTLoopback.ALC_FORMAT_CHANNELS_SOFT, attrs.get(0));
            Assertions.assertEquals(SOFTLoopback.ALC_STEREO_SOFT, attrs.get(1));
            Assertions.assertEquals(SOFTLoopback.ALC_FORMAT_TYPE_SOFT, attrs.get(2));
            Assertions.assertEquals(SOFTLoopback.ALC_SHORT_SOFT, attrs.get(3));
            Assertions.assertEquals(ALC10.ALC_FREQUENCY, attrs.get(4));
            Assertions.assertEquals(48000, attrs.get(5));
            Assertions.assertEquals(6554, attrs
                /* ALC_HRTF_SOFT */
                .get(6));
            Assertions.assertEquals(1, attrs.get(7));
            Assertions.assertEquals(0, attrs.get(8));
            Assertions.assertEquals(9, attrs.limit(), "属性表以终止符 0 收尾");
        }
    }
}

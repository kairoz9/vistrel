package kairo.vistrel.capture;

//? if gpu_device {
import kairo.vistrel.streaming.EncoderChannel;
//? if vulkan_backend {
import com.mojang.renderpearl.api.GpuFormat;
//? } else {
/*import com.mojang.blaze3d.textures.TextureFormat;*/
//? }
import com.mojang.renderpearl.api.buffers.GpuBuffer;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * Renderpearl 采集路径所依赖的上游 API 契约钉死用例。
 *
 * <p>
 * 紧凑 NV12 布局与 buffer 映射是采集管线与 Rust 编码侧的跨语言约定，其成立前提
 * 全部押在 vanilla 抽象的具体语义上：copyTextureToBuffer 按 {@code byteAlignment()}
 * 设置行对齐（紧凑无填充）、map 以 usage 位 1 判可读。上游一旦改动这些语义，
 * 本用例先红，而不是串流画面静默花屏。
 */
class RenderpearlCaptureContractTest {
    /**
     * 采集管线为 map(true, false) 请求的 usage 位组合（GpuBuffer.USAGE_COPY_DST | USAGE_MAP_READ）
     */
    private static final int CAPTURE_BUFFER_USAGE = GpuBuffer.USAGE_COPY_DST | GpuBuffer.USAGE_MAP_READ;

    //? if vulkan_backend {
    @Test
    void testGpuFormatByteAlignmentKeepsNv12Tight() {
        // GL 后端 copyTextureToBuffer 以 byteAlignment() 设 PACK_ALIGNMENT，而该值取的是
        // 分量字节数（8-bit 格式一律为 1）：两个采集平面任何宽度都无行尾填充；
        // 块字节数另有 blockSize()（R8=1、RG8=2），只用于容量校验与偏移对齐。
        // 26.1 的 TextureFormat 为裸枚举、不暴露这些元数据，该契约只在 26.2+ 可钉
        Assertions.assertEquals(1, GpuFormat.R8_UNORM.byteAlignment(), "亮度平面 R8 PACK 对齐 1 字节：行距=宽度");
        Assertions.assertEquals(1, GpuFormat.RG8_UNORM.byteAlignment(), "色度平面 RG8 PACK 对齐 1 字节：行距=宽度×2 且无填充");
        Assertions.assertEquals(1, GpuFormat.RGBA8_UNORM.byteAlignment(), "显存槽位 RGBA8 PACK 对齐 1 字节");
        Assertions.assertEquals(1, GpuFormat.R8_UNORM.blockSize(), "R8 纹理块 1 字节");
        Assertions.assertEquals(2, GpuFormat.RG8_UNORM.blockSize(), "RG8 纹理块 2 字节，UV 偏移须为其倍数");
    }

    @Test
    void testCaptureBufferUsageBitsSatisfyMapReadContract() {
        // Vulkan 后端 map 以 (usage & 1) 判可读、(usage & 2) 判可写；COPY_DST 是
        // copyTextureToBuffer 的必要用法位。位编号变动会使管线构造或映射直接抛错
        Assertions.assertEquals(1, GpuBuffer.USAGE_MAP_READ, "MAP_READ 必须是位 1：map(read=true) 按它校验");
        Assertions.assertEquals(8, GpuBuffer.USAGE_COPY_DST, "COPY_DST 必须是位 8：copyTextureToBuffer 依赖它");
        Assertions.assertTrue(
                (CAPTURE_BUFFER_USAGE & GpuBuffer.USAGE_MAP_READ) != 0 && (CAPTURE_BUFFER_USAGE
                & GpuBuffer.USAGE_COPY_DST) != 0,
                "采集 buffer 的 usage 组合必须同时满足拷贝目的与映射读取"
        );
    }

    //? }
    @Test
    void testNv12PlaneLayoutMatchesEncoderContract() {
        // 编码侧按 1.5 B/px 的紧凑 NV12 读取：Y 平面在偏移 0、UV 平面紧跟其后。
        // 偶数宽高下恒等式必须成立；Y 平面字节数还兼作 UV 拷贝的 bufferOffset，
        // 需满足纹理块字节对齐（R8 块 1 字节、RG8 块 2 字节）
        int[][] tiers = {{1280, 720}, {1366, 768}, {1920, 1080}, {2560, 1440}};
        for (int[] tier : tiers) {
            int width = tier[0];
            int height = tier[1];
            long lumaBytes = (long) width * height;
            long chromaBytes = (long) (width / 2) * (height / 2) * 2;
            Assertions.assertEquals(
                    lumaBytes * 3 / 2,
                    lumaBytes + chromaBytes,
                    width + "x" + height + " 双平面总量必须是 1.5 B/px"
            );
            Assertions.assertEquals(0, lumaBytes % 4, width + "x" + height + " 的 UV 平面偏移必须满足拷贝对齐");
            Assertions.assertEquals(0, lumaBytes % 2, width + "x" + height + " 的 UV 平面偏移必须是 RG8 纹理块字节数的倍数");
            Assertions.assertEquals(0, width % 2, "奇数宽度构不出 4:2:0，管线会拒绝采集");
            Assertions.assertEquals(0, height % 2, "奇数高度构不出 4:2:0，管线会拒绝采集");
        }
    }

    @Test
    void testRamPipelineDegradesToDroppedWithoutDevice() {
        // 无头测试环境没有 GpuDevice：构造必须走禁用分支而不是抛异常，
        // capture 返回主动丢帧 (Dropped) 而不是故障 (Failed)——调度器把 Failed
        // 计入断路器、把 Dropped 视为管线健康，语义不可互换
        RenderpearlRamPipeline pipeline = new RenderpearlRamPipeline((EncoderChannel) null, 1280, 720);
        try {
            Assertions.assertEquals(FrameSource.CaptureOutcome.Dropped, pipeline.capture(), "无设备构造的管线必须按主动丢帧处理，不得上报故障");
        } finally {
            pipeline.close();
        }
    }

    @Test
    void testRamPipelineCloseIsIdempotentWithoutDevice() {
        RenderpearlRamPipeline pipeline = new RenderpearlRamPipeline((EncoderChannel) null, 640, 480);
        pipeline.close();
        Assertions.assertDoesNotThrow(pipeline::close, "重复 close 不得抛异常（会话停止与重建可能各调一次）");
    }
}
//? }

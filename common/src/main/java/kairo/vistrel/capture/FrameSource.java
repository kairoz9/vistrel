package kairo.vistrel.capture;

/**
 * 单帧画面来源：从 Minecraft 渲染管线取得画面并直接投递至 EncoderChannel。
 *
 * <p>遵循 RAII：构造即就绪，{@link #close()} 释放全部显存与缓冲资源。实现分两类：
 * RAM 回读管线（{@code RenderpearlRamPipeline} / {@code OpenGlRamPipeline}）经系统内存
 * 投递，只随几何变化；VRAM 直通管线（{@code RenderpearlVramPipeline}）经外部显存槽位
 * 投递，除几何外还绑定编码器世代。
 */
public interface FrameSource extends AutoCloseable {
    /**
     * 单帧采集结果。
     */
    enum CaptureOutcome {
        /**
         * 本帧已投递至 EncoderChannel
         */
        Delivered,
        /**
         * 本帧被主动丢弃（背压 / 预热），不产生推流数据
         */
        Dropped,
        /**
         * 来源不可用：未就绪、拿不到必要句柄或内部异常
         */
        Failed
    }

    /**
     * 执行单帧捕获与通道推流
     *
     * @return 三态结果，含义见 {@link CaptureOutcome}
     */
    CaptureOutcome capture();

    /**
     * 释放所有显卡与缓冲资源
     */
    @Override
    void close();
}

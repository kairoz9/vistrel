package kairo.vistrel.capture;

/**
 * 视频画面采集管道统一接口
 * 遵循 RAII 规范：构造即就绪，负责从 Minecraft 渲染管线抓取画面并直接推流至 EncoderChannel。
 */
public interface VideoPipeline extends AutoCloseable {
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
         * 管线不可用：未就绪、拿不到必要句柄或内部异常
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

package com.kairo.vistrel.input;

import java.nio.ByteBuffer;

/**
 * 浏览器上行输入事件的拉取端口 (DataChannel 上行方向)。
 *
 * <p>
 * 事件由 Native 引擎自远端会话缓冲，仅应在会话活跃时于渲染帧内拉取。
 * 端口由消费方（输入注入子系统）声明，数据面实现（{@code streaming.natives}）同时
 * 提供本端口与下行编码通道两个面。
 */
public interface InputEventSource {
    /**
     * 批量拉取浏览器上行输入事件的原始字节流。
     *
     * @param buffer    用于接收原始事件数据的堆外直接缓冲区
     * @param maxEvents 单批次最大拉取事件数
     * @return 实际解包出的有效事件数
     */
    default int pollInputEvents(ByteBuffer buffer, int maxEvents) {
        return 0;
    }
}

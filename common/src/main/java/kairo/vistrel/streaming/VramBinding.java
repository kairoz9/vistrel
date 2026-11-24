package kairo.vistrel.streaming;

/**
 * 一段导出显存绑定到某个编码器代次的 Java 侧句柄：槽位认领、帧投递、心跳判定与注销结算。
 *
 * <p>生命周期由采集侧持有；构造即整批导入（全成全败），{@link #close()} 是唯一注销入口。
 * 绑定只对导入时的绑定轮次有效：编码器重建/失效重配后 Native 递增轮次，旧推送在入口丢弃。
 *
 * <p>线程约定：{@link #claim} / {@link #release} / {@link #push} / {@link #markPushed} /
 * {@link #heartbeatTimedOut} 在渲染线程调用；{@link #stageName} 只读共享块；
 * {@link #close()} 在渲染线程的管线拆除路径调用。
 */
public interface VramBinding {
    /**
     * 心跳冻结判定阈值：持续推流下心跳停在基准值超过此时长，判定编码线程卡死。
     *
     * <p>编码端每轮推进心跳，不推进只有卡死一种解释；判定必须在每个 tick 执行
     * （含全忙丢帧的 tick）——卡死后 push 会停，判定不能跟着停。
     */
    long STALL_HEARTBEAT_TIMEOUT_NS = 1_000_000_000L;

    /**
     * 投递一帧已注册槽位（单向，不等待编码结果）。
     *
     * @param slotIdx   槽位索引
     * @param signalVal 与 GPU 提交流绑定的时间线信号值；OpenGL 后端为 0
     * @throws IllegalStateException 绑定已关闭
     */
    void push(int slotIdx, long signalVal);

    /**
     * 认领一个空闲槽位：CAS 0→1，轮转优先、索引序降级。
     *
     * @param preferredIdx 首选槽位索引；非法值退化为索引序扫描
     * @return 认领到的槽位索引；全忙返回 -1（对应主动丢帧，不是故障）
     */
    int claim(int preferredIdx);

    /**
     * 归还一个槽位认领位（异常路径；正常归还由编码线程完成，幂等）。
     *
     * @param slotIdx 槽位索引
     */
    void release(int slotIdx);

    /**
     * 记录一次 push 成功后的心跳基准。
     *
     * @param nowNanos 本帧开始时刻（{@code System.nanoTime()} 域）
     */
    void markPushed(long nowNanos);

    /**
     * 心跳是否冻结超过停滞阈值。
     *
     * @param nowNanos 当前时刻（{@code System.nanoTime()} 域）
     * @return true 表示编码线程超过 1 秒没有推进心跳
     */
    boolean heartbeatTimedOut(long nowNanos);

    /**
     * 编码线程最后进入的阶段名，用于停滞日志。
     *
     * @return 阶段中文名；未知码原样展示
     */
    String stageName();

    /**
     * 本绑定导入时共享块中的绑定轮次。
     *
     * @return 导入令牌；与共享块的当前绑定轮次不同即说明编码器已重建
     */
    long bindingEpoch();

    /**
     * 注销槽位并结算。
     *
     * <p>幂等：重复调用返回同一结果。{@link ReleaseStatus#RETAINED} 表示编码线程可能仍
     * 在读导入资源，调用方不得销毁纹理 / 信号量。
     *
     * @return 注销结算结果
     */
    ReleaseStatus close();

    /**
     * 显存槽位注销的结算结果。
     *
     * <p>{@link #RELEASED} 是游戏侧销毁导出纹理 / 信号量 / VMA 池的唯一依据；
     * {@link #RETAINED} 表示编码线程未确认注销（排空失败、ACK 超时或队列满），
     * 可能仍在读导入资源，销毁即驱动层未定义行为。
     */
    enum ReleaseStatus {
        /**
         * Native 已确认槽位注销，游戏侧可以销毁导出资源。
         */
        RELEASED(true),
        /**
         * Native 未确认注销，导入引用与槽位表仍被保留；游戏侧资源必须留给引擎关停统一回收。
         */
        RETAINED(false);
        private final boolean released;

        ReleaseStatus(boolean released) {
            this.released = released;
        }

        /**
         * 是否得到 Native 的注销确认
         *
         * @return true 表示可以安全销毁游戏侧导出资源
         */
        public boolean released() {
            return released;
        }
    }
}

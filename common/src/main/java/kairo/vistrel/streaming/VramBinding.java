package kairo.vistrel.streaming;

/**
 * 一段导出显存绑定到某个编码器代次的 Java 侧句柄：槽位取用、帧投递、心跳判定与注销结算。
 *
 * <p>生命周期由采集侧持有；构造即整批导入（全成全败），{@link #close()} 是唯一注销入口。
 * 绑定只对导入时取得的句柄有效：编码器重建/失效重配后 Native 摘除绑定，旧句柄的推送
 * 在入口丢弃。槽位表、在途位与心跳全在 Native，Java 不持有任何共享内存视图。
 *
 * <p>线程约定：{@link #acquire(int, long)} / {@link #push(int, long)} /
 * {@link #heartbeatTimedOut(long)} 在渲染线程调用；{@link #stallDiagnostics()} 只在超时后
 * 调用；{@link #close()} 在管线拆除路径调用。
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
     * 取一个空闲槽位：向 Native 拉一次权威快照（在途掩码 + 心跳），本地选位。
     *
     * <p>本调用同时刷新停滞判定基准，因此每个采集 tick 必须调用一次（包括全忙的 tick）。
     * 未投递的槽位在 Native 侧从未置位，异常路径无须归还。
     *
     * @param preferredIdx 首选槽位索引；非法或忙时按索引序降级
     * @param nowNanos     本帧开始时刻（{@code System.nanoTime()} 域），用作心跳基准
     * @return 槽位索引；全忙或绑定已失效返回 -1（对应主动丢帧，不是故障）
     * @throws IllegalStateException 绑定已关闭
     * @throws EncoderChannel.ControlException Native 返回未知/致命状态
     */
    int acquire(int preferredIdx, long nowNanos);

    /**
     * 投递一帧已取用的槽位（单向，不等待编码结果）。
     *
     * <p>成功后槽位所有权移交 Native，由帧载荷析构归还；同一槽位在归还前不得再次投递。
     *
     * @param slot       {@link #acquire(int, long)} 返回的槽位索引
     * @param signalVal  与 GPU 提交流绑定的时间线信号值
     * @return true 表示已入队；false 表示绑定已失效、帧被入口丢弃
     * @throws IllegalStateException 绑定已关闭
     * @throws EncoderChannel.ControlException 槽位未取用、重复投递或线程不符
     */
    boolean push(int slot, long signalVal);

    /**
     * 心跳是否冻结超过停滞阈值。
     *
     * <p>基准由 {@link #acquire(int, long)} 每次成功拉取快照时刷新；绑定失效时快照
     * 不再推进，超过阈值即判定停滞并触发引擎自愈。
     *
     * @param nowNanos 当前时刻（{@code System.nanoTime()} 域）
     * @return true 表示编码线程超过 1 秒没有推进心跳
     */
    boolean heartbeatTimedOut(long nowNanos);

    /**
     * 编码线程最后进入的阶段名，用于停滞日志；只在超时后调用。
     *
     * @return 阶段中文名
     */
    String stallDiagnostics();

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

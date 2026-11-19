use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::encode::StampedVideoPacket;
use crate::encode::pool::FrameBuf;

/// 帧载荷
#[derive(Debug)]
pub(crate) enum FramePayload {
    /// 显存直通帧：持有槽位索引与时间线同步信号量值（由硬件编码器内部槽位表直接解析）
    VramSlot { slot_idx: u32, sync_value: u64 },
    /// 主机内存帧：生产者已把整帧写进池里的块（CUDA 锁页 / Vulkan 持久映射 / 堆），
    /// 后端按自己的设备决定走零拷贝还是 CPU 暂存
    Host { frame: FrameBuf },
}

impl FramePayload {
    /// 主机内存载荷的字节视图；显存槽位返回 `None`
    pub fn host_slice(&self) -> Option<&[u8]> {
        match self {
            FramePayload::Host { frame } => Some(frame.as_slice()),
            FramePayload::VramSlot { .. } => None,
        }
    }
}

/// 一帧在途载荷：单调出生纳秒 + 像素所在的内存域
///
/// 出生纳秒既是 RTP 时钟差分的输入也是排队过期判定的基准；载荷二选一
/// （显存槽位直通或主机池块），显存分支不携带像素数据，几何由槽位表按索引解析
#[derive(Debug)]
pub(crate) struct VideoFrame {
    pub birth_nanos: u64,
    pub payload: FramePayload,
}

/// 限频日志的单调毫秒基准：只比较差值，基准取哪个时刻无关紧要
static CLOCK_BASE: LazyLock<Instant> = LazyLock::new(Instant::now);

/// 限频日志的窗口：停滞现场既要立刻可见，也不能以采集帧率刷屏
pub(crate) const DROP_LOG_WINDOW_MS: u64 = 1000;
/// 失败支路的限频日志状态：每次发生都计数，首条立即打出，此后每窗口至多一条
///
/// 采集与编码两条线程上的失败分支共用：只碰两个原子，无锁无分配。「一个窗口一个赢家」
/// 由 CAS 保证（与 `KeyframeThrottle` 同一手法），时间戳只做差值比较，全程 Relaxed。
#[derive(Debug)]
pub(crate) struct RateLimitLog {
    /// 本实例的限频窗口：构造时定死，判窗口只读它
    window_ms: u64,
    /// 上次放行日志时的单调毫秒数（0 = 尚未打过，首条必放行）
    last_ms: AtomicU64,
    /// 自上条日志起的累计发生次数
    count: AtomicU64,
}

impl RateLimitLog {
    /// `window_ms` 是这条日志自己的限频窗口：不同支路可以各有各的宽严
    pub(crate) const fn new(window_ms: u64) -> Self {
        Self {
            window_ms,
            last_ms: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// 记一次发生；到达放行点则返回自上条日志起的累计次数（含本次）并清零
    pub(crate) fn record(&self) -> Option<u64> {
        self.record_within(self.window_ms)
    }

    /// 判窗口的实体；`window_ms` 独立入口只为了让测试用 0 模拟「窗口已过」
    fn record_within(&self, window_ms: u64) -> Option<u64> {
        let count = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        // 单调毫秒，从 1 起算：0 留给「从未打过」哨兵，避免首窗时间戳与之相撞
        let now = CLOCK_BASE.elapsed().as_millis() as u64 + 1;
        let last = self.last_ms.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < window_ms {
            return None;
        }
        // CAS 放行：并发下只放一个赢家，输家的这条并入下一个窗口的计数
        if self
            .last_ms
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        self.count.swap(0, Ordering::Relaxed);
        Some(count)
    }
}

/// 一帧进收件箱后的结局：每条入队的帧恰好落进一格
///
/// 「推入 N 帧、上链路 M 帧」的差额才算得清：差额 = 除 Admitted 外各格之和 + 收件箱里
/// 还在途的那一帧。收件箱覆盖那一路是静默发生的，没有逐格计数就无人能答这个问题。
#[derive(Debug, Clone, Copy)]
pub(crate) enum FrameOutcome {
    /// 编成包并写入绑定轨道
    Admitted,
    /// 未及上链就被放手（收件箱已被下一帧挤出、换代时清空，或发件箱被最新帧覆盖）
    Superseded,
    /// 池里没有可用块（编码线程停滞或正换代）
    PoolEmpty,
    /// 写进池中的块失败
    WriteFailed,
    /// 字节数与编码器几何不符，编码线程拒绝它
    GeometryMismatch,
    /// 编码线程消费了但它没上链路（无编码器 / 空 NALU / 后端报错）
    EncodeFailed,
    /// 已编码，但写超时：写轨道在时限内未完成，或排队等到过期，已丢弃
    WriteStalled,
    /// 已编码但写轨道失败
    TrackWriteFailed,
    /// 已编码，等关键帧门控态下主动跳过（非错误）
    GateSkipped,
    /// 在入口被拒收的显存帧
    FenceDropped,
}

impl FrameOutcome {
    /// 日志开头那句；不出声的几格给空串
    fn headline(self) -> &'static str {
        match self {
            FrameOutcome::Admitted
            | FrameOutcome::Superseded
            | FrameOutcome::GateSkipped
            | FrameOutcome::FenceDropped => "",
            FrameOutcome::PoolEmpty => "主机帧池无可用块",
            FrameOutcome::WriteFailed => "主机帧写入失败并丢弃",
            FrameOutcome::GeometryMismatch => "主机帧布局与编码器不一致",
            FrameOutcome::EncodeFailed => "编码未上链路",
            FrameOutcome::WriteStalled => "写超时",
            FrameOutcome::TrackWriteFailed => "写轨道失败",
        }
    }
}

/// 帧结局计数：十格只增不减，出声的六格各带一个限频器
///
/// 计数与日志同住一处：调用点只交代「这一帧是什么结局」加一句详情，限频、文案与级别
/// 都在这里定，免得四处各写一套。只碰 `Relaxed` 原子，无锁无分配；窗口本身仍归
/// [`RateLimitLog`] 判。
#[derive(Debug)]
pub(crate) struct FrameOutcomes {
    admitted: AtomicU64,
    superseded: AtomicU64,
    pool_empty: AtomicU64,
    write_failed: AtomicU64,
    geometry_mismatch: AtomicU64,
    encode_failed: AtomicU64,
    write_stalled: AtomicU64,
    track_write_failed: AtomicU64,
    gate_skipped: AtomicU64,
    fence_dropped: AtomicU64,
    /// 要出声的几格各带一个限频器（`Admitted` 是成功路径，`Superseded`、
    /// `GateSkipped` 与 `FenceDropped` 是常态，都不出声）
    pool_empty_log: RateLimitLog,
    write_failed_log: RateLimitLog,
    geometry_mismatch_log: RateLimitLog,
    encode_failed_log: RateLimitLog,
    write_stalled_log: RateLimitLog,
    track_write_failed_log: RateLimitLog,
}

impl FrameOutcomes {
    /// 十格归零，出声六格的日志各就位
    pub(crate) fn new() -> Self {
        Self {
            admitted: AtomicU64::new(0),
            superseded: AtomicU64::new(0),
            pool_empty: AtomicU64::new(0),
            write_failed: AtomicU64::new(0),
            geometry_mismatch: AtomicU64::new(0),
            encode_failed: AtomicU64::new(0),
            write_stalled: AtomicU64::new(0),
            track_write_failed: AtomicU64::new(0),
            gate_skipped: AtomicU64::new(0),
            fence_dropped: AtomicU64::new(0),
            // 五格各自限频；文案与级别见 `record_with`
            pool_empty_log: RateLimitLog::new(DROP_LOG_WINDOW_MS),
            write_failed_log: RateLimitLog::new(DROP_LOG_WINDOW_MS),
            geometry_mismatch_log: RateLimitLog::new(DROP_LOG_WINDOW_MS),
            encode_failed_log: RateLimitLog::new(DROP_LOG_WINDOW_MS),
            write_stalled_log: RateLimitLog::new(DROP_LOG_WINDOW_MS),
            track_write_failed_log: RateLimitLog::new(DROP_LOG_WINDOW_MS),
        }
    }

    /// 记一格无需详情的结局
    pub(crate) fn record(&self, outcome: FrameOutcome) {
        self.record_with(outcome, "");
    }

    /// 记一格带详情的结局；详情与文案都只在这一格真被放行时才拼
    pub(crate) fn record_with(&self, outcome: FrameOutcome, detail: impl std::fmt::Display) {
        self.counter(outcome).fetch_add(1, Ordering::Relaxed);

        // `Admitted` / `Superseded` / `GateSkipped` / `FenceDropped` 按设计不出声，
        // 其余六格各自限频
        let Some(log) = self.log(outcome) else {
            return;
        };
        let Some(window) = log.record() else {
            return;
        };

        let detail = detail.to_string();
        let text = if detail.is_empty() {
            format!("{}（窗口内 {window} 次）", outcome.headline())
        } else {
            format!("{}：{detail}（窗口内 {window} 次）", outcome.headline())
        };
        // 池空是暂时的饥饿、写超时/写失败是网络侧的瞬态，按 warn；
        // 其余几格是不变量被破坏，按 error
        if matches!(
            outcome,
            FrameOutcome::PoolEmpty | FrameOutcome::WriteStalled | FrameOutcome::TrackWriteFailed
        ) {
            log::warn!("{text}");
        } else if matches!(outcome, FrameOutcome::GeometryMismatch) {
            log::debug!("{text}");
        } else {
            log::error!("{text}");
        }
    }

    #[cfg(test)]
    pub(crate) fn count(&self, outcome: FrameOutcome) -> u64 {
        self.counter(outcome).load(Ordering::Relaxed)
    }

    fn counter(&self, outcome: FrameOutcome) -> &AtomicU64 {
        match outcome {
            FrameOutcome::Admitted => &self.admitted,
            FrameOutcome::Superseded => &self.superseded,
            FrameOutcome::PoolEmpty => &self.pool_empty,
            FrameOutcome::WriteFailed => &self.write_failed,
            FrameOutcome::GeometryMismatch => &self.geometry_mismatch,
            FrameOutcome::EncodeFailed => &self.encode_failed,
            FrameOutcome::WriteStalled => &self.write_stalled,
            FrameOutcome::TrackWriteFailed => &self.track_write_failed,
            FrameOutcome::GateSkipped => &self.gate_skipped,
            FrameOutcome::FenceDropped => &self.fence_dropped,
        }
    }

    fn log(&self, outcome: FrameOutcome) -> Option<&RateLimitLog> {
        match outcome {
            FrameOutcome::Admitted
            | FrameOutcome::Superseded
            | FrameOutcome::GateSkipped
            | FrameOutcome::FenceDropped => None,
            FrameOutcome::PoolEmpty => Some(&self.pool_empty_log),
            FrameOutcome::WriteFailed => Some(&self.write_failed_log),
            FrameOutcome::GeometryMismatch => Some(&self.geometry_mismatch_log),
            FrameOutcome::EncodeFailed => Some(&self.encode_failed_log),
            FrameOutcome::WriteStalled => Some(&self.write_stalled_log),
            FrameOutcome::TrackWriteFailed => Some(&self.track_write_failed_log),
        }
    }
}

/// 容量为一的邮箱：`post` 投递即覆盖未消费的旧值 (Latest-Frame-Wins)，被挤掉的计入
/// `Superseded`；`take` 取走即清空，`notified` 等下一次投递
///
/// 两个 seam 共用这一个核心：采集→编码的 [`FrameInbox`] 与编码→线缆的
/// `FrameOutbox` 发件箱。差别只在被挤掉那帧的寿命归属——收件箱把载荷交还 Actor 线程
/// 回收（本类型原样交还调用方），发件箱里的 `Bytes` 就地析构即可。
#[derive(Debug)]
pub(crate) struct Mailbox<T> {
    slot: Mutex<Option<T>>,
    bell: Arc<Notify>,
    outcomes: Arc<FrameOutcomes>,
}

impl<T> Mailbox<T> {
    /// 自带门铃的独立槽
    pub(crate) fn new(outcomes: Arc<FrameOutcomes>) -> Self {
        Self::with_bell(Arc::new(Notify::new()), outcomes)
    }

    /// 共享门铃：邮箱要与已有唤醒源（如控制通道）合并时用它
    pub(crate) fn with_bell(bell: Arc<Notify>, outcomes: Arc<FrameOutcomes>) -> Self {
        Self {
            slot: Mutex::new(None),
            bell,
            outcomes,
        }
    }

    /// 覆盖旧值并摇铃；返回被挤掉的旧值，归还路径由调用方定
    ///
    /// 锁在语句末即释放，入队/析构都在锁外。
    pub(crate) fn post(&self, value: T) -> Option<T> {
        let stale = self.slot.lock().replace(value);
        if stale.is_some() {
            self.outcomes.record(FrameOutcome::Superseded);
        }
        self.bell.notify_one();
        stale
    }

    /// 取出当前值并置空，所有权移交调用方
    pub(crate) fn take(&self) -> Option<T> {
        self.slot.lock().take()
    }

    /// 等下一次投递；`Notify` 的许可位保证与 `post` 之间不丢唤醒
    pub(crate) async fn notified(&self) {
        self.bell.notified().await
    }

    pub(crate) fn outcomes(&self) -> &FrameOutcomes {
        &self.outcomes
    }

    pub(crate) fn outcomes_arc(&self) -> Arc<FrameOutcomes> {
        Arc::clone(&self.outcomes)
    }
}

/// 编码产物的发件箱：单值覆盖、被挤掉的旧帧记 `Superseded`
pub(crate) type FrameOutbox = Mailbox<StampedVideoPacket>;

/// 帧收件箱（采集侧→编码侧）：单值覆盖、最新帧优先 (Latest-Frame-Wins)、线性移交所有权
///
/// 渲染主线程与 Dedicated Encoder Actor 之间的核心缓冲单元。
/// - 生产端（渲染主线程）调用 `push` 存入新帧，被挤出的旧帧只交出资源句柄，绝不阻塞渲染主线程；
/// - 消费端（Actor 编码线程）调用 `take` 原子取出当前帧并将收件箱置空，独占帧所有权并在消费完成后自动回收；
/// - 排空端调用 `clear` 立即清空可能在途的未消费帧。
///
/// 被挤出/清空的帧永远等不到编码回执：它们整帧（含显存槽位索引或主机块）进
/// [`Self::take_discarded`] 的待归还队列，由 Actor 取走——`FrameBuf` 析构要拿池锁，
/// 不能落在生产者的渲染主线程上。
///
/// 收件箱自带门铃：`push` 即唤醒，因此投递帧的调用方无需（也不应）自行唤醒 Actor。
#[derive(Debug)]
pub(crate) struct FrameInbox {
    latest: Mailbox<VideoFrame>,
    /// 被丢弃帧的载荷待归还队列，Actor 每轮循环取走：显存载荷清所有权位，主机载荷析构回池
    discarded: Mutex<Vec<FramePayload>>,
}

/// 待归还队列的堆积上限：健康路径每轮循环即清空，触顶只可能发生在 Actor 失联、
/// 心跳超时重启已成定局的窗口内。触顶清空比无界堆积可靠——此时主机块会在生产者
/// 线程上析构一次，但引擎已进入关停路径，优先保住内存不无界增长
const DISCARDED_ACK_CAP: usize = 64;

impl FrameInbox {
    /// 装配收件箱：门铃由调用方注入而非自建——Actor 的 `select!` 要在控制通道与本
    /// 门铃上合并等待，两条唤醒源必须共用同一 `Notify`（控制命令经它摇铃，帧经
    /// `push` 摇铃）。投递方只管 push，唤醒由门铃承担，无需自行唤醒 Actor
    pub fn new(doorbell: Arc<Notify>) -> Self {
        Self {
            latest: Mailbox::with_bell(doorbell, Arc::new(FrameOutcomes::new())),
            discarded: Mutex::new(Vec::new()),
        }
    }

    /// 帧归宿账本
    pub(crate) fn outcomes(&self) -> &FrameOutcomes {
        self.latest.outcomes()
    }

    /// 账本的共享句柄（会话的投递任务也要写同一本）
    pub(crate) fn outcomes_arc(&self) -> Arc<FrameOutcomes> {
        self.latest.outcomes_arc()
    }

    /// 写入新帧并覆写旧帧。被挤出的旧帧计入账本，资源句柄转交 Actor 回收
    ///
    /// 不打印日志：Latest-Frame-Wins 下的覆盖是既定行为，逐次告警只会刷屏。
    pub fn push(&self, frame: VideoFrame) {
        if let Some(stale) = self.latest.post(frame) {
            self.queue_discarded(stale.payload);
        }
    }

    /// 原子取出当前帧并将收件箱置空。返回 Some(frame) 移交所有权；空则返回 None
    pub fn take(&self) -> Option<VideoFrame> {
        self.latest.take()
    }

    /// 显式清空收件箱：在途帧走与覆盖相同的路径交出资源句柄
    ///
    /// 换代时的清空同样计入 `Superseded`：它们也是没上链路就被放手的帧，
    /// 少了这笔，账本在重建路径上就合不上
    pub fn clear(&self) {
        let stale = self.latest.take();
        if let Some(frame) = stale {
            self.latest.outcomes().record(FrameOutcome::Superseded);
            self.queue_discarded(frame.payload);
        }
    }

    /// 取走全部待归还的载荷（Actor 每轮循环调用，析构即在 Actor 线程回池）
    pub(crate) fn take_discarded(&self) -> Vec<FramePayload> {
        std::mem::take(&mut *self.discarded.lock())
    }

    /// 被丢弃帧的载荷入队：两种资源都不在生产线程上回收
    fn queue_discarded(&self, payload: FramePayload) {
        let mut queue = self.discarded.lock();
        if queue.len() >= DISCARDED_ACK_CAP {
            queue.clear();
        }
        queue.push(payload);
    }
}

/// 关键帧触发器
///
/// 具备原子覆写（Latest-Wins）行为，初值为 true 以确保新管线首帧必出 IDR。
#[derive(Debug, Clone)]
pub struct KeyframeTrigger {
    need: Arc<AtomicBool>,
}

impl Default for KeyframeTrigger {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeTrigger {
    pub fn new() -> Self {
        Self {
            // 初值 true：首帧必须是 IDR
            need: Arc::new(AtomicBool::new(true)),
        }
    }

    /// 挂起一次关键帧请求
    pub(crate) fn request(&self) {
        self.need.store(true, Ordering::Release);
    }

    /// 编码段原子消费挂起的关键帧请求
    ///
    /// 消费即清空，所以调用方必须在**确认产出关键帧之后**才算兑现；失败时应重新
    /// [`request`](Self::request)，否则请求会被静默吞掉。
    pub fn take(&self) -> bool {
        self.need.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::pool::{FramePool, POOL_DEPTH, heap_blocks};

    /// 从待归还队列里筛出显存槽位索引（主机载荷就地析构）
    fn slot_idxs(queue: Vec<FramePayload>) -> Vec<u32> {
        queue
            .into_iter()
            .filter_map(|payload| match payload {
                FramePayload::VramSlot { slot_idx, .. } => Some(slot_idx),
                FramePayload::Host { .. } => None,
            })
            .collect()
    }

    /// 初值、消费与复燃：旗标的三段生命周期
    #[test]
    fn test_trigger_lifecycle() {
        let trigger = KeyframeTrigger::new();
        assert!(trigger.take(), "初值必须为 true（首帧必出 IDR）");
        assert!(!trigger.take(), "消费即清空");
        trigger.request();
        assert!(trigger.take(), "request 后必须复燃");
        assert!(!trigger.take());
    }

    /// Clone 共享同一份旗标（原子覆写行为），而非各自独立
    #[test]
    fn test_clone_shares_state() {
        let trigger = KeyframeTrigger::new();
        let clone = trigger.clone();
        trigger.take();
        clone.request();
        assert!(
            trigger.take(),
            "Clone 必须共享旗标：clone 的 request 对原实例可见"
        );
    }

    /// 被覆盖/清空的帧必须交出资源：显存槽位与主机块都进待归还队列
    #[test]
    fn test_frame_inbox_discarded_slot_acks() {
        let doorbell = Arc::new(Notify::new());
        let inbox = FrameInbox::new(Arc::clone(&doorbell));

        let vram = |sync_value: u64| VideoFrame {
            birth_nanos: sync_value,
            payload: FramePayload::VramSlot {
                slot_idx: 2,
                sync_value,
            },
        };

        inbox.push(vram(10));
        // 覆盖：旧帧 (slot 2) 进队列，新帧留在收件箱
        inbox.push(vram(11));
        assert!(inbox.take().is_some(), "最新帧必须仍可取出");
        assert_eq!(
            slot_idxs(inbox.take_discarded()),
            vec![2],
            "被覆盖的显存帧必须记入待归还队列"
        );
        assert!(inbox.take_discarded().is_empty(), "取走后队列必须清空");

        // 主机帧被覆盖同样入队，搬运的是块本身
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        let mut buf = pool.acquire().expect("池里必须有余量");
        buf.write(&[1u8; 16]).unwrap();
        inbox.push(VideoFrame {
            birth_nanos: 1,
            payload: FramePayload::Host { frame: buf },
        });
        inbox.push(vram(12));
        let blocks = inbox.take_discarded();
        assert_eq!(blocks.len(), 1, "被覆盖的主机帧必须转入待归还队列");
        assert!(
            matches!(blocks[0], FramePayload::Host { .. }),
            "入队的应是主机载荷"
        );
        assert_eq!(pool.len(), POOL_DEPTH - 1, "块在队列里时不得提前归还");
        drop(blocks);
        assert_eq!(pool.len(), POOL_DEPTH, "Actor 取走后块必须回到池");

        // clear：显存帧同样入队
        inbox.clear();
        assert_eq!(
            slot_idxs(inbox.take_discarded()),
            vec![2],
            "换代清空的显存帧也必须归还"
        );
    }

    #[test]
    fn test_frame_inbox_overwrite_and_take() {
        use std::future::Future;
        use std::task::{Context, Waker};

        let doorbell = Arc::new(Notify::new());
        let inbox = FrameInbox::new(Arc::clone(&doorbell));
        assert!(inbox.take().is_none(), "初始收件箱必须为空");

        let pool = FramePool::new();
        pool.attach(heap_blocks());

        let mut buf1 = pool.acquire().expect("池里必须有余量");
        buf1.write(&[1u8; 128]).unwrap();
        inbox.push(VideoFrame {
            birth_nanos: 100,
            payload: FramePayload::Host { frame: buf1 },
        });

        // 再次 push 覆盖前一帧，前一帧必须自动通过 RAII Drop 归还给 pool
        let mut buf2 = pool.acquire().expect("池里必须有余量");
        buf2.write(&[2u8; 128]).unwrap();
        inbox.push(VideoFrame {
            birth_nanos: 200,
            payload: FramePayload::Host { frame: buf2 },
        });

        // 池深 3：两块被取走、旧的被覆盖帧已转入待归还队列 ⇒ 此刻仍只有一块可取
        assert_eq!(pool.len(), 1, "被覆盖的旧帧尚未由 Actor 回收");

        // Actor 每轮循环的归还动作：取走即析构回池
        drop(inbox.take_discarded());
        assert_eq!(pool.len(), 2, "Actor 取走后被覆盖的旧块必须回到复用池");

        // take 取出最新帧
        let frame = inbox.take().expect("必须能取出最新帧");
        assert_eq!(frame.birth_nanos, 200);
        assert!(inbox.take().is_none(), "take 后收件箱必须变空");

        // 帧离开作用域即归还
        drop(frame);
        assert_eq!(pool.len(), 3, "消费完成的帧必须自动归还");

        // 显式 clear
        inbox.clear();
        assert!(inbox.take().is_none(), "clear 后收件箱必须为空");

        // 投递即唤醒：调用方无需（也不应）另行唤醒 Actor。
        //
        // 两次 push 只应留下一个许可——这是「唤醒是状态而非消息」的全部意义。
        // 若唤醒排队，Actor 会被叫醒两次，第二次只能发现收件箱已空，白跑一轮。
        let mut cx = Context::from_waker(Waker::noop());
        let mut first = Box::pin(doorbell.notified());
        assert!(
            first.as_mut().poll(&mut cx).is_ready(),
            "push 必须留下唤醒许可"
        );
        let mut second = Box::pin(doorbell.notified());
        assert!(
            second.as_mut().poll(&mut cx).is_pending(),
            "两次 push 只应留下一个许可，唤醒必须被合并"
        );
    }
}

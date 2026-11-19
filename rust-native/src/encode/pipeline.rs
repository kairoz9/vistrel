//! 编码管线组合根 (Pipeline Composition Root)
//!
//! `VideoPipeline`：把编码段的各协作组件收进一个对象，使「一条管线
//! 的状态」有一个可命名的持有者，而不是散成 `StreamingEngine::new` 的五个构造参数。
//!
//! 分组依据是**机制**而非阶段：帧收件箱（capture→encode）、控制队列、门铃、关键帧触发器、
//! 主机帧池、编码线程句柄。它们跨阶段共享，按阶段切会切出互相引用而不是边界。
//!
//! 本模块只负责「构造、门面与关停」。管线的**行为**仍在各自模块：`actor` 是消费线程，
//! `control` 是命令协议，`output` 是编码产物的发件箱。

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::encode::actor::EncoderActor;
use crate::encode::control::{CONTROL_QUEUE_DEPTH, ControlCommand, ControlSender, ControlStatus};
use crate::encode::encoder::EncodeBackendKind;
use crate::encode::frame::{
    FrameInbox, FrameOutbox, FrameOutcome, FramePayload, Mailbox, VideoFrame,
};
use crate::encode::hardware::slots::SlotStateBlock;
use crate::encode::pool::FramePool;
use crate::encode::{
    KeyframeTrigger, StreamingProfile, bootstrap_hardware_caps, hardware_caps,
    is_hardware_caps_bootstrapped,
};
/// 关停等待编码线程的上限
///
/// 见 `VideoPipeline::shutdown`：设备销毁后 GPU 调用可能长时间不返回，无界等待会拖死渲染线程。
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 等待期间的轮询间隔：关停路径一次性的开销，10ms 足够且几乎不占 CPU
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 编码管线组合根：一条编码段的全部协作组件与它们的唯一持有者
///
/// 帧收件箱、控制队列、门铃、关键帧触发器、主机帧池、编码线程句柄按**机制**聚在一处
/// （组件跨阶段共享，按阶段切会切出互相引用而不是边界）；对外只露具名门面与
/// 「构造即完整、关停有界」的声明，行为仍在各组件模块
pub struct VideoPipeline {
    /// capture→encode seam：单值覆盖收件箱，写入即唤醒。
    inbox: Arc<FrameInbox>,
    /// 控制命令发送端：入队与唤醒绑定，调用方无法只做一半
    control: ControlSender,
    /// 关键帧触发器：请求方是推流段，消费方是编码线程
    keyframe: Arc<KeyframeTrigger>,
    /// 主机内存帧池：本管线是它唯一的所有者，编码器只是被 `attach` 进来的租户
    ///
    /// 帧块由当前后端在自己的设备上分配、经编码线程 `attach` 装进池；换代就在池内换批，
    /// 逐帧路径看不到「后端换了」——取不到块就丢帧，不会拿到别的设备的内存。
    pool: Arc<FramePool>,
    /// 槽位所有权共享块（泄漏分配与跨代隔离的设计理由见 `SlotStateBlock` 文档）
    slot_state: &'static SlotStateBlock,
    /// 发件箱（唯一活跃会话从它取件）：没有会话时帧照编照丢
    output: Arc<FrameOutbox>,
    /// 编码线程句柄，只在关停时取用一次
    actor_handle: Mutex<Option<JoinHandle<()>>>,
}

impl VideoPipeline {
    /// 槽位所有权共享块（游戏侧经当前活跃管线取得）
    pub(crate) fn slot_state(&self) -> &'static SlotStateBlock {
        self.slot_state
    }

    /// 发件箱：会话的写循环从这里取最新帧
    pub(crate) fn output(&self) -> Arc<FrameOutbox> {
        Arc::clone(&self.output)
    }

    /// 会话控制面的命令发送端：重配与跟随码率请求经它发往 Actor
    pub(crate) fn control(&self) -> &ControlSender {
        &self.control
    }

    /// 关键帧触发器：`VideoWriter` 用它在本会话的门控态就地要一次 IDR
    pub(crate) fn keyframe(&self) -> Arc<KeyframeTrigger> {
        Arc::clone(&self.keyframe)
    }

    /// 导入本次采集的显存槽位表（ask，阻塞式）
    pub(crate) fn import_vram_slots(
        &self,
        slots: Vec<crate::encode::hardware::VramSlotDescriptor>,
    ) -> Result<(), ControlStatus> {
        self.control.import_vram_slots(slots)
    }

    /// 会话建立前把编码器按档位就绪（ask），`Ok` 携带实际建成的后端身份
    pub(crate) async fn prepare_encoder(
        &self,
        profile: StreamingProfile,
    ) -> Result<EncodeBackendKind, ControlStatus> {
        self.control.prepare_encoder(profile).await
    }

    /// 释放显存槽位表（ask，阻塞式）
    pub(crate) fn clear_vram_slots(&self) -> Result<(), ControlStatus> {
        self.control.clear_vram_slots()
    }

    /// 构造全部 seam 并拉起编码线程
    ///
    /// 门铃、发件箱与编码线程全在内部建好：调用方拿到的就是完成品，没有「先建一半
    /// 再注入」的窗口。
    pub fn spawn() -> Arc<Self> {
        // 门铃与控制命令是两条独立的等待源，编码线程用 select! 同时阻塞在两者上。
        // 门铃必须是独立的：帧入箱不走通道，没有任何东西能唤醒 select。
        let doorbell = Arc::new(Notify::new());
        let (control_tx, control_rx) =
            tokio::sync::mpsc::channel::<ControlCommand>(CONTROL_QUEUE_DEPTH);
        let keyframe = Arc::new(KeyframeTrigger::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&doorbell)));
        let output: Arc<FrameOutbox> = Arc::new(Mailbox::new(inbox.outcomes_arc()));

        let actor_inbox = Arc::clone(&inbox);
        let actor_keyframe = Arc::clone(&keyframe);
        let actor_output = Arc::clone(&output);
        let actor_doorbell = Arc::clone(&doorbell);
        // 先建池再建 Actor：Actor 构造时就要把第一批块（软编的堆块）装进去，
        // 这样第一帧到达时池里已有可用块
        let pool = FramePool::new();
        let actor_pool = Arc::downgrade(&pool);
        // 槽位所有权块与引擎同代泄漏分配（为什么必须泄漏见 `SlotStateBlock` 文档）
        let slot_state = SlotStateBlock::new_leaked();
        let actor_slot_state = slot_state;

        let handle = std::thread::Builder::new()
            .name("vistrel-encoder-actor".to_string())
            .spawn(move || {
                // `extern "system"` 不允许 unwind 穿过互操作边界，编码线程必须自己兜住 panic
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let actor = EncoderActor::new(
                        actor_inbox,
                        actor_keyframe,
                        actor_output,
                        actor_pool,
                        actor_slot_state,
                    );
                    actor.run(control_rx, actor_doorbell);
                }));
                if outcome.is_err() {
                    log::error!("编码 Actor 发生 panic");
                }
            })
            .expect("Failed to spawn vistrel-encoder-actor thread");

        // 后端偏好只在这里定一次：本机能否硬编是启动期探测的产物，此后不随每次
        // 配编码器变化。探测尚未发生时不能默认按「无硬编」锁死——那会让本机永久跑软编且无人察觉，
        // 所以先补一次探测（正常路径上 `jni/api.rs` 已探测过，这里只读一次快照）
        if !is_hardware_caps_bootstrapped() {
            log::warn!("起编码管线时硬件能力尚未探测，就地补一次启动期探测");
            bootstrap_hardware_caps();
        }
        let prefer_hardware = hardware_caps().is_some();

        Arc::new(Self {
            inbox,
            control: ControlSender::new(control_tx, Arc::clone(&doorbell), prefer_hardware),
            keyframe,
            pool,
            slot_state,
            output,
            actor_handle: Mutex::new(Some(handle)),
        })
    }

    /// 只用调用方给的 seam 组装，不拉起编码线程
    ///
    /// 测试用它保留控制队列的接收端以断言派发了哪些命令，同时自己决定要不要真的跑编码
    /// 线程。生产路径用 [`VideoPipeline::spawn`]——那是唯一会拉起线程的入口。
    #[cfg(test)]
    pub(crate) fn assemble(
        control: ControlSender,
        keyframe: Arc<KeyframeTrigger>,
        inbox: Arc<FrameInbox>,
    ) -> Self {
        Self {
            inbox: Arc::clone(&inbox),
            control,
            keyframe,
            pool: FramePool::new(),
            slot_state: SlotStateBlock::new_leaked(),
            output: Arc::new(Mailbox::new(inbox.outcomes_arc())),
            actor_handle: Mutex::new(None),
        }
    }

    /// 投递一帧主机内存画面：从池里取一块写进去，交给编码线程
    ///
    /// `birth_nanos` 由采集侧给：它是端到端延迟的起点，必须在拷贝之前取。
    ///
    /// 取不到块说明编码器正把这一代块都用光（或正换代），此时丢帧：既不等，也不另起
    /// 一条普通内存的旁路——后端只接受池里的块，旁路换来的不是一帧画面而是一条错误。
    pub fn push_host_frame(&self, birth_nanos: u64, bytes: &[u8]) {
        // 字节数与编码器几何是否相符，由编码线程按当前档位校验（见 `EncoderActor::encode_frame`）：
        // 档位的事实源头在那里，同一处判才不会有陈旧值
        let Some(mut frame) = self.pool.acquire() else {
            // 丢帧是既定策略，计数与限频日志都在账本里做
            self.inbox.outcomes().record(FrameOutcome::PoolEmpty);
            return;
        };
        // 容量校验在 write 内做（块统一按最大几何分配）；失败时帧随 Drop 自动归还
        match frame.write(bytes) {
            Ok(()) => self.inbox.push(VideoFrame {
                birth_nanos,
                payload: FramePayload::Host { frame },
            }),
            Err(e) => self
                .inbox
                .outcomes()
                .record_with(FrameOutcome::WriteFailed, e),
        }
    }

    /// 投递一帧显存直通画面（槽位需先经 [`ControlSender::import_vram_slots`] 导入）
    pub fn push_vram_slot(&self, birth_nanos: u64, slot_idx: u32, sync_value: u64) {
        self.inbox.push(VideoFrame {
            birth_nanos,
            payload: FramePayload::VramSlot {
                slot_idx,
                sync_value,
            },
        });
    }

    /// 关停编码线程：投递 Shutdown 后**有界**等待
    ///
    /// 代价：超时返回后**不再保证 Actor 已退出**，调用方不得再假定显存槽位等资源已释放。
    /// 进程退出与编码停滞自愈重启都会走到本路径；被弃置的线程携带自己的编码上下文
    /// 与槽位引用继续存活，槽位状态块的代际隔离（块随引擎实例创建）使其干扰不到
    /// 后继引擎，泄漏资源随进程退出回收。
    pub fn shutdown(&self) {
        // 只投递不等回复：受理与否只说明命令是否入队，退出与否由下面的有界等待回答
        if !self.control.shutdown() {
            log::warn!("Shutdown 未能入队（队列已满或编码线程已退出）");
        }

        let Some(handle) = self.actor_handle.lock().take() else {
            return;
        };

        let deadline = Instant::now() + SHUTDOWN_JOIN_TIMEOUT;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                log::error!(
                    "编码线程 {} 秒内未退出，放弃等待",
                    SHUTDOWN_JOIN_TIMEOUT.as_secs()
                );
                return;
            }
            std::thread::sleep(SHUTDOWN_POLL_INTERVAL);
        }
        let _ = handle.join();
    }
}

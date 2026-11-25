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
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::encode::actor::EncoderActor;
use crate::encode::control::{CONTROL_QUEUE_DEPTH, ControlCommand, ControlSender, ControlStatus};
use crate::encode::encoder::EncodeBackendKind;
use crate::encode::frame::{
    FrameInbox, FrameOutbox, FrameOutcome, FramePayload, Mailbox, VideoFrame,
};
use crate::encode::hardware::VramSlotDescriptor;
use crate::encode::hardware::slots::VramBindings;
use crate::encode::pool::FramePool;
use crate::encode::{
    Heartbeat, StageCell, StreamingProfile, TierRequest, VideoCodec, bootstrap_hardware_caps,
    is_hardware_caps_bootstrapped,
};
/// 关停等待编码线程的上限
///
/// 见 `VideoPipeline::shutdown`：设备销毁后 GPU 调用可能长时间不返回，无界等待会拖死渲染线程。
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 等待期间的轮询间隔：关停路径一次性的开销，10ms 足够且几乎不占 CPU
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// packed 回复里状态字节的偏移：高 8 位存 `code + 1`，0 留给 JNI 异常默认值
const STATUS_SHIFT: u32 = 56;
/// packed 回复里心跳字段的偏移
const HEARTBEAT_SHIFT: u32 = 8;

/// 状态码 + 1：JNI 包装在 panic 时返回 0，0 必须落在未知/故障一侧。
pub(crate) fn pack_status(status: ControlStatus) -> i32 {
    status.code() + 1
}

/// 把槽位状态、在途掩码与心跳打成一个 `u64`（`vramSlotState` 快照专用）。
///
/// 状态存 `code + 1`：JNI 包装在 panic 时返回 0，若 `Ok` 编码为 0，一次异常会被
/// Java 读成「成功且全忙」，槽位永在途。0 必须落在未知/故障一侧。
pub(crate) fn pack_reply(status: ControlStatus, in_flight: u64, heartbeat: u64) -> u64 {
    ((pack_status(status) as u64) << STATUS_SHIFT)
        | ((heartbeat & 0xFFFF_FFFF) << HEARTBEAT_SHIFT)
        | (in_flight & 0xFF)
}

/// 编码管线组合根：一条编码段的全部协作组件与它们的唯一持有者
///
/// 帧收件箱、控制队列、门铃、主机帧池、编码线程句柄按**机制**聚在一处
/// （组件跨阶段共享，按阶段切会切出互相引用而不是边界）；对外只露具名门面与
/// 「构造即完整、关停有界」的声明，行为仍在各组件模块
pub struct VideoPipeline {
    /// capture→encode seam：单值覆盖收件箱，写入即唤醒。
    inbox: Arc<FrameInbox>,
    /// 控制命令发送端：入队与唤醒绑定，调用方无法只做一半
    control: ControlSender,
    /// 主机内存帧池：本管线是它唯一的所有者，编码器只是被 `attach` 进来的租户
    ///
    /// 帧块由当前后端在自己的设备上分配、经编码线程 `attach` 装进池；换代就在池内换批，
    /// 逐帧路径看不到「后端换了」——取不到块就丢帧，不会拿到别的设备的内存。
    pool: Arc<FramePool>,
    /// 当前显存绑定注册表：同一时刻至多一个绑定，Java 只持有句柄
    bindings: Arc<VramBindings>,
    /// 编码循环的存活信号：Actor 每轮 +1，JNI 快照只读
    heartbeat: Arc<Heartbeat>,
    /// 编码路径的当前阶段：Actor 与后端写入，JNI 诊断只读
    stage: Arc<StageCell>,
    /// 发件箱（唯一活跃会话从它取件）：没有会话时帧照编照丢
    output: Arc<FrameOutbox>,
    /// 编码线程句柄，只在关停时取用一次
    actor_handle: Mutex<Option<JoinHandle<()>>>,
}

impl VideoPipeline {
    /// 当前显存绑定注册表
    #[cfg(test)]
    pub(crate) fn bindings(&self) -> &Arc<VramBindings> {
        &self.bindings
    }

    /// 发件箱：会话的写循环从这里取最新帧
    pub(crate) fn output(&self) -> Arc<FrameOutbox> {
        Arc::clone(&self.output)
    }

    /// 会话控制面的命令发送端：重配与跟随码率请求经它发往 Actor
    pub(crate) fn control(&self) -> &ControlSender {
        &self.control
    }

    /// 导入本次采集的显存槽位表（ask，阻塞式）
    ///
    /// `owner` 是 JNI 入口捕获的渲染线程：随命令带入 Actor，供 push 校验线程归属。
    pub(crate) fn import_vram_slots(
        &self,
        owner: ThreadId,
        slots: Vec<VramSlotDescriptor>,
    ) -> Result<u64, ControlStatus> {
        self.control.import_vram_slots(owner, slots)
    }

    /// 会话建立前把编码器按定档请求与协商定死的 codec 就绪（ask），`Ok` 携带
    /// **实际生效**的档位、codec 与后端身份
    pub(crate) async fn prepare_encoder(
        &self,
        request: TierRequest,
        codec: VideoCodec,
        preferred_backend: Option<EncodeBackendKind>,
    ) -> Result<(StreamingProfile, VideoCodec, EncodeBackendKind), ControlStatus> {
        self.control
            .prepare_encoder(request, codec, preferred_backend)
            .await
    }

    /// 释放显存槽位表（ask，阻塞式）
    pub(crate) fn clear_vram_slots(&self, handle: u64) -> Result<(), ControlStatus> {
        self.control.clear_vram_slots(handle)
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
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&doorbell)));
        let output: Arc<FrameOutbox> = Arc::new(Mailbox::new(inbox.outcomes_arc()));

        let actor_inbox = Arc::clone(&inbox);
        let actor_output = Arc::clone(&output);
        let actor_doorbell = Arc::clone(&doorbell);
        // 先建池再建 Actor：Actor 构造时就要把第一批块（软编的堆块）装进去，
        // 这样第一帧到达时池里已有可用块
        let pool = FramePool::new();
        let actor_pool = Arc::downgrade(&pool);
        // 心跳、阶段与绑定注册表随引擎持有；旧 Actor 拿旧 Arc，写不到后继引擎
        let heartbeat = Arc::new(Heartbeat::new());
        let stage = Arc::new(StageCell::new());
        let bindings = VramBindings::new();
        let actor_heartbeat = Arc::clone(&heartbeat);
        let actor_stage = Arc::clone(&stage);
        let actor_bindings = Arc::clone(&bindings);

        let handle = std::thread::Builder::new()
            .name("vistrel-encoder-actor".to_string())
            .spawn(move || {
                // `extern "system"` 不允许 unwind 穿过互操作边界，编码线程必须自己兜住 panic
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let actor = EncoderActor::new(
                        actor_inbox,
                        actor_output,
                        actor_pool,
                        actor_heartbeat,
                        actor_stage,
                        actor_bindings,
                    );
                    actor.run(control_rx, actor_doorbell);
                }));
                if outcome.is_err() {
                    log::error!("编码 Actor 发生 panic");
                }
            })
            .expect("Failed to spawn vistrel-encoder-actor thread");

        // 自动后端以能力快照为依据：未探测时补一次启动期探测（正常路径上 `jni/api.rs` 已探测过）
        if !is_hardware_caps_bootstrapped() {
            log::warn!("起编码管线时硬件能力尚未探测，就地补一次启动期探测");
            bootstrap_hardware_caps();
        }

        Arc::new(Self {
            inbox,
            control: ControlSender::new(control_tx, Arc::clone(&doorbell)),
            pool,
            bindings,
            heartbeat,
            stage,
            output,
            actor_handle: Mutex::new(Some(handle)),
        })
    }

    /// 只用调用方给的 seam 组装，不拉起编码线程
    ///
    /// 测试用它保留控制队列的接收端以断言派发了哪些命令，同时自己决定要不要真的跑编码
    /// 线程。生产路径用 [`VideoPipeline::spawn`]——那是唯一会拉起线程的入口。
    #[cfg(test)]
    pub(crate) fn assemble(control: ControlSender, inbox: Arc<FrameInbox>) -> Self {
        Self {
            inbox: Arc::clone(&inbox),
            control,
            pool: FramePool::new(),
            bindings: VramBindings::new(),
            heartbeat: Arc::new(Heartbeat::new()),
            stage: Arc::new(StageCell::new()),
            output: Arc::new(Mailbox::new(inbox.outcomes_arc())),
            actor_handle: Mutex::new(None),
        }
    }

    /// 投递一帧主机内存画面：从池里取一块写进去，交给编码线程
    ///
    /// `birth_nanos` 是端到端延迟的起点，必须在拷贝之前取。`display` 是本帧形状，
    /// 由调用方随帧报。补齐不在这里——编码尺寸只有编码线程知道。
    ///
    /// 取不到块说明块全在途（或正换代）：丢帧，不等也不另起旁路。
    pub fn push_host_frame(&self, birth_nanos: u64, bytes: &[u8], display: (usize, usize)) {
        let Some(mut frame) = self.pool.acquire() else {
            // 丢帧是既定策略，计数与限频日志都在账本里做
            self.inbox.outcomes().record(FrameOutcome::PoolEmpty);
            return;
        };
        // 容量校验在写内做（块统一按最大几何分配）；失败时帧随 Drop 自动归还
        match frame.write(bytes, Some(display)) {
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
    ///
    /// `handle` 是导入返回的绑定句柄，`slot` 是渲染线程从 `vram_slot_state` 快照里选中的
    /// 空闲槽位。句柄、线程、槽位任一不符都在入口拒绝，帧不进入收件箱。
    ///
    /// 只回答「帧收没收到」；掩码与心跳只在 `vram_slot_state` 快照里返回。
    pub(crate) fn push_vram_slot(
        &self,
        handle: u64,
        slot: u32,
        birth_nanos: u64,
        sync_value: u64,
    ) -> ControlStatus {
        self.bindings
            .with_current(handle, |binding| {
                if binding.owner() != std::thread::current().id() {
                    return ControlStatus::WrongThread;
                }
                if binding.is_poisoned() {
                    return ControlStatus::Poisoned;
                }
                // 先置位再入箱：编码极快时 ack 不能清掉尚未置的位
                let Some(owned) = binding.try_acquire(slot) else {
                    return ControlStatus::SlotNotFree;
                };
                self.inbox.push(VideoFrame {
                    birth_nanos,
                    payload: FramePayload::VramSlot {
                        slot: owned,
                        sync_value,
                    },
                });
                ControlStatus::Ok
            })
            // 旧绑定：收件箱里没有新帧，也没有任何槽位被置位
            .unwrap_or(ControlStatus::StaleBinding)
    }

    /// 当前在途掩码与心跳快照：游戏侧每 tick 调用一次，同时刷新停滞判定基准
    pub(crate) fn vram_slot_state(&self, handle: u64) -> u64 {
        match self.bindings.with_current(handle, |binding| {
            if binding.is_poisoned() {
                (ControlStatus::Poisoned, binding.in_flight())
            } else {
                (ControlStatus::Ok, binding.in_flight())
            }
        }) {
            Some((status, in_flight)) => pack_reply(status, in_flight, self.heartbeat.get()),
            None => pack_reply(ControlStatus::StaleBinding, 0, 0),
        }
    }

    /// 句柄有效时返回编码路径当前阶段码（诊断冷路径）
    pub(crate) fn stall_stage(&self, handle: u64) -> Option<u64> {
        self.bindings.with_current(handle, |_| self.stage.get())
    }

    /// 关停编码线程：投递 Shutdown 后**有界**等待
    ///
    /// 代价：超时返回后**不再保证 Actor 已退出**，调用方不得再假定显存槽位等资源已释放。
    /// 进程退出与编码停滞自愈重启都会走到本路径；被弃置的线程携带自己的编码上下文
    /// 与绑定引用继续存活，心跳、阶段与绑定注册表按引擎隔离（新引擎是新的 `Arc`），
    /// 其干扰不到后继引擎，泄漏资源随进程退出回收。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::frame::FrameInbox;
    use crate::encode::hardware::slots::VramBinding;
    use crate::encode::nv12_len;
    use crate::encode::pool::{HOST_SLOT_BYTES, HostBlock};
    use tokio::sync::Notify;

    /// 投递只落原始字节与采集几何：编码尺寸不在生产者视野里，补齐在编码线程做
    #[test]
    fn test_push_host_frame_carries_display_and_raw_bytes() {
        let doorbell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&doorbell)));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let pipeline =
            VideoPipeline::assemble(ControlSender::new(tx, doorbell), Arc::clone(&inbox));
        pipeline
            .pool
            .attach(vec![HostBlock::Heap(vec![0u8; HOST_SLOT_BYTES])]);
        assert_eq!(pipeline.pool.len(), 1, "attach 之后池里必须有块");

        let display = (8usize, 8usize);
        let source = vec![7u8; nv12_len(display.0, display.1)];
        pipeline.push_host_frame(0, &source, display);

        let mut written = host_frame(&inbox, &pipeline);
        assert_eq!(
            written.len(),
            nv12_len(display.0, display.1),
            "投递阶段不补齐：字节数就是采集尺寸"
        );
        assert_eq!(
            written.display(),
            Some(display),
            "帧必须带上生产者的采集几何，编码线程据此校验与补齐"
        );

        // 编码线程拿到的编码尺寸才是补齐的目标
        let coded = (16usize, 16usize);
        written
            .pad_in_place(coded)
            .expect("采集几何合法的帧必须能就地补齐");
        assert_eq!(
            written.len(),
            nv12_len(coded.0, coded.1),
            "补齐后字节数按编码尺寸"
        );
    }

    /// 旧句柄的推送必须被拒绝，且不触碰任何槽位
    #[test]
    fn test_stale_vram_push_is_rejected_without_touching_slots() {
        let doorbell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&doorbell)));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let pipeline =
            VideoPipeline::assemble(ControlSender::new(tx, doorbell), Arc::clone(&inbox));

        let binding = VramBinding::new_for_test(3);
        pipeline.bindings().install(Arc::clone(&binding), || {});
        let stale = binding.handle() + 1_000;

        let reply = pipeline.push_vram_slot(stale, 1, 0, 7);
        assert_eq!(
            reply,
            ControlStatus::StaleBinding,
            "旧句柄必须回 StaleBinding"
        );
        assert_eq!(binding.in_flight(), 0, "旧句柄不得触碰槽位");
        assert!(inbox.take().is_none(), "旧句柄的帧不得进入收件箱");
    }

    /// 当前句柄推送后槽位置位；载荷析构归还
    #[test]
    fn test_vram_push_marks_slot_and_drop_acks() {
        let doorbell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&doorbell)));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let pipeline =
            VideoPipeline::assemble(ControlSender::new(tx, doorbell), Arc::clone(&inbox));

        let binding = VramBinding::new_for_test(3);
        pipeline.bindings().install(Arc::clone(&binding), || {});

        let reply = pipeline.push_vram_slot(binding.handle(), 2, 0, 9);
        assert_eq!(reply, ControlStatus::Ok, "当前句柄必须受理");
        assert!(binding.is_in_flight(2), "push 必须置位在途");
        assert_eq!(binding.in_flight(), 1 << 2);

        let frame = inbox.take().expect("帧必须入箱");
        drop(frame);
        assert!(!binding.is_in_flight(2), "载荷析构必须归还槽位");
    }

    /// 毒化后绑定拒绝推送，快照回报 Poisoned
    #[test]
    fn test_poisoned_binding_reports_poisoned() {
        let doorbell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&doorbell)));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let pipeline =
            VideoPipeline::assemble(ControlSender::new(tx, doorbell), Arc::clone(&inbox));

        let binding = VramBinding::new_for_test(3);
        pipeline.bindings().install(Arc::clone(&binding), || {});
        let poisoned = binding.try_acquire(0).unwrap();
        poisoned.poison();
        drop(poisoned);
        assert!(binding.is_poisoned());

        let reply = pipeline.push_vram_slot(binding.handle(), 1, 0, 7);
        assert_eq!(reply, ControlStatus::Poisoned, "毒化后推送必须回 Poisoned");
        assert_eq!(binding.in_flight(), 1, "被拒推送不得置位新槽位");
        assert!(inbox.take().is_none(), "毒化后的帧不得进入收件箱");

        let state = pipeline.vram_slot_state(binding.handle());
        assert_eq!(
            ((state >> 56) as i32) - 1,
            ControlStatus::Poisoned.code(),
            "快照必须让游戏侧看到毒化"
        );
    }

    /// 取出收件箱里的主机帧（唯一一帧，取完即空）
    fn host_frame(
        inbox: &Arc<FrameInbox>,
        pipeline: &VideoPipeline,
    ) -> crate::encode::pool::FrameBuf {
        let outcomes = pipeline.inbox.outcomes();
        let frame = inbox.take().unwrap_or_else(|| {
            panic!(
                "必须入箱；pool_empty={} write_failed={}",
                outcomes.count(FrameOutcome::PoolEmpty),
                outcomes.count(FrameOutcome::WriteFailed)
            )
        });
        match frame.payload {
            FramePayload::Host { frame } => frame,
            _ => panic!("主机帧必须是 Host 载荷"),
        }
    }
}

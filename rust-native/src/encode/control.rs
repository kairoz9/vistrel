//! 控制队列协议
//!
//! 调用方（渲染线程 / 主线程）经 [`ControlSender`] 入队生命周期命令。每条命令的等待纪律
//! 写在方法上，应答通道由变体自身携带 —— tell 变体不带应答，ask 变体自带一次性应答通道，
//! 所以「这条命令该不该等」在类型上就没有歧义。
//! 管线死亡时通过 Channel Drop 实现 0ms fail-closed 应答。

use crate::encode::encoder::EncodeBackendKind;
use crate::encode::hardware::slots::VramSlotDescriptor;
use crate::encode::{StreamingProfile, TierRequest, VideoCodec};
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, SyncSender};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;

/// 控制命令 ACK 等待上限，超时按 fail-closed 处理
///
/// 用例经 [`ControlSender::with_ack_timeout`] 注入更短的上限：超时分支必须真的跑到，
/// 而全 crate 级的缩短会把别处的真实线程往返也一起压成超时。
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// 控制队列容量
///
/// 命令都是罕见操作（会话建立、档位变更、显存槽位注册、关停），16 格足够；满即说明编码
/// 线程已经不消费了，此时丢弃并返回 `QueueFull` 比阻塞发送方更诚实。
pub(crate) const CONTROL_QUEUE_DEPTH: usize = 16;

/// 控制命令结果码：0 为成功，非零为失败原因。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlStatus {
    Ok = 0,
    /// 批量注册的数组长度不是每槽位 `long` 数的整数倍,协议错位
    MalformedBatch = 1,
    /// 本机无可用硬件编码器
    NoHardwareEncoder = 2,
    /// 外部显存或时间线信号量导入失败（fd 非法、DMA-BUF 不支持等）
    ImportFailed = 3,
    /// Actor 已死
    ActorDead = 4,
    /// Actor ACK 超时（5s）
    AckTimeout = 5,
    /// Actor 拒绝 Profile 变更（编码器重配失败），档位保持不变
    ReconfigureFailed = 6,
    /// 控制队列已满，命令未被受理
    QueueFull = 7,
    /// 注销槽位前排空失败：编码器可能仍在导入的资源上等待，引用与槽位表均未释放
    ReleaseFailed = 8,
    /// 硬编建成失败：探测认证过的候选后端全部建不起来。与 `NoHardwareEncoder`
    /// 的区别是后者表示「本机探测不到硬编」，本码表示「探测说有，实际建不出来」
    EncoderBuildFailed = 9,
    /// 显存绑定轮次已前移：调用方持有的槽位导入令牌属于旧绑定，整批拒绝
    StaleBinding = 10,
}

/// 控制命令
///
/// 形状即纪律：tell 变体不带应答（投递即完成），ask 变体自带一次性应答通道。因此
/// `Shutdown` 不可能被等待，`PrepareEncoder` 也不可能被静默投递。变体只给 Actor 侧看，
/// 调用方一律经 [`ControlSender`] 的具名方法。
#[derive(Debug)]
pub(crate) enum ControlCommand {
    /// 排空并关停编码线程
    Shutdown,
    /// 设置跟随码率
    BitrateChange(usize),
    /// 挂起一次关键帧请求：下一帧以 IDR 编码（tell）
    /// 请求是幂等意图而非逐条事件：重复投递在消费点塌缩成一个挂起标志
    RequestKeyframe,
    /// 按定档请求重定档位，回送实际生效的档位或拒绝原因
    Retier {
        request: TierRequest,
        reply: tokio::sync::oneshot::Sender<Result<StreamingProfile, ControlStatus>>,
    },
    /// 协商落账：按定档请求与协商定死的 codec/后端偏好配好编码器，回送实际生效的
    /// 档位、codec 与后端身份（硬编建败回退软编如实上报）
    PrepareEncoder {
        request: TierRequest,
        codec: VideoCodec,
        preferred_backend: Option<EncodeBackendKind>,
        reply: tokio::sync::oneshot::Sender<
            Result<(StreamingProfile, VideoCodec, EncodeBackendKind), ControlStatus>,
        >,
    },
    /// 导入显存直通槽位
    ImportVramSlots {
        binding_epoch: u64,
        slots: Vec<VramSlotDescriptor>,
        reply: SyncSender<ControlStatus>,
    },
    /// 释放槽位表但保持编码器存活
    ClearVramSlots {
        binding_epoch: u64,
        reply: SyncSender<ControlStatus>,
    },
}

/// 控制面发送端
///
/// 把「入队」与「唤醒」绑成一个动作：投递成功即唤醒 Actor，调用方没有「只入队不唤醒」
/// 这个选项——两步分离会让命令滞留到下一次无关唤醒。
#[derive(Clone)]
pub(crate) struct ControlSender {
    tx: Sender<ControlCommand>,
    doorbell: Arc<Notify>,
    /// ACK 等待上限，生产固定为 [`ACK_TIMEOUT`]
    ack_timeout: Duration,
}

impl ControlSender {
    /// 构造发送端
    pub(crate) fn new(tx: Sender<ControlCommand>, doorbell: Arc<Notify>) -> Self {
        Self {
            tx,
            doorbell,
            ack_timeout: ACK_TIMEOUT,
        }
    }

    /// 只给用例用：把 ACK 等待上限压到毫秒级，让超时分支真的跑到
    #[cfg(test)]
    fn with_ack_timeout(mut self, timeout: Duration) -> Self {
        self.ack_timeout = timeout;
        self
    }

    /// 关停编码线程（tell）
    ///
    /// 受理即返回 `true`，不等 Actor 退出：退出与否由
    /// [`super::pipeline::VideoPipeline::shutdown`] 的有界 join 负责。队列满恰恰说明 Actor
    /// 已经卡住，此刻阻塞投递会连调用方一起拖死 —— 那正是要消除的卡死形态。
    pub(crate) fn shutdown(&self) -> bool {
        self.send(ControlCommand::Shutdown).is_ok()
    }

    /// 跟随码率下发（tell），`bps` 单位是 bit/s
    ///
    /// 码率环路下一拍会再发，投递失败不必上报：丢掉一拍追随量与丢掉整条命令不同。
    pub(crate) fn set_bitrate(&self, bps: usize) {
        let _ = self.send(ControlCommand::BitrateChange(bps));
    }

    /// 关键帧请求（tell）：Actor 把挂起标志置位，下一帧以 IDR 编码
    pub(crate) fn request_keyframe(&self) {
        let _ = self.send(ControlCommand::RequestKeyframe);
    }

    /// 请求重定档位（ask）
    ///
    /// `Ok` 是 Actor **实际生效**的档位：调用方提交 Actor 说的准，而不是
    /// 「成功就假定请求生效、失败就回读旧值」这套两阶段推断。`Err(ReconfigureFailed)`
    /// 表示 Actor 拒绝了这次重定档、维持原档位；其余错误码表示命令没送达或应答超时，
    /// 调用方同样维持已知档位。
    pub(crate) async fn retier(
        &self,
        request: TierRequest,
    ) -> Result<StreamingProfile, ControlStatus> {
        let (reply, wait) = tokio::sync::oneshot::channel();
        self.send(ControlCommand::Retier { request, reply })
            .map_err(Self::send_error)?;
        match tokio::time::timeout(self.ack_timeout, wait).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(ControlStatus::ActorDead),
            Err(_) => Err(ControlStatus::AckTimeout),
        }
    }

    /// 协商落账：按定档请求把编码器配好（ask），新会话接入、顶号、预热都走这一条
    ///
    /// codec 与后端偏好随命令携带（偏好 `None` = 自动）；`Ok` 才代表已就绪，档位、
    /// codec 与后端都是实际生效值——硬编建败回退软编时如实携带回退值，调用方不必靠猜
    pub(crate) async fn prepare_encoder(
        &self,
        request: TierRequest,
        codec: VideoCodec,
        preferred_backend: Option<EncodeBackendKind>,
    ) -> Result<(StreamingProfile, VideoCodec, EncodeBackendKind), ControlStatus> {
        let (reply, wait) = tokio::sync::oneshot::channel();
        self.send(ControlCommand::PrepareEncoder {
            request,
            codec,
            preferred_backend,
            reply,
        })
        .map_err(Self::send_error)?;
        match tokio::time::timeout(self.ack_timeout, wait).await {
            Ok(Ok(outcome)) => outcome,
            // 应答通道随 Actor 线程一起消失
            Ok(Err(_)) => Err(ControlStatus::ActorDead),
            Err(_) => Err(ControlStatus::AckTimeout),
        }
    }

    /// 导入显存直通槽位（ask）
    ///
    /// 由 Actor 在专用线程完成驱动导入并激活硬件直通。同步等待：调用方在渲染线程上，不能 `.await`，而应答必须能按时放弃 —— 所以走
    /// `std::sync::mpsc`（`oneshot::blocking_recv` 没有超时版本）。
    /// 投递同样非阻塞：队列满说明 Actor 已卡住不消费，此刻阻塞投递会连渲染线程
    /// 一起拖死，直接回 [`ControlStatus::QueueFull`] 让调用方走各自的降级分支。
    pub(crate) fn import_vram_slots(
        &self,
        binding_epoch: u64,
        slots: Vec<VramSlotDescriptor>,
    ) -> Result<(), ControlStatus> {
        self.ask_blocking(|reply| ControlCommand::ImportVramSlots {
            binding_epoch,
            slots,
            reply,
        })
    }

    /// 释放槽位表但保持硬件编码器存活（ask）
    ///
    /// 次序是「先尝试排空设备队列、再丢弃导入」：最后一帧的跨 API 回执还在队列上，先丢弃导入
    /// 会让它写到已弃掉的信号量上。排空失败只记日志、不进结果码——`Ok` 只表示槽位已释放。
    pub(crate) fn clear_vram_slots(&self, binding_epoch: u64) -> Result<(), ControlStatus> {
        self.ask_blocking(|reply| ControlCommand::ClearVramSlots {
            binding_epoch,
            reply,
        })
    }

    /// 非阻塞入队并唤醒 Actor
    fn send(&self, cmd: ControlCommand) -> Result<(), TrySendError<ControlCommand>> {
        self.tx.try_send(cmd)?;
        self.doorbell.notify_one();
        Ok(())
    }

    fn send_error(e: TrySendError<ControlCommand>) -> ControlStatus {
        match e {
            TrySendError::Full(_) => ControlStatus::QueueFull,
            TrySendError::Closed(_) => ControlStatus::ActorDead,
        }
    }

    /// 同步往返：投递一条带码应答的 ask 命令并等待 ACK
    fn ask_blocking(
        &self,
        build: impl FnOnce(SyncSender<ControlStatus>) -> ControlCommand,
    ) -> Result<(), ControlStatus> {
        let (reply, wait) = std::sync::mpsc::sync_channel(1);
        self.send(build(reply)).map_err(Self::send_error)?;
        match wait.recv_timeout(self.ack_timeout) {
            Ok(code) => code.into_result(),
            Err(RecvTimeoutError::Disconnected) => Err(ControlStatus::ActorDead),
            Err(RecvTimeoutError::Timeout) => Err(ControlStatus::AckTimeout),
        }
    }
}

impl ControlStatus {
    /// 结果码的数值形式：返回值与日志共用的唯一出口
    pub(crate) fn code(self) -> i32 {
        self as i32
    }

    /// 结果码转成结果：`Ok` 是唯一成功值
    fn into_result(self) -> Result<(), Self> {
        match self {
            Self::Ok => Ok(()),
            other => Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::VideoCodec;
    use std::sync::Mutex;
    use std::sync::mpsc::Receiver as StdReceiver;
    use std::time::Instant;

    /// 假 Actor：普通线程上消费控制队列，剧本拿到命令自行应答
    ///
    /// 用普通线程而非 tokio 任务：同步派发路径必须在 runtime 之外调用，那正是渲染线程上
    /// 的真实形态。
    fn fake_actor<F>(mut script: F) -> ControlSender
    where
        F: FnMut(ControlCommand) + Send + 'static,
    {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ControlCommand>(CONTROL_QUEUE_DEPTH);
        let bell = Arc::new(Notify::new());
        std::thread::spawn(move || {
            while let Some(cmd) = rx.blocking_recv() {
                script(cmd);
            }
        });
        ControlSender::new(tx, bell)
    }

    /// 只回 `Ok`、并把命令名转发出来的假 Actor
    fn recording_actor() -> (ControlSender, StdReceiver<&'static str>) {
        let (recorded_tx, recorded_rx) = std::sync::mpsc::channel();
        let sender = fake_actor(move |cmd| {
            if recorded_tx.send(label(&cmd)).is_err() {
                return;
            }
            match cmd {
                ControlCommand::PrepareEncoder { reply, .. } => {
                    let default = StreamingProfile::default();
                    let _ =
                        reply.send(Ok((default, VideoCodec::H264, EncodeBackendKind::Software)));
                }
                ControlCommand::ImportVramSlots { reply, .. } => {
                    let _ = reply.send(ControlStatus::Ok);
                }
                ControlCommand::ClearVramSlots { reply, .. } => {
                    let _ = reply.send(ControlStatus::Ok);
                }
                ControlCommand::Retier { reply, .. } => {
                    let default = StreamingProfile::default();
                    let _ = reply.send(Ok(default));
                }
                ControlCommand::Shutdown
                | ControlCommand::BitrateChange(_)
                | ControlCommand::RequestKeyframe => {}
            }
        });
        (sender, recorded_rx)
    }

    /// 命令的稳定名字：用例断言的是「跨过接口的是哪条命令」，不是枚举布局
    fn label(cmd: &ControlCommand) -> &'static str {
        match cmd {
            ControlCommand::Shutdown => "shutdown",
            ControlCommand::BitrateChange(_) => "set_bitrate",
            ControlCommand::RequestKeyframe => "request_keyframe",
            ControlCommand::Retier { .. } => "retier",
            ControlCommand::PrepareEncoder { .. } => "prepare_encoder",
            ControlCommand::ImportVramSlots { .. } => "import_vram_slots",
            ControlCommand::ClearVramSlots { .. } => "clear_vram_slots",
        }
    }

    #[test]
    fn test_ask_code_is_answered() {
        let (sender, recorded) = recording_actor();
        assert_eq!(sender.clear_vram_slots(1), Ok(()));
        assert_eq!(
            recorded.recv_timeout(Duration::from_secs(1)).unwrap(),
            "clear_vram_slots"
        );
    }

    #[test]
    fn test_ask_without_responder_reports_dead_actor() {
        let (tx, rx) = tokio::sync::mpsc::channel::<ControlCommand>(CONTROL_QUEUE_DEPTH);
        let bell = Arc::new(Notify::new());
        drop(rx);
        let sender = ControlSender::new(tx, bell);
        assert_eq!(
            sender.clear_vram_slots(1),
            Err(ControlStatus::ActorDead),
            "没有接收端时必须报管线已死，绝不能报成功"
        );
    }

    #[test]
    fn test_queue_full_is_reported_without_blocking() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<ControlCommand>(CONTROL_QUEUE_DEPTH);
        for _ in 0..CONTROL_QUEUE_DEPTH {
            tx.try_send(ControlCommand::Shutdown)
                .expect("预热控制队列必须成功");
        }
        let bell = Arc::new(Notify::new());
        let sender = ControlSender::new(tx, bell);
        let start = Instant::now();
        assert_eq!(sender.clear_vram_slots(1), Err(ControlStatus::QueueFull));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "队列满必须立即返回，不得阻塞调用方"
        );
    }

    #[test]
    fn test_tell_is_not_waited_on() {
        let (sender, recorded) = recording_actor();
        let start = Instant::now();
        sender.set_bitrate(4_000_000);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "tell 命令不得等待应答"
        );
        assert_eq!(
            recorded.recv_timeout(Duration::from_secs(1)).unwrap(),
            "set_bitrate"
        );
    }

    /// Actor 收下命令却压住应答：调用方必须按时放弃，而不是假装成功
    #[test]
    fn test_ack_timeout_is_fail_closed() {
        let held = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&held);
        let sender = fake_actor(move |cmd| {
            if let ControlCommand::ClearVramSlots { reply, .. } = cmd {
                sink.lock().unwrap().push(reply);
            }
        })
        .with_ack_timeout(Duration::from_millis(50));
        let start = Instant::now();
        assert_eq!(sender.clear_vram_slots(1), Err(ControlStatus::AckTimeout));
        assert!(start.elapsed() < Duration::from_secs(1), "超时必须按时返回");
        drop(held);
    }

    /// 后端偏好随命令逐次携带：偏好值必须真的送达 Actor，否则「指定后端」只是注释。
    /// codec 同为协商定死的随命令事实，一并钉住送达
    #[test]
    fn test_prepare_encoder_carries_backend_preference() {
        let (recorded_tx, recorded_rx) = std::sync::mpsc::channel();
        let sender = fake_actor(move |cmd| {
            if let ControlCommand::PrepareEncoder {
                codec,
                preferred_backend,
                reply,
                ..
            } = cmd
            {
                let _ = recorded_tx.send((codec, preferred_backend));
                let default = StreamingProfile::default();
                let _ = reply.send(Ok((default, codec, EncodeBackendKind::Software)));
            }
        });
        let request = TierRequest {
            width: 1280,
            height: 720,
            fps: 60,
            bpp: 0.1,
        };
        let rt = tokio::runtime::Runtime::new().expect("测试 runtime 必须能建起来");
        assert_eq!(
            rt.block_on(sender.prepare_encoder(
                request,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            )),
            Ok((
                StreamingProfile::default(),
                VideoCodec::H264,
                EncodeBackendKind::Software
            ))
        );
        assert_eq!(
            recorded_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (VideoCodec::H264, Some(EncodeBackendKind::Nvenc)),
            "随命令携带的 codec 与后端偏好必须送达 Actor"
        );
    }

    /// 数据型应答必须是 Actor 送来的生效档位，不能被降级成结果码
    #[tokio::test]
    async fn test_retier_returns_applied_tier() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ControlCommand>(CONTROL_QUEUE_DEPTH);
        let bell = Arc::new(Notify::new());
        let applied = StreamingProfile::new(1024, 768, 48, 2_000_000);
        std::thread::spawn(move || {
            while let Some(cmd) = rx.blocking_recv() {
                if let ControlCommand::Retier { reply, .. } = cmd {
                    let _ = reply.send(Ok(applied));
                }
            }
        });

        let request = TierRequest {
            width: 1280,
            height: 720,
            fps: 60,
            bpp: 0.1,
        };
        let got = ControlSender::new(tx, bell)
            .retier(request)
            .await
            .expect("假 Actor 应答过，必须拿到值");
        assert_ne!(
            got,
            request.derive(VideoCodec::H264),
            "应答不得是请求值的回显"
        );
        assert_eq!(got, applied, "应答必须是 Actor 实际生效的档位");
    }

    /// Actor 拒绝重定档时，拒绝原因必须跨过接口，而不是让调用方比对档位去猜
    #[tokio::test]
    async fn test_retier_rejection_is_reported() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ControlCommand>(CONTROL_QUEUE_DEPTH);
        let bell = Arc::new(Notify::new());
        std::thread::spawn(move || {
            while let Some(cmd) = rx.blocking_recv() {
                if let ControlCommand::Retier { reply, .. } = cmd {
                    let _ = reply.send(Err(ControlStatus::ReconfigureFailed));
                }
            }
        });

        let err = ControlSender::new(tx, bell)
            .retier(TierRequest {
                width: 1280,
                height: 720,
                fps: 60,
                bpp: 0.1,
            })
            .await
            .expect_err("拒绝必须原样上报，不得折成「维持原档位」让调用方自己推断");
        assert_eq!(err, ControlStatus::ReconfigureFailed);
    }
}

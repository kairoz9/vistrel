//! 专用编码执行器
//!
//! Actor 线程独占全部编码器实例与 CUDA Context，消费投递的帧，
//! 经 `control` 串行处理生命周期命令并以一次性 ACK 应答调用方；
//! Actor panic 时管线进入 fail-closed。

use std::sync::{Arc, Weak};
use std::thread::ThreadId;

use arc_swap::ArcSwapOption;
use tokio::sync::Notify;
use tokio::sync::mpsc::Receiver;

use crate::encode::control::{ControlCommand, ControlStatus, RetierError};
use crate::encode::frame::{FrameInbox, FrameOutbox, FrameOutcome, VideoFrame};
use crate::encode::hardware::VramSlotDescriptor;
use crate::encode::hardware::slots::{MAX_VRAM_SLOTS, VramBinding};
use crate::encode::pool::{FramePool, HostBlock};
use crate::encode::{
    EncodeStage, FramePayload, Heartbeat, StageCell, StampedVideoPacket, StreamingProfile,
    TierRequest, VideoCodec, VideoPacket, nv12_len,
};

use super::encoder::{EncodeBackendKind, VideoEncoder};

/// 后端构建入口：生产固定为真实驱动工厂，测试注入假后端；`kind` 为本次要建的身份
type HardwareBuilder = dyn Fn(
        StreamingProfile,
        VideoCodec,
        EncodeBackendKind,
        Arc<StageCell>,
    ) -> Result<(VideoEncoder, Vec<HostBlock>), String>
    + Send
    + Sync;

/// 编码执行器：单线程 Actor，独占编码器实例与 CUDA Context
///
/// 主循环「等待输入 → 命令清空 → 最新帧编码入箱」（Latest-Frame-Wins），生命周期命令
/// 经 control 串行处理并以一次性 ACK 应答；构造即以软编打底就绪，硬编后端经工厂在
/// 重建时升级、建不起来整体回退软编——不存在「编码器起来了、帧块却没就绪」的中间态
pub struct EncoderActor {
    /// 帧收件箱 (Capture→Encode)
    inbox: Arc<FrameInbox>,
    /// 挂起的关键帧请求：控制命令置位、帧步消费、编码未兑现时回挂。
    /// 初值 true 确保新管线首帧必出 IDR
    pending_keyframe: bool,
    /// 编码器：Hardware 或 Software；无硬件或未就绪时为 None
    encoder: Option<VideoEncoder>,
    /// 编码产物发件箱：管线持有、会话取用。这里只做同步入箱，写轨道由会话自己的写循环
    /// 完成，编码线程永不等待网络
    output: Arc<FrameOutbox>,
    /// Followed Bitrate (bps)，由 RateControl 在值变化时经 BitrateChange 派发。
    bitrate: usize,
    /// 生效档位：几何与码率上限的事实快照（本结构是档位的唯一事实源头）
    profile: StreamingProfile,
    /// 生效 codec：协商定死的会话常量，只被协商落账命令改写；重定档按在位值派生
    codec: VideoCodec,
    /// 主机帧池：只持一个弱引用，用来把后端新分配的块装进池。
    pool: Weak<FramePool>,
    /// 编码循环的存活信号：主循环每轮 +1
    heartbeat: Arc<Heartbeat>,
    /// 编码路径的当前阶段：主循环与硬件后端写入
    stage: Arc<StageCell>,
    /// 当前显存绑定：与管线共享的原子指针，导入时发布，注销/换档时置空
    binding: Arc<ArcSwapOption<VramBinding>>,
    /// 硬编后端工厂：唯一依赖注入点
    hardware_builder: Arc<HardwareBuilder>,
    /// true＝拒收显存帧：档位已换等重导入、或槽位表已清空等重新导入。
    /// 只开不关——恢复接收只认导入命令，见 `end_profile_command`；主机帧不受影响
    rejecting_frames: bool,
}

impl EncoderActor {
    /// 生产构造：注入真实驱动工厂并立即以软编打底，构造完成即具备编码能力
    ///
    /// 只装配不启动线程，主循环由 [`Self::run`] 在专用线程驱动；后端新分配的帧块
    /// 随构造装入弱引用的帧池。测试注入假工厂走 `new_with_hardware_builder`
    pub fn new(
        inbox: Arc<FrameInbox>,
        output: Arc<FrameOutbox>,
        pool: Weak<FramePool>,
        heartbeat: Arc<Heartbeat>,
        stage: Arc<StageCell>,
        binding: Arc<ArcSwapOption<VramBinding>>,
    ) -> Self {
        Self::new_with_hardware_builder(
            inbox,
            output,
            pool,
            heartbeat,
            stage,
            binding,
            Arc::new(|profile, codec, kind, stage| {
                VideoEncoder::new_backend(profile, codec, kind, Some(stage))
            }),
        )
    }

    /// 构造器注入硬编工厂；生产入口 [`Self::new`] 固定用真实驱动
    fn new_with_hardware_builder(
        inbox: Arc<FrameInbox>,
        output: Arc<FrameOutbox>,
        pool: Weak<FramePool>,
        heartbeat: Arc<Heartbeat>,
        stage: Arc<StageCell>,
        binding: Arc<ArcSwapOption<VramBinding>>,
        hardware_builder: Arc<HardwareBuilder>,
    ) -> Self {
        Self {
            inbox,
            pending_keyframe: true,
            encoder: None,
            output,
            bitrate: 0,
            profile: StreamingProfile::default(),
            codec: VideoCodec::H264,
            pool,
            heartbeat,
            stage,
            binding,
            hardware_builder,
            rejecting_frames: false,
        }
    }

    /// 把后端新分配的块装进池
    ///
    /// 这是唯一的改池动作与旧批析构点（线程约束见模块文档不变量 2/3）。
    /// 池已关停时块在这里就地析构（此时设备仍由块自己拖住）。
    fn attach(&self, blocks: Vec<HostBlock>) {
        match self.pool.upgrade() {
            Some(pool) => pool.attach(blocks),
            None => {
                if !blocks.is_empty() {
                    log::warn!("主机帧池已关闭，新建帧块就地释放");
                }
            }
        }
    }

    /// 采纳生效 codec；换代时丢弃发件箱里旧 codec 的包
    ///
    /// 发件箱单槽且非空时停止入箱，旧包会在无会话期间滞留；换 codec 后它属于
    /// 另一条轨道，必须清掉。
    fn adopt_codec(&mut self, codec: VideoCodec) {
        if self.codec != codec
            && let Some(packet) = self.output.take()
        {
            self.output.outcomes().record(FrameOutcome::Superseded);
            drop(packet);
        }
        self.codec = codec;
    }

    /// Actor 主循环：等待任一输入 -> 命令清空 -> 最新帧编码并放入发件箱
    ///
    /// 只用 current_thread runtime 驱动 `select!`/门铃/命令通道（没有定时器需求）：
    /// 一条线程跑完编码与入槽，写轨道不在本线程上等待网络。
    pub fn run(mut self, mut control_rx: Receiver<ControlCommand>, doorbell: Arc<Notify>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("编码线程 runtime 构建失败");
        runtime.block_on(async move {
            loop {
                // 1. 阻塞等待第一个输入。
                self.stage.set(EncodeStage::Idle);
                tokio::select! {
                    msg = control_rx.recv() => match msg {
                        Some(cmd) => {
                            if !self.apply(cmd) {
                                return;
                            }
                        }
                        // 所有发送端消失 = 控制面已关停
                        None => return,
                    },
                    _ = doorbell.notified() => {}
                }

                // 2. 心跳：Actor 停止迭代即心跳冻结，游戏侧据此判定编码线程停滞
                self.heartbeat.bump();

                // 3. 补归还被收件箱覆盖的显存帧：这些帧从未被编码，纹理无人读取，
                //    生产侧正等着位清零后重新认领
                self.ack_discarded();

                // 4. 清空可能积压的其余命令
                while let Ok(cmd) = control_rx.try_recv() {
                    if !self.apply(cmd) {
                        return;
                    }
                }

                // 5. 有帧就处理并把产物放进管线的发件箱（Latest-Frame-Wins）
                // 入箱是同步的：本帧处理完立刻取下一帧，控制命令最坏延迟 = 一次编码
                if let Some(frame) = self.inbox.take() {
                    self.handle_frame(frame);
                }
            }
        })
    }

    /// 处理一条已收到的命令；返回 `false` 表示 Actor 应当退出
    ///
    /// 会重建编码器的命令（`PrepareEncoder` / `Retier` / `ImportVramSlots` /
    /// `ClearVramSlots`）会让旧编码器上的一切（显存槽位注册、会话状态）失效。
    fn apply(&mut self, cmd: ControlCommand) -> bool {
        match cmd {
            ControlCommand::Shutdown => {
                self.stage.set(EncodeStage::Shutdown);
                // 摘除绑定并清空在途帧：句柄从此失效
                self.invalidate_binding();
                // 编码器随 Actor 一起析构：先摘掉引用，避免后续路径误用已排空的编码器
                self.encoder = None;
                return false;
            }
            ControlCommand::BitrateChange(bitrate) => {
                self.stage.set(EncodeStage::Reconfigure);
                self.bitrate = bitrate;
                Self::apply_bitrate(&mut self.encoder, bitrate);
            }
            ControlCommand::RequestKeyframe => {
                self.pending_keyframe = true;
            }
            ControlCommand::Retier { request, reply } => {
                let (before_profile, before_codec, had_table) = self.begin_profile_command();
                let outcome = self.retier(request);
                self.end_profile_command(before_profile, before_codec, had_table);
                let _ = reply.send(outcome);
            }
            ControlCommand::PrepareEncoder {
                request,
                codec,
                preferred_backend,
                reply,
            } => {
                let (before_profile, before_codec, had_table) = self.begin_profile_command();
                let outcome = self.prepare_encoder(request, codec, preferred_backend);
                self.end_profile_command(before_profile, before_codec, had_table);
                let _ = reply.send(outcome);
            }
            ControlCommand::ImportVramSlots {
                owner,
                slots,
                reply,
            } => {
                self.discard_frames_before_rebuild();
                let code = self.import_vram_slots(owner, slots);
                // 恢复接收只认导入命令，无论成败：成功即新表生效；失败时旧表恢复或
                // 表空，交给既有索引查找与几何兜底
                self.rejecting_frames = false;
                let outcome = if code == ControlStatus::Ok {
                    self.current_binding_handle()
                        .ok_or(ControlStatus::VramFault)
                } else {
                    Err(code)
                };
                let _ = reply.send(outcome);
            }
            ControlCommand::ClearVramSlots { handle, reply } => {
                if self.current_binding_handle() != Some(handle) {
                    // 旧句柄：旧导入已随实例销毁，注销是空操作，不动当前拒收状态
                    let _ = reply.send(ControlStatus::Ok);
                } else {
                    self.discard_frames_before_rebuild();
                    let code = self.clear_vram_slots(handle);
                    if code == ControlStatus::Ok {
                        // 表空：继续拒收直到下一次导入
                        self.rejecting_frames = true;
                    } else {
                        // 排空失败，注销没发生，旧表原样保留——旧帧仍合法
                        self.rejecting_frames = false;
                    }
                    let _ = reply.send(code);
                }
            }
        }
        true
    }

    /// 换档类命令（`Retier` / `PrepareEncoder`）的统一前置：
    /// 置阶段码、清收件箱，返回命令前快照 `(档位, codec, 此前是否硬编在位)`——
    /// 只有硬编可能持有槽位注册，软编在位时无从谈起拒收
    fn begin_profile_command(&mut self) -> (StreamingProfile, VideoCodec, bool) {
        self.stage.set(EncodeStage::Reconfigure);
        self.discard_frames_before_rebuild();
        (
            self.profile,
            self.codec,
            self.encoder.as_ref().is_some_and(|e| e.is_hardware()),
        )
    }

    /// 换档类命令的统一收尾：几何或 codec 实际变化且此前硬编在位时，开始拒收
    /// 显存帧。只开不关——一旦拒收，恢复只认导入命令，中途的 fps-only 重配
    /// 不得提前放行旧代帧；本体重配失败不开（档位没变就没有漂移）
    fn end_profile_command(
        &mut self,
        before: StreamingProfile,
        before_codec: VideoCodec,
        hardware_table: bool,
    ) {
        let after = &self.profile;
        if hardware_table
            && (before.width() != after.width()
                || before.height() != after.height()
                || before_codec != self.codec)
        {
            self.rejecting_frames = true;
            log::debug!("档位已变，等待槽位重导入：期间显存帧在入口拒收");
        }
    }

    /// 换代命令的前置收尾：清空收件箱并在本线程归还未编码帧的资源；必须先于命令本体
    /// 执行。已取走的帧不可能与本命令并发（同一线程）。
    fn discard_frames_before_rebuild(&mut self) {
        self.inbox.clear();
        self.ack_discarded();
    }

    /// 归还被收件箱丢弃的帧持有的资源：这些帧从未被编码，槽位随载荷析构归还；
    /// 主机块析构要拿池锁，因此必须在编码线程回收。
    fn ack_discarded(&mut self) {
        drop(self.inbox.take_discarded());
    }

    /// 摘除当前绑定并清空在途帧：句柄从此失效。
    /// 指针替换前已载入旧绑定的 push 仍可能把旧代帧塞进收件箱，由入口代次校验丢弃。
    fn invalidate_binding(&mut self) {
        if self.binding.load().is_none() {
            return;
        }
        self.binding.store(None);
        self.inbox.clear();
        self.ack_discarded();
    }

    /// 当前绑定句柄：无绑定返回 None
    fn current_binding_handle(&self) -> Option<u64> {
        self.binding.load().as_ref().map(|binding| binding.handle())
    }

    /// 注销显存槽位；排空失败时保留导入引用并回 `VramFault`——`Ok` 才是游戏侧
    /// 销毁导出纹理/信号量的唯一依据。没有编码器则没有导入引用，无需排空。
    fn clear_vram_slots(&mut self, handle: u64) -> ControlStatus {
        if self.current_binding_handle() != Some(handle) {
            // 句柄已不是当前绑定：导入已随旧编码器销毁，注销是空操作。
            // 返回 Ok 让游戏侧可以立即复用/销毁导出资源
            return ControlStatus::Ok;
        }
        let Some(mut encoder) = self.encoder.take() else {
            // 无编码器：没有导入引用，摘除绑定即可
            self.invalidate_binding();
            return ControlStatus::Ok;
        };
        self.stage.set(EncodeStage::Drain);
        if let Err(e) = encoder.drain() {
            // 排空失败即上下文不可信：后端整体弃置（mem::forget 永不析构）
            std::mem::forget(encoder);
            log::error!("槽位注销前排空失败，后端弃置，保留导入引用: {e}");
            return ControlStatus::VramFault;
        }
        self.stage.set(EncodeStage::ClearSlots);
        // 排空成功：先摘绑定并清空在途帧，再释放导入
        self.invalidate_binding();
        encoder.clear_vram_slots();
        self.encoder = Some(encoder);
        ControlStatus::Ok
    }

    /// 导入显存直通槽位（不动档位）：成功即安装新绑定并回送句柄；失败时后端整批回滚，
    /// 旧表与旧绑定继续有效。
    fn import_vram_slots(
        &mut self,
        owner: ThreadId,
        mut slots: Vec<VramSlotDescriptor>,
    ) -> ControlStatus {
        if let Some(bad) = slots
            .iter()
            .find(|descriptor| descriptor.slot_idx as usize >= MAX_VRAM_SLOTS)
        {
            log::error!("显存槽位索引越界: {} >= {}", bad.slot_idx, MAX_VRAM_SLOTS);
            // 未进入导入：描述符 Drop 关闭原始句柄
            return ControlStatus::VramFault;
        }
        if !self.encoder.as_ref().is_some_and(|e| e.is_hardware()) {
            log::error!("硬件编码不可用");
            return ControlStatus::EncoderUnavailable;
        }
        let Some(mut encoder) = self.encoder.take() else {
            return ControlStatus::EncoderUnavailable;
        };
        // 换表全清前置 drain
        self.stage.set(EncodeStage::Drain);
        if let Err(e) = encoder.drain() {
            // 排空失败即上下文不可信：后端整体弃置（mem::forget 永不析构）
            std::mem::forget(encoder);
            log::error!("槽位导入前排空失败，后端弃置，整批拒绝: {e}");
            return ControlStatus::EncoderUnavailable;
        }
        self.stage.set(EncodeStage::ImportSlots);
        match encoder.import_vram_slots(&mut slots) {
            Ok(()) => {
                self.encoder = Some(encoder);
                let slot_mask = slots.iter().fold(0u64, |mask, descriptor| {
                    mask | (1u64 << descriptor.slot_idx)
                });
                let binding = Arc::new(VramBinding::new(slot_mask, owner));
                // 先清收件箱再发布新代：窗口内旧句柄晚到的帧由编码入口按代次丢弃
                self.inbox.clear();
                self.binding.store(Some(binding));
                self.ack_discarded();
                ControlStatus::Ok
            }
            Err(e) => {
                // 后端整批回滚，旧表与旧绑定保持有效
                log::error!("显存直通槽位注册失败: {}", e);
                self.encoder = Some(encoder);
                ControlStatus::VramFault
            }
        }
    }

    /// 下发码率，并把失败写进日志
    ///
    /// 相同原因只记一次：后端恒定不支持时（目前只剩 Vulkan Video），GCC 每跨一次 10% 台阶
    /// 就会重试，逐次告警只会把日志刷满。某次成功即清空记录，使后续再次失败重新可见。
    fn apply_bitrate(encoder: &mut Option<VideoEncoder>, bitrate: usize) {
        let Some(encoder) = encoder.as_mut() else {
            return;
        };
        match encoder.set_bitrate(bitrate) {
            Ok(()) => {}
            Err(e) => {
                log::warn!("码率下发失败 ({} kbps): {}", bitrate / 1000, e)
            }
        }
    }

    /// 协商落账：按定档请求与协商定死的 codec 配好编码器。档位派生在此发生，
    /// 同一后端在位时只重配不重建；`Ok` 携带实际生效的档位、codec 与后端身份
    fn prepare_encoder(
        &mut self,
        request: TierRequest,
        codec: VideoCodec,
        preferred_backend: Option<EncodeBackendKind>,
    ) -> Result<(StreamingProfile, VideoCodec, EncodeBackendKind), ControlStatus> {
        let candidates = Self::candidate_backends(preferred_backend);

        // 在位后端仍是首选候选时只重配不重建（省一次设备上下文构建）
        let preferred = candidates[0];
        // 目标即软编时请求先顶格（几何钳 + codec 钳 H264）：重配与重建两条路吃同一份钳制
        let (request, codec) = Self::clamp_for(preferred, request, codec);
        let profile = request.derive(codec);
        let before_width = self.profile.width();
        let before_height = self.profile.height();
        let before_codec = self.codec;
        if self.current_backend() == Some(preferred)
            && let Some(encoder) = self.encoder.as_mut()
        {
            match encoder.reconfigure(profile, codec) {
                Ok(fresh_blocks) => {
                    self.attach(fresh_blocks);
                    self.profile = profile;
                    self.adopt_codec(codec);
                    // 同实例重配只保留槽位；几何或 codec 变化会让旧导入失效
                    if before_width != profile.width()
                        || before_height != profile.height()
                        || before_codec != codec
                    {
                        self.invalidate_binding();
                    }
                    return Ok(self.finish_prepare());
                }
                Err(e) => log::warn!("编码器重配被拒，按候选列表重建: {}", e),
            }
        }

        // 候选依次尝试：探测清单本来就是「这台机器认证可用」的集合，跳过其中可用项
        // 直接报错等于浪费探测结果；全部失败才是真正的建成失败
        let mut last_error = ControlStatus::EncoderUnavailable;
        for candidate in candidates {
            let (candidate_request, candidate_codec) = Self::clamp_for(candidate, request, codec);
            let code = self.rebuild_encoder(candidate_request, candidate_codec, candidate);
            if code == ControlStatus::Ok {
                return Ok(self.finish_prepare());
            }
            // 软编是最后一个候选，它也会失败只可能是 OpenH264 库缺失
            last_error = code;
        }
        log::error!("协商落账失败：候选后端全部创建失败（{last_error:?}）");
        Err(last_error)
    }

    /// 建成后收尾：码率复位并读取实际生效的身份
    ///
    /// `Ok` 却无编码器意味着不变量已破坏，按拒绝收口而不是编造一个后端。
    fn finish_prepare(&mut self) -> (StreamingProfile, VideoCodec, EncodeBackendKind) {
        // 码率复位到**生效档位**的上限
        self.bitrate = self.profile.bitrate();
        Self::apply_bitrate(&mut self.encoder, self.bitrate);
        let backend = self
            .current_backend()
            .expect("rebuild/reconfigure 成功即必须有编码器在位");
        (self.profile, self.codec, backend)
    }

    /// 本次协商的后端候选序列（按尝试次序）
    ///
    /// 自动偏好按探测清单原序（厂商优先级）逐个尝试，软编恒收尾——软编永远是可行的
    /// 兜底目标，因为它不需要驱动背书。
    fn candidate_backends(preferred: Option<EncodeBackendKind>) -> Vec<EncodeBackendKind> {
        if let Some(kind) = preferred {
            return vec![kind];
        }
        let mut candidates: Vec<_> = crate::encode::hardware_backends()
            .map(|list| list.iter().map(|p| p.backend_type).collect())
            .unwrap_or_default();
        candidates.push(EncodeBackendKind::Software);
        candidates
    }

    /// 按目标后端钳制定档请求：软编只出 H.264 且几何顶格，硬编原样透传
    fn clamp_for(
        target: EncodeBackendKind,
        request: TierRequest,
        codec: VideoCodec,
    ) -> (TierRequest, VideoCodec) {
        if target == EncodeBackendKind::Software {
            (Self::software_ceiling_request(request), VideoCodec::H264)
        } else {
            (request, codec)
        }
    }

    /// 当前编码器后端身份；编码器尚未就绪时为 `None`（回执层如实呈现为「未定」）
    fn current_backend(&self) -> Option<EncodeBackendKind> {
        self.encoder.as_ref().map(|e| e.backend_kind())
    }

    /// 重建编码器，销毁旧编码器的一切：CUcontext、已导入的显存槽位、当前 GOP
    ///
    /// 目标即软编时请求顶格 + codec 钳 H264（软编只出 H264 码流）。单一职责：
    /// 目标建不起来就整体失败（Actor 留在无编码器态）
    fn rebuild_encoder(
        &mut self,
        request: TierRequest,
        codec: VideoCodec,
        target: EncodeBackendKind,
    ) -> ControlStatus {
        let had_hardware = self.encoder.as_ref().is_some_and(|e| e.is_hardware());
        if had_hardware {
            log::warn!("重建硬件编码器：已导入的显存槽位随旧编码器失效，需重新注册");
        }
        // 旧实例即将销毁：先摘除绑定，旧句柄的导入/推送/注销从此刻起全部失效
        self.invalidate_binding();

        let mut request = request;
        let mut codec = codec;
        if target == EncodeBackendKind::Software {
            request = Self::software_ceiling_request(request);
            codec = VideoCodec::H264;
        }
        let profile = request.derive(codec);

        // 必须先释放旧编码器
        self.encoder = None;
        let built = if target == EncodeBackendKind::Software {
            self.install_software(profile, codec);
            self.encoder.is_some()
        } else {
            match (self.hardware_builder.as_ref())(profile, codec, target, Arc::clone(&self.stage))
            {
                Ok((hw, blocks)) => {
                    log::debug!("硬件编码器就绪!");
                    self.attach(blocks);
                    self.encoder = Some(hw);
                    true
                }
                Err(e) => {
                    log::error!("硬件编码器初始化失败: {e}");
                    false
                }
            }
        };

        if !built {
            // 建成失败意味着此刻没有可用编码器；重配被拒由 retier 的局部错误表达，旧档位仍在
            log::error!("编码器建立失败，档位维持 {:?}", self.profile);
            return ControlStatus::EncoderUnavailable;
        }

        self.profile = profile;
        self.adopt_codec(codec);
        ControlStatus::Ok
    }

    /// 软编请求顶格：`min(Content Resolution, 1080p) @ 60`，只缩不放、保留 bpp。
    /// 钳制发生在派生之前：derive 的预算与码率模型消费的是顶格后的请求
    fn software_ceiling_request(request: TierRequest) -> TierRequest {
        let (w, h, f) =
            crate::encode::clamp_to_software_ceiling(request.width, request.height, request.fps);
        if (w, h, f) != (request.width, request.height, request.fps) {
            log::warn!(
                "软编顶格：请求 {}x{}@{} 钳制为 {w}x{h}@{f}",
                request.width,
                request.height,
                request.fps
            );
        }
        TierRequest {
            width: w,
            height: h,
            fps: f,
            bpp: request.bpp,
        }
    }

    /// 建软编并把它的堆块装进池
    fn install_software(&mut self, profile: StreamingProfile, codec: VideoCodec) {
        match VideoEncoder::new_software(profile, codec) {
            Ok((encoder, blocks)) => {
                self.attach(blocks);
                self.encoder = Some(encoder);
            }
            Err(e) => {
                log::error!("软编码器初始化失败: {}", e);
                self.encoder = None;
            }
        }
    }

    /// 应用定档请求，返回**实际生效**的档位或拒绝原因（codec 是会话常量，本命令不改写）
    fn retier(&mut self, request: TierRequest) -> Result<StreamingProfile, RetierError> {
        // 软编在跑：请求几何先顶格 min(Content, 1080p)@60，生效值如实反映钳制
        let request = if self.encoder.as_ref().is_some_and(|e| !e.is_hardware()) {
            Self::software_ceiling_request(request)
        } else {
            request
        };
        let profile = request.derive(self.codec);
        let before_width = self.profile.width();
        let before_height = self.profile.height();

        if self.profile.width() == profile.width()
            && self.profile.height() == profile.height()
            && self.profile.fps() == profile.fps()
        {
            // 形状未变：编码器不必重建，直接采纳（bpp 只改码率上限，由码率回路兑现）。
            // 采纳而不是忽略，是为了不留下「会话用新值、Actor 用旧值」的分叉
            self.profile = profile;
            return Ok(profile);
        }

        // 编码器尚未就绪：直接采纳，后续按新档位构建
        let reconfig = match &mut self.encoder {
            // 在位编码器的 codec 与 self.codec 恒等：唯一改写口是协商落账
            Some(encoder) => encoder.reconfigure(profile, self.codec),
            None => Ok(Vec::new()),
        };
        let fresh_blocks = match reconfig {
            Ok(blocks) => blocks,
            Err(e) => {
                // 重建失败已拆掉会话与槽位表：绑定立即失效，停止接收旧代显存帧
                if self
                    .encoder
                    .as_ref()
                    .is_some_and(|encoder| !encoder.is_usable())
                {
                    self.invalidate_binding();
                    self.rejecting_frames = true;
                    log::error!(
                        "档位变更失败，编码器会话已拆除，等待重新协商 {:?}: {}",
                        self.profile,
                        e
                    );
                } else {
                    log::error!("档位变更失败，维持原档位 {:?}: {}", self.profile, e);
                }
                return Err(RetierError::Rejected);
            }
        };
        self.attach(fresh_blocks);

        self.profile = profile;
        // 几何变化会让旧编码器上的槽位导入失效；fps/bpp 变化只改码控，保留绑定
        if before_width != profile.width() || before_height != profile.height() {
            self.invalidate_binding();
        }
        if self.bitrate > 0 {
            let target = self.bitrate.min(profile.bitrate());
            Self::apply_bitrate(&mut self.encoder, target);
        }
        Ok(self.profile)
    }

    /// 编码一帧并把产物放进发件箱。
    ///
    /// 编码器由控制面（Profile 确立 / 槽位注册）预先配置完成，本方法只做编码与入箱。
    /// 入箱是同步的：门控、连接与过期判定都在会话侧取帧之后，本线程不等待网络。
    fn encode_frame(&mut self, payload: &mut FramePayload, birth_nanos: u64) {
        let Some(encoder) = &mut self.encoder else {
            // 未配置编码器时消费掉的帧同样没上链路，与编码失败并账。
            // 槽位认领随帧载荷析构自动归还
            self.inbox
                .outcomes()
                .record_with(FrameOutcome::EncodeFailed, "尚无编码器");
            return;
        };

        // 发件箱未消费时不得抢跑：覆盖已编码帧会切断参考链，丢弃未编码输入则
        // 参考链完整。判定先于 `force_keyframe` 取用，挂起的请求不被消费。
        if !self.output.is_empty() {
            self.inbox
                .outcomes()
                .record_with(FrameOutcome::Superseded, "发件箱未消费，编码前丢弃本帧");
            return;
        }

        // 长度与布局由 `pad_in_place` 判；本处只判它无从知道的事：这帧是不是当前档位产出的
        if let FramePayload::Host { frame } = payload {
            let coded = encoder.coded_extent();
            let display = (self.profile.width(), self.profile.height());
            let err = match frame.display() {
                // 换档后在途的旧帧：比编码尺寸小也会被补齐成「合法」的一屏
                Some(declared) if declared != display => Some(format!(
                    "本帧采集几何 {declared:?} 与当前档位 {display:?} 不同步"
                )),
                Some(_) => frame.pad_in_place(coded).err(),
                // 未声明几何（成品布局）：只能校长度
                None => (frame.len() != nv12_len(coded.0, coded.1)).then(|| {
                    format!(
                        "本帧 {} 字节，成品布局应为编码尺寸 {coded:?} 的 {} 字节",
                        frame.len(),
                        nv12_len(coded.0, coded.1)
                    )
                }),
            };
            if let Some(e) = err {
                self.inbox
                    .outcomes()
                    .record_with(FrameOutcome::GeometryMismatch, e);
                return;
            }
        }

        let force_keyframe = std::mem::take(&mut self.pending_keyframe);
        self.stage.set(EncodeStage::EncodeFrame);
        let payload = &*payload;
        match encoder.encode(payload, force_keyframe) {
            Ok((nalu, is_keyframe)) if !nalu.is_empty() => {
                self.output.post(StampedVideoPacket::now(VideoPacket {
                    nalu,
                    birth_nanos,
                    is_keyframe,
                }));
            }
            Ok(_) => {
                // 空 NALU：编码器消费了强制关键帧，却没产出码流，同样没上链路
                let source = if matches!(payload, FramePayload::VramSlot { .. }) {
                    "显存直通"
                } else {
                    "主机帧"
                };
                self.inbox
                    .outcomes()
                    .record_with(FrameOutcome::EncodeFailed, format!("空 NALU（{source}）"));
                self.rearm_keyframe(force_keyframe);
            }
            Err(e) => {
                self.inbox
                    .outcomes()
                    .record_with(FrameOutcome::EncodeFailed, e);
                self.rearm_keyframe(force_keyframe);
            }
        }
    }

    /// 编码未兑现时把关键帧请求挂回去
    ///
    /// 挂起标志是幂等布尔，只在本轮确实带着请求时才重置——否则会凭空多要一个 IDR。
    fn rearm_keyframe(&mut self, requested: bool) {
        if requested {
            self.pending_keyframe = true;
        }
    }

    /// 处理取出的最新帧：换代拒收先于编码
    ///
    /// 旧表/旧代槽位帧在入口记 `FenceDropped`：旧 sync_value 可能落到新导入的信号量上
    /// 长期等待，旧索引也会按新表解释。主机帧自带像素，照常编码；槽位认领随载荷析构归还旧代。
    fn handle_frame(&mut self, mut frame: VideoFrame) {
        if self.rejecting_frames && matches!(frame.payload, FramePayload::VramSlot { .. }) {
            self.inbox.outcomes().record_with(
                FrameOutcome::FenceDropped,
                "槽位表重建中：显存帧暂不接收，等待重新导入",
            );
            return;
        }
        if let FramePayload::VramSlot { slot, .. } = &frame.payload {
            let current = self.binding.load();
            let stale = !current
                .as_ref()
                .is_some_and(|binding| binding.handle() == slot.binding_handle());
            if stale {
                self.inbox
                    .outcomes()
                    .record_with(FrameOutcome::FenceDropped, "绑定已换代：显存帧入口丢弃");
                return;
            }
        }
        self.encode_frame(&mut frame.payload, frame.birth_nanos);
    }
}

#[cfg(test)]
mod tests {
    /// 采集侧按编码尺寸出帧：软编不补齐（对齐粒度 1），测试载荷即编码尺寸 × 1.5
    fn host_bytes(profile: &StreamingProfile) -> usize {
        crate::encode::host_nv12_len(profile, (1, 1))
    }

    use bytes::Bytes;

    use crate::encode::VideoCodec;
    use crate::encode::encoder::{EncodeBackendKind, VideoEncodeBackend};
    use crate::encode::frame::{Mailbox, VideoFrame};
    use crate::encode::hardware::VramSlotDescriptor;
    use crate::encode::hardware::slots::VramBinding;
    use crate::encode::pool::test_support::host_payload;
    use crate::encode::pool::{FramePool, POOL_DEPTH, fake_device_blocks};
    use crate::encode::{Heartbeat, StageCell};

    use super::*;

    /// 测试用发件箱：直接取用 `FrameOutbox`，断言与生产同一条路径
    fn test_outbox() -> Arc<FrameOutbox> {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(bell));
        Arc::new(Mailbox::new(inbox.outcomes_arc()))
    }

    /// 空的当前绑定指针：与生产同一条共享路径
    fn empty_binding() -> Arc<ArcSwapOption<VramBinding>> {
        Arc::new(ArcSwapOption::from(None))
    }

    /// 假硬编后端：只上报硬件身份，不碰任何驱动
    struct FakeBackend {
        kind: EncodeBackendKind,
    }

    impl VideoEncodeBackend for FakeBackend {
        fn kind(&self) -> EncodeBackendKind {
            self.kind
        }

        fn coded_align(&self) -> (u32, u32) {
            (1, 1)
        }

        fn encode(
            &mut self,
            _payload: &FramePayload,
            _force_idr: bool,
            _frame_idx: u64,
        ) -> Result<(Bytes, bool), String> {
            Err("假后端不编码".to_string())
        }

        fn reconfigure(
            &mut self,
            _new_profile: StreamingProfile,
            _new_codec: VideoCodec,
        ) -> Result<Vec<HostBlock>, String> {
            Ok(Vec::new())
        }

        fn set_bitrate(&mut self, _bps: u32) -> Result<(), String> {
            Ok(())
        }

        fn import_vram_slots(
            &mut self,
            _descriptors: &mut [VramSlotDescriptor],
        ) -> Result<(), String> {
            Err("假后端无显存槽位".to_string())
        }

        fn clear_vram_slots(&mut self) {}

        fn drain(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    const FAKE_DEVICE_DOMAIN: usize = 0xDEAD_BEEF;

    /// 用例定档请求：bpp 落下限，与旧构造路径（MIN_BPP 档）的派生值完全一致。
    /// codec 不在请求里，派生时按用例在位常量传入
    fn tier(w: usize, h: usize, fps: usize) -> TierRequest {
        TierRequest {
            width: w,
            height: h,
            fps,
            bpp: crate::encode::MIN_BPP,
        }
    }

    /// 假硬编工厂：按给定后端身份交出编码器与新域块，形状与生产入口一致
    fn fake_hardware_builder() -> Arc<HardwareBuilder> {
        Arc::new(move |profile, codec, kind, _stage| {
            Ok((
                VideoEncoder::from_backend(profile, codec, Box::new(FakeBackend { kind })),
                fake_device_blocks(FAKE_DEVICE_DOMAIN),
            ))
        })
    }

    /// 建一个测试 Actor：池由测试持有（与生产一致，Actor 只有弱引用）
    fn test_actor(
        inbox: Arc<FrameInbox>,
        output: Arc<FrameOutbox>,
    ) -> (EncoderActor, Arc<FramePool>) {
        test_actor_with_builder(
            inbox,
            output,
            Arc::new(|profile, codec, kind, stage| {
                VideoEncoder::new_backend(profile, codec, kind, Some(stage))
            }),
        )
    }

    /// 注入自定义硬编工厂的 Actor 构造
    fn test_actor_with_builder(
        inbox: Arc<FrameInbox>,
        output: Arc<FrameOutbox>,
        hardware_builder: Arc<HardwareBuilder>,
    ) -> (EncoderActor, Arc<FramePool>) {
        let pool = FramePool::new();
        let actor = EncoderActor::new_with_hardware_builder(
            inbox,
            output,
            Arc::downgrade(&pool),
            Arc::new(Heartbeat::new()),
            Arc::new(StageCell::new()),
            empty_binding(),
            hardware_builder,
        );
        (actor, pool)
    }

    /// 控制命令的单一出口必须在 ACK 之前把后端交回的块装进池；换代时旧域块整体下水
    ///
    /// 硬编由注入的假后端模拟（硬件身份 + 假设备块），不触碰真实驱动；
    /// 驱动契约仍由 `encode::hardware` 的真设备用例覆盖
    #[test]
    fn test_actor_prepare_switches_frame_domain() {
        crate::encode::software::init_api_for_tests();

        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, pool) =
            test_actor_with_builder(inbox, Arc::clone(&output), fake_hardware_builder());
        let request = tier(1280, 720, 60);

        // 1. 请求硬编：ACK 时池必须已换成假设备块
        assert_eq!(
            prepare(
                &mut actor,
                request,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        assert_eq!(pool.len(), POOL_DEPTH, "装批必须在 ACK 之前完成");
        let frame = pool.acquire().expect("硬编必须给块");
        assert_eq!(
            frame.fake_domain(),
            Some(FAKE_DEVICE_DOMAIN),
            "池必须装的是假后端交回的设备块"
        );
        drop(frame);
        assert_eq!(pool.stale_len(), 0);

        // 2. 退回软编：旧域块在换批时释放，ACK 时池已是堆块
        assert_eq!(
            prepare(
                &mut actor,
                request,
                VideoCodec::H264,
                Some(EncodeBackendKind::Software)
            ),
            ControlStatus::Ok
        );
        let frame = pool.acquire().expect("软编必须给块");
        assert!(
            frame.cuda_host().is_none()
                && frame.vulkan_buffer().is_none()
                && frame.fake_domain().is_none(),
            "软编的块必须来自堆，不得持有任何设备内存"
        );
        drop(frame);
        assert_eq!(pool.stale_len(), 0, "换域时旧设备块必须已释放");
    }

    /// 换 codec 时发件箱里的旧 codec 包必须清掉：新会话的轨道只认新 codec 的码流
    #[test]
    fn test_prepare_codec_change_discards_old_outbox_packet() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) =
            test_actor_with_builder(inbox, Arc::clone(&output), fake_hardware_builder());

        // 旧 codec 在位，发件箱里留一帧旧 codec 的包
        assert_eq!(
            prepare(
                &mut actor,
                tier(1280, 720, 60),
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        output.post(StampedVideoPacket::now(VideoPacket {
            nalu: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
            birth_nanos: 0,
            is_keyframe: true,
        }));
        assert!(!output.is_empty());

        // 新会话协商 H265：旧包属于另一条轨道，必须被清掉
        assert_eq!(
            prepare(
                &mut actor,
                tier(1280, 720, 60),
                VideoCodec::H265,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        assert!(output.is_empty(), "换 codec 后旧 codec 的包不得留在发件箱");
        assert_eq!(
            output.outcomes().count(FrameOutcome::Superseded),
            1,
            "清掉的旧包要计入 Superseded"
        );
    }

    /// **软编只能是目标，不能是回退**：指定硬编建不起来时必须显式失败，绝不静默换成软编
    #[test]
    fn test_prepare_fails_instead_of_falling_back_to_software() {
        crate::encode::software::init_api_for_tests();

        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) = test_actor_with_builder(
            inbox,
            Arc::clone(&output),
            Arc::new(|_profile, _codec, _kind, _stage| Err("硬编构建失败".to_string())),
        );
        let request = tier(1280, 720, 60);

        // 指定硬编建败：显式失败，不落软编
        assert_eq!(
            prepare_kind(
                &mut actor,
                request,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            Err(ControlStatus::EncoderUnavailable),
            "指定硬编建不起来时必须显式失败"
        );
        assert!(
            actor.encoder.is_none(),
            "失败路径不得残留任何编码器（尤其是静默换上的软编）"
        );

        // 显式指定软编仍能建成：软编作为**目标**永远合法
        assert_eq!(
            prepare(
                &mut actor,
                request,
                VideoCodec::H264,
                Some(EncodeBackendKind::Software)
            ),
            ControlStatus::Ok,
            "软编作为显式目标是合法路径，不受上一条影响"
        );
        assert_eq!(actor.current_backend(), Some(EncodeBackendKind::Software));
    }

    /// 自动路径按探测清单依次尝试：首选建败即换下一个候选，不因单点失败就放弃
    ///
    /// 探测清单本来就是「本机认证可用」的集合，跳过其中可用项直接报错等于浪费探测结果。
    #[test]
    fn test_prepare_auto_tries_next_candidate_after_build_failure() {
        crate::encode::software::init_api_for_tests();

        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        // 硬编一律建败，软编（走 install_software，不经该工厂）能成
        let (mut actor, _pool) = test_actor_with_builder(
            inbox,
            Arc::clone(&output),
            Arc::new(|_profile, _codec, _kind, _stage| Err("硬编构建失败".to_string())),
        );
        let request = tier(1280, 720, 60);

        // 无显式偏好 = 自动：硬编候选全败后落到软编（清单的最后一个候选）
        let (effective, codec, backend) = prepare_kind(&mut actor, request, VideoCodec::H264, None)
            .expect("自动路径必须走完候选列表");
        assert_eq!(
            backend,
            EncodeBackendKind::Software,
            "硬编候选全败后，自动路径落到软编候选"
        );
        // 软编候选会把请求钳进自己的能力域：几何顶格 + codec 落 H264
        let (w, h, f) = crate::encode::clamp_to_software_ceiling(1280, 720, 60);
        assert_eq!(
            (effective.width(), effective.height(), effective.fps()),
            (w, h, f),
            "软编候选必须带上顶格钳制后的几何"
        );
        assert_eq!(
            codec,
            VideoCodec::H264,
            "软编只出 H264，回执 codec 必须钳制"
        );
    }

    /// 换代前置收尾只归还从未编码的帧；drain 成功后才摘除绑定
    ///
    /// 已被编码器取走的帧不可能与控制命令并发（同一线程）；未编码帧的槽位所有权
    /// 随载荷析构归还，不会与渲染线程对同一张图并发读写。
    #[test]
    fn test_rebuild_prefix_acks_only_uncoded_frames() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) = fence_actor(inbox.clone(), Arc::clone(&output), true, true);
        let request = tier(1280, 720, 60);
        assert_eq!(
            prepare(
                &mut actor,
                request,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        assert_eq!(import_slots(&mut actor, 0), ControlStatus::Ok);
        let binding = current_binding(&actor);

        // 帧在收件箱（从未编码）：前置收尾清空并归还槽位
        inbox.push(vram_frame(&binding, 0, 5));
        assert!(binding.is_in_flight(0));
        actor.discard_frames_before_rebuild();
        assert!(!binding.is_in_flight(0), "清空必须归还未编码帧的槽位");

        // drain 成功：摘除绑定，旧句柄失效
        assert_eq!(actor.clear_vram_slots(binding.handle()), ControlStatus::Ok);
        assert!(actor.binding.load().is_none(), "注销后不得残留绑定");
        assert!(
            actor.current_binding_handle().is_none(),
            "注销后旧句柄必须失效"
        );
    }

    /// Actor 入口按代次校验：换代窗口内晚到的旧绑定帧不得进入新表
    #[test]
    fn test_stale_binding_frame_is_dropped_at_actor_entry() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) = fence_actor(inbox.clone(), Arc::clone(&output), true, true);
        let request = tier(1280, 720, 60);
        assert_eq!(
            prepare(
                &mut actor,
                request,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        assert_eq!(import_slots(&mut actor, 0), ControlStatus::Ok);
        let old = current_binding(&actor);

        // 新代发布后旧句柄失效；旧帧仍可能已在 push 的载入窗口里
        assert_eq!(import_slots(&mut actor, 0), ControlStatus::Ok);
        assert_ne!(current_binding(&actor).handle(), old.handle());

        inbox.push(vram_frame(&old, 0, 5));
        let frame = inbox.take().expect("帧在收件箱");
        assert!(old.is_in_flight(0));
        actor.handle_frame(frame);

        assert!(!old.is_in_flight(0), "旧代帧析构必须归还旧代槽位");
        assert_eq!(
            actor.inbox.outcomes().count(FrameOutcome::FenceDropped),
            1,
            "旧代帧必须在入口丢弃"
        );
        assert_eq!(
            actor.inbox.outcomes().count(FrameOutcome::Admitted),
            0,
            "旧代帧不得进入编码"
        );
        assert!(output.take().is_none(), "旧代帧不得产出码流");
    }

    /// 堆块路径必须真的产出码流
    #[test]
    fn test_soft_path_encodes_heap_frame() {
        crate::encode::software::init_api_for_tests();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut state, pool) = test_actor(inbox, Arc::clone(&output));

        let (w, h) = (1280usize, 720usize);
        let profile = StreamingProfile::new(w, h, 60, 4_000_000);
        state.profile = profile;
        if let Ok((encoder, blocks)) = VideoEncoder::new_software(profile, VideoCodec::H264) {
            pool.attach(blocks);
            state.encoder = Some(encoder);
        }

        let mut payload = host_payload(&pool, vec![128u8; host_bytes(&state.profile)]);
        state.encode_frame(&mut payload, 0);
        // payload 离开作用域时自动通过 RAII 归还给关联的池
        drop(payload);

        let (nalu, is_keyframe) = output
            .take()
            .map(|f| (f.packet.nalu, f.packet.is_keyframe))
            .expect("软编路径必须产出编码产物");
        assert!(!nalu.is_empty(), "编码产物不应为空");
        assert!(is_keyframe, "首帧编码产物必须携带 is_keyframe = true");
        assert_eq!(pool.len(), POOL_DEPTH, "编码完成的块必须归还到池中");
    }

    /// 关键帧请求走控制面：命令置位挂起标志，下一帧以 IDR 编码，消费即清空
    #[test]
    fn test_request_keyframe_command_forces_next_idr() {
        crate::encode::software::init_api_for_tests();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut state, pool) = test_actor(inbox, Arc::clone(&output));

        let profile = StreamingProfile::new(1280, 720, 60, 4_000_000);
        state.profile = profile;
        if let Ok((encoder, blocks)) = VideoEncoder::new_software(profile, VideoCodec::H264) {
            pool.attach(blocks);
            state.encoder = Some(encoder);
        }

        let keyframe_of = |out: &Arc<FrameOutbox>| {
            out.take()
                .map(|f| f.packet.is_keyframe)
                .expect("必须产出编码产物")
        };
        let payload_bytes = || vec![0u8; host_bytes(&profile)];

        // 初值请求让首帧出 IDR；第二帧无挂起请求，回到 P 帧
        let mut first = host_payload(&pool, payload_bytes());
        state.encode_frame(&mut first, 0);
        drop(first);
        assert!(keyframe_of(&output), "首帧必须携带挂起的初值请求出 IDR");

        let mut second = host_payload(&pool, payload_bytes());
        state.encode_frame(&mut second, 16);
        drop(second);
        assert!(!keyframe_of(&output), "请求已兑现，第二帧不得再出 IDR");

        // 控制命令置位：第三帧以 IDR 编码
        assert!(state.apply(ControlCommand::RequestKeyframe));
        let mut third = host_payload(&pool, payload_bytes());
        state.encode_frame(&mut third, 33);
        drop(third);
        assert!(keyframe_of(&output), "RequestKeyframe 后下一帧必须出 IDR");
    }

    /// 发件箱未消费时丢弃输入而不是覆盖已编码帧：覆盖会切断参考链。
    #[test]
    fn test_encode_frame_drops_input_when_outbox_is_full() {
        crate::encode::software::init_api_for_tests();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut state, pool) = test_actor(inbox, Arc::clone(&output));

        let profile = StreamingProfile::new(1280, 720, 60, 4_000_000);
        state.profile = profile;
        if let Ok((encoder, blocks)) = VideoEncoder::new_software(profile, VideoCodec::H264) {
            pool.attach(blocks);
            state.encoder = Some(encoder);
        }

        // 首帧编码后故意不取走：发件箱保持非空
        let mut first = host_payload(&pool, vec![0u8; host_bytes(&profile)]);
        state.encode_frame(&mut first, 0);
        drop(first);
        assert!(!output.is_empty(), "首帧必须已入发件箱");

        // 第二帧入口即被丢弃：发件箱与挂起的关键帧请求都不受影响
        state.pending_keyframe = true;
        let mut second = host_payload(&pool, vec![0u8; host_bytes(&profile)]);
        state.encode_frame(&mut second, 16);
        drop(second);
        assert_eq!(
            state.inbox.outcomes().count(FrameOutcome::Superseded),
            1,
            "发件箱未消费时必须在编码前丢弃输入"
        );
        assert!(state.pending_keyframe, "丢弃输入不得消费挂起的关键帧请求");
        assert_eq!(
            output.take().map(|p| p.packet.birth_nanos),
            Some(0),
            "发件箱必须仍是首帧，不得被第二帧覆盖"
        );
    }

    /// 主机帧字节数与编码器几何不符时必须丢帧：错行距的像素交给编码器换的是一屏花屏
    #[test]
    fn test_soft_path_drops_mismatched_host_frame() {
        crate::encode::software::init_api_for_tests();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut state, pool) = test_actor(inbox, Arc::clone(&output));

        let profile = StreamingProfile::new(1280, 720, 60, 4_000_000);
        state.profile = profile;
        if let Ok((encoder, blocks)) = VideoEncoder::new_software(profile, VideoCodec::H264) {
            pool.attach(blocks);
            state.encoder = Some(encoder);
        }

        // 按旧几何的一半长度送帧（典型现场：换分辨率后采集侧还按旧尺寸回读）
        let mut payload = host_payload(&pool, vec![128u8; host_bytes(&profile) / 2]);
        state.encode_frame(&mut payload, 0);
        drop(payload);

        assert!(output.take().is_none(), "布局不符的帧不得产出编码产物");
        assert_eq!(pool.len(), POOL_DEPTH, "被丢弃的帧块必须归还到池中");
    }

    /// 分辨率变化时重建软编器：尺寸不匹配的旧编码器继续用会编出错帧
    #[test]
    fn test_soft_encoder_rebuilt_on_dimension_change() {
        crate::encode::software::init_api_for_tests();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut state, pool) = test_actor(inbox, Arc::clone(&output));

        let p1_profile = StreamingProfile::new(1280, 720, 60, 4_000_000);
        state.profile = p1_profile;
        if let Ok((encoder, blocks)) = VideoEncoder::new_software(p1_profile, VideoCodec::H264) {
            pool.attach(blocks);
            state.encoder = Some(encoder);
        }

        let mut p1 = host_payload(&pool, vec![0u8; host_bytes(&p1_profile)]);
        state.encode_frame(&mut p1, 0);
        drop(p1);
        assert!(output.take().is_some());

        let new_profile = StreamingProfile::new(1600, 900, 60, 5_000_000);
        state.profile = new_profile;
        if let Some(encoder) = &mut state.encoder {
            // 软编不碰设备，重配不该交回新批
            assert!(
                encoder
                    .reconfigure(new_profile, VideoCodec::H264)
                    .unwrap()
                    .is_empty()
            );
        }

        let frame_bytes = host_bytes(&new_profile);
        let mut p2 = host_payload(&pool, vec![0u8; frame_bytes]);
        state.encode_frame(&mut p2, 16);
        drop(p2);
        assert!(output.take().is_some(), "换尺寸后必须仍能产出编码产物");
        if let Some(encoder) = &state.encoder {
            assert_eq!(encoder.profile(), new_profile, "重配后门面档位必须同步");
        } else {
            panic!("Expected software encoder");
        }
    }

    /// 读取一次性应答：`apply` 在返回前就已回送，轮询只为消除线程调度抖动
    fn wait_reply<T>(mut rx: tokio::sync::oneshot::Receiver<T>) -> T {
        let start = std::time::Instant::now();
        loop {
            match rx.try_recv() {
                Ok(value) => return value,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    assert!(
                        start.elapsed() < std::time::Duration::from_secs(1),
                        "等一次性应答超时"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("应答通道已断开: {e}"),
            }
        }
    }

    /// 直接走 Actor 的控制出口配好编码器并取回结果码
    fn prepare(
        actor: &mut EncoderActor,
        request: TierRequest,
        codec: VideoCodec,
        preferred_backend: Option<EncodeBackendKind>,
    ) -> ControlStatus {
        match prepare_kind(actor, request, codec, preferred_backend) {
            Ok(_) => ControlStatus::Ok,
            Err(code) => code,
        }
    }

    /// 直接走 Actor 的控制出口配好编码器并取回生效档位、codec 与实际后端身份
    fn prepare_kind(
        actor: &mut EncoderActor,
        request: TierRequest,
        codec: VideoCodec,
        preferred_backend: Option<EncodeBackendKind>,
    ) -> Result<(StreamingProfile, VideoCodec, EncodeBackendKind), ControlStatus> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        assert!(actor.apply(ControlCommand::PrepareEncoder {
            request,
            codec,
            preferred_backend,
            reply,
        }));
        wait_reply(rx)
    }

    /// 直接走 Actor 的控制出口换档并取回生效档位或拒绝原因
    fn switch_profile(
        actor: &mut EncoderActor,
        request: TierRequest,
    ) -> Result<StreamingProfile, RetierError> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        assert!(actor.apply(ControlCommand::Retier { request, reply }));
        wait_reply(rx)
    }

    /// 直接走 Actor 的控制出口下发跟随码率
    fn set_bitrate(actor: &mut EncoderActor, bps: usize) {
        assert!(actor.apply(ControlCommand::BitrateChange(bps)));
    }

    #[test]
    fn test_encoder_actor_thread_run_loop_and_drain() {
        crate::encode::software::init_api_for_tests();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let (ctl_tx, ctl_rx) = tokio::sync::mpsc::channel::<ControlCommand>(16);
        let output = test_outbox();

        // 池由测试线程持有，Actor 只拿弱引用：池消失即等于管线关停
        let pool = FramePool::new();
        let actor_pool = Arc::downgrade(&pool);
        let actor_inbox = Arc::clone(&inbox);
        let actor_bell = Arc::clone(&bell);
        let actor_output = Arc::clone(&output);
        let join = std::thread::Builder::new()
            .name("test-actor".to_string())
            .spawn(move || {
                let actor = EncoderActor::new(
                    actor_inbox,
                    actor_output,
                    actor_pool,
                    Arc::new(Heartbeat::new()),
                    Arc::new(StageCell::new()),
                    empty_binding(),
                );
                actor.run(ctl_rx, actor_bell);
            })
            .unwrap();

        // 0. 协商落账建编码器（Actor 构造后没有编码器，必须先建再推帧）：
        //    720p 软编打底，同时装好帧池的堆块
        let request = tier(1280, 720, 60);
        let profile = request.derive(VideoCodec::H264);
        let frame_bytes = host_bytes(&profile);
        let (ack0_tx, ack0_rx) = tokio::sync::oneshot::channel();
        assert!(
            ctl_tx
                .blocking_send(ControlCommand::PrepareEncoder {
                    request,
                    codec: VideoCodec::H264,
                    preferred_backend: Some(EncodeBackendKind::Software),
                    reply: ack0_tx,
                })
                .is_ok(),
            "控制队列必须受理"
        );
        assert_eq!(
            wait_reply(ack0_rx),
            Ok((profile, VideoCodec::H264, EncodeBackendKind::Software)),
            "协商落账必须返回实际生效的档位、codec 与后端身份"
        );

        // 1. 推送一帧（push 内部已唤醒）
        inbox.push(VideoFrame {
            birth_nanos: 1000,
            payload: host_payload(&pool, vec![100u8; frame_bytes]),
        });

        // 验证接收到编码包（防御性超时，避免测试异常时永久挂起）
        let start = std::time::Instant::now();
        let item = loop {
            if let Some(item) = output.take() {
                break item;
            }
            if start.elapsed() > std::time::Duration::from_secs(3) {
                panic!("Actor 未能在 3 秒内产出视频编码包");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(!item.packet.nalu.is_empty());
        assert_eq!(pool.len(), POOL_DEPTH, "RAII 缓冲归还");

        // 2. 发送 ClearVramSlots 控制命令并等待 ACK（排空+注销单命令往返）
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        assert!(
            ctl_tx
                .blocking_send(ControlCommand::ClearVramSlots {
                    handle: 0,
                    reply: ack_tx,
                })
                .is_ok(),
            "控制队列必须受理"
        );
        assert_eq!(
            ack_rx
                .recv_timeout(std::time::Duration::from_millis(500))
                .unwrap(),
            ControlStatus::Ok
        );

        // 3. 停止 Actor（入队即唤醒）
        assert!(
            ctl_tx.blocking_send(ControlCommand::Shutdown).is_ok(),
            "控制队列必须受理"
        );

        join.join().expect("Actor thread join 成功");
    }

    /// Actor 的码率缓存只跟随 `BitrateChange`，不因档位变化自行改写
    ///
    /// 档位上限的 clamp 是 `RateControl` 的职责——它的 `max` 就是档位码率，`set_bps`
    /// 会重新派发一次跟随值。Actor 若在这里顺手 clamp 并写回缓存，缓存就偏离了 RC
    /// 实际派发过的值，之后 RC 的死区会拿一个编码器没在跑的数值做参照。
    #[test]
    fn test_bitrate_cache_follows_rate_control_only() {
        // 软编打底路径要真建 OpenH264 编码器：不依赖其他用例先行装好测试夹具
        crate::encode::software::init_api_for_tests();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let (mut actor, _pool) = test_actor(inbox, test_outbox());

        // 1. 初始为 1080p 档位：走公共缝装配，不直写字段
        let p1080 = tier(1920, 1080, 60);
        assert_eq!(
            prepare(
                &mut actor,
                p1080,
                VideoCodec::H264,
                Some(EncodeBackendKind::Software)
            ),
            ControlStatus::Ok
        );

        // 2. 模拟 Rate Control 将 Followed Bitrate 调至 5_000_000 bps
        set_bitrate(&mut actor, 5_000_000);
        assert_eq!(actor.bitrate, 5_000_000);

        // 3. 切到 720p（派生上限高于 5M）：缓存不动
        let p720 = tier(1280, 720, 60);
        let _ = switch_profile(&mut actor, p720);
        assert_eq!(
            actor.bitrate, 5_000_000,
            "档位上限高于当前跟随值时，缓存必须原样保持"
        );

        // 4. RC 把跟随值抬到 10M
        set_bitrate(&mut actor, 10_000_000);
        assert_eq!(actor.bitrate, 10_000_000);

        // 5. 切到上限更低的档位：下发值被压到新上限，但**缓存仍是 RC 派发过的 10M**，
        //    等 `RateControl::set_bps` 把它改写成真正的跟随值
        let p720_15fps = tier(1280, 720, 15);
        assert!(
            p720_15fps.derive(VideoCodec::H264).bitrate() < 10_000_000,
            "本用例前提：新档位上限低于 10M"
        );
        let _ = switch_profile(&mut actor, p720_15fps);
        assert_eq!(
            actor.bitrate, 10_000_000,
            "缓存不得被档位变化改写：它只反映 RC 派发过的值"
        );
    }

    /// 围栏测试后端：槽位表行为可配置，不碰任何驱动；编码恒产出非空 NALU，
    /// 让「被围栏拒收」与「被编码 admitted」在账本上可区分。槽位认领由载荷析构归还
    struct FenceBackend {
        drain_ok: bool,
        import_ok: bool,
        /// 模拟重建式重配失败；失败后后端转为不可用
        fail_reconfigure: bool,
        usable: bool,
    }

    impl VideoEncodeBackend for FenceBackend {
        fn kind(&self) -> EncodeBackendKind {
            EncodeBackendKind::Nvenc
        }

        fn coded_align(&self) -> (u32, u32) {
            (1, 1)
        }

        fn encode(
            &mut self,
            _payload: &FramePayload,
            _force_idr: bool,
            _frame_idx: u64,
        ) -> Result<(Bytes, bool), String> {
            Ok((Bytes::from_static(b"fence-test-nalu"), false))
        }

        fn reconfigure(
            &mut self,
            _new_profile: StreamingProfile,
            _new_codec: VideoCodec,
        ) -> Result<Vec<HostBlock>, String> {
            if self.fail_reconfigure {
                self.usable = false;
                return Err("重配失败模拟".to_string());
            }
            Ok(Vec::new())
        }

        fn is_usable(&self) -> bool {
            self.usable
        }

        fn set_bitrate(&mut self, _bps: u32) -> Result<(), String> {
            Ok(())
        }

        fn import_vram_slots(
            &mut self,
            _descriptors: &mut [VramSlotDescriptor],
        ) -> Result<(), String> {
            if self.import_ok {
                Ok(())
            } else {
                Err("导入失败模拟".to_string())
            }
        }

        fn clear_vram_slots(&mut self) {}

        fn drain(&mut self) -> Result<(), String> {
            if self.drain_ok {
                Ok(())
            } else {
                Err("排空失败模拟".to_string())
            }
        }
    }

    /// 围栏测试工厂：可额外模拟重建失败
    fn fence_backend_builder_with(
        drain_ok: bool,
        import_ok: bool,
        fail_reconfigure: bool,
    ) -> Arc<HardwareBuilder> {
        Arc::new(move |profile, codec, _kind, _stage| {
            Ok((
                VideoEncoder::from_backend(
                    profile,
                    codec,
                    Box::new(FenceBackend {
                        drain_ok,
                        import_ok,
                        fail_reconfigure,
                        usable: true,
                    }),
                ),
                fake_device_blocks(FAKE_DEVICE_DOMAIN),
            ))
        })
    }

    /// 围栏测试专用构造
    fn fence_actor(
        inbox: Arc<FrameInbox>,
        output: Arc<FrameOutbox>,
        drain_ok: bool,
        import_ok: bool,
    ) -> (EncoderActor, Arc<FramePool>) {
        fence_actor_with(inbox, output, drain_ok, import_ok, false)
    }

    /// 围栏测试专用构造：可额外模拟重建失败
    fn fence_actor_with(
        inbox: Arc<FrameInbox>,
        output: Arc<FrameOutbox>,
        drain_ok: bool,
        import_ok: bool,
        fail_reconfigure: bool,
    ) -> (EncoderActor, Arc<FramePool>) {
        let pool = FramePool::new();
        let actor = EncoderActor::new_with_hardware_builder(
            inbox,
            output,
            Arc::downgrade(&pool),
            Arc::new(Heartbeat::new()),
            Arc::new(StageCell::new()),
            empty_binding(),
            fence_backend_builder_with(drain_ok, import_ok, fail_reconfigure),
        );
        (actor, pool)
    }

    /// 当前绑定（导入成功后取用）
    fn current_binding(actor: &EncoderActor) -> Arc<VramBinding> {
        actor.binding.load_full().expect("导入成功后必须有绑定")
    }

    /// 槽位帧构造：在给定绑定上占用 `slot_idx`，载荷 Drop 即归还
    fn vram_frame(binding: &Arc<VramBinding>, slot_idx: u32, sync_value: u64) -> VideoFrame {
        VideoFrame {
            birth_nanos: 0,
            payload: FramePayload::VramSlot {
                slot: binding.try_acquire(slot_idx).expect("测试槽位必须空闲"),
                sync_value,
            },
        }
    }

    /// 读取槽位类命令的阻塞应答（槽位命令的控制面走 std 通道）
    fn wait_control_reply(rx: std::sync::mpsc::Receiver<ControlStatus>) -> ControlStatus {
        rx.recv_timeout(std::time::Duration::from_millis(500))
            .expect("槽位命令应答超时")
    }

    /// 读取导入命令的应答：成功折叠成 `Ok`，失败保留结果码
    fn wait_import_reply(
        rx: std::sync::mpsc::Receiver<Result<u64, ControlStatus>>,
    ) -> ControlStatus {
        match rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .expect("槽位导入应答超时")
        {
            Ok(_handle) => ControlStatus::Ok,
            Err(code) => code,
        }
    }

    /// 经控制出口导入一批 720p 槽位并取回结果码
    fn import_slots(actor: &mut EncoderActor, slot_idx: u32) -> ControlStatus {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        assert!(actor.apply(ControlCommand::ImportVramSlots {
            owner: std::thread::current().id(),
            slots: vec![VramSlotDescriptor {
                slot_idx,
                mem_handle: -1,
                sem_handle: -1,
                width: 1280,
                height: 720,
                size_bytes: 0,
                offset: 0,
                handles_taken: true,
            }],
            reply,
        }));
        wait_import_reply(rx)
    }

    /// 用例 1：几何变化开窗——窗口内槽位帧入口拒收并归还认领，主机帧照收；
    /// 导入完成后关窗，新槽位帧恢复编码
    #[test]
    fn test_fence_drops_window_frames_then_admits_new_generation() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, pool) = fence_actor(inbox.clone(), output.clone(), true, true);

        let p1080 = tier(1920, 1080, 60);
        assert_eq!(
            prepare(
                &mut actor,
                p1080,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        assert!(!actor.rejecting_frames);
        assert_eq!(import_slots(&mut actor, 0), ControlStatus::Ok);
        let old = current_binding(&actor);

        // 几何变化生效：窗口开启
        let request720 = tier(1280, 720, 60);
        let p720 = request720.derive(VideoCodec::H264);
        assert_eq!(switch_profile(&mut actor, request720), Ok(p720));
        assert!(actor.rejecting_frames);

        // 窗口内槽位帧：入口拒收、认领归还、不进编码
        inbox.push(vram_frame(&old, 0, 5));
        let frame = inbox.take().expect("帧在收件箱");
        actor.handle_frame(frame);
        assert_eq!(
            inbox.outcomes().count(FrameOutcome::FenceDropped),
            1,
            "窗口内槽位帧必须被围栏拒收"
        );
        assert!(!old.is_in_flight(0), "被拒帧的槽位必须在入口归还");
        assert!(
            output.take().is_none(),
            "被拒帧不得进编码，发件箱不得有产物"
        );

        // 窗口内主机帧：自带像素不引用表，照收。
        // `Admitted` 由会话写循环记账，Actor 层以发件箱产物为准
        let host = host_payload(&pool, vec![100u8; host_bytes(&p720)]);
        inbox.push(VideoFrame {
            birth_nanos: 1,
            payload: host,
        });
        let frame = inbox.take().expect("主机帧在收件箱");
        actor.handle_frame(frame);
        assert!(output.take().is_some(), "窗口内主机帧必须照收");

        // 导入完成：关窗，新槽位帧恢复编码
        assert_eq!(import_slots(&mut actor, 0), ControlStatus::Ok);
        assert!(!actor.rejecting_frames);

        let fresh = current_binding(&actor);
        inbox.push(vram_frame(&fresh, 0, 6));
        let frame = inbox.take().expect("新代帧在收件箱");
        actor.handle_frame(frame);
        assert!(output.take().is_some(), "新代槽位帧必须恢复编码");
        assert!(!fresh.is_in_flight(0), "编码出口由载荷 Drop 归还槽位");
    }

    /// 用例 2：同几何换 codec 也开窗——旧 sync_value 帧不得落进新导入的信号量。
    /// 换 codec 只发生在协商落账（`PrepareEncoder`，顶号换偏好的跨会话路径）：
    /// 重配触发面的语义洞（几何比对拦不住几何相同的旧帧）由入口拒收堵上
    #[test]
    fn test_fence_covers_same_geometry_codec_change() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) = fence_actor(inbox.clone(), output.clone(), true, true);

        let p1080 = tier(1920, 1080, 60);
        assert_eq!(
            prepare(
                &mut actor,
                p1080,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        assert_eq!(import_slots(&mut actor, 3), ControlStatus::Ok);
        let old = current_binding(&actor);

        // 同几何同后端换 codec：协商落账重配生效，围栏开窗
        let (effective, codec, backend) = prepare_kind(
            &mut actor,
            p1080,
            VideoCodec::H265,
            Some(EncodeBackendKind::Nvenc),
        )
        .expect("同几何换 codec 必须重配成功");
        assert_eq!(
            (codec, backend),
            (VideoCodec::H265, EncodeBackendKind::Nvenc)
        );
        assert_eq!(effective, p1080.derive(VideoCodec::H265));
        assert!(actor.rejecting_frames);

        inbox.push(vram_frame(&old, 3, 100));
        let frame = inbox.take().expect("帧在收件箱");
        actor.handle_frame(frame);
        assert_eq!(
            inbox.outcomes().count(FrameOutcome::FenceDropped),
            1,
            "同几何换 codec 的旧 sync_value 帧必须被围栏拒收"
        );
        assert!(!old.is_in_flight(3), "被拒帧的槽位必须归还");
        assert!(output.take().is_none(), "被拒帧不得进编码");

        assert_eq!(import_slots(&mut actor, 3), ControlStatus::Ok);
        assert!(!actor.rejecting_frames);
    }

    /// 用例 3：导入前排空失败——后端弃置不降级，整批拒绝且窗口不滞留；
    /// 此后导入一律失败，自愈（重启引擎）是唯一恢复路径
    #[test]
    fn test_fence_closes_window_when_import_drain_fails() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, pool) = fence_actor(inbox.clone(), output.clone(), false, true);

        let p1080 = tier(1920, 1080, 60);
        assert_eq!(
            prepare(
                &mut actor,
                p1080,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        let request720 = tier(1280, 720, 60);
        let p720 = request720.derive(VideoCodec::H264);
        let _ = switch_profile(&mut actor, request720);
        assert!(actor.rejecting_frames);

        assert_eq!(
            import_slots(&mut actor, 0),
            ControlStatus::EncoderUnavailable,
            "排空失败弃置后端：整批拒绝且如实上报无编码器可跑"
        );
        assert!(!actor.rejecting_frames);
        // 排空失败即上下文不可信：后端弃置且不降级——运行期不改后端身份，
        // 自愈（重启引擎重新协商）是唯一恢复路径
        assert!(actor.encoder.is_none(), "弃置后不得残留任何编码器");

        // 主机帧被消费但不产出码流：无编码器可跑
        let host = host_payload(&pool, vec![100u8; host_bytes(&p720)]);
        inbox.push(VideoFrame {
            birth_nanos: 1,
            payload: host,
        });
        let frame = inbox.take().expect("主机帧在收件箱");
        actor.handle_frame(frame);
        assert!(output.take().is_none(), "无编码器时主机帧不得产出码流");

        // 此后的导入一律失败：硬件前置检查在没有编码器时必然不过
        assert_eq!(
            import_slots(&mut actor, 0),
            ControlStatus::EncoderUnavailable
        );
    }

    /// 用例 4：清空槽位表后，显存帧拒收直到下一次导入
    #[test]
    fn test_slots_cleared_rejects_until_next_import() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) = fence_actor(inbox.clone(), output.clone(), true, true);

        let p1080 = tier(1920, 1080, 60);
        assert_eq!(
            prepare(
                &mut actor,
                p1080,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        assert!(!actor.rejecting_frames);
        assert_eq!(import_slots(&mut actor, 2), ControlStatus::Ok);
        let old = current_binding(&actor);

        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        assert!(actor.apply(ControlCommand::ClearVramSlots {
            handle: old.handle(),
            reply
        }));
        assert_eq!(wait_control_reply(rx), ControlStatus::Ok);
        assert!(actor.rejecting_frames);

        inbox.push(vram_frame(&old, 2, 9));
        let frame = inbox.take().expect("帧在收件箱");
        actor.handle_frame(frame);
        assert_eq!(
            inbox.outcomes().count(FrameOutcome::FenceDropped),
            1,
            "槽位表清空后的槽位帧必须被拒收"
        );
        assert!(!old.is_in_flight(2), "被拒帧的槽位必须归还");

        assert_eq!(import_slots(&mut actor, 2), ControlStatus::Ok);
        assert!(!actor.rejecting_frames);

        let fresh = current_binding(&actor);
        inbox.push(vram_frame(&fresh, 2, 10));
        let frame = inbox.take().expect("新代帧在收件箱");
        actor.handle_frame(frame);
        assert!(output.take().is_some(), "新导入后槽位帧恢复编码");
    }

    /// 用例 4b：旧轮次的注销不得改动当前轮次的拒收状态
    #[test]
    fn test_stale_clear_is_noop() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) = fence_actor(inbox.clone(), output.clone(), true, true);

        let p1080 = tier(1920, 1080, 60);
        assert_eq!(
            prepare(
                &mut actor,
                p1080,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        assert_eq!(import_slots(&mut actor, 0), ControlStatus::Ok);
        assert!(!actor.rejecting_frames);
        let current = current_binding(&actor);
        let stale = current.handle() + 1_000;

        // 旧句柄注销：回报 Ok（旧导入已随旧实例销毁），但不得把当前轮次打成拒收
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        assert!(actor.apply(ControlCommand::ClearVramSlots {
            handle: stale,
            reply
        }));
        assert_eq!(wait_control_reply(rx), ControlStatus::Ok);
        assert!(!actor.rejecting_frames, "旧轮次注销不得阻断当前轮次");

        // 当前轮次注销：正常进入拒收，等待下一次当前轮次导入
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        assert!(actor.apply(ControlCommand::ClearVramSlots {
            handle: current.handle(),
            reply
        }));
        assert_eq!(wait_control_reply(rx), ControlStatus::Ok);
        assert!(actor.rejecting_frames);
    }

    /// 用例 4c：槽位索引超出掩码容量时整批拒绝，不进入导入
    #[test]
    fn test_import_rejects_slot_index_beyond_mask_capacity() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) = fence_actor(inbox, output, true, true);

        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        assert!(actor.apply(ControlCommand::ImportVramSlots {
            owner: std::thread::current().id(),
            slots: vec![VramSlotDescriptor {
                slot_idx: MAX_VRAM_SLOTS as u32,
                mem_handle: -1,
                sem_handle: -1,
                width: 1280,
                height: 720,
                size_bytes: 0,
                offset: 0,
                handles_taken: true,
            }],
            reply
        }));
        assert_eq!(
            wait_import_reply(rx),
            ControlStatus::VramFault,
            "越界槽位索引必须整批拒绝"
        );
        assert!(actor.binding.load().is_none(), "拒绝的导入不得安装绑定");
    }

    /// 用例 5：拒收只开不关——窗口开着时来一条 fps-only 重配，不得提前放行
    /// 旧代帧；恢复只认导入
    #[test]
    fn test_fps_only_change_keeps_rejecting_until_import() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) = fence_actor(inbox.clone(), output.clone(), true, true);

        let p1080 = tier(1920, 1080, 60);
        assert_eq!(
            prepare(
                &mut actor,
                p1080,
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        assert_eq!(import_slots(&mut actor, 1), ControlStatus::Ok);
        let old = current_binding(&actor);

        // fps-only 重配不动表：单独发生时不开窗
        let p1080_30fps = tier(1920, 1080, 30);
        let _ = switch_profile(&mut actor, p1080_30fps);
        assert!(!actor.rejecting_frames, "fps-only 重配不得开窗");
        assert!(actor.binding.load().is_some(), "fps-only 重配不得摘除绑定");

        // 同几何换 codec（协商落账路径）开窗：旧 sync_value 帧自此不得进编码
        let _ = prepare_kind(
            &mut actor,
            p1080,
            VideoCodec::H265,
            Some(EncodeBackendKind::Nvenc),
        )
        .expect("同几何换 codec 必须重配成功");
        assert!(actor.rejecting_frames);
        assert!(actor.binding.load().is_none(), "换 codec 必须摘除绑定");

        // 窗口期间来一条 fps-only 重配：不得关闭拒收
        let _ = switch_profile(&mut actor, p1080_30fps);
        assert!(actor.rejecting_frames, "fps-only 重配不得提前放行旧代帧");

        inbox.push(vram_frame(&old, 1, 77));
        let frame = inbox.take().expect("帧在收件箱");
        actor.handle_frame(frame);
        assert_eq!(
            inbox.outcomes().count(FrameOutcome::FenceDropped),
            1,
            "窗口内旧代帧必须仍被拒收"
        );

        assert_eq!(import_slots(&mut actor, 1), ControlStatus::Ok);
        assert!(!actor.rejecting_frames);
    }

    /// 重建失败即后端不可用：Actor 必须摘除绑定并拒收显存帧，等待重新协商
    #[test]
    fn test_retier_rebuild_failure_detaches_binding() {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) = fence_actor_with(inbox.clone(), output, true, true, true);

        assert_eq!(
            prepare(
                &mut actor,
                tier(1280, 720, 60),
                VideoCodec::H264,
                Some(EncodeBackendKind::Nvenc)
            ),
            ControlStatus::Ok
        );
        assert_eq!(import_slots(&mut actor, 1), ControlStatus::Ok);
        let old = current_binding(&actor);

        assert!(
            switch_profile(&mut actor, tier(1600, 900, 60)).is_err(),
            "重建失败必须拒绝档位变更"
        );
        assert!(actor.binding.load().is_none(), "失败后绑定必须摘除");
        assert!(actor.rejecting_frames, "失败后必须拒收显存帧");

        inbox.push(vram_frame(&old, 1, 77));
        let frame = inbox.take().expect("帧在收件箱");
        actor.handle_frame(frame);
        assert_eq!(
            inbox.outcomes().count(FrameOutcome::FenceDropped),
            1,
            "失败后的旧代帧必须在入口拒收"
        );
    }
}

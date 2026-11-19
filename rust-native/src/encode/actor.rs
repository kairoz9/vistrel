//! 专用编码执行器
//!
//! Actor 线程独占全部编码器实例与 CUDA Context，消费投递的帧，
//! 经 `control` 串行处理生命周期命令并以一次性 ACK 应答调用方；
//! Actor panic 时管线进入 fail-closed。

use std::sync::{Arc, Weak};

use tokio::sync::Notify;
use tokio::sync::mpsc::Receiver;

use crate::encode::control::{ControlCommand, ControlStatus};
use crate::encode::frame::{FrameInbox, FrameOutbox, FrameOutcome, VideoFrame};
use crate::encode::hardware::VramSlotDescriptor;
use crate::encode::hardware::slots::{ActorStage, SlotStateBlock};
use crate::encode::pool::{FramePool, HostBlock};
use crate::encode::{
    FramePayload, KeyframeTrigger, StampedVideoPacket, StreamingProfile, VideoPacket,
    display_nv12_len,
};

use super::encoder::VideoEncoder;

/// 硬编后端构建入口：生产固定为真实驱动工厂，测试注入假后端
type HardwareBuilder = dyn Fn(StreamingProfile, &'static SlotStateBlock) -> Result<(VideoEncoder, Vec<HostBlock>), String>
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
    /// 关键帧触发器，Actor 拥有并直接消费/触发
    keyframe: Arc<KeyframeTrigger>,
    /// 编码器：Hardware 或 Software；无硬件或未就绪时为 None
    encoder: Option<VideoEncoder>,
    /// 编码产物发件箱：管线持有、会话取用。这里只做同步入箱，写轨道由会话自己的写循环
    /// 完成，编码线程永不等待网络
    output: Arc<FrameOutbox>,
    /// Followed Bitrate (bps)，由 RateControl 在值变化时经 BitrateChange 派发。
    bitrate: usize,
    profile: StreamingProfile,
    /// 主机帧池：只持一个弱引用，用来把后端新分配的块装进池。
    pool: Weak<FramePool>,
    /// 槽位所有权共享块：与本 Actor 同代（`VideoPipeline::spawn` 泄漏分配）
    slot_state: &'static SlotStateBlock,
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
        keyframe: Arc<KeyframeTrigger>,
        output: Arc<FrameOutbox>,
        pool: Weak<FramePool>,
        slot_state: &'static SlotStateBlock,
    ) -> Self {
        Self::new_with_hardware_builder(
            inbox,
            keyframe,
            output,
            pool,
            slot_state,
            Arc::new(VideoEncoder::new_hardware),
        )
    }

    /// 构造器注入硬编工厂；生产入口 [`Self::new`] 固定用真实驱动
    fn new_with_hardware_builder(
        inbox: Arc<FrameInbox>,
        keyframe: Arc<KeyframeTrigger>,
        output: Arc<FrameOutbox>,
        pool: Weak<FramePool>,
        slot_state: &'static SlotStateBlock,
        hardware_builder: Arc<HardwareBuilder>,
    ) -> Self {
        let profile = StreamingProfile::default();
        // 软编随时就绪，作为系统默认底座（同时由它带上第一批堆块）
        let (encoder, blocks) = match VideoEncoder::new_software(profile) {
            Ok((encoder, blocks)) => (Some(encoder), blocks),
            Err(_) => (None, Vec::new()),
        };
        let actor = Self {
            inbox,
            keyframe,
            encoder,
            output,
            bitrate: 0,
            profile,
            pool,
            slot_state,
            hardware_builder,
            rejecting_frames: false,
        };
        actor.attach(blocks);
        actor
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
                self.slot_state.set_stage(ActorStage::Idle);
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
                self.slot_state.heartbeat_bump();

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
    /// 会重建编码器的命令（`PrepareEncoder` / `ProfileChange` / `ImportVramSlots` /
    /// `ClearVramSlots`）会让旧编码器上的一切（显存槽位注册、会话状态）失效。
    fn apply(&mut self, cmd: ControlCommand) -> bool {
        match cmd {
            ControlCommand::Shutdown => {
                self.slot_state.set_stage(ActorStage::Shutdown);
                return false;
            }
            ControlCommand::BitrateChange(bitrate) => {
                self.slot_state.set_stage(ActorStage::Reconfigure);
                self.bitrate = bitrate;
                Self::apply_bitrate(&mut self.encoder, bitrate);
            }
            ControlCommand::ProfileChange { profile, reply } => {
                let (before, had_table) = self.begin_profile_command();
                let outcome = self.change_profile(profile);
                self.end_profile_command(before, had_table);
                let _ = reply.send(outcome);
            }
            ControlCommand::PrepareEncoder {
                profile,
                prefer_hardware,
                reply,
            } => {
                let (before, had_table) = self.begin_profile_command();
                let code = self.prepare_encoder(profile, prefer_hardware);
                self.end_profile_command(before, had_table);
                let _ = reply.send(code);
            }
            ControlCommand::ImportVramSlots { slots, reply } => {
                self.discard_frames_before_rebuild();
                let code = self.import_vram_slots(slots);
                // 恢复接收只认导入，无论成败：成功即新表生效；失败时旧表恢复或
                // 表空，交给既有索引查找与几何兜底
                self.rejecting_frames = false;
                let _ = reply.send(code);
            }
            ControlCommand::ClearVramSlots { reply } => {
                self.discard_frames_before_rebuild();
                let code = self.clear_vram_slots();
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
        true
    }

    /// 换档类命令（`ProfileChange` / `PrepareEncoder`）的统一前置：
    /// 置阶段码、清收件箱，返回命令前快照 `(档位, 此前是否硬编在位)`——
    /// 只有硬编可能持有槽位注册，软编在位时无从谈起拒收
    fn begin_profile_command(&mut self) -> (StreamingProfile, bool) {
        self.slot_state.set_stage(ActorStage::Reconfigure);
        self.discard_frames_before_rebuild();
        (
            self.profile,
            self.encoder.as_ref().is_some_and(|e| e.is_hardware()),
        )
    }

    /// 换档类命令的统一收尾：几何或 codec 实际变化且此前硬编在位时，开始拒收
    /// 显存帧。只开不关——一旦拒收，恢复只认导入命令，中途的 fps-only 重配
    /// 不得提前放行旧代帧；本体重配失败不开（档位没变就没有漂移）
    fn end_profile_command(&mut self, before: StreamingProfile, hardware_table: bool) {
        let after = &self.profile;
        if hardware_table
            && (before.width() != after.width()
                || before.height() != after.height()
                || before.codec() != after.codec())
        {
            self.rejecting_frames = true;
            log::debug!("档位已变，等待槽位重导入：期间显存帧在入口拒收");
        }
    }

    /// 换代命令的前置收尾：清空收件箱，被丢弃帧的资源在本线程归还
    ///
    /// 必须先于命令本体执行。这里刻意不清所有权位：被清掉的帧经 ack 归还位，
    /// 逃脱清空的帧在本轮循环被取走、由编码出口归还——位协议自行闭环。
    /// 全清只属于 drain 成功后的换表命令，见 `clear_vram_slots` / `import_vram_slots`
    fn discard_frames_before_rebuild(&mut self) {
        self.inbox.clear();
        self.ack_discarded();
    }

    /// 归还被收件箱丢弃的帧持有的资源（覆盖与清空两路）
    ///
    /// 这些帧从未被编码，纹理无人读取，槽位可以立即重新认领；主机块析构即回池。
    /// 两件事都必须在本线程做完：槽位位属于共享块、不属于编码器（无编码器也需归还），
    /// 块的回收要拿池锁，不能落在生产者的渲染主线程上。
    fn ack_discarded(&mut self) {
        for payload in self.inbox.take_discarded() {
            match payload {
                FramePayload::VramSlot { slot_idx, .. } => self.slot_state.mark_free(slot_idx),
                // 主机块在这里析构即回池，且落在编码线程
                FramePayload::Host { frame } => drop(frame),
            }
        }
    }

    /// 注销显存槽位；排空失败时不丢引用，把失败交给调用方
    ///
    /// `Ok` 是游戏侧销毁导出纹理/信号量的唯一依据（`SlotLease::isReleased`）。排空失败
    /// 意味着编码器可能仍在这些导入资源上等待：此时一边丢引用一边回 `Ok`，等于让游戏侧
    /// 释放一张驱动仍在读的纹理。没有编码器则没有导入引用，无需排空。
    fn clear_vram_slots(&mut self) -> ControlStatus {
        let Some(encoder) = &mut self.encoder else {
            return ControlStatus::Ok;
        };
        self.slot_state.set_stage(ActorStage::Drain);
        if let Err(e) = encoder.drain() {
            log::error!("槽位注销前排空失败，保留导入引用: {e}");
            return ControlStatus::ReleaseFailed;
        }
        self.slot_state.set_stage(ActorStage::ClearSlots);
        self.slot_state.mark_all_free();
        encoder.clear_vram_slots();
        ControlStatus::Ok
    }

    /// 导入显存直通槽位并激活硬编（不动档位）
    fn import_vram_slots(&mut self, mut slots: Vec<VramSlotDescriptor>) -> ControlStatus {
        if !self.ensure_hardware_backend() {
            log::error!("硬件编码不可用");
            return ControlStatus::NoHardwareEncoder;
        }
        let Some(encoder) = &mut self.encoder else {
            return ControlStatus::NoHardwareEncoder;
        };
        // 换表全清前置 drain
        self.slot_state.set_stage(ActorStage::Drain);
        if let Err(e) = encoder.drain() {
            log::error!("槽位导入前排空失败，整批拒绝: {e}");
            return ControlStatus::ImportFailed;
        }
        self.slot_state.set_stage(ActorStage::ImportSlots);
        self.slot_state.mark_all_free();
        match encoder.import_vram_slots(&mut slots) {
            Ok(()) => ControlStatus::Ok,
            Err(e) => {
                log::error!("显存直通槽位注册失败: {}", e);
                ControlStatus::ImportFailed
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

    /// 按给定档位配好编码器：档位与后端必须同进同退
    /// **后端未变时不重建。**
    fn prepare_encoder(
        &mut self,
        profile: StreamingProfile,
        prefer_hardware: bool,
    ) -> ControlStatus {
        let same_backend = self
            .encoder
            .as_ref()
            .is_some_and(|e| e.is_hardware() == prefer_hardware);

        let mut reconfigured = false;
        let mut fresh_blocks = Vec::new();
        if same_backend && let Some(encoder) = self.encoder.as_mut() {
            match encoder.reconfigure(profile) {
                Ok(blocks) => {
                    reconfigured = true;
                    fresh_blocks = blocks;
                }
                Err(e) => {
                    log::warn!("编码器重配被拒，按新档位重建: {}", e);
                }
            }
        }
        // 重配可能换了内存域（如 Vulkan 换了设备），此时后端交回新批
        self.attach(fresh_blocks);

        let code = if reconfigured {
            self.profile = profile;
            ControlStatus::Ok
        } else {
            self.rebuild_encoder(profile, prefer_hardware)
        };
        if code != ControlStatus::Ok {
            return code;
        }

        // 码率复位到**生效档位**的上限
        self.bitrate = self.profile.bitrate();
        Self::apply_bitrate(&mut self.encoder, self.bitrate);
        ControlStatus::Ok
    }

    /// 重建编码器。会销毁旧编码器的一切：CUcontext、已导入的显存槽位、当前 GOP。
    fn rebuild_encoder(
        &mut self,
        profile: StreamingProfile,
        prefer_hardware: bool,
    ) -> ControlStatus {
        let had_hardware = self.encoder.as_ref().is_some_and(|e| e.is_hardware());
        if had_hardware {
            log::warn!("重建硬件编码器：已导入的显存槽位随旧编码器失效，需重新注册");
        }

        // 软编路径的档位顶格：内容驱动档位退化为 min(Content, 1080p)@60。
        // 生效档位必须如实反映钳制结果，否则 Java 上报值与实际码流分叉
        let mut effective = profile;
        if !prefer_hardware {
            effective = Self::software_ceiling_profile(profile);
        }

        if prefer_hardware {
            // 必须先释放旧编码器
            self.encoder = None;
            // 后端连带交回它自己设备上的帧块：后端建不起来（含帧块分配失败）就整体回退软编，
            // 不存在「编码器起来了、帧块却没就绪」的中间态
            match (self.hardware_builder.as_ref())(profile, self.slot_state) {
                Ok((hw, blocks)) => {
                    log::debug!("硬件编码器就绪!");
                    self.attach(blocks);
                    self.encoder = Some(hw);
                }
                Err(e) => {
                    log::error!("硬件编码器初始化失败，回退软编码: {}", e);
                    effective = Self::software_ceiling_profile(profile);
                    self.install_software(effective);
                }
            }
        } else {
            // 不要求硬编：按顶格后的档位重建软编。
            self.install_software(effective);
        }

        if self.encoder.is_none() {
            log::error!("编码器建立失败，档位维持 {:?}", self.profile);
            return ControlStatus::ReconfigureFailed;
        }

        self.profile = effective;
        ControlStatus::Ok
    }

    /// 软编档位顶格：`min(Content Resolution, 1080p) @ 60`，只缩不放、保留 bpp
    fn software_ceiling_profile(profile: StreamingProfile) -> StreamingProfile {
        let (w, h, f) = crate::encode::clamp_to_software_ceiling(
            profile.width(),
            profile.height(),
            profile.fps(),
        );
        if (w, h, f) != (profile.width(), profile.height(), profile.fps()) {
            log::warn!(
                "软编顶格：请求 {}x{}@{} 钳制为 {w}x{h}@{f}",
                profile.width(),
                profile.height(),
                profile.fps()
            );
        }
        StreamingProfile::new_with_bpp(w, h, f, Some(profile.bpp()), profile.codec())
    }

    /// 建软编并把它的堆块装进池
    fn install_software(&mut self, profile: StreamingProfile) {
        match VideoEncoder::new_software(profile) {
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

    /// 确保当前是硬编后端；不动档位，按缓存补回跟随码率
    fn ensure_hardware_backend(&mut self) -> bool {
        if self.encoder.as_ref().is_some_and(|e| e.is_hardware()) {
            return true;
        }

        let profile = self.profile;
        // 不看返回码：`rebuild_encoder` 只在「一个编码器都建不起来」时失败，
        // 而硬编创建失败会静默回退软编并返回 Ok。这里要问的是「是不是硬编」。
        self.slot_state.set_stage(ActorStage::Reconfigure);
        self.rebuild_encoder(profile, true);
        if !self.encoder.as_ref().is_some_and(|e| e.is_hardware()) {
            return false;
        }

        // 重建把编码器打回了档位 CBR，必须补回 RC 上次派发的值——
        // 不补的话 RC 以为还在跟随，编码器实际跑着档位上限。
        if self.bitrate > 0 {
            Self::apply_bitrate(&mut self.encoder, self.bitrate);
        }
        true
    }

    /// 应用档位，返回**实际生效**的档位或拒绝原因
    fn change_profile(
        &mut self,
        profile: StreamingProfile,
    ) -> Result<StreamingProfile, ControlStatus> {
        // 软编在跑：档位顶格 min(Content, 1080p)@60，生效档位如实反映钳制结果
        let profile = if self.encoder.as_ref().is_some_and(|e| !e.is_hardware()) {
            Self::software_ceiling_profile(profile)
        } else {
            profile
        };

        if self.profile.width() == profile.width()
            && self.profile.height() == profile.height()
            && self.profile.fps() == profile.fps()
            && self.profile.codec() == profile.codec()
        {
            // 形状未变：编码器不必重建，直接采纳。bpp 这类软参数由调用方从当前档位带过来，
            // 实际不会变；采纳而不是忽略，是为了不留下「会话用新值、Actor 用旧值」的分叉。
            self.profile = profile;
            return Ok(profile);
        }

        // 编码器尚未就绪：直接采纳，后续按新档位构建
        let reconfig = match &mut self.encoder {
            Some(encoder) => encoder.reconfigure(profile),
            None => Ok(Vec::new()),
        };
        let fresh_blocks = match reconfig {
            Ok(blocks) => blocks,
            Err(e) => {
                log::error!("Profile 变更失败，维持原档位 {:?}: {}", self.profile, e);
                return Err(ControlStatus::ReconfigureFailed);
            }
        };
        self.attach(fresh_blocks);

        self.profile = profile;
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
    fn encode_frame(&mut self, payload: &FramePayload, birth_nanos: u64) {
        let Some(encoder) = &mut self.encoder else {
            // 未配置编码器时消费掉的帧同样没上链路，与编码失败并账。
            // 显存槽位的所有权位与编码器解耦：帧已被取走，位必须归还
            self.inbox
                .outcomes()
                .record_with(FrameOutcome::EncodeFailed, "尚无编码器");
            if let FramePayload::VramSlot { slot_idx, .. } = payload {
                self.slot_state.mark_free(*slot_idx);
            }
            return;
        };

        // 采集侧送来的字节数必须与编码器此刻的几何一致：本处是档位的唯一事实源头，
        // 换分辨率后采集侧若还按旧尺寸送帧，就在这里拦下而不是换出一屏错行距花屏
        if let FramePayload::Host { frame } = payload {
            let expected = display_nv12_len(&self.profile);
            if frame.len() != expected {
                let (width, height) = (self.profile.width(), self.profile.height());
                self.inbox.outcomes().record_with(
                    FrameOutcome::GeometryMismatch,
                    format!(
                        "本帧 {} 字节，期望 {expected}（编码器 {width}x{height}）",
                        frame.len()
                    ),
                );
                return;
            }
        }

        let force_keyframe = self.keyframe.take();
        self.slot_state.set_stage(ActorStage::EncodeFrame);
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
    /// `request()` 是幂等的 `store(true)`，所以只在本轮确实带着请求时才重挂——否则会凭空
    /// 多要一个 IDR。
    fn rearm_keyframe(&self, requested: bool) {
        if requested {
            self.keyframe.request();
        }
    }

    /// 处理取出的最新帧：换档拒收先于编码
    ///
    /// 档位已换、槽位表待重导入期间到达的槽位帧引用的是重导入前的旧槽位表
    /// （几何过期或索引悬空，旧 sync_value 还可能落到新导入的信号量上长期等待），
    /// 在入口丢弃并
    /// 归还所有权位；主机帧自带像素、不引用表，照常编码。正常期两类帧都进编码。
    /// 帧的归还在 `FrameBuf::drop` 里完成（析构即归还），各出口都不会泄漏或提前释放
    fn handle_frame(&mut self, frame: VideoFrame) {
        if self.rejecting_frames
            && let FramePayload::VramSlot { slot_idx, .. } = &frame.payload
        {
            self.slot_state.mark_free(*slot_idx);
            self.inbox.outcomes().record_with(
                FrameOutcome::FenceDropped,
                "槽位表重建中：显存帧暂不接收，等待重新导入",
            );
            return;
        }
        self.encode_frame(&frame.payload, frame.birth_nanos);
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::encode::VideoCodec;
    use crate::encode::encoder::{EncodeBackendKind, VideoEncodeBackend};
    use crate::encode::frame::{Mailbox, VideoFrame};
    use crate::encode::hardware::VramSlotDescriptor;
    use crate::encode::hardware::slots::SlotStateBlock;
    use crate::encode::pool::test_support::host_payload;
    use crate::encode::pool::{FramePool, POOL_DEPTH, fake_device_blocks};

    use super::*;

    /// 测试用发件箱：直接取用 `FrameOutbox`，断言与生产同一条路径
    fn test_outbox() -> Arc<FrameOutbox> {
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(bell));
        Arc::new(Mailbox::new(inbox.outcomes_arc()))
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

    /// 假硬编工厂：交出硬件身份编码器与新域块，形状与生产入口一致
    fn fake_hardware_builder(kind: EncodeBackendKind) -> Arc<HardwareBuilder> {
        Arc::new(move |profile, _slot_state| {
            Ok((
                VideoEncoder::from_backend(profile, Box::new(FakeBackend { kind })),
                fake_device_blocks(FAKE_DEVICE_DOMAIN),
            ))
        })
    }

    /// 建一个测试 Actor：池由测试持有（与生产一致，Actor 只有弱引用）
    fn test_actor(
        inbox: Arc<FrameInbox>,
        keyframe: Arc<KeyframeTrigger>,
        output: Arc<FrameOutbox>,
    ) -> (EncoderActor, Arc<FramePool>) {
        test_actor_with_builder(
            inbox,
            keyframe,
            output,
            Arc::new(VideoEncoder::new_hardware),
        )
    }

    /// 注入自定义硬编工厂的 Actor 构造
    fn test_actor_with_builder(
        inbox: Arc<FrameInbox>,
        keyframe: Arc<KeyframeTrigger>,
        output: Arc<FrameOutbox>,
        hardware_builder: Arc<HardwareBuilder>,
    ) -> (EncoderActor, Arc<FramePool>) {
        let pool = FramePool::new();
        let actor = EncoderActor::new_with_hardware_builder(
            inbox,
            keyframe,
            output,
            Arc::downgrade(&pool),
            SlotStateBlock::new_leaked(),
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

        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, pool) = test_actor_with_builder(
            inbox,
            keyframe,
            Arc::clone(&output),
            fake_hardware_builder(EncodeBackendKind::Nvenc),
        );
        let profile = StreamingProfile::new(1280, 720, 60, VideoCodec::H264);

        // 1. 请求硬编：ACK 时池必须已换成假设备块
        assert_eq!(prepare(&mut actor, profile, true), ControlStatus::Ok);
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
        assert_eq!(prepare(&mut actor, profile, false), ControlStatus::Ok);
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

    /// 所有权位纪律：换代前置收尾不得清在途认领位，全清只属于 drain 成功后的换表命令
    ///
    /// 位承载「GPU 正在读这张纹理」——清早了，渲染线程下次 claim 会与编码器对
    /// 同一张图并发读写。这条用例钉死时序：清零必须发生在排空之后
    #[test]
    fn test_bit_clearing_only_after_drain() {
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool) = test_actor_with_builder(
            inbox,
            keyframe,
            Arc::clone(&output),
            fake_hardware_builder(EncodeBackendKind::Nvenc),
        );
        let profile = StreamingProfile::new(1280, 720, 60, VideoCodec::H264);
        assert_eq!(prepare(&mut actor, profile, true), ControlStatus::Ok);

        // 模拟渲染线程已认领槽位 0（帧在途）
        actor.slot_state.set_busy(0);

        // 换代前置收尾：收件箱清空可以，在途认领位不许动
        actor.discard_frames_before_rebuild();
        assert!(
            actor.slot_state.is_busy(0),
            "换代前置收尾不得清在途认领位，否则编码读与渲染写并发"
        );

        // 注销路径：drain 成功（假后端恒 Ok）才允许全清
        assert_eq!(actor.clear_vram_slots(), ControlStatus::Ok);
        assert!(!actor.slot_state.is_busy(0), "drain 成功后的注销必须全清");

        // 导入路径同理：drain 成功即先全清，导入本身失败也不回滚（排空后清零本就安全）
        actor.slot_state.set_busy(1);
        assert_eq!(
            actor.import_vram_slots(Vec::new()),
            ControlStatus::ImportFailed,
            "假后端导入必失败，但 drain 已成功"
        );
        assert!(!actor.slot_state.is_busy(1), "导入路径的清零同样只认 drain");
    }

    /// 堆块路径必须真的产出码流
    #[test]
    fn test_soft_path_encodes_heap_frame() {
        crate::encode::software::init_api_for_tests();
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut state, pool) = test_actor(inbox, keyframe, Arc::clone(&output));

        let (w, h) = (1280usize, 720usize);
        let profile = StreamingProfile::new(w, h, 60, VideoCodec::H264);
        state.profile = profile;
        if let Ok((encoder, blocks)) = VideoEncoder::new_software(profile) {
            pool.attach(blocks);
            state.encoder = Some(encoder);
        }

        let payload = host_payload(&pool, vec![128u8; display_nv12_len(&state.profile)]);
        state.encode_frame(&payload, 0);
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

    /// 主机帧字节数与编码器几何不符时必须丢帧：错行距的像素交给编码器换的是一屏花屏
    #[test]
    fn test_soft_path_drops_mismatched_host_frame() {
        crate::encode::software::init_api_for_tests();
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut state, pool) = test_actor(inbox, keyframe, Arc::clone(&output));

        let profile = StreamingProfile::new(1280, 720, 60, VideoCodec::H264);
        state.profile = profile;
        if let Ok((encoder, blocks)) = VideoEncoder::new_software(profile) {
            pool.attach(blocks);
            state.encoder = Some(encoder);
        }

        // 按旧几何的一半长度送帧（典型现场：换分辨率后采集侧还按旧尺寸回读）
        let payload = host_payload(&pool, vec![128u8; display_nv12_len(&profile) / 2]);
        state.encode_frame(&payload, 0);
        drop(payload);

        assert!(output.take().is_none(), "布局不符的帧不得产出编码产物");
        assert_eq!(pool.len(), POOL_DEPTH, "被丢弃的帧块必须归还到池中");
    }

    /// 分辨率变化时重建软编器：尺寸不匹配的旧编码器继续用会编出错帧
    #[test]
    fn test_soft_encoder_rebuilt_on_dimension_change() {
        crate::encode::software::init_api_for_tests();
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut state, pool) = test_actor(inbox, keyframe, Arc::clone(&output));

        let p1_profile = StreamingProfile::new(1280, 720, 60, VideoCodec::H264);
        state.profile = p1_profile;
        if let Ok((encoder, blocks)) = VideoEncoder::new_software(p1_profile) {
            pool.attach(blocks);
            state.encoder = Some(encoder);
        }

        let p1 = host_payload(&pool, vec![0u8; display_nv12_len(&p1_profile)]);
        state.encode_frame(&p1, 0);
        drop(p1);
        assert!(output.take().is_some());

        let new_profile = StreamingProfile::new(1600, 900, 60, VideoCodec::H264);
        state.profile = new_profile;
        if let Some(encoder) = &mut state.encoder {
            // 软编不碰设备，重配不该交回新批
            assert!(encoder.reconfigure(new_profile).unwrap().is_empty());
        }

        let frame_bytes = display_nv12_len(&new_profile);
        let p2 = host_payload(&pool, vec![0u8; frame_bytes]);
        state.encode_frame(&p2, 16);
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
        profile: StreamingProfile,
        prefer_hardware: bool,
    ) -> ControlStatus {
        let (reply, rx) = tokio::sync::oneshot::channel();
        assert!(actor.apply(ControlCommand::PrepareEncoder {
            profile,
            prefer_hardware,
            reply,
        }));
        wait_reply(rx)
    }

    /// 直接走 Actor 的控制出口换档并取回生效档位或拒绝原因
    fn switch_profile(
        actor: &mut EncoderActor,
        profile: StreamingProfile,
    ) -> Result<StreamingProfile, ControlStatus> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        assert!(actor.apply(ControlCommand::ProfileChange { profile, reply }));
        wait_reply(rx)
    }

    /// 直接走 Actor 的控制出口下发跟随码率
    fn set_bitrate(actor: &mut EncoderActor, bps: usize) {
        assert!(actor.apply(ControlCommand::BitrateChange(bps)));
    }

    #[test]
    fn test_encoder_actor_thread_run_loop_and_drain() {
        crate::encode::software::init_api_for_tests();
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let (ctl_tx, ctl_rx) = tokio::sync::mpsc::channel::<ControlCommand>(16);
        let output = test_outbox();

        // 池由测试线程持有，Actor 只拿弱引用：池消失即等于管线关停
        let pool = FramePool::new();
        let actor_pool = Arc::downgrade(&pool);
        let actor_inbox = Arc::clone(&inbox);
        let actor_keyframe = Arc::clone(&keyframe);
        let actor_bell = Arc::clone(&bell);
        let actor_output = Arc::clone(&output);
        let join = std::thread::Builder::new()
            .name("test-actor".to_string())
            .spawn(move || {
                let actor = EncoderActor::new(
                    actor_inbox,
                    actor_keyframe,
                    actor_output,
                    actor_pool,
                    SlotStateBlock::new_leaked(),
                );
                actor.run(ctl_rx, actor_bell);
            })
            .unwrap();

        // 0. 调整 Profile 为 720p (1280x720)
        let profile = StreamingProfile::new(1280, 720, 60, VideoCodec::H264);
        let frame_bytes = display_nv12_len(&profile);
        let (ack0_tx, ack0_rx) = tokio::sync::oneshot::channel();
        assert!(
            ctl_tx
                .blocking_send(ControlCommand::ProfileChange {
                    profile,
                    reply: ack0_tx,
                })
                .is_ok(),
            "控制队列必须受理"
        );
        assert_eq!(
            wait_reply(ack0_rx),
            Ok(profile),
            "应答必须是 Actor 实际生效的档位"
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
                .blocking_send(ControlCommand::ClearVramSlots { reply: ack_tx })
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
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let (mut actor, _pool) = test_actor(inbox, keyframe, test_outbox());

        // 1. 初始为 1080p Profile (上限 20_000_000 bps)：走公共缝装配，不直写字段
        let p1080 = StreamingProfile::new(1920, 1080, 60, VideoCodec::H264);
        assert_eq!(prepare(&mut actor, p1080, false), ControlStatus::Ok);

        // 2. 模拟 Rate Control 将 Followed Bitrate 调至 5_000_000 bps
        set_bitrate(&mut actor, 5_000_000);
        assert_eq!(actor.bitrate, 5_000_000);

        // 3. 切到 720p（上限 7_533_282 bps，高于 5M）：缓存不动
        let p720 = StreamingProfile::new(1280, 720, 60, VideoCodec::H264);
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
        let p720_15fps = StreamingProfile::new(1280, 720, 15, VideoCodec::H264);
        assert!(
            p720_15fps.bitrate() < 10_000_000,
            "本用例前提：新档位上限低于 10M"
        );
        let _ = switch_profile(&mut actor, p720_15fps);
        assert_eq!(
            actor.bitrate, 10_000_000,
            "缓存不得被档位变化改写：它只反映 RC 派发过的值"
        );
    }

    /// 围栏测试后端：槽位表行为可配置，不碰任何驱动；编码恒产出非空 NALU，
    /// 让「被围栏拒收」与「被编码 admitted」在账本上可区分。成功出口归还
    /// 所有权位，与真实后端同一位协议纪律
    struct FenceBackend {
        slot_state: &'static SlotStateBlock,
        drain_ok: bool,
        import_ok: bool,
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
            payload: &FramePayload,
            _force_idr: bool,
            _frame_idx: u64,
        ) -> Result<(Bytes, bool), String> {
            if let FramePayload::VramSlot { slot_idx, .. } = payload {
                self.slot_state.mark_free(*slot_idx);
            }
            Ok((Bytes::from_static(b"fence-test-nalu"), false))
        }

        fn reconfigure(
            &mut self,
            _new_profile: StreamingProfile,
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

    /// 围栏测试工厂：交出硬件身份编码器（行为按用例配置）
    fn fence_backend_builder(drain_ok: bool, import_ok: bool) -> Arc<HardwareBuilder> {
        Arc::new(move |profile, slot_state| {
            Ok((
                VideoEncoder::from_backend(
                    profile,
                    Box::new(FenceBackend {
                        slot_state,
                        drain_ok,
                        import_ok,
                    }),
                ),
                fake_device_blocks(FAKE_DEVICE_DOMAIN),
            ))
        })
    }

    /// 围栏测试专用构造：额外交出槽位所有权块，供用例模拟渲染线程置位与断言位协议
    fn fence_actor(
        inbox: Arc<FrameInbox>,
        keyframe: Arc<KeyframeTrigger>,
        output: Arc<FrameOutbox>,
        drain_ok: bool,
        import_ok: bool,
    ) -> (EncoderActor, Arc<FramePool>, &'static SlotStateBlock) {
        let pool = FramePool::new();
        let block = SlotStateBlock::new_leaked();
        let actor = EncoderActor::new_with_hardware_builder(
            inbox,
            keyframe,
            output,
            Arc::downgrade(&pool),
            block,
            fence_backend_builder(drain_ok, import_ok),
        );
        (actor, pool, block)
    }

    /// 槽位帧构造（句柄 -1 且已接管，Drop 不会触碰系统句柄）
    fn vram_frame(slot_idx: u32, sync_value: u64) -> VideoFrame {
        VideoFrame {
            birth_nanos: 0,
            payload: FramePayload::VramSlot {
                slot_idx,
                sync_value,
            },
        }
    }

    /// 读取槽位类命令的阻塞应答（槽位命令的控制面走 std 通道）
    fn wait_control_reply(rx: std::sync::mpsc::Receiver<ControlStatus>) -> ControlStatus {
        rx.recv_timeout(std::time::Duration::from_millis(500))
            .expect("槽位命令应答超时")
    }

    /// 经控制出口导入一批 720p 槽位并取回结果码
    fn import_slots(actor: &mut EncoderActor, slot_idx: u32) -> ControlStatus {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        assert!(actor.apply(ControlCommand::ImportVramSlots {
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
        wait_control_reply(rx)
    }

    /// 用例 1：几何变化开窗——窗口内槽位帧入口拒收并归还所有权位，主机帧照收；
    /// 导入完成后关窗，新槽位帧恢复编码
    #[test]
    fn test_fence_drops_window_frames_then_admits_new_generation() {
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, pool, block) =
            fence_actor(inbox.clone(), keyframe, output.clone(), true, true);

        let p1080 = StreamingProfile::new(1920, 1080, 60, VideoCodec::H264);
        assert_eq!(prepare(&mut actor, p1080, true), ControlStatus::Ok);
        assert!(!actor.rejecting_frames);

        // 几何变化生效：窗口开启
        let p720 = StreamingProfile::new(1280, 720, 60, VideoCodec::H264);
        assert_eq!(switch_profile(&mut actor, p720), Ok(p720));
        assert!(actor.rejecting_frames);

        // 窗口内槽位帧：入口拒收、busy 位归还、不进编码
        block.set_busy(0);
        inbox.push(vram_frame(0, 5));
        let frame = inbox.take().expect("帧在收件箱");
        actor.handle_frame(frame);
        assert_eq!(
            inbox.outcomes().count(FrameOutcome::FenceDropped),
            1,
            "窗口内槽位帧必须被围栏拒收"
        );
        assert!(!block.is_busy(0), "被拒帧的所有权位必须在入口归还");
        assert!(
            output.take().is_none(),
            "被拒帧不得进编码，发件箱不得有产物"
        );

        // 窗口内主机帧：自带像素不引用表，照收。
        // `Admitted` 由会话写循环记账，Actor 层以发件箱产物为准
        let host = host_payload(&pool, vec![100u8; display_nv12_len(&p720)]);
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

        block.set_busy(0);
        inbox.push(vram_frame(0, 6));
        let frame = inbox.take().expect("新代帧在收件箱");
        actor.handle_frame(frame);
        assert!(output.take().is_some(), "新代槽位帧必须恢复编码");
        assert!(!block.is_busy(0), "编码出口归还所有权位");
    }

    /// 用例 2：同几何换 codec 也开窗——旧 sync_value 帧不得落进新导入的信号量。
    /// 这是重配触发面的语义洞：几何比对拦不住几何相同的旧帧，只有入口拒收能
    #[test]
    fn test_fence_covers_same_geometry_codec_change() {
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool, block) =
            fence_actor(inbox.clone(), keyframe, output.clone(), true, true);

        let p1080_h264 = StreamingProfile::new(1920, 1080, 60, VideoCodec::H264);
        assert_eq!(prepare(&mut actor, p1080_h264, true), ControlStatus::Ok);

        let p1080_h265 = StreamingProfile::new(1920, 1080, 60, VideoCodec::H265);
        assert_eq!(switch_profile(&mut actor, p1080_h265), Ok(p1080_h265));
        assert!(actor.rejecting_frames);

        block.set_busy(3);
        inbox.push(vram_frame(3, 100));
        let frame = inbox.take().expect("帧在收件箱");
        actor.handle_frame(frame);
        assert_eq!(
            inbox.outcomes().count(FrameOutcome::FenceDropped),
            1,
            "同几何换 codec 的旧 sync_value 帧必须被围栏拒收"
        );
        assert!(!block.is_busy(3), "被拒帧的所有权位必须归还");
        assert!(output.take().is_none(), "被拒帧不得进编码");

        assert_eq!(import_slots(&mut actor, 3), ControlStatus::Ok);
        assert!(!actor.rejecting_frames);
    }

    /// 用例 3：导入前排空失败——整批拒绝且窗口不滞留，旧代恢复接收
    #[test]
    fn test_fence_closes_window_when_import_drain_fails() {
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool, _block) =
            fence_actor(inbox.clone(), keyframe, output.clone(), false, true);

        let p1080 = StreamingProfile::new(1920, 1080, 60, VideoCodec::H264);
        assert_eq!(prepare(&mut actor, p1080, true), ControlStatus::Ok);
        let p720 = StreamingProfile::new(1280, 720, 60, VideoCodec::H264);
        let _ = switch_profile(&mut actor, p720);
        assert!(actor.rejecting_frames);

        assert_eq!(
            import_slots(&mut actor, 0),
            ControlStatus::ImportFailed,
            "排空失败必须整批拒绝"
        );
        assert!(!actor.rejecting_frames);

        inbox.push(vram_frame(0, 7));
        let frame = inbox.take().expect("帧在收件箱");
        actor.handle_frame(frame);
        assert!(output.take().is_some(), "导入失败后窗口关闭，帧恢复接收");
        assert_eq!(inbox.outcomes().count(FrameOutcome::FenceDropped), 0);
    }

    /// 用例 4：清空槽位表后，显存帧拒收直到下一次导入
    #[test]
    fn test_slots_cleared_rejects_until_next_import() {
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool, block) =
            fence_actor(inbox.clone(), keyframe, output.clone(), true, true);

        let p1080 = StreamingProfile::new(1920, 1080, 60, VideoCodec::H264);
        assert_eq!(prepare(&mut actor, p1080, true), ControlStatus::Ok);
        assert!(!actor.rejecting_frames);

        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        assert!(actor.apply(ControlCommand::ClearVramSlots { reply }));
        assert_eq!(wait_control_reply(rx), ControlStatus::Ok);
        assert!(actor.rejecting_frames);

        block.set_busy(2);
        inbox.push(vram_frame(2, 9));
        let frame = inbox.take().expect("帧在收件箱");
        actor.handle_frame(frame);
        assert_eq!(
            inbox.outcomes().count(FrameOutcome::FenceDropped),
            1,
            "槽位表清空后的槽位帧必须被拒收"
        );
        assert!(!block.is_busy(2), "被拒帧的所有权位必须归还");

        assert_eq!(import_slots(&mut actor, 2), ControlStatus::Ok);
        assert!(!actor.rejecting_frames);

        inbox.push(vram_frame(2, 10));
        let frame = inbox.take().expect("新代帧在收件箱");
        actor.handle_frame(frame);
        assert!(output.take().is_some(), "新导入后槽位帧恢复编码");
    }

    /// 用例 5：拒收只开不关——窗口开着时来一条 fps-only 重配，不得提前放行
    /// 旧代帧；恢复只认导入
    #[test]
    fn test_fps_only_change_keeps_rejecting_until_import() {
        let keyframe = Arc::new(KeyframeTrigger::new());
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let output = test_outbox();
        let (mut actor, _pool, block) =
            fence_actor(inbox.clone(), keyframe, output.clone(), true, true);

        let p1080 = StreamingProfile::new(1920, 1080, 60, VideoCodec::H264);
        assert_eq!(prepare(&mut actor, p1080, true), ControlStatus::Ok);

        // fps-only 重配不动表：单独发生时不开窗
        let p1080_30fps_h264 = StreamingProfile::new(1920, 1080, 30, VideoCodec::H264);
        let _ = switch_profile(&mut actor, p1080_30fps_h264);
        assert!(!actor.rejecting_frames, "fps-only 重配不得开窗");

        // 同几何换 codec 开窗（旧 sync_value 帧自此不得进编码）
        let p1080_h265 = StreamingProfile::new(1920, 1080, 60, VideoCodec::H265);
        let _ = switch_profile(&mut actor, p1080_h265);
        assert!(actor.rejecting_frames);

        // 窗口期间来一条 fps-only 重配：不得关闭拒收
        let p1080_30fps = StreamingProfile::new(1920, 1080, 30, VideoCodec::H265);
        let _ = switch_profile(&mut actor, p1080_30fps);
        assert!(actor.rejecting_frames, "fps-only 重配不得提前放行旧代帧");

        block.set_busy(1);
        inbox.push(vram_frame(1, 77));
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
}

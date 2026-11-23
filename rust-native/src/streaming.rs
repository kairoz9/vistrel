pub(crate) mod audio_loopback;
mod cancellation;
pub mod input;
pub(crate) mod rate_control;
pub mod session;
pub(crate) mod transport;
mod writers;

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwapOption;
use rtc::peer_connection::sdp::RTCSessionDescription;
use webrtc::peer_connection::RTCIceCandidateInit;

use crate::encode::BackendCapability;
use crate::encode::EncodeBackendKind;
use crate::encode::StreamingProfile;
use crate::encode::TierRequest;
use crate::encode::VideoCodec;
use crate::encode::VideoPipeline;
use crate::streaming::audio_loopback::AudioLoopback;
use crate::streaming::session::{IceCandidateSink, SessionHandler, StreamingSession};

/// 串流引擎：会话主控与编码/音频管线的组合根
///
/// 单活跃会话语义——会话槽位以 [`ArcSwapOption`] 原子替换，纪元快照结算保证
/// 「建立期间被关闭」的会话就地自弃，绝不装进已关停的引擎；ICE 候选上抛经
/// 弱引用 handler 回指，避免 Engine→Session→PC→Handler 强引用环
pub struct StreamingEngine {
    udp_bind: Arc<str>,

    /// 当前活跃远程连接
    active_session: ArcSwapOption<StreamingSession>,

    /// 会话纪元：每次关闭 +1，安装按进入时的快照结算——建立期间引擎被关闭，
    /// 安装方就地自弃，绝不把建好的会话装进已关停的引擎
    ///
    /// 顺序不变量：close 侧先 add 再摘槽；安装侧先 swap 再复核。
    session_epoch: AtomicU64,

    /// 编码管线的组合根：帧收件箱、控制队列、关键帧触发器、CPU 帧池、编码线程
    video: Arc<VideoPipeline>,

    /// 音频回环：Opus 编码与当前会话的有界队列发送端
    audio: Arc<AudioLoopback>,

    /// 服务端 ICE 候选上抛出口 (trickle)
    candidate_sink: ArcSwapOption<IceCandidateSink>,
}

/// 远程玩家随 offer 携带的编码偏好：缺省为自动；指定的值必须可满足，否则协商拒绝
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncodePreferences {
    /// 编码后端偏好（线上词汇 `nvenc` / `vulkan_video` / `software`；`None` 为自动）
    pub backend: Option<EncodeBackendKind>,
    /// Codec 偏好（`VideoCodec` 线上令牌；`None` 为自动）
    pub codec: Option<VideoCodec>,
}

/// 档位 ask 的应答（被动数据载体）：只有生效值能被回传。
pub enum ProfileApply {
    /// 请求已被编码器接受并生效（codec 是会话常量，不随档位应答）
    Applied(StreamingProfile),
    /// 请求被拒绝（0 几何 / 0 帧率的暂停信号，或编码器重配失败）；
    /// 旧档位由调用方自己的基线保持
    Rejected,
    /// 无活跃会话
    NoSession,
}

impl StreamingEngine {
    /// 装配组合根：只接线引用并清空会话槽位与纪元，无 I/O 副作用，也不建立会话
    pub(crate) fn new(
        video: Arc<VideoPipeline>,
        audio: Arc<AudioLoopback>,
        udp_bind: String,
    ) -> Self {
        Self {
            active_session: ArcSwapOption::from(None),
            session_epoch: AtomicU64::new(0),
            udp_bind: udp_bind.into(),
            video,
            audio,
            candidate_sink: ArcSwapOption::from(None),
        }
    }

    /// 编码管线组合根
    ///
    /// 帧投递、控制命令、排空与关停都经它的具名方法；管线内部（收件箱、帧池、线程句柄）
    /// 不外露，避免调用方把必须成对的操作拆开。
    pub fn video(&self) -> &Arc<VideoPipeline> {
        &self.video
    }

    /// 音频回环（推流协程与采集线程共用）
    pub(crate) fn audio(&self) -> &Arc<AudioLoopback> {
        &self.audio
    }

    /// 设置本端 ICE 候选上抛回调
    pub fn set_candidate_sink(&self, sink: IceCandidateSink) {
        self.candidate_sink.store(Some(Arc::new(sink)));
    }

    /// 触发候选上抛（会话每采集到一条就调一次）
    pub fn dispatch_ice_candidate(&self, candidate_json: String) {
        if let Some(sink) = &*self.candidate_sink.load() {
            sink(candidate_json);
        }
    }

    /// 装配 ICE 候选上抛 handler：弱引用回指，防止 Engine→Session→PC→Handler 闭包强引用环
    pub(crate) fn candidate_handler(self: &Arc<Self>) -> Arc<SessionHandler> {
        let engine = Arc::downgrade(self);
        Arc::new(SessionHandler::new(Arc::new(move |json| {
            if let Some(engine) = engine.upgrade() {
                engine.dispatch_ice_candidate(json);
            }
        })))
    }

    /// 关停引擎的唯一出口，幂等可重复调用
    ///
    /// 次序固定为「先关会话，再停编码线程」。
    pub async fn stop(&self) {
        self.close_session().await;
        self.video.shutdown();
    }

    /// 关闭当前活跃会话
    ///
    /// 只拆会话不拆管线
    pub async fn close_session(&self) {
        self.session_epoch.fetch_add(1, Ordering::SeqCst);
        let old = self.active_session.swap(None);
        if let Some(session) = old {
            session.close().await;
        }
        // 没有会话就没有收件人，音频编码在入队前短路
        self.audio.detach();
    }

    /// 向活跃会话转发 trickle 候选
    ///
    /// 无活跃会话即丢弃并告警：协商期间到达的候选由信令层同 channel 串行投递兜底
    /// （排队到 [`Self::handle_offer`] 返回之后才被应用），无需在此缓存
    pub async fn add_ice_candidate(&self, candidate: RTCIceCandidateInit) {
        if let Some(session) = self.active_session.load().as_ref() {
            session.add_ice_candidate(candidate).await;
        } else {
            // shortcut: 依赖信令层同 channel 串行投递（协商期间的候选会排队到 handle_offer
            // 返回之后）；若协商改成异步，需在此缓存待应用候选，届时升级
            log::warn!("无活跃会话，ICE 候选已丢弃（协商未完成或会话已断开）");
        }
    }

    /// 提取当前会话输入环的事件；无会话即无输入
    pub fn drain_input_events(&self, out_buf: &mut [u8], max_events: usize) -> usize {
        let guard = self.active_session.load();
        let Some(session) = guard.as_ref() else {
            return 0;
        };
        session.drain_input_events(out_buf, max_events)
    }

    /// 一步到位协商 Profile 与 WebRTC SDP，并拉起会话生命周期任务
    ///
    /// 几何与帧率来自游戏内容快照（Java 侧已合并视口上限），bpp 与编码偏好来自
    /// 浏览器端；offer 成功即 Java 信令层自持的「活跃」起点。
    pub async fn handle_offer(
        self: &Arc<Self>,
        offer: RTCSessionDescription,
        width: usize,
        height: usize,
        fps: usize,
        bpp: Option<f32>,
        preferences: EncodePreferences,
    ) -> Result<
        (
            RTCSessionDescription,
            StreamingProfile,
            VideoCodec,
            EncodeBackendKind,
        ),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        if width == 0 || height == 0 || fps == 0 {
            // 0×0 是暂停信号，不是需要「修正」的最小档位；协商必须被显式拒绝
            return Err("内容档位为 0（暂停态不应发起协商）".into());
        }
        let epoch = self.session_epoch.load(Ordering::SeqCst);
        let old_session = self.active_session.swap(None);
        if let Some(old_session) = old_session {
            old_session.close().await;
            self.audio.detach();
        }

        // 指定的后端必须落在能力清单（协商应答回给选择器的同一份可选值）内，无法满足即拒绝
        let backends = crate::encode::hardware_backends();
        let preferred = match preferences.backend {
            None => None,
            Some(kind) => {
                if !crate::encode::is_selectable(kind) {
                    return Err(
                        format!("指定的编码后端 {kind:?} 不在本机能力清单内，协商拒绝").into(),
                    );
                }
                Some(kind)
            }
        };
        // codec 解析对准指定后端的 codec 表，而不是默认胜者的；
        // 软编域没有探测条目——无条目即仅 H.264 可行 + 几何顶格，正是软编的可行域
        let caps_entry = match preferred.as_ref() {
            Some(EncodeBackendKind::Software) => None,
            Some(kind) => backends
                .as_deref()
                .and_then(|list| list.iter().find(|p| &p.backend_type == kind)),
            None => backends.as_deref().and_then(|list| list.first()),
        };
        // codec 是协商产物（offer 提供 ∩ 服务端可行，偏好为硬约束）
        let codec = resolve_codec(Some(&offer), caps_entry, preferences.codec)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        let request = resolve_initial_request(width, height, fps, bpp, caps_entry);

        // prepare 回执携带实际生效档位、codec 与后端身份；指定维度被替换（建败回退）在此拒绝
        let (effective, codec, backend) = self
            .video
            .prepare_encoder(request, codec, preferred)
            .await
            .map_err(|e| format!("编码器未能在会话建立前就绪 (code={})", e.code()))?;
        if let Some(kind) = preferred
            && backend != kind
        {
            return Err(format!(
                "指定的编码后端 {kind:?} 未能就绪（实际建成 {backend:?}），协商拒绝"
            )
            .into());
        }
        if let Some(preferred_codec) = preferences.codec
            && codec != preferred_codec
        {
            return Err(format!(
                "指定的 codec {preferred_codec:?} 未能就绪（实际建成 {codec:?}），协商拒绝"
            )
            .into());
        }

        let handler = self.candidate_handler();

        let (session, answer) =
            StreamingSession::establish(self.as_ref(), offer, effective, codec, handler).await?;

        // 安装结算：先落账再复核纪元
        let old_session = self.active_session.swap(Some(Arc::clone(&session)));
        if self.session_epoch.load(Ordering::SeqCst) != epoch {
            // 撤回自己的发布
            let mine = Some(Arc::clone(&session));
            let _ = self.active_session.compare_and_swap(&mine, None);
            session.close().await;
            return Err("会话建立期间引擎已关闭，会话未安装".into());
        }
        if let Some(old_session) = old_session {
            old_session.close().await;
        }

        Ok((answer, effective, codec, backend))
    }

    /// 应用新的画质档位：阻塞 ask，应答见 [`ProfileApply`]。档位的唯一写路径
    pub async fn apply_profile(
        &self,
        width: usize,
        height: usize,
        fps: usize,
        bpp: f32,
    ) -> ProfileApply {
        let Some(session) = self.active_session.load_full() else {
            log::warn!("无活跃会话，档位请求被拒绝");
            return ProfileApply::NoSession;
        };
        if width == 0 || height == 0 || fps == 0 {
            // 0×0 是暂停信号：不是需要「修正」的最小档位，直接拒绝
            return ProfileApply::Rejected;
        }
        let request = TierRequest {
            width,
            height,
            fps,
            bpp,
        };
        // 控制面所有权在引擎：ask 直接发起，生效码率回填会话的码率回路；
        // 拒绝路径不经过回路，上界自然保持旧值
        match self.video.control().retier(request).await {
            Ok(applied) => {
                session.commit_bps(applied.bitrate());
                ProfileApply::Applied(applied)
            }
            Err(e) => {
                log::warn!("档位变更被拒绝（{e:?}），继续沿用旧档位");
                ProfileApply::Rejected
            }
        }
    }
}

/// 结合内容快照与硬件探测产出初始定档请求（几何与画质）
///
/// 探测条目由调用方注入（生产读全局快照，单测注入假结果）；无条目即软编：
/// 请求几何先钳进 `min(Content Resolution, 1080p) @ 60` 只缩不放——放大发的是假细节。
/// codec 不在此解析（见 [`resolve_codec`]）：它是协商产物，派生本身发生在
/// Actor 的定档命令处理里，这里只产出请求
fn resolve_initial_request(
    width: usize,
    height: usize,
    fps: usize,
    bpp: Option<f32>,
    caps_entry: Option<&BackendCapability>,
) -> TierRequest {
    let mut request = TierRequest {
        width,
        height,
        fps,
        // 缺省即下限画质
        bpp: bpp.unwrap_or(crate::encode::MIN_BPP),
    };
    if caps_entry.is_none() {
        let (w, h, f) =
            crate::encode::clamp_to_software_ceiling(request.width, request.height, request.fps);
        if (w, h, f) != (request.width, request.height, request.fps) {
            log::warn!("软编回退：初始档位钳制为 {w}x{h}@{f}");
        }
        request.width = w;
        request.height = h;
        request.fps = f;
    }
    request
}

/// 收集 offer 实际提供的 codec 集合（rtpmap 识别）：offer 载荷列表承载浏览器解码
/// 可行性而非偏好，本集合是指定偏好可行性校验的依据
fn parse_sdp_offered_codecs(offer: &RTCSessionDescription) -> Option<Vec<VideoCodec>> {
    let desc = offer.unmarshal().ok()?;
    // 只取第一个 video 媒体行
    let media = desc
        .media_descriptions
        .into_iter()
        .find(|m| m.media_name.media == "video")?;

    let offered_pts = media.media_name.formats;

    // 构建 pt -> codec 映射
    let pt_to_codec: HashMap<String, VideoCodec> = media
        .attributes
        .into_iter()
        .filter(|attr| attr.key == "rtpmap")
        .filter_map(|attr| {
            let value = attr.value?;
            let (pt, info) = value.split_once(' ')?;
            let codec = if info.starts_with("AV1") {
                Some(VideoCodec::Av1)
            } else if info.starts_with("H265") || info.starts_with("HEVC") {
                Some(VideoCodec::H265)
            } else if info.starts_with("H264") || info.starts_with("AVC") {
                Some(VideoCodec::H264)
            } else {
                None
            }?;
            Some((pt.to_string(), codec))
        })
        .collect();

    // 按 pt 顺序去重（顺序无语义，仅为去重稳定）
    let mut seen = HashSet::new();
    let offered: Vec<VideoCodec> = offered_pts
        .into_iter()
        .filter_map(|pt| pt_to_codec.get(&pt).copied())
        .filter(|codec| seen.insert(*codec))
        .collect();

    (!offered.is_empty()).then_some(offered)
}

/// 协商 codec：指定的偏好是硬约束——必须在「offer 实际提供 ∩ 服务端支持」内可满足，
/// 无法满足即 `Err` 拒绝，绝不静默替换；auto 按服务端默认档 AV1 > H265 > H264 取第一条
/// 可行项，兜底 H.264。SDP 载荷序只承载浏览器解码可行性，不参与判定
fn resolve_codec(
    offer: Option<&RTCSessionDescription>,
    caps_entry: Option<&BackendCapability>,
    preferred: Option<VideoCodec>,
) -> Result<VideoCodec, String> {
    let supported_codecs = caps_entry
        .map(|p| p.supported_codecs.as_slice())
        .unwrap_or(&[]);

    // 浏览器实际提供的 codec 集合；解析不出时不设限（SDP 本身损坏会在建 answer 时暴露）
    let offered = offer.and_then(parse_sdp_offered_codecs);
    let feasible = |codec: VideoCodec| -> bool {
        let offered_ok = offered
            .as_ref()
            .map(|set| set.contains(&codec))
            .unwrap_or(true);
        offered_ok && (codec == VideoCodec::H264 || supported_codecs.contains(&codec))
    };

    // 指定的偏好不可行（浏览器未提供或服务端无法编码）→ 协商显式拒绝
    if let Some(codec) = preferred {
        if feasible(codec) {
            return Ok(codec);
        }
        return Err(format!(
            "指定的 codec {codec:?} 不可行：浏览器未提供或服务端无法编码"
        ));
    }

    // auto：服务端默认档位取第一条可行项，兜底 H.264
    for candidate in [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264] {
        if feasible(candidate) {
            return Ok(candidate);
        }
    }
    Ok(VideoCodec::H264)
}

/// 引擎级测试基建，供本模块与 `jni` 层的线契约用例共用：jni 缝测试驱动同一批假后端，
/// 「出口 → 核心 → 引擎」整条线只有这一份假 Actor 行为
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::encode::control::{ControlCommand, ControlSender, ControlStatus};
    use crate::encode::encoder::EncodeBackendKind;
    use crate::encode::frame::FrameInbox;
    use tokio::sync::Notify;

    /// 假 Actor 转发出来的命令摘要：只保留断言要看的几个事实，避免用字符串当命令名
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum Rec {
        /// PrepareEncoder：定档请求与后端偏好
        Prepare(TierRequest, Option<EncodeBackendKind>),
        Retier,
        SetBitrate,
        RequestKeyframe,
        Other,
    }

    /// 造一个引擎，并配一个**假编码线程**
    ///
    /// `handle_offer` 要等 `PrepareEncoder` 的 ACK，没有应答者就会走满 5s 超时，
    /// 所有会话用例都会退化成超时失败。假线程只回 `Ok`、不碰编码器，同时把收到的命令
    /// 转发出来供断言——真正的编码行为由 `encode/actor.rs` 的用例覆盖。
    /// `reject_change` 让档位重配一律被拒（回传 `ReconfigureFailed`），供拒绝路径用例。
    pub(crate) fn make_test_engine(
        reject_change: bool,
    ) -> (Arc<StreamingEngine>, std::sync::mpsc::Receiver<Rec>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ControlCommand>(16);
        let (recorded_tx, recorded_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // 假 Actor 跑在普通线程上，用阻塞接收；它不在任何 runtime 上下文里
            while let Some(cmd) = rx.blocking_recv() {
                let rec = match cmd {
                    // 数据型应答要回生效值，结果码回答不了「现在跑的是什么」
                    ControlCommand::Retier { request, reply } => {
                        let _ = if reject_change {
                            reply.send(Err(ControlStatus::ReconfigureFailed))
                        } else {
                            // 假 Actor 无会话常量可依：派生 codec 占位取 H264，
                            // 断言只看几何；真实行为由 encode/actor.rs 的用例覆盖
                            let profile = request.derive(VideoCodec::H264);
                            reply.send(Ok(profile))
                        };
                        Rec::Retier
                    }
                    ControlCommand::PrepareEncoder {
                        request,
                        codec,
                        preferred_backend,
                        reply,
                        ..
                    } => {
                        let profile = request.derive(codec);
                        let _ = reply.send(Ok((profile, codec, EncodeBackendKind::Software)));
                        Rec::Prepare(request, preferred_backend)
                    }
                    ControlCommand::ImportVramSlots { reply, .. } => {
                        let _ = reply.send(ControlStatus::Ok);
                        Rec::Other
                    }
                    ControlCommand::ClearVramSlots { reply } => {
                        let _ = reply.send(ControlStatus::Ok);
                        Rec::Other
                    }
                    ControlCommand::Shutdown => Rec::Other,
                    ControlCommand::RequestKeyframe => Rec::RequestKeyframe,
                    ControlCommand::BitrateChange(_) => Rec::SetBitrate,
                };
                if recorded_tx.send(rec).is_err() {
                    break;
                }
            }
        });

        let audio = AudioLoopback::new();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let pipeline = Arc::new(VideoPipeline::assemble(ControlSender::new(tx, bell), inbox));
        let mgr = Arc::new(StreamingEngine::new(
            pipeline,
            audio,
            "0.0.0.0:0".to_string(),
        ));
        (mgr, recorded_rx)
    }

    pub(crate) fn make_test_offer() -> RTCSessionDescription {
        let sdp = "v=0\r\n\
o=- 1495799811084970 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:testufrag\r\n\
a=ice-pwd:testpassword01234567890123\r\n\
a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtpmap:102 H264/90000\r\n";
        RTCSessionDescription::offer(sdp.to_string()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Rec, make_test_engine, make_test_offer};
    use super::*;
    use crate::encode::control::{ControlCommand, ControlSender};
    use crate::encode::encoder::EncodeBackendKind;
    use crate::encode::frame::FrameInbox;
    use std::time::Duration;
    use tokio::sync::Notify;

    /// 新会话接入必须替换旧会话，`close` 必须清空
    #[tokio::test]
    async fn test_single_session_lifecycle_and_preemption() {
        let (mgr, _rx) = make_test_engine(false);
        assert!(!mgr.active_session.load().is_some());

        let (answer1, profile1, _codec1, _backend1) = mgr
            .handle_offer(
                make_test_offer(),
                1920,
                1080,
                60,
                None,
                EncodePreferences::default(),
            )
            .await
            .unwrap();
        assert!(!answer1.sdp.is_empty());
        assert_eq!(profile1.width(), 1920);
        assert!(mgr.active_session.load().is_some());

        let session1 = mgr.active_session.load_full().expect("应已存在 session1");

        // 模拟新会话接入抢占 (Kick-Old)
        let (answer2, _, _, _) = mgr
            .handle_offer(
                make_test_offer(),
                1280,
                720,
                30,
                None,
                EncodePreferences::default(),
            )
            .await
            .unwrap();
        assert!(!answer2.sdp.is_empty());

        let session2 = mgr.active_session.load_full().expect("应已存在 session2");
        assert!(!Arc::ptr_eq(&session1, &session2), "旧会话已被新会话替换");
        assert!(mgr.active_session.load().is_some());

        // 主动关闭会话清理
        mgr.close_session().await;
        assert!(!mgr.active_session.load().is_some());
    }

    /// 不被 Engine→Session→PC→Handler→Engine 强环拖住
    #[tokio::test]
    async fn test_candidate_handler_does_not_pin_engine() {
        let (mgr, _rx) = make_test_engine(false);
        let weak = Arc::downgrade(&mgr);
        {
            let (session, _answer, _codec, _backend) = mgr
                .handle_offer(
                    make_test_offer(),
                    1920,
                    1080,
                    60,
                    None,
                    EncodePreferences::default(),
                )
                .await
                .unwrap();
            assert!(weak.upgrade().is_some(), "协商期间引擎必须存活");
            drop(session);
        }
        drop(mgr);
        assert!(weak.upgrade().is_none(), "候选闭包不得令引擎永生");
    }

    /// 安装结算：建立期间引擎被关闭（纪元换代）→ 会话就地自弃且不安装
    ///
    /// PrepareEncoder 的应答由测试放行，把协商确定性钉在闸外，给 close() 留出
    /// 插队窗口。钉死核心不变量：建好的 PC 绝不落进已关停的引擎（否则 PC 永持
    /// 媒体端口，后续所有协商 EADDRINUSE 直到进程重启）
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_install_superseded_when_engine_closed_during_establish() {
        // 假 Actor：收到 PrepareEncoder 先回报「协商已过入口」，再等放行才回 ACK
        let (entry_tx, entry_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ControlCommand>(16);
        std::thread::spawn(move || {
            while let Some(cmd) = rx.blocking_recv() {
                match cmd {
                    ControlCommand::PrepareEncoder {
                        request,
                        codec,
                        reply,
                        ..
                    } => {
                        let _ = entry_tx.send(());
                        let _ = release_rx.recv();
                        let profile = request.derive(codec);
                        let _ = reply.send(Ok((profile, codec, EncodeBackendKind::Software)));
                    }
                    ControlCommand::Shutdown => break,
                    _ => {}
                }
            }
        });
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let pipeline = Arc::new(VideoPipeline::assemble(ControlSender::new(tx, bell), inbox));
        let mgr = Arc::new(StreamingEngine::new(
            pipeline,
            AudioLoopback::new(),
            "0.0.0.0:0".to_string(),
        ));

        let offer_task = {
            let mgr = Arc::clone(&mgr);
            tokio::spawn(async move {
                mgr.handle_offer(
                    make_test_offer(),
                    640,
                    360,
                    30,
                    None,
                    EncodePreferences::default(),
                )
                .await
            })
        };
        // 确定性同步：协商已过入口（纪元已快照）并停在 prepare ask 上
        entry_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("协商必须抵达 prepare ask");

        // 插队关闭：纪元换代
        mgr.close_session().await;
        assert!(!mgr.active_session.load().is_some());

        // 放行协商：建立照常完成，但安装结算按过期纪元就地自弃
        release_tx.send(()).expect("放行协商");
        let result = offer_task.await.expect("协商任务不得 panic");
        assert!(
            result.is_err(),
            "建立期间引擎已关闭，安装必须自弃而不是落入孤儿引擎"
        );
        assert!(!mgr.active_session.load().is_some(), "自弃路径不得留下会话");
    }

    /// 内容驱动档位：硬件路径按请求档位生效；软编路径退化为 min(Content, 1080p)@60。
    /// 请求只承载几何/bpp；codec 由偏好独立解析（见 `test_resolve_codec_with_sdp`）
    #[test]
    fn test_negotiate_profile_codec_hierarchy() {
        let offer_av1 = RTCSessionDescription::offer(
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 AV1/90000\r\n".to_string()
        ).unwrap();
        let av1_probe = BackendCapability {
            backend_type: EncodeBackendKind::Nvenc,
            supported_codecs: vec![VideoCodec::Av1, VideoCodec::H264],
            gpu_name: String::new(),
        };

        // 硬件路径（offer 提供 AV1 且硬件支持 → auto 首选 AV1）：请求按内容档位生效
        let codec =
            resolve_codec(Some(&offer_av1), Some(&av1_probe), None).expect("可行偏好必须成功");
        assert_eq!(codec, VideoCodec::Av1);
        let r = resolve_initial_request(1920, 1080, 60, None, Some(&av1_probe));
        assert_eq!((r.width, r.height, r.fps), (1920, 1080, 60));

        // 软编钳制只缩不放：大内容缩进 1080p，小内容原样保留
        let r = resolve_initial_request(3840, 2160, 240, None, None);
        assert_eq!((r.width, r.height, r.fps), (1920, 1080, 60));
        let r = resolve_initial_request(800, 600, 90, None, None);
        assert_eq!((r.width, r.height, r.fps), (800, 600, 60));

        // offer 携带的 bpp 进入请求
        let r = resolve_initial_request(1920, 1080, 60, Some(0.2), Some(&av1_probe));
        assert_eq!(r.bpp, 0.2);
    }

    /// 档位 ask 事务：成功返回实际档位、无会话与 0 几何/帧率显式拒绝。
    /// 请求不携带 codec——它是协商定死的会话常量
    #[tokio::test]
    async fn test_apply_profile_ask_transaction() {
        let (mgr, _rx) = make_test_engine(false);

        // 无活跃会话：NoSession
        assert!(matches!(
            mgr.apply_profile(1920, 1080, 60, 0.2).await,
            ProfileApply::NoSession
        ));

        mgr.handle_offer(
            make_test_offer(),
            1920,
            1080,
            60,
            None,
            EncodePreferences::default(),
        )
        .await
        .unwrap();

        // 成功路径：假 Actor 回显请求档位
        let ProfileApply::Applied(applied) = mgr.apply_profile(1600, 900, 90, 0.2).await else {
            panic!("活跃会话上的合法请求必须生效");
        };
        assert_eq!(
            (applied.width(), applied.height(), applied.fps()),
            (1600, 900, 90)
        );

        // 0 几何（暂停信号）按拒绝处理，不捏造最小档位
        let ProfileApply::Rejected = mgr.apply_profile(0, 0, 60, 0.2).await else {
            panic!("0 几何必须被拒绝");
        };

        // 帧率为 0 同样拒绝
        assert!(matches!(
            mgr.apply_profile(1600, 900, 0, 0.2).await,
            ProfileApply::Rejected
        ));
    }

    /// codec 协商语义：指定的偏好是硬约束，必须在「offer 实际提供 ∩ 服务端支持」内
    /// 满足，不可行显式拒绝；auto 的档位序（AV1 > H265 > H264）由服务端定死，与
    /// 浏览器载荷序无关
    #[test]
    fn test_resolve_codec_with_sdp() {
        use crate::encode::BackendCapability;

        let fake_probe = BackendCapability {
            backend_type: EncodeBackendKind::Nvenc,
            supported_codecs: vec![VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264],
            gpu_name: String::new(),
        };
        let h264_only_probe = BackendCapability {
            supported_codecs: vec![VideoCodec::H264],
            ..fake_probe.clone()
        };

        let make_offer = |sdp: &str| -> RTCSessionDescription {
            let full_sdp = if !sdp.starts_with("v=") {
                format!(
                    "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n{}\r\n",
                    sdp
                )
            } else {
                sdp.to_string()
            };
            RTCSessionDescription::offer(full_sdp).unwrap()
        };

        // 1. 无硬件 + auto：软编唯一可行域是 H.264
        let offer_all = make_offer(
            "m=video 9 UDP/TLS/RTP/SAVPF 96 104 102\r\na=rtpmap:96 AV1/90000\r\na=rtpmap:104 H265/90000\r\na=rtpmap:102 H264/90000",
        );
        assert_eq!(
            resolve_codec(Some(&offer_all), None, None),
            Ok(VideoCodec::H264)
        );

        // 2. 无硬件 + 指定 AV1：软编机无法满足就显式拒绝，绝不悄悄换 H.264
        assert!(resolve_codec(Some(&offer_all), None, Some(VideoCodec::Av1)).is_err());
        // 指定 H.264 在软编机上可行，照常采纳
        assert_eq!(
            resolve_codec(Some(&offer_all), None, Some(VideoCodec::H264)),
            Ok(VideoCodec::H264)
        );

        // 3. auto：默认档位按可行性取首个，浏览器全提供时首选 AV1
        assert_eq!(
            resolve_codec(Some(&offer_all), Some(&fake_probe), None),
            Ok(VideoCodec::Av1)
        );

        // 4. auto：浏览器只提供 H.264（老浏览器解不动 AV1/H.265）→ 默认档位跳过不可行项
        let offer_h264_only =
            make_offer("m=video 9 UDP/TLS/RTP/SAVPF 102\r\na=rtpmap:102 H264/90000");
        assert_eq!(
            resolve_codec(Some(&offer_h264_only), Some(&fake_probe), None),
            Ok(VideoCodec::H264)
        );

        // 5. 显式偏好可行：offer 提供 + 硬件支持 → 直接采纳（即使不是默认档首选）
        assert_eq!(
            resolve_codec(Some(&offer_all), Some(&fake_probe), Some(VideoCodec::H265)),
            Ok(VideoCodec::H265)
        );

        // 6. 显式偏好在浏览器侧不可行（offer 未提供 AV1）→ 显式拒绝
        assert!(
            resolve_codec(
                Some(&offer_h264_only),
                Some(&fake_probe),
                Some(VideoCodec::Av1)
            )
            .is_err()
        );

        // 7. 显式偏好在硬件侧不可行（硬件不支持 AV1）→ 显式拒绝
        assert!(
            resolve_codec(
                Some(&offer_all),
                Some(&h264_only_probe),
                Some(VideoCodec::Av1)
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn test_push_audio_sample_pipeline() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<ControlCommand>(16);
        let audio = AudioLoopback::new();
        let mut audio_rx = audio.attach_with_capacity(16);
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let pipeline = Arc::new(VideoPipeline::assemble(ControlSender::new(tx, bell), inbox));
        let engine = Arc::new(StreamingEngine::new(
            pipeline,
            audio,
            "0.0.0.0:0".to_string(),
        ));

        // 构造静音 PCM 帧 (1920 个采样点)
        let silence = vec![0i16; crate::encode::AUDIO_TOTAL_SAMPLES];
        engine.audio().push_sample(&silence);

        // 验证音频包成功进入队列
        let packet = audio_rx.recv().await.expect("应该从 audio_rx 收到音频包");
        assert!(!packet.data.is_empty());
        assert_eq!(
            packet.duration,
            Duration::from_millis(crate::encode::AUDIO_FRAME_DURATION_MS)
        );
    }

    #[tokio::test]
    async fn test_audio_queue_overflow_drop_tail() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<ControlCommand>(16);
        let audio = AudioLoopback::new();
        let mut audio_rx = audio.attach_with_capacity(2);
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let pipeline = Arc::new(VideoPipeline::assemble(ControlSender::new(tx, bell), inbox));
        let mgr = Arc::new(StreamingEngine::new(
            pipeline,
            audio,
            "0.0.0.0:0".to_string(),
        ));

        let silence = vec![0i16; crate::encode::AUDIO_TOTAL_SAMPLES];
        mgr.audio().push_sample(&silence);
        mgr.audio().push_sample(&silence);
        // 第 3 帧：此时队列已满 (容量为 2)，应当被静默丢弃，不发生 panic 也不阻塞
        mgr.audio().push_sample(&silence);

        // 验证队列里只有前两帧
        assert!(audio_rx.recv().await.is_some());
        assert!(audio_rx.recv().await.is_some());
        // 尝试非阻塞收取第三帧，应该为空
        assert!(audio_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_handle_offer_leaves_keyframe_arming_to_session() {
        let (mgr, rx) = make_test_engine(false);
        let _ = mgr
            .handle_offer(
                make_test_offer(),
                1920,
                1080,
                60,
                None,
                EncodePreferences::default(),
            )
            .await
            .unwrap();
        // 引擎不替会话请求关键帧：waiting 自愈归会话写循环的节流路径。
        // 假 Actor 线程仍持发送端，接收端只能 try_recv 排空，不得用阻塞迭代
        let mut saw_request = false;
        while let Ok(rec) = rx.try_recv() {
            if matches!(rec, Rec::RequestKeyframe) {
                saw_request = true;
            }
        }
        assert!(!saw_request, "establish 不得隐式投递关键帧命令");
    }

    /// 会话建立必须是一条原子命令，而不是若干条互不等待的命令
    ///
    /// 这条用例钉的是唯一命令约束：会话建立对控制面只派发一条带 ACK 的
    /// `PrepareEncoder`，「编码器已按档位就绪」必须是调用方可等待的事实，
    /// 而不是若干条互不等待命令的隐式产物。
    #[tokio::test]
    async fn test_new_session_dispatches_single_prepare_encoder() {
        let (mgr, rx) = make_test_engine(false);
        let _ = mgr
            .handle_offer(
                make_test_offer(),
                1920,
                1080,
                60,
                None,
                EncodePreferences::default(),
            )
            .await
            .unwrap();

        let mut prepare_count = 0;
        while let Ok(rec) = rx.try_recv() {
            match rec {
                Rec::Prepare(request, preferred) => {
                    assert_eq!(request.width, 1920);
                    assert_eq!(request.height, 1080);
                    assert_eq!(request.fps, 60);
                    assert_eq!(preferred, None, "本用例不带后端偏好，缝上必须按无偏好送达");
                    prepare_count += 1;
                }
                // 建立路径上不该再出现任何零散命令：档位与初始码率都由
                // PrepareEncoder 内部一次设定，关键帧请求只属于会话写循环
                Rec::Retier | Rec::SetBitrate | Rec::RequestKeyframe => {
                    panic!("会话建立路径出现了本应被 PrepareEncoder 取代的命令");
                }
                Rec::Other => {}
            }
        }
        assert_eq!(
            prepare_count, 1,
            "每次会话建立必须恰好派发一条 PrepareEncoder"
        );
    }
}

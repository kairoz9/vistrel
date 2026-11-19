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

use crate::encode::HardwareProbeResult;
use crate::encode::StreamingProfile;
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

/// 档位 ask 的应答（被动数据载体）：只有生效值能被回传
pub enum ProfileApply {
    /// 请求已被编码器接受并生效
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
    /// 几何与帧率来自游戏内容快照（Java 侧已合并视口上限），bpp 来自浏览器；会话对
    /// Java 的启停通知已删除，offer 成功即 Java 信令层自持的「活跃」起点。
    pub async fn handle_offer(
        self: &Arc<Self>,
        offer: RTCSessionDescription,
        width: usize,
        height: usize,
        fps: usize,
        bpp: Option<f32>,
    ) -> Result<(RTCSessionDescription, StreamingProfile), Box<dyn std::error::Error + Send + Sync>>
    {
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

        let probe = crate::encode::hardware_caps();
        let profile = resolve_initial_profile(width, height, fps, bpp, &offer, probe.as_deref());

        if let Err(e) = self.video.prepare_encoder(profile).await {
            return Err(format!("编码器未能在会话建立前就绪 (code={})", e.code()).into());
        }

        let handler = self.candidate_handler();

        let (session, answer) =
            StreamingSession::establish(self.as_ref(), offer, profile, handler).await?;

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

        Ok((answer, profile))
    }

    /// 应用新的画质档位：阻塞 ask，应答见 [`ProfileApply`]
    ///
    /// 这是档位的唯一写路径。
    pub async fn apply_profile(
        &self,
        width: usize,
        height: usize,
        fps: usize,
        bpp: f32,
        codec: VideoCodec,
    ) -> ProfileApply {
        let Some(session) = self.active_session.load_full() else {
            log::warn!("无活跃会话，档位请求被拒绝");
            return ProfileApply::NoSession;
        };
        if width == 0 || height == 0 || fps == 0 {
            // 0×0 是暂停信号：不是需要「修正」的最小档位，直接拒绝
            return ProfileApply::Rejected;
        }
        let requested = StreamingProfile::new_with_bpp(width, height, fps, Some(bpp), codec);
        // 控制面所有权在引擎：ask 直接发起，生效码率回填会话的码率回路；
        // 拒绝路径不经过回路，上界自然保持旧值
        match self.video.control().change_profile(requested).await {
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

/// 结合 SDP 格式支持、内容快照与硬件探测协商初始 Profile
///
/// 探测快照由调用方注入：生产路径读全局快照，单测注入假结果，避免触碰真实驱动。
/// 无探测快照即软编：内容驱动档位退化为 `min(Content Resolution, 1080p) @ 60`，
/// 只缩不放——把 800×600 抬到 1080p 发的是假细节。
fn resolve_initial_profile(
    width: usize,
    height: usize,
    fps: usize,
    bpp: Option<f32>,
    offer: &RTCSessionDescription,
    probe: Option<&HardwareProbeResult>,
) -> StreamingProfile {
    let codec = resolve_codec(Some(offer), probe);
    let profile = StreamingProfile::new_with_bpp(width, height, fps, bpp, codec);
    if probe.is_none() {
        let (w, h, f) = crate::encode::clamp_to_software_ceiling(
            profile.width(),
            profile.height(),
            profile.fps(),
        );
        if (w, h, f) != (profile.width(), profile.height(), profile.fps()) {
            log::warn!("软编回退：初始档位钳制为 {w}x{h}@{f}");
        }
        return StreamingProfile::new_with_bpp(w, h, f, Some(profile.bpp()), codec);
    }
    profile
}

/// 从 RTCSessionDescription 实例直接解析客户端偏好的视频编解码器列表
fn parse_sdp_codec_preferences(offer: &RTCSessionDescription) -> Option<Vec<VideoCodec>> {
    let desc = offer.unmarshal().ok()?;
    // 只取第一个 video 媒体行
    let media = desc
        .media_descriptions
        .into_iter()
        .find(|m| m.media_name.media == "video")?;

    // pt 顺序即浏览器偏好顺序
    let preferred_pts = media.media_name.formats;

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

    // 按 pt 顺序去重
    let mut seen = HashSet::new();
    let preferences: Vec<VideoCodec> = preferred_pts
        .into_iter()
        .filter_map(|pt| pt_to_codec.get(&pt).copied())
        .filter(|codec| seen.insert(*codec))
        .collect();

    (!preferences.is_empty()).then_some(preferences)
}

/// 根据硬件支持列表及 RTCSessionDescription 实例中的偏好顺序协商出编解码器
fn resolve_codec(
    offer: Option<&RTCSessionDescription>,
    probe: Option<&HardwareProbeResult>,
) -> VideoCodec {
    let has_hw = probe.is_some();
    let supported_codecs = probe.map(|p| p.supported_codecs.as_slice()).unwrap_or(&[]);

    // 无硬件编码器时，恒回退至软编支持的 H.264
    if !has_hw {
        return VideoCodec::H264;
    }

    let default_preferences = vec![VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264];
    // 从 RTCSessionDescription 提取客户端偏好顺序列表
    let client_preferences = if let Some(offer) = offer {
        parse_sdp_codec_preferences(offer).unwrap_or(default_preferences)
    } else {
        default_preferences
    };

    // 按照客户端从高到低的偏好，找到第一个被服务端硬件所支持的编解码器
    for candidate in client_preferences {
        if candidate == VideoCodec::H264 || supported_codecs.contains(&candidate) {
            return candidate;
        }
    }

    // 兜底回退
    VideoCodec::H264
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::KeyframeTrigger;
    use crate::encode::control::{ControlCommand, ControlSender, ControlStatus};
    use crate::encode::encoder::EncodeBackendKind;
    use crate::encode::frame::FrameInbox;
    use std::time::Duration;
    use tokio::sync::Notify;

    /// 假 Actor 转发出来的命令摘要：只保留断言要看的两个事实，避免用字符串当命令名
    #[derive(Debug, Clone, Copy)]
    enum Rec {
        Prepare(StreamingProfile),
        ChangeProfile,
        SetBitrate,
        Other,
    }

    /// 造一个引擎，并配一个**假编码线程**
    ///
    /// `handle_offer` 现在要等 `PrepareEncoder` 的 ACK，没有应答者就会走满 5s 超时，
    /// 所有会话用例都会退化成超时失败。假线程只回 `Ok`、不碰编码器，同时把收到的命令
    /// 转发出来供断言——真正的编码行为由 `encode/actor.rs` 的用例覆盖。
    /// `reject_change` 让档位重配一律被拒（回传 `ReconfigureFailed`），供拒绝路径用例。
    fn make_test_engine(
        reject_change: bool,
    ) -> (
        Arc<StreamingEngine>,
        std::sync::mpsc::Receiver<Rec>,
        KeyframeTrigger,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ControlCommand>(16);
        let (recorded_tx, recorded_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // 假 Actor 跑在普通线程上，用阻塞接收；它不在任何 runtime 上下文里
            while let Some(cmd) = rx.blocking_recv() {
                let rec = match cmd {
                    // 数据型应答要回档位，结果码回答不了「现在跑的是什么」
                    ControlCommand::ProfileChange { profile, reply } => {
                        let _ = if reject_change {
                            reply.send(Err(ControlStatus::ReconfigureFailed))
                        } else {
                            reply.send(Ok(profile))
                        };
                        Rec::ChangeProfile
                    }
                    ControlCommand::PrepareEncoder { profile, reply, .. } => {
                        let _ = reply.send(ControlStatus::Ok);
                        Rec::Prepare(profile)
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
                    ControlCommand::BitrateChange(_) => Rec::SetBitrate,
                };
                if recorded_tx.send(rec).is_err() {
                    break;
                }
            }
        });

        let audio = AudioLoopback::new();
        let keyframe = KeyframeTrigger::new();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let pipeline = Arc::new(VideoPipeline::assemble(
            ControlSender::new(tx, bell, true),
            Arc::new(keyframe.clone()),
            inbox,
        ));
        let mgr = Arc::new(StreamingEngine::new(
            pipeline,
            audio,
            "0.0.0.0:0".to_string(),
        ));
        (mgr, recorded_rx, keyframe)
    }

    fn make_test_offer() -> RTCSessionDescription {
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

    /// 新会话接入必须替换旧会话，`close` 必须清空
    #[tokio::test]
    async fn test_single_session_lifecycle_and_preemption() {
        let (mgr, _rx, _kf) = make_test_engine(false);
        assert!(!mgr.active_session.load().is_some());

        let (answer1, profile1) = mgr
            .handle_offer(make_test_offer(), 1920, 1080, 60, None)
            .await
            .unwrap();
        assert!(!answer1.sdp.is_empty());
        assert_eq!(profile1.width(), 1920);
        assert!(mgr.active_session.load().is_some());

        let session1 = mgr.active_session.load_full().expect("应已存在 session1");

        // 模拟新会话接入抢占 (Kick-Old)
        let (answer2, _) = mgr
            .handle_offer(make_test_offer(), 1280, 720, 30, None)
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
        let (mgr, _rx, _kf) = make_test_engine(false);
        let weak = Arc::downgrade(&mgr);
        {
            let (session, _answer) = mgr
                .handle_offer(make_test_offer(), 1920, 1080, 60, None)
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
                    ControlCommand::PrepareEncoder { reply, .. } => {
                        let _ = entry_tx.send(());
                        let _ = release_rx.recv();
                        let _ = reply.send(ControlStatus::Ok);
                    }
                    ControlCommand::Shutdown => break,
                    _ => {}
                }
            }
        });
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let pipeline = Arc::new(VideoPipeline::assemble(
            ControlSender::new(tx, bell, true),
            Arc::new(KeyframeTrigger::new()),
            inbox,
        ));
        let mgr = Arc::new(StreamingEngine::new(
            pipeline,
            AudioLoopback::new(),
            "0.0.0.0:0".to_string(),
        ));

        let offer_task = {
            let mgr = Arc::clone(&mgr);
            tokio::spawn(async move {
                mgr.handle_offer(make_test_offer(), 640, 360, 30, None)
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

    /// 内容驱动档位：硬件路径按请求档位生效；软编路径退化为 min(Content, 1080p)@60
    #[test]
    fn test_negotiate_profile_codec_hierarchy() {
        let offer_h264 = RTCSessionDescription::offer(
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\na=rtpmap:102 H264/90000\r\n".to_string()
        ).unwrap();
        let offer_av1 = RTCSessionDescription::offer(
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 AV1/90000\r\n".to_string()
        ).unwrap();
        let av1_probe = HardwareProbeResult {
            backend_type: EncodeBackendKind::Nvenc,
            supported_codecs: vec![VideoCodec::Av1, VideoCodec::H264],
            coded_align: (1, 1),
            gpu_name: String::new(),
        };
        let h264_probe = HardwareProbeResult {
            supported_codecs: vec![VideoCodec::H264],
            ..av1_probe.clone()
        };

        // H.264 offer 不受硬件能力影响
        let p = resolve_initial_profile(1920, 1080, 60, None, &offer_h264, Some(&av1_probe));
        assert_eq!(p.codec(), VideoCodec::H264);
        assert_eq!((p.width(), p.height(), p.fps()), (1920, 1080, 60));

        // 硬件支持 AV1 时 AV1 offer 升为 AV1
        let p = resolve_initial_profile(1920, 1080, 60, None, &offer_av1, Some(&av1_probe));
        assert_eq!(p.codec(), VideoCodec::Av1);

        // 硬件不支持 AV1 或无硬件时回退 H.264
        let p = resolve_initial_profile(1280, 720, 30, None, &offer_av1, Some(&h264_probe));
        assert_eq!(p.codec(), VideoCodec::H264);
        let p = resolve_initial_profile(1280, 720, 30, None, &offer_av1, None);
        assert_eq!(p.codec(), VideoCodec::H264);

        // 软编钳制只缩不放：大内容缩进 1080p，小内容原样保留
        let p = resolve_initial_profile(3840, 2160, 240, None, &offer_av1, None);
        assert_eq!((p.width(), p.height(), p.fps()), (1920, 1080, 60));
        let p = resolve_initial_profile(800, 600, 90, None, &offer_av1, None);
        assert_eq!((p.width(), p.height(), p.fps()), (800, 600, 60));

        // offer 携带的 bpp 进入档位
        let p = resolve_initial_profile(1920, 1080, 60, Some(0.2), &offer_h264, Some(&av1_probe));
        assert_eq!(p.bpp(), 0.2);
    }

    /// 档位 ask 事务：成功返回实际档位、无会话与 0 几何/帧率显式拒绝
    #[tokio::test]
    async fn test_apply_profile_ask_transaction() {
        let (mgr, _rx, _kf) = make_test_engine(false);
        let codec = VideoCodec::H264;

        // 无活跃会话：NoSession
        assert!(matches!(
            mgr.apply_profile(1920, 1080, 60, 0.2, codec).await,
            ProfileApply::NoSession
        ));

        mgr.handle_offer(make_test_offer(), 1920, 1080, 60, None)
            .await
            .unwrap();

        // 成功路径：假 Actor 回显请求档位，codec 随请求原样带出
        let ProfileApply::Applied(applied) = mgr.apply_profile(1600, 900, 90, 0.2, codec).await
        else {
            panic!("活跃会话上的合法请求必须生效");
        };
        assert_eq!(
            (applied.width(), applied.height(), applied.fps()),
            (1600, 900, 90)
        );
        assert_eq!(applied.bpp(), 0.2);
        assert_eq!(applied.codec(), codec);

        // 0 几何（暂停信号）按拒绝处理，不捏造最小档位
        let ProfileApply::Rejected = mgr.apply_profile(0, 0, 60, 0.2, codec).await else {
            panic!("0 几何必须被拒绝");
        };

        // 帧率为 0 同样拒绝
        assert!(matches!(
            mgr.apply_profile(1600, 900, 0, 0.2, codec).await,
            ProfileApply::Rejected
        ));
    }

    #[test]
    fn test_resolve_codec_with_sdp() {
        use crate::encode::HardwareProbeResult;

        let fake_probe = HardwareProbeResult {
            backend_type: EncodeBackendKind::Nvenc,
            supported_codecs: vec![VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264],
            coded_align: (1, 1),
            gpu_name: String::new(),
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

        // 1. 无硬件时，恒回退 H.264
        let offer_av1_only = make_offer("m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 AV1/90000");
        assert_eq!(resolve_codec(Some(&offer_av1_only), None), VideoCodec::H264);

        // 2. 客户端偏好首选 AV1 (PT 96 优先)，且硬件支持 AV1 -> 胜出 AV1
        let offer_prefer_av1 = make_offer(
            "m=video 9 UDP/TLS/RTP/SAVPF 96 102\r\na=rtpmap:96 AV1/90000\r\na=rtpmap:102 H264/90000",
        );
        assert_eq!(
            resolve_codec(Some(&offer_prefer_av1), Some(&fake_probe)),
            VideoCodec::Av1
        );

        // 3. 客户端偏好首选 H264 (PT 102 优先)，即使硬件支持 AV1，也严格按偏好选 H.264
        let offer_prefer_h264 = make_offer(
            "m=video 9 UDP/TLS/RTP/SAVPF 102 96\r\na=rtpmap:102 H264/90000\r\na=rtpmap:96 AV1/90000",
        );
        assert_eq!(
            resolve_codec(Some(&offer_prefer_h264), Some(&fake_probe)),
            VideoCodec::H264
        );

        // 4. 客户端偏好首选 AV1，但服务端硬件仅支持 H264/H265 (不支持 AV1) -> 顺位回退至其第二个偏好 H.265
        let fake_probe_no_av1 = HardwareProbeResult {
            backend_type: EncodeBackendKind::Nvenc,
            supported_codecs: vec![VideoCodec::H265, VideoCodec::H264],
            coded_align: (1, 1),
            gpu_name: String::new(),
        };
        let offer_av1_then_h265 = make_offer(
            "m=video 9 UDP/TLS/RTP/SAVPF 96 104 102\r\na=rtpmap:96 AV1/90000\r\na=rtpmap:104 H265/90000\r\na=rtpmap:102 H264/90000",
        );
        assert_eq!(
            resolve_codec(Some(&offer_av1_then_h265), Some(&fake_probe_no_av1)),
            VideoCodec::H265
        );
    }

    #[tokio::test]
    async fn test_push_audio_sample_pipeline() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<ControlCommand>(16);
        let audio = AudioLoopback::new();
        let mut audio_rx = audio.attach_with_capacity(16);
        let keyframe = KeyframeTrigger::new();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let pipeline = Arc::new(VideoPipeline::assemble(
            ControlSender::new(tx, bell, true),
            Arc::new(keyframe.clone()),
            inbox,
        ));
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
        let keyframe = KeyframeTrigger::new();
        let bell = Arc::new(Notify::new());
        let inbox = Arc::new(FrameInbox::new(Arc::clone(&bell)));
        let pipeline = Arc::new(VideoPipeline::assemble(
            ControlSender::new(tx, bell, true),
            Arc::new(keyframe.clone()),
            inbox,
        ));
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
        let (mgr, _rx, kf) = make_test_engine(false);
        // 初值消费掉，证明 establish 流程不存在任何隐式置位
        assert!(kf.take(), "初值必须为 true");
        assert!(!kf.take(), "消费后必须为 false");

        let _ = mgr
            .handle_offer(make_test_offer(), 1920, 1080, 60, None)
            .await
            .unwrap();
        // 引擎不替会话请求关键帧：waiting 自愈归 send_video 的丢弃路径
        assert!(!kf.take(), "handle_offer 不得触碰关键帧旗标");
    }

    /// 会话建立必须是一条原子命令，而不是若干条互不等待的命令
    ///
    /// 这条用例钉的是唯一命令约束：会话建立对控制面只派发一条带 ACK 的
    /// `PrepareEncoder`，「编码器已按档位就绪」必须是调用方可等待的事实，
    /// 而不是若干条互不等待命令的隐式产物。
    #[tokio::test]
    async fn test_new_session_dispatches_single_prepare_encoder() {
        let (mgr, rx, _kf) = make_test_engine(false);
        let _ = mgr
            .handle_offer(make_test_offer(), 1920, 1080, 60, None)
            .await
            .unwrap();

        let mut prepare_count = 0;
        while let Ok(rec) = rx.try_recv() {
            match rec {
                Rec::Prepare(profile) => {
                    assert_eq!(profile.width(), 1920);
                    assert_eq!(profile.height(), 1080);
                    assert_eq!(profile.fps(), 60);
                    prepare_count += 1;
                }
                // 建立路径上不该再出现任何零散命令：档位与初始码率都由
                // PrepareEncoder 内部一次设定
                Rec::ChangeProfile | Rec::SetBitrate => {
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

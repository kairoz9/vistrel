use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use super::cancellation::CancellationToken;
use async_trait::async_trait;
use parking_lot::Mutex;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_HEVC, MediaEngine,
};
use rtc::peer_connection::configuration::setting_engine::SettingEngineBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RTCRtpHeaderExtensionCapability, RtpCodecKind,
};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::MediaStreamTrack;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceCandidateInit,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};

use crate::encode::{StreamingProfile, VideoCodec};
use crate::streaming::input::InputRingBuffer;
use crate::streaming::rate_control::RateControl;
use crate::streaming::transport::{self, KeyframeThrottle, VIDEO_CLOCK_RATE};
use crate::streaming::writers::AudioWriter;
use crate::streaming::writers::VideoWriter;

/// 本端 ICE 候选上抛出口 (trickle)：入参是 `RTCIceCandidateInit` 的 JSON
pub type IceCandidateSink = Arc<dyn Fn(String) + Send + Sync>;

/// WebRTC 事件处理器
///
/// 绑定只在会话装配末尾发生一次，之后各事件回调并发只读，因此用 `OnceLock` 而非锁
pub(crate) struct SessionHandler {
    session: OnceLock<Weak<StreamingSession>>,
    candidate_sink: IceCandidateSink,
}

impl SessionHandler {
    /// 装配候选 trickle 出口
    pub(super) fn new(on_candidate: IceCandidateSink) -> Self {
        Self {
            session: OnceLock::new(),
            candidate_sink: on_candidate,
        }
    }
}
#[async_trait]
impl PeerConnectionEventHandler for SessionHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(candidate) = event.candidate.to_json()
            && let Ok(json) = serde_json::to_string(&candidate)
        {
            (self.candidate_sink)(json);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let session = self.session.get().and_then(Weak::upgrade);
        if let Some(session) = session {
            session.on_connection_state_change(state);
        }
    }

    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let session = self.session.get().and_then(Weak::upgrade);
        if let Some(session) = session {
            session.handle_data_channel(dc);
        }
    }
}

/// 会话关闭总预算：写者排空（500ms，一次在途写之内）+ Peer 关闭收尾余量。
/// 预算住在 [`StreamingSession::close`]
const SESSION_CLOSE_BUDGET: Duration = Duration::from_millis(1000);

/// 关闭会话时等待写循环退出的上限：一次在途写（250ms）之内必须收尾
const WRITER_JOIN_TIMEOUT: Duration = Duration::from_millis(500);

/// 会话收尾执行体：写者先排空、Peer 后关闭
fn spawn_janitor(
    pc: Arc<dyn PeerConnection>,
    cancel: CancellationToken,
    writers: Vec<(&'static str, tokio::task::JoinHandle<()>)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        cancel.cancelled().await;
        let deadline = tokio::time::Instant::now() + WRITER_JOIN_TIMEOUT;
        for (name, writer) in writers {
            if tokio::time::timeout_at(deadline, writer).await.is_err() {
                log::warn!("{name} 写入任务在 {WRITER_JOIN_TIMEOUT:?} 内未退出，已放弃等待");
            }
        }
        let _ = pc.close().await;
    })
}

/// 统一的推流会话 (StreamingSession)
///
/// 拥有单个远程客户端完整的生命周期状态、WebRTC 传输、输入分发、画质协商与控制面。
///
/// 媒体侧的样本适配（时序、关键帧门控、节流）不在会话内：它们挂在会话的 `VideoWriter` 上，
/// 随会话建立、随会话关闭消失，会话本身只管传输、输入与码率回路。档位不进会话：
/// 请求与应答都归引擎（`StreamingEngine::apply_profile`），会话只承接生效码率作为回路上界。
pub(crate) struct StreamingSession {
    pc: Arc<dyn PeerConnection>,
    state: Arc<AtomicU8>,
    rate: Arc<RateControl>,
    cancel: CancellationToken,
    input_ring: InputRingBuffer,
    /// 收尾执行体句柄：close() 取走并等待其完成；close 缺席时由 Drop 的
    /// cancel 唤醒，端口释放与写者收编不随会话丢弃而失联
    janitor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for StreamingSession {
    fn drop(&mut self) {
        self.state
            .store(RTCPeerConnectionState::Closed as u8, Ordering::Release);
        self.cancel.cancel();
    }
}

/// 会话装配产物：媒体引擎 + 视频/音频编码参数 + 各自生效的 payload type
type MediaEngineBundle = (
    MediaEngine,
    RTCRtpCodecParameters,
    u8,
    RTCRtpCodecParameters,
    u8,
);

impl StreamingSession {
    /// 建立 WebRTC 传输并初始化完整会话
    pub async fn establish(
        engine: &super::StreamingEngine,
        offer: RTCSessionDescription,
        profile: StreamingProfile,
        handler: Arc<SessionHandler>,
    ) -> Result<(Arc<Self>, RTCSessionDescription), Box<dyn std::error::Error + Send + Sync>> {
        let state = Arc::new(AtomicU8::new(RTCPeerConnectionState::New as u8));
        let cancel = CancellationToken::new();
        let throttle = KeyframeThrottle::new(engine.video().control().clone());
        let outbox = engine.video().output();
        // 码率回路直连控制面：GCC 估计经 RateControl 钳位后派发给编码器，
        // 发送方克隆自引擎的控制面（控制面所有权在引擎，会话不另持一份）
        let rate = Arc::new(RateControl::new(profile.bitrate() as u32, {
            let control = engine.video().control().clone();
            move |bps: u32| {
                control.set_bitrate(bps as usize);
            }
        }));
        let udp_bind: &str = &engine.udp_bind;

        // 1. 装配 MediaEngine 与音视频编解码规格（档位按值流入，会话不留副本）
        let (media_engine, video_codec, primary_pt, audio_codec, primary_audio_pt) =
            Self::build_media_engine(
                profile.codec(),
                crate::encode::MAX_WIDTH,
                crate::encode::MAX_HEIGHT,
                crate::encode::sdp_ceiling_fps(),
                crate::encode::stream_max_bitrate_kbps(profile.codec()),
            )?;

        // 2. 创建本地发送视频轨与音频轨 (TrackLocalStaticSample)
        let video_track = Self::create_video_track(&video_codec)?;
        let audio_track = Self::create_audio_track(&audio_codec)?;

        // 3. 构建 PeerConnection（Handler 由调用方装配，弱引用回绑在末尾完成）
        let pc =
            Self::build_peer_connection(media_engine, &rate, &throttle, handler.clone(), udp_bind)
                .await?;
        // PC 建成即已绑定媒体端口：此后任一步失败都必须显式关闭再返回
        let negotiated = async {
            pc.set_remote_description(offer).await?;
            // 4. 执行 SDP 协商
            Self::negotiate_sdp(
                pc.clone(),
                video_track.clone(),
                audio_track.clone(),
                profile.codec(),
                primary_pt,
                primary_audio_pt,
            )
            .await
        }
        .await;
        let (final_answer, payload_type, audio_payload_type) = match negotiated {
            Ok(ok) => ok,
            Err(e) => {
                let _ = pc.close().await;
                return Err(e);
            }
        };

        // 5. 两个写者同生共死：构造即完整、按值被各自写循环消费
        //    （视频取发件箱，音频取会话队列）；同一个取消信号，同一个收编点
        let video_writer = tokio::spawn(
            VideoWriter::new(
                video_track,
                payload_type,
                Arc::clone(&state),
                throttle,
                outbox,
                cancel.clone(),
            )
            .write_loop(),
        );
        let audio_writer = tokio::spawn(
            AudioWriter::new(
                audio_track,
                audio_payload_type,
                Arc::clone(&state),
                engine.audio().attach(),
                cancel.clone(),
            )
            .write_loop(),
        );

        let session = Arc::new(Self {
            pc: Arc::clone(&pc),
            state: Arc::clone(&state),
            rate,
            cancel: cancel.clone(),
            janitor: Mutex::new(Some(spawn_janitor(
                Arc::clone(&pc),
                cancel.clone(),
                vec![("video", video_writer), ("audio", audio_writer)],
            ))),
            input_ring: InputRingBuffer::new(),
        });

        // 6. 将 StreamingSession 实例弱引用反向绑定至 SessionHandler，打破强引用循环 (避免会话泄漏)
        handler
            .session
            .set(Arc::downgrade(&session))
            .expect("SessionHandler.session 只绑定一次");

        Ok((session, final_answer))
    }

    /// 装配 MediaEngine，注册所选视频编码规格、RTX 重传码流、Opus 音频规格以及 RTP 头部扩展
    ///
    /// 三种编码的 SDP Level 都按分辨率/帧率/码率推导，与编码器写进 SPS/序列头的 Level
    /// 同源——协商层宣称的等级低于码流实际等级时，对端解码器会拒绝或行为异常。
    fn build_media_engine(
        codec: VideoCodec,
        width: usize,
        height: usize,
        fps: usize,
        bitrate_kbps: u32,
    ) -> Result<MediaEngineBundle, Box<dyn std::error::Error + Send + Sync>> {
        let mut media_engine = MediaEngine::default();
        let (mime_type, fmtp, primary_pt, rtx_pt) = match codec {
            VideoCodec::H264 => (
                MIME_TYPE_H264,
                transport::h264_sdp_fmtp(width, height, fps, bitrate_kbps),
                102,
                103,
            ),
            VideoCodec::H265 => (
                MIME_TYPE_HEVC,
                transport::hevc_sdp_fmtp(width, height, fps, bitrate_kbps),
                104,
                105,
            ),
            VideoCodec::Av1 => (
                MIME_TYPE_AV1,
                transport::av1_sdp_fmtp(width, height, fps, bitrate_kbps),
                106,
                107,
            ),
        };

        let video_codec = RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: mime_type.to_owned(),
                clock_rate: VIDEO_CLOCK_RATE,
                channels: 0,
                sdp_fmtp_line: fmtp,
                rtcp_feedback: vec![
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: "".to_owned(),
                    },
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: "pli".to_owned(),
                    },
                    RTCPFeedback {
                        typ: "transport-cc".to_owned(),
                        parameter: "".to_owned(),
                    },
                ],
            },
            payload_type: primary_pt,
        };
        media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)?;

        // 注册绑定的 RTX 码流
        let rtx_codec = RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: "video/rtx".to_owned(),
                clock_rate: VIDEO_CLOCK_RATE,
                channels: 0,
                sdp_fmtp_line: format!("apt={primary_pt}"),
                rtcp_feedback: vec![],
            },
            payload_type: rtx_pt,
        };
        media_engine.register_codec(rtx_codec, RtpCodecKind::Video)?;

        // 注册 Opus 音频规格 (48kHz, 立体声双通道, 带内前向纠错)
        let audio_codec = RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS
                    .to_owned(),
                clock_rate: crate::encode::AUDIO_SAMPLE_RATE,
                channels: crate::encode::AUDIO_CHANNELS as u16,
                sdp_fmtp_line: transport::OPUS_SDP_FMTP.to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: transport::OPUS_PAYLOAD_TYPE,
        };
        media_engine.register_codec(audio_codec.clone(), RtpCodecKind::Audio)?;

        // 注册 TWCC 与 SDES 扩展头
        media_engine.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
                    .to_owned(),
            },
            RtpCodecKind::Video,
            None,
        )?;
        media_engine.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: "urn:ietf:params:rtp-hdrext:sdes:mid".to_owned(),
            },
            RtpCodecKind::Video,
            None,
        )?;

        Ok((
            media_engine,
            video_codec,
            primary_pt,
            audio_codec,
            transport::OPUS_PAYLOAD_TYPE,
        ))
    }

    /// 创建本地发送音频轨，绑定固定 SSRC 与 Opus 编码参数
    fn create_audio_track(
        audio_codec: &RTCRtpCodecParameters,
    ) -> Result<Arc<TrackLocalStaticSample>, Box<dyn std::error::Error + Send + Sync>> {
        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "vistrel-audio-stream".to_string(),
                "vistrel-audio".to_string(),
                "audio".to_string(),
                RtpCodecKind::Audio,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(transport::AUDIO_TRACK_SSRC),
                        ..Default::default()
                    },
                    codec: audio_codec.rtp_codec.clone(),
                    ..Default::default()
                }],
            ),
        )?);
        Ok(track)
    }

    /// 创建本地发送视频轨，绑定全局固定 SSRC 与编码参数
    fn create_video_track(
        video_codec: &RTCRtpCodecParameters,
    ) -> Result<Arc<TrackLocalStaticSample>, Box<dyn std::error::Error + Send + Sync>> {
        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "vistrel-video-stream".to_string(),
                "vistrel-video".to_string(),
                "video".to_string(),
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(transport::VIDEO_TRACK_SSRC),
                        ..Default::default()
                    },
                    codec: video_codec.rtp_codec.clone(),
                    ..Default::default()
                }],
            ),
        )?);
        Ok(track)
    }

    /// 装配底层 PeerConnection、超时策略及 GCC + TWCC 拦截器链
    async fn build_peer_connection(
        mut media_engine: MediaEngine,
        rate: &Arc<RateControl>,
        throttle: &KeyframeThrottle,
        handler: Arc<SessionHandler>,
        udp_bind: &str,
    ) -> Result<Arc<dyn PeerConnection>, Box<dyn std::error::Error + Send + Sync>> {
        let config = RTCConfigurationBuilder::new()
            .with_ice_servers(vec![])
            .build();

        let registry = transport::build_interceptor_registry(&mut media_engine, rate, throttle)?;
        let setting_engine = SettingEngineBuilder::new()
            .with_ice_timeouts(
                Some(Duration::from_secs(5)),
                Some(Duration::from_secs(10)),
                Some(Duration::from_secs(2)),
            )
            .build();

        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::<String>::new()
                .with_configuration(config)
                .with_setting_engine(setting_engine)
                .with_media_engine(media_engine)
                .with_interceptor_registry(registry)
                .with_handler(handler)
                .with_udp_addrs(vec![udp_bind.to_owned()])
                .build()
                .await?,
        );

        Ok(pc)
    }

    /// 执行 SDP 交换（挂轨、设置描述、创建 Answer）、动态解析生效的 PayloadType
    async fn negotiate_sdp(
        pc: Arc<dyn PeerConnection>,
        video_track: Arc<TrackLocalStaticSample>,
        audio_track: Arc<TrackLocalStaticSample>,
        codec: VideoCodec,
        primary_pt: u8,
        audio_pt: u8,
    ) -> Result<(RTCSessionDescription, u8, u8), Box<dyn std::error::Error + Send + Sync>> {
        let video_sender = pc.add_track(video_track).await?;
        let audio_sender = pc.add_track(audio_track).await?;

        let answer = pc.create_answer(None).await?;
        pc.set_local_description(answer.clone()).await?;

        let payload_type = if let Ok(params) = video_sender.get_parameters().await
            && let Some(pt) =
                transport::resolve_codec_payload_type(&params.rtp_parameters.codecs, codec)
        {
            pt
        } else {
            primary_pt
        };

        let audio_payload_type = if let Ok(params) = audio_sender.get_parameters().await
            && let Some(param) = params.rtp_parameters.codecs.iter().find(|c| {
                c.rtp_codec.mime_type.eq_ignore_ascii_case(
                    rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS,
                )
            }) {
            param.payload_type
        } else {
            audio_pt
        };

        let final_answer = pc.local_description().await.unwrap_or(answer);
        Ok((final_answer, payload_type, audio_payload_type))
    }

    /// 向 Peer 连接添加远端 trickle 候选（尽力而为）
    ///
    /// 单条失败只告警不中断——ICE 连通性靠候选潮整体达成，晚到或重复的
    /// 个别候选失败无需上抛；调用方无需等待连通结果，状态由连接状态回调反映
    pub async fn add_ice_candidate(&self, candidate: RTCIceCandidateInit) {
        let ufrag = candidate.username_fragment.clone();
        let mline = candidate.sdp_mline_index;
        if let Err(e) = self.pc.add_ice_candidate(candidate).await {
            log::warn!(
                "远端 ICE 候选添加失败 (ufrag={:?}, mline={:?}): {:?}",
                ufrag,
                mline,
                e
            );
        }
    }

    /// 关闭会话
    ///
    /// 取消写者与输入环，并在 [`SESSION_CLOSE_BUDGET`] 内等待收尾执行体完成
    /// （写者排空 → Peer 关闭 → 端口释放）。超时即告警放行：收尾任务仍在后台
    /// 继续完成端口释放与写者收编，调用方不阻塞
    pub async fn close(&self) {
        self.state
            .store(RTCPeerConnectionState::Closed as u8, Ordering::Release);
        self.cancel.cancel();
        let janitor = self.janitor.lock().take();
        let Some(janitor) = janitor else {
            return;
        };
        match tokio::time::timeout(SESSION_CLOSE_BUDGET, janitor).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => log::error!("会话收尾任务异常退出，端口释放与写者收编未完成"),
            Err(_) => {
                log::warn!("会话收尾超出 {SESSION_CLOSE_BUDGET:?} 预算，端口释放与写者收编交还后台")
            }
        }
    }

    /// 统一状态流转：单次 CAS 保证 Connected 和 Closed 只记录一次
    ///
    /// 会话对 Java 的启停通知已删除：Java 信令层用「offer 成功 / WS 关闭」自持状态，
    /// 这里只维护写者节奏依赖的 PC 状态位。
    pub(crate) fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        log::info!("WebRTC connection state: {:?}", state);
        if state == RTCPeerConnectionState::Connected {
            let _ = self.state.compare_exchange(
                RTCPeerConnectionState::New as u8,
                RTCPeerConnectionState::Connected as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        } else if state == RTCPeerConnectionState::Failed || state == RTCPeerConnectionState::Closed
        {
            // 严格单次触发：原子 CAS / swap 至 Closed
            let _ = self
                .state
                .swap(RTCPeerConnectionState::Closed as u8, Ordering::AcqRel);
        }
    }

    /// 统一入口：根据 label 分发 DataChannel 并托管异步循环
    pub fn handle_data_channel(self: &Arc<Self>, dc: Arc<dyn DataChannel>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let label = dc.label().await.unwrap_or_default();
            match label.as_str() {
                "input" => this.input_event_loop(dc).await,
                other => log::warn!("忽略未知 DataChannel 标签: {}", other),
            }
        });
    }

    /// 从 input DataChannel 读二进制按键，投递给本地输入缓冲
    async fn input_event_loop(&self, dc: Arc<dyn DataChannel>) {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                event = dc.poll() => match event {
                    Some(DataChannelEvent::OnMessage(msg)) => {
                        self.input_ring.parse_and_push(&msg.data);
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }

    /// 码率落账：把引擎侧阻塞 ask 的生效码率写入本会话的回路上界。
    /// ask 的发起与档位记录都在引擎层，会话不持档位——拒绝路径不经过这里，
    /// 上界自然保持旧值
    pub(crate) fn commit_bps(&self, bps: usize) {
        self.rate.set_bps(bps as u32);
    }

    /// 从本会话输入环批量提取事件；仅渲染主线程单消费者调用
    pub(crate) fn drain_input_events(&self, out_buf: &mut [u8], max_events: usize) -> usize {
        self.input_ring.drain_to_buffer(out_buf, max_events)
    }

    /// 单测会话装配：管线照常组装（死控制面只为产出写者要用的发件箱），
    /// 控制面由用例在 ask 时自备（活/死皆可）
    #[cfg(test)]
    async fn new_mock() -> (Arc<Self>, Arc<VideoWriter>) {
        let video_codec = RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: VIDEO_CLOCK_RATE,
                channels: 0,
                sdp_fmtp_line: transport::h264_sdp_fmtp(1920, 1080, 60, 20_000),
                rtcp_feedback: vec![],
            },
            payload_type: 102,
        };
        let track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "mock-stream".to_string(),
                    "mock-video".to_string(),
                    "video".to_string(),
                    RtpCodecKind::Video,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(transport::VIDEO_TRACK_SSRC),
                            ..Default::default()
                        },
                        codec: video_codec.rtp_codec,
                        ..Default::default()
                    }],
                ),
            )
            .unwrap(),
        );
        struct DummyHandler;
        #[async_trait]
        impl PeerConnectionEventHandler for DummyHandler {}

        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::<&str>::new()
                .with_handler(Arc::new(DummyHandler))
                .with_udp_addrs(vec!["0.0.0.0:0", "[::]:0"])
                .build()
                .await
                .unwrap(),
        );

        let state = Arc::new(AtomicU8::new(RTCPeerConnectionState::Connected as u8));
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let doorbell = Arc::new(tokio::sync::Notify::new());
        let control = crate::encode::control::ControlSender::new(tx, Arc::clone(&doorbell));
        let throttle = KeyframeThrottle::new(control.clone());
        let pipeline = Arc::new(crate::encode::VideoPipeline::assemble(
            control,
            Arc::new(crate::encode::frame::FrameInbox::new(Arc::clone(&doorbell))),
        ));
        let writer = VideoWriter::new(
            Arc::clone(&track),
            102,
            Arc::clone(&state),
            throttle,
            pipeline.output(),
            CancellationToken::new(),
        );
        let writer = Arc::new(writer);

        let cancel = CancellationToken::new();
        let janitor = spawn_janitor(Arc::clone(&pc), cancel.clone(), Vec::new());

        (
            Arc::new(Self {
                pc,
                state,
                rate: Arc::new(RateControl::new(20_000_000, |_| {})),
                cancel,
                janitor: Mutex::new(Some(janitor)),
                input_ring: InputRingBuffer::new(),
            }),
            writer,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::VideoCodec;
    use crate::encode::control::ControlSender;
    use std::sync::atomic::AtomicBool;

    /// 死控制面：Actor 侧无接收者，ask 走 Err(ActorDead) 兜底
    fn dead_control() -> ControlSender {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        ControlSender::new(tx, Arc::new(tokio::sync::Notify::new()))
    }

    /// 测试用引擎：假控制面 + 空管线（不起编码线程），只提供会话建立所需的部件
    fn test_engine() -> Arc<crate::streaming::StreamingEngine> {
        test_engine_with_bind("0.0.0.0:0")
    }

    /// 同上，但绑定指定地址——端口重绑类断言需要固定端口
    fn test_engine_with_bind(udp_bind: &str) -> Arc<crate::streaming::StreamingEngine> {
        let audio = crate::streaming::audio_loopback::AudioLoopback::new();
        let pipeline = Arc::new(crate::encode::VideoPipeline::assemble(
            dead_control(),
            Arc::new(crate::encode::frame::FrameInbox::new(Arc::new(
                tokio::sync::Notify::new(),
            ))),
        ));
        Arc::new(crate::streaming::StreamingEngine::new(
            pipeline,
            audio,
            udp_bind.to_string(),
        ))
    }

    /// 最小合法视频 offer：只含 SDP 协商所需字段
    fn test_offer() -> RTCSessionDescription {
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
        rtc::peer_connection::sdp::RTCSessionDescription::offer(sdp.to_string()).unwrap()
    }

    #[tokio::test]
    async fn test_streaming_session_establish_and_close() {
        let offer = test_offer();
        let initial_profile = StreamingProfile::new(1920, 1080, 60, VideoCodec::H264);

        let engine = test_engine();
        let handler = Arc::new(SessionHandler::new(Arc::new(|_| {})));

        let (session, answer) =
            StreamingSession::establish(&engine, offer, initial_profile, handler)
                .await
                .unwrap();

        assert!(answer.sdp.contains("102"));

        // 关闭会话
        session.close().await;
        assert_eq!(
            session.state.load(Ordering::Acquire),
            RTCPeerConnectionState::Closed as u8
        );
    }

    /// PC 状态位只流转一次：Connected 记录、Failed/Closed 幂等落 Closed。
    /// 会话不再向 Java 推送状态，这里只钉住写者节奏依赖的状态机本身
    #[tokio::test]
    async fn test_session_handler_idempotent_closed_transition() {
        let (session, _writer) = StreamingSession::new_mock().await;

        session
            .state
            .store(RTCPeerConnectionState::New as u8, Ordering::Release);

        let handler = SessionHandler::new(Arc::new(|_| {}));
        handler
            .session
            .set(Arc::downgrade(&session))
            .expect("SessionHandler.session 只绑定一次");

        // 1. Connected
        handler
            .on_connection_state_change(RTCPeerConnectionState::Connected)
            .await;
        assert_eq!(
            session.state.load(Ordering::Acquire),
            RTCPeerConnectionState::Connected as u8
        );

        // 2. Failed -> 记录 Closed
        handler
            .on_connection_state_change(RTCPeerConnectionState::Failed)
            .await;
        assert_eq!(
            session.state.load(Ordering::Acquire),
            RTCPeerConnectionState::Closed as u8
        );

        // 3. 随后又来 Closed -> 保持 Closed
        handler
            .on_connection_state_change(RTCPeerConnectionState::Closed)
            .await;
        assert_eq!(
            session.state.load(Ordering::Acquire),
            RTCPeerConnectionState::Closed as u8
        );
    }

    #[tokio::test]
    async fn test_streaming_session_drop_cancels_tracked_tasks() {
        let (session, _writer) = StreamingSession::new_mock().await;

        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&flag);
        let cancel = session.cancel.clone();

        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    flag_clone.store(true, Ordering::Release);
                }
            }
        });

        // drop session
        drop(session);

        // 验证任务被取消信号立即终止而不是睡满 10 秒
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !flag.load(Ordering::Acquire),
            "任务应在 drop 时被取消信号终止"
        );
    }

    /// 契约：close 缺席（裸 drop）时兜底收尾必须释放媒体端口。
    /// 证据取端口重绑——固定端口 establish 后裸弃会话，同端口终将可再绑定
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_drop_without_close_still_frees_media_port() {
        // 借一个空闲端口：绑定读取端口号后立刻释放，交给测试引擎
        let port = {
            let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let engine = test_engine_with_bind(&format!("127.0.0.1:{port}"));
        let handler = Arc::new(SessionHandler::new(Arc::new(|_| {})));
        let initial_profile = StreamingProfile::new(1920, 1080, 60, VideoCodec::H264);
        let (session, _answer) =
            StreamingSession::establish(&engine, test_offer(), initial_profile, handler)
                .await
                .unwrap();

        // 刻意不调 close：整会话直接丢弃，收尾交给兜底任务
        drop(session);

        // 兜底收尾的端到端证据：同一端口最终可重新绑定（不收尾则一直占用）
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while std::net::UdpSocket::bind(("127.0.0.1", port)).is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "兜底收尾未在期限内释放媒体端口"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 契约：输入环随会话生灭——事件不跨会话，新会话环必为空
    #[tokio::test]
    async fn test_session_input_ring_is_session_scoped() {
        let buf = &mut [0u8; 64];
        let (session, _writer) = StreamingSession::new_mock().await;
        assert_eq!(session.drain_input_events(buf, 16), 0, "新会话环为空");

        assert!(
            session
                .input_ring
                .parse_and_push(&crate::streaming::input::test_motion_event(3, -4))
        );
        assert_eq!(session.drain_input_events(buf, 16), 1);

        let (session2, _writer2) = StreamingSession::new_mock().await;
        assert_eq!(
            session2.drain_input_events(buf, 16),
            0,
            "事件不得跨会话残留"
        );
    }

    #[tokio::test]
    async fn test_establish_session_drop_breaks_reference_cycle() {
        let sdp = "v=0\r\n\
o=- 0 0 IN IP4 127.0.0.1\r\n\
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
        let offer = RTCSessionDescription::offer(sdp.to_string()).unwrap();
        let profile = StreamingProfile::new(1920, 1080, 60, VideoCodec::H264);
        let engine = test_engine();
        let handler = Arc::new(SessionHandler::new(Arc::new(|_| {})));

        let (session, _) = StreamingSession::establish(&engine, offer, profile, handler)
            .await
            .expect("establish 应当成功");

        let weak = Arc::downgrade(&session);
        // 主动关闭并丢弃 session
        session.close().await;
        drop(session);

        assert!(
            weak.upgrade().is_none(),
            "StreamingSession 应当在 drop 后彻底释放，不存在循环引用导致泄漏"
        );
    }

    /// 收尾次序不变量：写者先被收编、Peer 后关闭——写者在传输存活期排空在途写，
    /// Peer 关闭不得抢在写者退出之前。证据取端口重绑：Peer 占住固定端口，
    /// 端口变为可重绑的最早时刻即 Peer 关闭时刻，须晚于写者退出时刻
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_janitor_joins_writers_before_peer_close() {
        struct DummyHandler;
        #[async_trait]
        impl PeerConnectionEventHandler for DummyHandler {}

        // 借一个空闲端口：绑定读取端口号后立刻释放，交给 Peer 占住
        let port = {
            let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let bind_v4 = format!("127.0.0.1:{port}");
        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::<String>::new()
                .with_handler(Arc::new(DummyHandler))
                .with_udp_addrs(vec![bind_v4, "[::]:0".to_string()])
                .build()
                .await
                .unwrap(),
        );

        // 写者故意拖延退出：若次序颠倒，端口会在写者退出前就被释放
        let writer_done = Arc::new(std::sync::Mutex::new(None));
        let marker = Arc::clone(&writer_done);
        let writer = tokio::spawn(async move {
            tokio::time::sleep(WRITER_JOIN_TIMEOUT / 10).await;
            *marker.lock().unwrap() = Some(std::time::Instant::now());
        });

        let cancel = CancellationToken::new();
        let janitor = spawn_janitor(Arc::clone(&pc), cancel.clone(), vec![("video", writer)]);
        cancel.cancel();

        // 端口变为可重绑的最早时刻即 Peer 关闭时刻
        let mut freed_at = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while freed_at.is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "Peer 未在期限内关闭释放媒体端口"
            );
            if std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok() {
                freed_at = Some(std::time::Instant::now());
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        janitor.await.unwrap();

        let writer_done = writer_done.lock().unwrap().expect("写者应已退出并被收编");
        assert!(writer_done < freed_at.unwrap(), "Peer 关闭必须晚于写者收编");
    }
}

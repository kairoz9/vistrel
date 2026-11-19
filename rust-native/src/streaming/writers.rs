//! 会话的两个媒体写者 (Video Writer / Audio Writer)
//!
//! 一条会话的媒体出口是两个孪生写者：`VideoWriter` 从管线发件箱取最新编码帧，过门控与
//! 过期闸后写轨道；`AudioWriter` 从会话音频队列串行取包写轨道。两者构造即完整、按值被
//! 各自的写循环消费、随会话取消退出、由会话有界 join——同生共死。
//!
//! 门控与节流窗口随写者新生：2000ms 的关键帧节流窗口不跨会话继承——重连的客户端不会
//! 黑屏到窗口过期（节流归会话，触发器归管线）。视频链路上会等网络的只有写循环这一处。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rtc::media::Sample;
use tokio::sync::mpsc::Receiver;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::RTCPeerConnectionState;

use crate::encode::frame::{FrameOutbox, FrameOutcome};
use crate::encode::{AudioPacket, StampedVideoPacket, VideoPacket};
use crate::streaming::cancellation::CancellationToken;
use crate::streaming::transport::{
    AUDIO_TRACK_SSRC, KeyframeThrottle, TickDiffer, VIDEO_TRACK_SSRC,
};

/// 单帧写轨道上限：写卡住时不能无限期占住写循环，超时就丢帧并要求下一次 IDR
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);

/// 排队过期门槛：帧在发件箱等到这个时长，出口已经落后，本帧对交互流无价值
const STALE_AFTER: Duration = Duration::from_millis(33);

/// 一次会话的视频写者：轨道、门控与节流在构造期一次配齐
pub(crate) struct VideoWriter {
    track: Arc<TrackLocalStaticSample>,
    payload_type: u8,

    /// 与会话共享的连接状态
    state: Arc<AtomicU8>,

    differ: Mutex<TickDiffer>,

    /// 等关键帧门控：为 true 时 P 帧直接丢弃，零分配
    waiting_for_keyframe: AtomicBool,

    throttle: KeyframeThrottle,

    /// 管线发件箱：编码线程写、本写者的写循环取
    outbox: Arc<FrameOutbox>,

    /// 会话生命周期令牌：随构造注入，写循环与在途写都受它管束
    cancel: CancellationToken,

    /// 测试接收端：有值时写轨道短路，只回传算出的样本时长，不碰真链路
    #[cfg(test)]
    test_sink: Option<tokio::sync::mpsc::Sender<Duration>>,
}

impl VideoWriter {
    /// 七个入参都是会话装配时一次性注入的部件，拆成中间结构只会多一层转手
    pub(crate) fn new(
        track: Arc<TrackLocalStaticSample>,
        payload_type: u8,
        state: Arc<AtomicU8>,
        throttle: KeyframeThrottle,
        outbox: Arc<FrameOutbox>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            track,
            payload_type,
            state,
            differ: Mutex::new(TickDiffer::new()),
            waiting_for_keyframe: AtomicBool::new(true),
            throttle,
            outbox,
            cancel,
            #[cfg(test)]
            test_sink: None,
        }
    }

    /// 写循环：门铃唤醒后取箱里最新的一帧写上链，直到注入的取消令牌触发
    ///
    /// 按值消费：写者生来只为这一个循环，spawn 后无人再持有它。
    pub(crate) async fn write_loop(self) {
        loop {
            if !self.drain_pending().await {
                break;
            }
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break,
                _ = self.outbox.notified() => {}
            }
        }
    }

    /// 取一帧写上链；返回 `false` 表示会话已取消
    ///
    /// 门控、连接态与排队过期都在这里落地：编码线程只管入箱，这里拒收的每一帧
    /// 都是本会话自己的决定。单测直接驱动它，免得再起一个任务。
    async fn drain_pending(&self) -> bool {
        // 先取再判：没有帧就绪就让门铃负责下一次唤醒
        let Some(pending) = self.outbox.take() else {
            return true;
        };
        if pending.posted_at.elapsed() > STALE_AFTER {
            // 过期帧没碰过链路，不必要 IDR：后面的帧序仍然自洽。
            // 这一闸排在门控之前：等 IDR 的会话同样不该写一帧已经过期的 IDR
            self.outbox
                .outcomes()
                .record_with(FrameOutcome::WriteStalled, "排队超期，已丢帧");
            return true;
        }
        // 门控未开：不要 P 帧，就地再要一次 IDR（走节流版：写卡住时逐帧无条件请求，
        // 编码器会在最坏的时机连发 IDR）
        if self.waiting_for_keyframe.load(Ordering::Acquire) && !pending.packet.is_keyframe {
            self.throttle.request_throttled();
            self.outbox.outcomes().record(FrameOutcome::GateSkipped);
            return true;
        }
        if self.state.load(Ordering::Acquire) != RTCPeerConnectionState::Connected as u8 {
            // 连接未就绪：帧照丢但不算写失败，会话仍可能在协商中
            self.outbox.outcomes().record(FrameOutcome::GateSkipped);
            return true;
        }
        self.admit(pending).await
    }

    /// 单帧上链：以「诞生时刻 + 过期门槛 + 写上限」为死线，排队与写共用一次计时
    async fn admit(&self, pending: StampedVideoPacket) -> bool {
        let deadline = pending.posted_at + STALE_AFTER + WRITE_TIMEOUT;
        let write = tokio::time::timeout_at(deadline, self.write_sample(pending.packet));
        tokio::select! {
            _ = self.cancel.cancelled() => {
                // 在途样本随会话关停一起放手：它连超时都没等到
                self.outbox
                    .outcomes()
                    .record(FrameOutcome::Superseded);
                false
            }
            result = write => {
                match result {
                    Ok(WriteResult::Sent) => self.outbox
                        .outcomes()
                        .record(FrameOutcome::Admitted),
                    Ok(WriteResult::Failed) => {
                        self.outbox
                            .outcomes()
                            .record_with(FrameOutcome::TrackWriteFailed, "写轨道失败");
                        self.throttle.request_throttled();
                    }
                    Err(_) => {
                        self.outbox.outcomes().record_with(
                            FrameOutcome::WriteStalled,
                            format!("{}ms 内未写完，已丢帧", WRITE_TIMEOUT.as_millis()),
                        );
                        self.throttle.request_throttled();
                    }
                };
                true
            }
        }
    }

    /// 写一帧到本会话的轨道（含时序推进）
    ///
    /// 只有写循环调用它：单写者保证 `differ` 严格按帧序推进。
    async fn write_sample(&self, packet: VideoPacket) -> WriteResult {
        let ticks = self.differ.lock().advance(packet.birth_nanos);
        let duration = TickDiffer::ticks_to_duration(ticks);

        #[cfg(test)]
        if let Some(sink) = &self.test_sink {
            let _ = sink.send(duration).await;
            if packet.is_keyframe {
                self.waiting_for_keyframe.store(false, Ordering::Release);
            }
            return WriteResult::Sent;
        }

        let sample = Sample {
            data: packet.nalu,
            duration,
            timestamp: Instant::now(),
            packet_timestamp: 0,
            prev_dropped_packets: 0,
            prev_padding_packets: 0,
        };

        match self
            .track
            .sample_writer(VIDEO_TRACK_SSRC, self.payload_type)
            .write_sample(&sample)
            .await
        {
            Ok(()) => {
                if packet.is_keyframe {
                    self.waiting_for_keyframe.store(false, Ordering::Release);
                }
                WriteResult::Sent
            }
            Err(_) => WriteResult::Failed,
        }
    }
}

/// 一次写轨道的结局
enum WriteResult {
    Sent,
    Failed,
}

/// 一次会话的音频写者：队列接收端、取消令牌与轨道在构造期一次配齐
///
/// 它生来只为 [`AudioWriter::write_loop`] 这一个循环：spawn 后无人再持有它，按值消费即可。
pub(crate) struct AudioWriter {
    track: Arc<TrackLocalStaticSample>,
    payload_type: u8,
    /// 与会话共享的连接状态
    state: Arc<AtomicU8>,
    /// 本会话队列的接收端：构造即装配，任务唯一所有
    rx: Receiver<AudioPacket>,
    /// 会话生命周期令牌：随构造注入，队列等待与在途写都受它管束
    cancel: CancellationToken,
}

impl AudioWriter {
    /// 五件套一次配齐：轨道、payload type、连接状态快照、本会话队列接收端与取消令牌
    ///
    /// 构造即完整、只装配不 spawn——写循环由调用方 `AudioWriter::new(...).write_loop()`
    /// 一步起任务；接收端必须来自 `AudioLoopback::attach`，构造点唯一（会话建立）
    pub(crate) fn new(
        track: Arc<TrackLocalStaticSample>,
        payload_type: u8,
        state: Arc<AtomicU8>,
        rx: Receiver<AudioPacket>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            track,
            payload_type,
            state,
            rx,
            cancel,
        }
    }

    /// 写循环：串行取本会话队列并写轨道，随注入的取消令牌退出
    ///
    /// 音频不设写超时（连续性优先）；取消靠 select 直接打断在途写，关停由会话有界 join 收尾。
    pub(crate) async fn write_loop(mut self) {
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break,
                packet = self.rx.recv() => {
                    let Some(packet) = packet else { break };
                    tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => break,
                        _ = self.write_sample(packet) => {}
                    }
                }
            }
        }
    }

    async fn write_sample(&self, packet: AudioPacket) {
        if self.state.load(Ordering::Acquire) != RTCPeerConnectionState::Connected as u8 {
            return;
        }
        let sample = Sample {
            data: packet.data,
            duration: packet.duration,
            timestamp: Instant::now(),
            packet_timestamp: 0,
            prev_dropped_packets: 0,
            prev_padding_packets: 0,
        };
        let _ = self
            .track
            .sample_writer(AUDIO_TRACK_SSRC, self.payload_type)
            .write_sample(&sample)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::KeyframeTrigger;
    use crate::encode::frame::{FrameOutcomes, Mailbox};
    use crate::streaming::transport::{VIDEO_CLOCK_RATE, h264_sdp_fmtp};
    use bytes::Bytes;
    use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;
    use rtc::rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
        RtpCodecKind,
    };
    use webrtc::media_stream::MediaStreamTrack;

    /// 测试轨道：声明 H264 编码但不挂 PeerConnection，真实写入必 Err（覆盖失败分支）
    fn test_track() -> Arc<TrackLocalStaticSample> {
        let video_codec = RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: VIDEO_CLOCK_RATE,
                channels: 0,
                sdp_fmtp_line: h264_sdp_fmtp(1920, 1080, 60, 20_000),
                rtcp_feedback: vec![],
            },
            payload_type: 102,
        };
        Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "mock-stream".to_string(),
                    "mock-video".to_string(),
                    "video".to_string(),
                    RtpCodecKind::Video,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(VIDEO_TRACK_SSRC),
                            ..Default::default()
                        },
                        codec: video_codec.rtp_codec,
                        ..Default::default()
                    }],
                ),
            )
            .unwrap(),
        )
    }

    /// 视频写者最小装配：独立账本的发件箱 + 新生节流窗口；测试模块就在本文件里，直接触碰私有字段
    fn make_video_writer(
        trigger: KeyframeTrigger,
        sink: Option<tokio::sync::mpsc::Sender<Duration>>,
    ) -> VideoWriter {
        let mut writer = VideoWriter::new(
            test_track(),
            102,
            Arc::new(AtomicU8::new(RTCPeerConnectionState::Connected as u8)),
            KeyframeThrottle::new(trigger),
            Arc::new(Mailbox::new(Arc::new(FrameOutcomes::new()))),
            CancellationToken::new(),
        );
        writer.test_sink = sink;
        writer
    }

    /// 测试帧：IDR 带 0x65，P 帧带 0x61
    fn video_frame(birth_nanos: u64, is_keyframe: bool) -> VideoPacket {
        let marker = if is_keyframe { 0x65 } else { 0x61 };
        VideoPacket {
            nalu: Bytes::from(vec![0, 0, 0, 1, marker]),
            birth_nanos,
            is_keyframe,
        }
    }

    /// 直接入箱
    fn post(writer: &VideoWriter, packet: VideoPacket) {
        writer.outbox.post(StampedVideoPacket::now(packet));
    }

    #[tokio::test]
    async fn test_waiting_keyframe_self_heals_request() {
        let trigger = KeyframeTrigger::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let writer = make_video_writer(trigger.clone(), Some(tx));

        // 消费掉初值 true，模拟旗标已被上一个会话吃光
        assert!(trigger.take(), "初值必须为 true");
        assert!(!trigger.take(), "消费后必须为 false");

        // 新会话吃到的第一帧是 P 帧：就地拒收 + waiting 自愈再要 IDR
        post(&writer, video_frame(1_000_000, false));
        assert!(writer.drain_pending().await);
        assert_eq!(writer.outbox.outcomes().count(FrameOutcome::GateSkipped), 1);
        assert!(trigger.take(), "丢弃 P 帧必须自请求关键帧");
        assert!(!trigger.take(), "自请求为一次性置位");

        // IDR 到达：写上链后清除等待，之后不再自请求
        post(&writer, video_frame(2_000_000, true));
        assert!(writer.drain_pending().await);
        assert!(rx.recv().await.is_some());
        assert!(!trigger.take(), "IDR 已兑现，不得再请求");

        // 等待清除后的 P 帧正常写上链，同样不触碰旗标
        post(&writer, video_frame(3_000_000, false));
        assert!(writer.drain_pending().await);
        assert!(rx.recv().await.is_some());
        assert!(!trigger.take());
    }

    /// 连续入箱但不写：箱里只留最新一帧，被覆盖的帧进 `Superseded`
    #[tokio::test]
    async fn test_post_keeps_latest_and_counts_superseded() {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let writer = make_video_writer(KeyframeTrigger::new(), Some(tx));

        // 首帧 IDR 先兑现门控
        post(&writer, video_frame(1, true));
        assert!(writer.drain_pending().await);

        for birth in [2u64, 3, 4] {
            post(&writer, video_frame(birth, false));
        }
        assert_eq!(
            writer.outbox.outcomes().count(FrameOutcome::Superseded),
            2,
            "2、3 两帧必须在前一帧入箱时就地记入账本"
        );
        assert!(writer.drain_pending().await);
        assert_eq!(
            writer.outbox.outcomes().count(FrameOutcome::Admitted),
            2,
            "一封 IDR + 最后一帧 P 帧上链路"
        );
    }

    /// 排队超过两个帧间隔的帧直接丢弃，不碰链路
    #[tokio::test(start_paused = true)]
    async fn test_delivery_drops_stale_pending_without_writing() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let writer = make_video_writer(KeyframeTrigger::new(), Some(tx));

        post(&writer, video_frame(1, true));
        // 60fps 下门槛为 33ms：空等 60ms 已经过了两个帧间隔
        tokio::time::advance(Duration::from_millis(60)).await;
        assert!(writer.drain_pending().await);

        assert!(rx.try_recv().is_err(), "过期帧不得上链路");
        assert_eq!(
            writer.outbox.outcomes().count(FrameOutcome::WriteStalled),
            1
        );
        assert_eq!(writer.outbox.outcomes().count(FrameOutcome::Admitted), 0);
    }

    /// 写失败时门控保持关闭：下一次 P 帧入口就自愈请求，不等下一个 IDR
    #[tokio::test]
    async fn test_write_failure_keeps_gate_shut() {
        // 不挂测试接收端：轨道未绑定 PeerConnection，每次写都会 Err
        let writer = make_video_writer(KeyframeTrigger::new(), None);

        post(&writer, video_frame(1, true));
        assert!(writer.drain_pending().await);
        assert_eq!(
            writer
                .outbox
                .outcomes()
                .count(FrameOutcome::TrackWriteFailed),
            1
        );
        assert_eq!(writer.outbox.outcomes().count(FrameOutcome::Admitted), 0);

        // IDR 没写出去：门控仍是关的，P 帧被跳过并就地再要一次
        post(&writer, video_frame(2, false));
        assert!(writer.drain_pending().await);
        assert_eq!(writer.outbox.outcomes().count(FrameOutcome::GateSkipped), 1);
    }

    #[tokio::test]
    async fn test_new_session_clock_baseline_isolated_after_idle_gap() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let writer = make_video_writer(KeyframeTrigger::new(), Some(tx));

        // 新会话首帧的诞生时刻距上一会话最后一帧跃迁 60 秒：differ 以满帧间隔重新起拍
        post(&writer, video_frame(60_001_000_000, true));
        assert!(writer.drain_pending().await);
        let first = rx.recv().await.expect("首帧必须上链");
        assert!(
            first.as_millis() <= 20,
            "首帧基线必须是一个帧间隔，实际为 {:?}",
            first
        );

        // 次帧在 16.6ms 之后到达：duration 依然约 16.6ms，不被旧时钟或空闲间隔污染
        post(&writer, video_frame(60_001_000_000 + 16_666_666, true));
        assert!(writer.drain_pending().await);
        let second = rx.recv().await.expect("次帧必须上链");
        assert!(
            second.as_millis() <= 20,
            "次帧 duration 必须跟随物理真实时间（约 16ms），实际为 {:?}",
            second
        );
    }
}

//! WebRTC 传输层装配与底层组件
//!
//! 负责底层 WebRTC 协商（SDP/ICE）、媒体轨创建、编解码参数配置、
//! 以及 TWCC、GCC 和反馈观察者（FeedbackObserver）的拦截器链组装。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use rtc::interceptor::{
    Attribute, CongestionControlBuilder, Gcc, Interceptor, Packet, Registry, Slot, StreamInfo,
    TaggedPacket, TwccSenderBuilder,
};
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters;
use rtc::sansio::Protocol;
use rtc::shared::error::Error;

use crate::encode::{KeyframeTrigger, VideoCodec};
use crate::streaming::rate_control::RateControl;

/// 视频轨的 SSRC。只在本模块构造轨道与解析编解码参数时用得到
pub(crate) const VIDEO_TRACK_SSRC: u32 = 5000;
/// 音频轨的 SSRC
pub(crate) const AUDIO_TRACK_SSRC: u32 = 5001;
/// Opus 音频默认 RTP 载荷类型 (RFC 7587)
pub(crate) const OPUS_PAYLOAD_TYPE: u8 = 111;
/// Opus 协商参数串 (RFC 7587)
pub(crate) const OPUS_SDP_FMTP: &str = "minptime=10;useinbandfec=1";

/// H.264 协商参数串 (RFC 6184)
///
/// `profile-level-id` 与编码器实际写入 SPS 的 Level 同源推导，避免协商层宣称的
/// Level 与码流不一致（例如 SDP 宣称 Level 6.0 而实际码流是 4.2）。
pub(crate) fn h264_sdp_fmtp(width: usize, height: usize, fps: usize, bitrate_kbps: u32) -> String {
    let level_idc = crate::encode::h264_level_idc(width, height, fps, bitrate_kbps);
    format!(
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=6400{:02x}",
        level_idc
    )
}

/// H.265 (HEVC) 协商参数串 (RFC 7798)
///
/// `level-id` 就是码流 SPS 里的 `general_level_idc`（30 × level）。写死常量会让
/// 1080p60 宣称 Level 3.1 而实际需要 4.1——解码器可能直接拒绝或行为异常。
pub(crate) fn hevc_sdp_fmtp(width: usize, height: usize, fps: usize, bitrate_kbps: u32) -> String {
    let level_idc = crate::encode::hevc_level_idc(width, height, fps, bitrate_kbps);
    format!("level-id={};profile-space=0;tier-flag=0", level_idc)
}

/// AV1 协商参数串 (RFC 9585)
///
/// `level-idx` 是序列头 `seq_level_idx` 的取值，同样按分辨率/帧率/码率推导。
pub(crate) fn av1_sdp_fmtp(width: usize, height: usize, fps: usize, bitrate_kbps: u32) -> String {
    let level_idx = crate::encode::av1_level_idx(width, height, fps, bitrate_kbps);
    format!("profile=0;level-idx={}", level_idx)
}

/// 关键帧节流器
///
/// 限制 PLI/FIR 反馈与发送队列溢出触发的关键帧。
/// 在 Intra-Refresh 模式下作为终极熔断兜底，日常丢包依赖条带滚动快速自愈。
pub(crate) struct KeyframeThrottle {
    /// 上次放行时刻（相对 [`CLOCK_BASE`] 的纳秒）；0 表示从未放行。
    /// 内层 Arc 保证同一会话内反馈观察者与发送错误路径共用同一个节流窗口
    last: Arc<AtomicU64>,
    trigger: KeyframeTrigger,
}

const KEYFRAME_THROTTLE: Duration = Duration::from_millis(3000);

const KEYFRAME_THROTTLE_NANOS: u64 = KEYFRAME_THROTTLE.as_nanos() as u64;

/// 纳秒时间戳的全进程基准：节流只比较差值，基准取哪个时刻无关紧要
static CLOCK_BASE: LazyLock<Instant> = LazyLock::new(Instant::now);

impl Clone for KeyframeThrottle {
    fn clone(&self) -> Self {
        // AtomicU64 未实现 Clone，这里必须共享同一个原子而非复制时间戳：
        // 复制会让每份 clone 各持独立窗口，节流上限放大为共享方数量倍
        Self {
            last: Arc::clone(&self.last),
            trigger: self.trigger.clone(),
        }
    }
}

impl KeyframeThrottle {
    pub(crate) fn new(trigger: KeyframeTrigger) -> Self {
        Self {
            last: Arc::new(AtomicU64::new(0)),
            trigger,
        }
    }

    /// 尝试请求关键帧：距离上次触发超过 2000ms 时才真正触发信标
    ///
    /// 「检查窗口 + 盖章」由单次 CAS 成对完成，并发调用恰好一个赢家盖章并触发，
    /// 其余看到赢家刚写入的时刻落在窗口内而退回——「一个窗口一个赢家」由 CAS
    /// 显式保证。时间戳只用于差值比较，无其他内存副作用，全程 Relaxed 足够。
    pub(crate) fn request_throttled(&self) -> bool {
        let mut prev = self.last.load(Ordering::Relaxed);
        loop {
            // now 必须每轮重取：CAS 失败时 prev 换成了赢家的盖章值，若沿用本轮
            // 之前读的 now，win 的盖章可能晚于该 now，差值下溢（u64 回绕成巨数，
            // 节流窗被击穿）。单调时钟保证每轮重取的 now ≥ 任何历史盖章
            let now = CLOCK_BASE.elapsed().as_nanos() as u64;
            if prev != 0 && now - prev < KEYFRAME_THROTTLE_NANOS {
                return false;
            }
            match self
                .last
                .compare_exchange_weak(prev, now, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => {
                    self.trigger.request();
                    return true;
                }
                Err(p) => prev = p,
            }
        }
    }
}

/// H.264、H.265 (HEVC)、AV1 视频轨 RTP 时钟率 (RFC 强制 90kHz)。
pub const VIDEO_CLOCK_RATE: u32 = 90_000;

/// RTP 时钟差分器：把单调出生纳秒流差分成每帧 90kHz 时钟的 duration ticks
///
/// 基线始终取原始值、零逐帧累积误差；首帧无前驱，物理上无从得知其显示时长，
/// 以 1ms 护栏占位、次帧即被真实差分覆盖。输出上界只来自 RTP 时间戳的 u32 位宽
/// （90kHz 下约 13.26 小时回绕），无策略钳制
pub struct TickDiffer {
    last_samples: Option<u64>,
}

impl Default for TickDiffer {
    fn default() -> Self {
        Self::new()
    }
}

impl TickDiffer {
    /// 视频轨 RTP 时钟率
    const CLOCK_RATE: u64 = VIDEO_CLOCK_RATE as u64;
    /// 输出下限护栏（1ms）：吸收未来脏路径可能出现的退化 delta
    const MIN_TICKS: u64 = 90;

    pub fn new() -> Self {
        Self { last_samples: None }
    }

    /// 推进一格：输入原始出生纳秒，输出本帧 duration ticks。
    /// u128 中间量承载 90kHz 放大，长时运行不溢出；基线取原始值，零逐帧累积。
    /// 上界仅来自 RTP 时间戳的 u32 位宽（90kHz 下约 13.26 小时），非策略钳制。
    /// 首帧没有前驱，物理上无法得知它会显示多久：取 1ms 护栏占位，次帧即被真实差分覆盖。
    pub fn advance(&mut self, birth_nanos: u64) -> u32 {
        let samples =
            (u128::from(birth_nanos) * u128::from(Self::CLOCK_RATE) / 1_000_000_000) as u64;
        let delta = match self.last_samples.replace(samples) {
            Some(last) => samples.saturating_sub(last).max(Self::MIN_TICKS),
            None => Self::MIN_TICKS,
        };
        delta.min(u64::from(u32::MAX)) as u32
    }

    /// ticks → Sample.duration：纳秒向上取整，保证打包器 as_secs_f64×clock_rate
    /// 还原为原始 tick（向下取整会丢 1 tick 并随帧累积）
    pub fn ticks_to_duration(ticks: u32) -> Duration {
        let nanos = (u64::from(ticks) * 1_000_000_000).div_ceil(Self::CLOCK_RATE);
        Duration::from_nanos(nanos)
    }
}

/// GCC 的链路容量上限 (bps)
const GCC_CAPACITY_BPS: u32 = 100_000_000;

/// GCC 的估计下限 (bps)：绝对地板而非档位比例——GCC 只报路径真实容量，
/// 档位语义归 RateControl 的钳位所有，滑杆改档无需触碰 GCC
const GCC_FLOOR_BPS: u32 = 1_000_000;

/// 装配拦截器链 (GCC + TWCC 发送端)。
pub(crate) fn build_interceptor_registry(
    media_engine: &mut MediaEngine,
    rate: &Arc<RateControl>,
    throttle: &KeyframeThrottle,
) -> Result<Registry, Error> {
    let floor_bitrate = f64::from(GCC_FLOOR_BPS);
    let start_bitrate = f64::from(rate.max());
    let capacity_bitrate = f64::from(GCC_CAPACITY_BPS);

    let throttle_for_observer = throttle.clone();
    let rate_for_observer = Arc::clone(rate);
    let observer = FeedbackObserver::new(
        move || {
            if throttle_for_observer.request_throttled() {
                log::info!("收到 PLI/FIR，已请求 IDR 关键帧");
            }
        },
        move |bps| {
            rate_for_observer.report(bps as u32);
        },
    );

    let cc =
        CongestionControlBuilder::new(Gcc::new(start_bitrate, floor_bitrate, capacity_bitrate))
            .build();
    let registry = register_default_interceptors(Registry::new(), media_engine)?
        .with(Slot::CongestionControl, cc)
        .with(Slot::TwccSender, TwccSenderBuilder::new().build())
        .with(Slot::from(14_000), observer);

    Ok(registry)
}

/// WebRTC 传输层反馈观察者 (Feedback Observer)
///
/// 专用于在 RTCP 接收流与 Interceptor 消息链路上以非侵入方式旁路嗅探两项关键控制信号：
/// 1. 客户端 IDR 关键帧请求（RTCP PLI / FIR）
/// 2. GCC 拥塞控制算出的发送端即时目标码率（Attribute::TargetBitrateChanged）
///
/// 本结构体充当只读 Observer，不修改、不拦截、不丢弃任何传输包。
struct FeedbackObserver<FKeyframe, FRate> {
    on_keyframe: FKeyframe,
    on_bitrate: FRate,
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

impl<FKeyframe, FRate> FeedbackObserver<FKeyframe, FRate>
where
    FKeyframe: Fn() + Send + Sync + 'static,
    FRate: Fn(u64) + Send + Sync + 'static,
{
    fn new(on_keyframe: FKeyframe, on_bitrate: FRate) -> Self {
        Self {
            on_keyframe,
            on_bitrate,
            read_queue: VecDeque::new(),
            write_queue: VecDeque::new(),
        }
    }
}

impl<FKeyframe, FRate> Protocol<TaggedPacket, TaggedPacket, ()>
    for FeedbackObserver<FKeyframe, FRate>
where
    FKeyframe: Fn() + Send + Sync + 'static,
    FRate: Fn(u64) + Send + Sync + 'static,
{
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Some(Attribute::TargetBitrateChanged { bits_per_second }) =
            msg.message.get(&Attribute::TargetBitrateChanged {
                bits_per_second: 0.0,
            })
        {
            (self.on_bitrate)(bits_per_second.max(0.0) as u64);
        }

        if let Packet::Rtcp(packets) = &msg.message.packet {
            for packet in packets {
                let payload = packet.as_any();
                if payload.is::<PictureLossIndication>() || payload.is::<FullIntraRequest>() {
                    (self.on_keyframe)();
                }
            }
        }
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}

impl<FKeyframe, FRate> Interceptor for FeedbackObserver<FKeyframe, FRate>
where
    FKeyframe: Fn() + Send + Sync + 'static,
    FRate: Fn(u64) + Send + Sync + 'static,
{
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}

/// 从协商编解码列表解析指定编解码器的 PayloadType
pub(crate) fn resolve_codec_payload_type(
    codecs: &[RTCRtpCodecParameters],
    codec: VideoCodec,
) -> Option<u8> {
    let target_mime = codec.as_str().to_lowercase();
    let target_mime = target_mime.as_str();
    codecs
        .iter()
        .find(|c| {
            let mime = c.rtp_codec.mime_type.to_lowercase();
            mime.contains(target_mime) || (codec == VideoCodec::H265 && mime.contains("hevc"))
        })
        .or_else(|| codecs.first())
        .map(|c| c.payload_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::rtp_transceiver::rtp_sender::RTCRtpCodec;

    /// 恒定 60fps 投递：首帧占位 1ms，此后每帧恒 1500 ticks，600 帧零累积漂移
    #[test]
    fn test_tick_differ_constant_pacing_no_drift() {
        let mut differ = TickDiffer::new();
        // 首帧无前驱：1ms 护栏占位，不猜 1/fps
        assert_eq!(differ.advance(16_666_667), 90);
        for i in 2..=600u64 {
            let birth = i * 16_666_667;
            assert_eq!(differ.advance(birth), 1500, "第 {} 帧出现漂移", i);
        }
    }

    /// 真实卡顿 gap 原样透传（无策略上界），且基线不受输出钳制污染：恢复后即回正常节奏
    #[test]
    fn test_tick_differ_gaps_pass_through_unclamped() {
        let mut differ = TickDiffer::new();
        differ.advance(0);
        // 300ms 卡顿：300ms × 90kHz = 27000 ticks
        assert_eq!(differ.advance(300_000_000), 27_000);
        // 恢复后回到正常 delta（内部基线始终吃原始值）
        assert_eq!(differ.advance(316_666_667), 1500);
        // 3s 卡顿同样全量透传，绝不截断
        differ.advance(10_000_000_000);
        assert_eq!(differ.advance(13_000_000_000), 270_000);
    }

    /// 退化 delta 被下限吸收（1ms），重置后回到首帧占位行为
    #[test]
    fn test_tick_differ_floor_and_reset() {
        let mut differ = TickDiffer::new();
        // 首帧：与出生时刻无关的 1ms 占位
        assert_eq!(differ.advance(1_000), 90);
        // 同纳秒重复推进：输出 1ms 护栏值而非 0
        assert_eq!(differ.advance(1_000), 90);
        // 重建而非重置：下一帧回到首帧行为，不被上一次基线污染
        differ = TickDiffer::new();
        assert_eq!(differ.advance(500_000_000), 90);
    }

    /// 档位 interval 与打包器换算往返无损：as_secs_f64 × clock_rate 还原原始 tick
    #[test]
    fn test_ticks_to_duration_roundtrip_at_packetizer() {
        for fps in [30u32, 60, 120, 180, 240] {
            let ticks = VIDEO_CLOCK_RATE / fps;
            let duration = TickDiffer::ticks_to_duration(ticks);
            assert_eq!(
                (duration.as_secs_f64() * f64::from(VIDEO_CLOCK_RATE)) as u32,
                ticks,
                "fps {} 档位 interval 往返失真",
                fps
            );
        }
        // 非档位 tick 值与极端值同样无损（含 1ms 下限与 u32 上界）
        for ticks in [90u32, 375, 500, 977, 27_000, 90_000, u32::MAX] {
            let duration = TickDiffer::ticks_to_duration(ticks);
            assert_eq!(
                (duration.as_secs_f64() * f64::from(VIDEO_CLOCK_RATE)) as u32,
                ticks,
                "tick {} 往返失真",
                ticks
            );
        }
    }

    #[test]
    fn test_resolve_codec_payload_type() {
        let codec = |mime: &str, pt: u8| RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: mime.to_string(),
                clock_rate: VIDEO_CLOCK_RATE,
                channels: 0,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![],
            },
            payload_type: pt,
        };
        let h264 = codec("video/h264", 102);
        let hevc = codec("video/H265", 104);
        let av1 = codec("video/AV1", 106);
        let other = codec("video/VP8", 96);

        let list = [other.clone(), h264.clone(), hevc.clone(), av1.clone()];

        assert_eq!(
            resolve_codec_payload_type(&list, VideoCodec::H264),
            Some(102)
        );
        assert_eq!(
            resolve_codec_payload_type(&list, VideoCodec::H265),
            Some(104)
        );
        assert_eq!(
            resolve_codec_payload_type(&list, VideoCodec::Av1),
            Some(106)
        );

        assert_eq!(resolve_codec_payload_type(&[], VideoCodec::H264), None);
    }

    fn test_feedback_observer(
        throttle: KeyframeThrottle,
    ) -> FeedbackObserver<impl Fn(), impl Fn(u64)> {
        FeedbackObserver::new(
            move || {
                let _ = throttle.request_throttled();
            },
            |_| {},
        )
    }

    #[test]
    fn test_feedback_observer_throttles_pli_storm() {
        let trigger = KeyframeTrigger::new();
        let throttle = KeyframeThrottle::new(trigger.clone());
        let mut observer = test_feedback_observer(throttle);
        let mk_pli = || TaggedPacket {
            now: Instant::now(),
            transport: Default::default(),
            message: rtc::interceptor::AttributedPacket {
                attributes: vec![],
                packet: Packet::Rtcp(vec![Box::new(PictureLossIndication {
                    sender_ssrc: 0,
                    media_ssrc: VIDEO_TRACK_SSRC,
                })]),
            },
        };

        trigger.take();

        assert!(observer.handle_read(mk_pli()).is_ok());
        assert!(trigger.take(), "首个 PLI 应触发 request_keyframe");

        assert!(observer.handle_read(mk_pli()).is_ok());
        assert!(observer.handle_read(mk_pli()).is_ok());
        assert!(!trigger.take(), "节流窗内的 PLI 不得再触发 IDR");
    }

    /// 并发请求最终只有单一赢家
    ///
    /// 钉住 CAS 方案的核心性质：并发请求的赢家由「唯一成功者」保证。同时也钉住
    /// 手写 Clone 的共享窗口行为——
    /// 若 clone 误复制时间戳，各线程窗口相互独立，赢家会变成 8 个。
    #[test]
    fn test_concurrent_requests_admit_single_winner() {
        use std::sync::Barrier;
        use std::thread;

        let trigger = KeyframeTrigger::new();
        let throttle = KeyframeThrottle::new(trigger.clone());
        trigger.take();

        const THREADS: usize = 8;
        let barrier = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let throttle = throttle.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    throttle.request_throttled()
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1, "同一窗口内的并发请求必须只剩单一赢家");
    }

    #[test]
    fn test_interceptor_chain_carries_send_side_congestion_control() {
        let mut media_engine = MediaEngine::default();
        let registry = build_interceptor_registry(
            &mut media_engine,
            &Arc::new(RateControl::new(20_000_000, |_| {})),
            &KeyframeThrottle::new(KeyframeTrigger::new()),
        )
        .unwrap();
        let slots: Vec<Slot> = registry.slots().into_iter().map(|(slot, _)| slot).collect();

        for expected in [
            Slot::CongestionControl,
            Slot::TwccSender,
            Slot::TwccReceiver,
            Slot::from(14_000),
        ] {
            assert!(
                slots.contains(&expected),
                "拦截器链缺少 {expected:?}，实际 {slots:?}"
            );
        }
    }
}

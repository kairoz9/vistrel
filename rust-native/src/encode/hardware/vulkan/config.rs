//! 编码会话的静态配置与运行期码控状态
//!
//! [`SessionConfig`] 在会话创建前一次算好：格式、对齐后的 extent、DPB、level、
//! 序列头参数、码流缓冲、GOP 与 intra-refresh 周期。运行期会变的码率/帧率、
//! applied 快照与重发标记归 [`CodingControl`]，静态上限来自 [`RateControlLimits`]。
//!
//! 探测结果 [`CodecCaps`] 只用于构造 [`SessionConfig`]；`VideoSession` 与
//! `FrameEncoder` 不再直接读驱动能力字段。

use ash::vk;

use super::caps::{CodecCaps, CodecSpecific};
use super::frame_encoder::{frame_num_bits, level_enum};
use crate::encode::{StreamingProfile, VideoCodec, nv12_len};

/// 编码输入图格式：NV12（两平面 4:2:0）
const PICTURE_FORMAT: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;

/// 启用 intra-refresh 时的 GOP 长度：12 位 `frame_num` 的上界
///
/// 不能取 16 位的 65535——驱动一旦静默忽略 intra-refresh，解码错误会残留到下一个 IDR。
const INTRA_REFRESH_GOP: u32 = 4095;

/// 默认 CBR 虚拟缓冲长度（毫秒）
const DEFAULT_VBV_MS: u32 = 500;

/// 会话级策略：调用方希望怎么编码，与驱动能力无关
#[derive(Debug, Clone, Copy)]
pub struct EncodePolicy {
    /// intra-refresh 启用策略
    pub intra_refresh: IntraRefreshPolicy,
    /// GOP 长度策略
    pub gop: GopPolicy,
    /// CBR 虚拟缓冲长度（毫秒）
    pub vbv_ms: u32,
}

impl Default for EncodePolicy {
    fn default() -> Self {
        Self {
            intra_refresh: IntraRefreshPolicy::Prefer,
            gop: GopPolicy::Auto,
            vbv_ms: DEFAULT_VBV_MS,
        }
    }
}

/// intra-refresh 的启用策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntraRefreshPolicy {
    /// 驱动支持就启用，周期取 fps/2 并按驱动上限封顶
    Prefer,
    /// 显式周期；驱动不支持或超出范围时构造失败
    #[allow(dead_code)] // 生产只用 Prefer；测试与调用方按需构造
    Fixed(u32),
    /// 不启用
    #[allow(dead_code)] // 生产只用 Prefer；测试与调用方按需构造
    Disable,
}

/// GOP 策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GopPolicy {
    /// 无 intra-refresh 时 1 秒一个 IDR；启用时用 [`INTRA_REFRESH_GOP`]
    Auto,
    /// 固定 GOP 长度
    #[allow(dead_code)] // 生产只用 Auto；测试与调用方按需构造
    Fixed(u32),
}

/// 静态会话配置：建会话后不再变化
#[derive(Debug, Clone)]
pub struct SessionConfig {
    queue_family: Option<u32>,
    picture_format: vk::Format,
    coded_extent: vk::Extent2D,
    display_extent: vk::Extent2D,
    image_extent: vk::Extent2D,
    dpb: DpbConfig,
    bitstream_size: u64,
    sequence: SequenceConfig,
    rate_limits: RateControlLimits,
    bitrate_bps: u64,
    gop_length: u32,
    intra_refresh_cycle: Option<u32>,
    gop_remaining_frames: bool,
    feedback_has_overrides: bool,
    std_header_version: vk::ExtensionProperties,
}

impl SessionConfig {
    /// 把驱动能力、协商档位与策略煮成静态会话配置
    ///
    /// 所有钳制、level 门禁、DPB 与 GOP/intra-refresh 决策都在这里完成。
    /// `device_supports_intra_refresh` 来自逻辑设备 feature 位，不能只看能力查询。
    pub fn build(
        caps: &CodecCaps,
        profile: &StreamingProfile,
        policy: &EncodePolicy,
        device_supports_intra_refresh: bool,
    ) -> Result<Self, String> {
        caps.validate()?;
        let codec = caps.codec();
        let width = profile.width() as u32;
        let height = profile.height() as u32;
        let fps = profile.fps() as u32;
        let display_extent = vk::Extent2D { width, height };
        let coded_extent = caps.align_extent(width, height)?;
        let dpb = DpbConfig::new(
            codec,
            caps.max_dpb_slots,
            caps.max_active_reference_pictures,
        );

        let intra_refresh_cycle = match policy.intra_refresh {
            IntraRefreshPolicy::Disable => None,
            IntraRefreshPolicy::Prefer => {
                if caps.supports_intra_refresh() && device_supports_intra_refresh {
                    Some((fps / 2).max(2).min(caps.intra_refresh.max_cycle_duration))
                } else {
                    None
                }
            }
            IntraRefreshPolicy::Fixed(cycle) => {
                if !caps.supports_intra_refresh() || !device_supports_intra_refresh {
                    return Err(
                        "驱动未提供 intra-refresh（能力查询或设备 feature 位不成立），不能启用"
                            .to_string(),
                    );
                }
                if cycle < 2 || cycle > caps.intra_refresh.max_cycle_duration {
                    return Err(format!(
                        "intra-refresh 周期必须在 2..={} 之间，收到 {}",
                        caps.intra_refresh.max_cycle_duration, cycle
                    ));
                }
                Some(cycle)
            }
        };

        let gop_length = match policy.gop {
            GopPolicy::Fixed(gop) if gop >= 2 => gop,
            GopPolicy::Fixed(gop) => {
                return Err(format!("GOP 长度必须 ≥ 2，收到 {gop}"));
            }
            GopPolicy::Auto => {
                if intra_refresh_cycle.is_some() {
                    INTRA_REFRESH_GOP
                } else {
                    fps.max(2)
                }
            }
        };

        let rate_limits = RateControlLimits::from_caps(caps, policy.vbv_ms);
        let requested_bitrate = profile.bitrate() as u64;
        let bitrate_bps = rate_limits.clamp_bitrate(requested_bitrate);
        if bitrate_bps != requested_bitrate {
            log::warn!(
                "请求码率 {requested_bitrate} bps 超过驱动上限 {} bps，已钳制",
                rate_limits.max_bitrate()
            );
        }

        let level = crate::encode::profile_level(
            codec,
            profile.width(),
            profile.height(),
            profile.fps(),
            (bitrate_bps / 1000) as u32,
        );
        let level_vk = level_enum(codec, level)?;
        if level_vk > caps.max_level() {
            return Err(format!(
                "{codec:?} 档位 {}x{}@{} 需要 level 枚举 {}（码流值 {}），超过驱动声明的上限 {}",
                profile.width(),
                profile.height(),
                profile.fps(),
                level_vk,
                level,
                caps.max_level()
            ));
        }

        let sequence = match codec {
            VideoCodec::H264 => SequenceConfig::H264(H264SequenceConfig {
                level_idc: level,
                frame_num_bits: frame_num_bits(gop_length),
            }),
            VideoCodec::H265 => SequenceConfig::H265(H265SequenceConfig {
                level_idc: level,
                frame_num_bits: frame_num_bits(gop_length),
            }),
            VideoCodec::Av1 => {
                let specific = match &caps.specific {
                    CodecSpecific::Av1(specific) => specific,
                    _ => unreachable!("codec 与能力快照变体不一致"),
                };
                SequenceConfig::Av1(Av1SequenceConfig {
                    level_idx: level,
                    superblock_128: specific.av1_use_128x128_superblock(),
                    operating_point_count: u32::from(specific.max_operating_points > 0),
                })
            }
        };

        let frame_bytes =
            nv12_len(coded_extent.width as usize, coded_extent.height as usize) as u64;
        let raw_size = frame_bytes.max(bitrate_bps / 8 * 2);
        let bitstream_size = raw_size.next_multiple_of(caps.min_bitstream_size_alignment.max(1));
        let pad = caps.input_pad(coded_extent);
        if pad.dropped {
            log::warn!(
                "{codec:?} 输入 padding 超出图像上限 maxImageDimension2D={}，\
                 超限轴退回 coded extent；实际分配 {}x{}",
                caps.max_image_dimension,
                pad.extent.width,
                pad.extent.height
            );
        } else if pad.extent != coded_extent {
            log::debug!(
                "{codec:?} 输入图按 encodeInputPictureGranularity 补齐: coded {}x{} → 分配 {}x{}",
                coded_extent.width,
                coded_extent.height,
                pad.extent.width,
                pad.extent.height
            );
        }

        Ok(Self {
            queue_family: caps.queue_family,
            picture_format: PICTURE_FORMAT,
            coded_extent,
            display_extent,
            image_extent: pad.extent,
            dpb,
            bitstream_size,
            sequence,
            rate_limits,
            bitrate_bps,
            gop_length,
            intra_refresh_cycle,
            gop_remaining_frames: caps.wants_gop_remaining_frames(),
            feedback_has_overrides: caps
                .feedback_flags
                .contains(vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_HAS_OVERRIDES),
            std_header_version: caps.std_header_version,
        })
    }

    pub fn codec(&self) -> VideoCodec {
        self.sequence.codec()
    }

    pub fn queue_family(&self) -> Option<u32> {
        self.queue_family
    }

    pub fn picture_format(&self) -> vk::Format {
        self.picture_format
    }

    pub fn coded_extent(&self) -> vk::Extent2D {
        self.coded_extent
    }

    pub fn display_extent(&self) -> vk::Extent2D {
        self.display_extent
    }

    pub fn image_extent(&self) -> vk::Extent2D {
        self.image_extent
    }

    pub fn dpb_slots(&self) -> u32 {
        self.dpb.slots
    }

    pub fn active_references(&self) -> u32 {
        self.dpb.active_references
    }

    pub fn sequence(&self) -> &SequenceConfig {
        &self.sequence
    }

    pub fn gop_length(&self) -> u32 {
        self.gop_length
    }

    pub fn intra_refresh_cycle(&self) -> Option<u32> {
        self.intra_refresh_cycle
    }

    pub fn gop_remaining_frames(&self) -> bool {
        self.gop_remaining_frames
    }

    pub fn feedback_has_overrides(&self) -> bool {
        self.feedback_has_overrides
    }

    pub fn std_header_version(&self) -> &vk::ExtensionProperties {
        &self.std_header_version
    }

    pub fn bitstream_size(&self) -> u64 {
        self.bitstream_size
    }

    /// 用这份静态配置初始化运行期码控
    pub fn coding_control(&self, fps: u32) -> CodingControl {
        CodingControl::new(self.rate_limits, self.bitrate_bps, fps)
    }
}

/// DPB 槽位与活跃参考数：AV1 声明 8 个逻辑槽，H.26x 只要当前帧 + 1 参考
#[derive(Debug, Clone, Copy)]
struct DpbConfig {
    slots: u32,
    active_references: u32,
}

impl DpbConfig {
    /// `caps.validate()` 已保证 `max_slots >= 2`、`max_active >= 1`
    fn new(codec: VideoCodec, max_slots: u32, max_active: u32) -> Self {
        let slots = max_slots.min(if matches!(codec, VideoCodec::Av1) {
            8
        } else {
            2
        });
        let active_references = max_active.min(if matches!(codec, VideoCodec::Av1) {
            slots
        } else {
            1
        });
        Self {
            slots,
            active_references,
        }
    }
}

/// codec 专属的静态序列头参数
#[derive(Debug, Clone, Copy)]
pub enum SequenceConfig {
    H264(H264SequenceConfig),
    H265(H265SequenceConfig),
    Av1(Av1SequenceConfig),
}

impl SequenceConfig {
    pub fn codec(&self) -> VideoCodec {
        match self {
            Self::H264(_) => VideoCodec::H264,
            Self::H265(_) => VideoCodec::H265,
            Self::Av1(_) => VideoCodec::Av1,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct H264SequenceConfig {
    pub(super) level_idc: u32,
    pub(super) frame_num_bits: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct H265SequenceConfig {
    pub(super) level_idc: u32,
    pub(super) frame_num_bits: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Av1SequenceConfig {
    pub(super) level_idx: u32,
    pub(super) superblock_128: bool,
    pub(super) operating_point_count: u32,
}

/// 码控的静态上限：驱动码率上限与量化范围
#[derive(Debug, Clone, Copy)]
pub struct RateControlLimits {
    max_bitrate: u64,
    quant: QuantRange,
    vbv_ms: u32,
}

impl RateControlLimits {
    fn from_caps(caps: &CodecCaps, vbv_ms: u32) -> Self {
        Self {
            max_bitrate: caps.max_bitrate,
            quant: QuantRange::from_caps(caps),
            vbv_ms,
        }
    }

    pub fn max_bitrate(&self) -> u64 {
        self.max_bitrate
    }

    pub fn quant(&self) -> QuantRange {
        self.quant
    }

    pub fn vbv_ms(&self) -> u32 {
        self.vbv_ms
    }

    /// 0 表示驱动未设上限
    pub fn clamp_bitrate(&self, bps: u64) -> u64 {
        if self.max_bitrate > 0 && bps > self.max_bitrate {
            self.max_bitrate
        } else {
            bps
        }
    }
}

/// 码控的量化范围：H.26x 用 QP，AV1 用量化索引，两者数值域不同
#[derive(Debug, Clone, Copy)]
pub enum QuantRange {
    Qp { min: i32, max: i32 },
    QIndex { min: u32, max: u32 },
}

impl QuantRange {
    fn from_caps(caps: &CodecCaps) -> Self {
        match &caps.specific {
            CodecSpecific::H264(specific) | CodecSpecific::H265(specific) => Self::Qp {
                min: specific.min_qp,
                max: specific.max_qp,
            },
            CodecSpecific::Av1(specific) => Self::QIndex {
                min: specific.min_q_index,
                max: specific.max_q_index,
            },
        }
    }

    /// 只由 H.264/H.265 路径调用：码控分派按 codec 走，AV1 不进这里
    pub fn qp_range(&self, floor: i32) -> (i32, i32) {
        let (min, max) = match self {
            Self::Qp { min, max } => (*min, *max),
            Self::QIndex { .. } => unreachable!("AV1 的量化索引范围不适用 QP 门禁"),
        };
        let lo = min.min(max);
        let hi = min.max(max);
        let min = floor.clamp(lo, hi);
        let max = 51.min(hi).max(min);
        (min, max)
    }

    /// 只由 AV1 路径调用
    pub fn q_index_range(&self) -> (u32, u32) {
        match self {
            Self::QIndex { min, max } => (*min, *max),
            Self::Qp { .. } => unreachable!("H.26x 的 QP 范围不适用 AV1 量化索引"),
        }
    }
}

/// 上一次真正配置进会话的码控值
///
/// begin 的 `VkVideoEncodeRateControlInfoKHR` 必须与已配置状态逐字段一致（08254），
/// 因此单独留存；当前值在作用域内再配进去。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppliedRateControl {
    pub bitrate_bps: u64,
    pub fps: u32,
}

/// 运行期码控状态
#[derive(Debug)]
pub struct CodingControl {
    limits: RateControlLimits,
    bitrate_bps: u64,
    fps: u32,
    applied: Option<AppliedRateControl>,
    dirty: bool,
}

impl CodingControl {
    fn new(limits: RateControlLimits, requested_bitrate: u64, fps: u32) -> Self {
        Self {
            limits,
            bitrate_bps: limits.clamp_bitrate(requested_bitrate),
            fps,
            applied: None,
            dirty: true,
        }
    }

    pub fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    pub fn vbv_ms(&self) -> u32 {
        self.limits.vbv_ms()
    }

    pub fn qp_range(&self, floor: i32) -> (i32, i32) {
        self.limits.quant().qp_range(floor)
    }

    pub fn q_index_range(&self) -> (u32, u32) {
        self.limits.quant().q_index_range()
    }

    /// 运行期改码率：下一帧随码控命令重发，不需要重建会话
    ///
    /// 先钳制再比较：请求超出驱动上限、但落值等于当前值时不算变更，不重发码控。
    pub fn set_bitrate(&mut self, bps: u64) {
        if bps == 0 {
            return;
        }
        let bps = self.limits.clamp_bitrate(bps);
        if bps == self.bitrate_bps {
            return;
        }
        self.bitrate_bps = bps;
        self.dirty = true;
    }

    /// 运行期改帧率：码控每帧都带 `frame_rate_numerator`，改字段 + 置脏即可生效
    pub fn set_fps(&mut self, fps: u32) {
        if fps == 0 || fps == self.fps {
            return;
        }
        self.fps = fps;
        self.dirty = true;
    }

    /// 首帧或码率/帧率变更后需要重新下发码控
    pub fn needs_config(&self) -> bool {
        self.dirty || self.applied.is_none()
    }

    pub fn has_applied(&self) -> bool {
        self.applied.is_some()
    }

    pub fn applied(&self) -> Option<AppliedRateControl> {
        self.applied
    }

    /// 编码成功后记录本次真正配置进会话的值
    pub fn mark_applied(&mut self) {
        self.applied = Some(AppliedRateControl {
            bitrate_bps: self.bitrate_bps,
            fps: self.fps,
        });
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::super::caps::{H26xCaps, IntraRefreshCaps};
    use super::*;

    /// 最小可用的 H.264 能力快照：1080p60 → level 4.2，上限 5.2（枚举 15）
    fn h264_caps() -> CodecCaps {
        CodecCaps {
            queue_family: Some(0),
            max_coded_extent: vk::Extent2D {
                width: 4096,
                height: 4096,
            },
            max_image_dimension: 16384,
            picture_access_granularity: vk::Extent2D {
                width: 16,
                height: 16,
            },
            min_bitstream_size_alignment: 1,
            max_dpb_slots: 8,
            max_active_reference_pictures: 4,
            max_bitrate: 20_000_000,
            rate_control_modes: vk::VideoEncodeRateControlModeFlagsKHR::CBR,
            feedback_flags: vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
            specific: CodecSpecific::H264(H26xCaps {
                max_level_idc: 15,
                min_qp: 10,
                max_qp: 40,
                max_p_picture_l0_references: 1,
                max_slice_count: 1,
                ..H26xCaps::default()
            }),
            ..CodecCaps::default()
        }
    }

    fn with_intra_refresh(mut caps: CodecCaps) -> CodecCaps {
        caps.intra_refresh = IntraRefreshCaps {
            modes: vk::VideoEncodeIntraRefreshModeFlagsKHR::PER_PICTURE_PARTITION,
            max_cycle_duration: 120,
            max_active_reference_pictures: 1,
        };
        caps
    }

    fn profile() -> StreamingProfile {
        StreamingProfile::new(1920, 1080, 60, 8_000_000)
    }

    fn build(
        caps: &CodecCaps,
        policy: &EncodePolicy,
        device_intra: bool,
    ) -> Result<SessionConfig, String> {
        SessionConfig::build(caps, &profile(), policy, device_intra)
    }

    /// 码率先钳制再参与 level 推导与码控初值；DPB/参考帧按策略取小
    #[test]
    fn build_clamps_bitrate_and_derives_sequence() {
        let mut caps = h264_caps();
        caps.max_bitrate = 5_000_000;
        let config = build(&caps, &EncodePolicy::default(), false).expect("配置应成立");

        assert_eq!(config.gop_length(), 60);
        assert_eq!(config.intra_refresh_cycle(), None);
        assert_eq!(config.coded_extent().height, 1088, "1080 按 16 粒度对齐");
        assert_eq!(config.dpb_slots(), 2);
        assert_eq!(config.active_references(), 1);

        let mut coding = config.coding_control(60);
        assert_eq!(coding.bitrate_bps(), 5_000_000, "超过驱动上限必须钳制");
        assert!(coding.needs_config(), "首帧必须下发码控");
        coding.mark_applied();
        assert!(!coding.needs_config());

        match config.sequence() {
            SequenceConfig::H264(sequence) => {
                assert_eq!(
                    sequence.level_idc,
                    u32::from(crate::encode::h264_level_idc(1920, 1080, 60, 5_000))
                );
                assert_eq!(sequence.frame_num_bits, 6, "60 帧 GOP 需要 61 个不重复值");
            }
            other => panic!("能力快照是 H.264，序列配置却是 {other:?}"),
        }
    }

    /// intra-refresh 要能力三项与设备 feature 位同时成立；GOP 随启用状态切换
    #[test]
    fn intra_refresh_requires_device_feature() {
        let caps = with_intra_refresh(h264_caps());
        let policy = EncodePolicy::default();

        let without_feature = build(&caps, &policy, false).expect("配置应成立");
        assert_eq!(without_feature.intra_refresh_cycle(), None);
        assert_eq!(without_feature.gop_length(), 60);

        let with_feature = build(&caps, &policy, true).expect("配置应成立");
        assert_eq!(with_feature.intra_refresh_cycle(), Some(30), "周期取 fps/2");
        assert_eq!(with_feature.gop_length(), INTRA_REFRESH_GOP);
    }

    /// 显式周期必须在 2..=驱动上限内，超出直接拒绝而不是截断
    #[test]
    fn fixed_intra_refresh_rejects_out_of_range() {
        let caps = with_intra_refresh(h264_caps());
        let policy = |cycle| EncodePolicy {
            intra_refresh: IntraRefreshPolicy::Fixed(cycle),
            ..Default::default()
        };

        assert!(build(&caps, &policy(1), true).is_err(), "周期 < 2 必须拒绝");
        assert!(
            build(&caps, &policy(121), true).is_err(),
            "超过驱动上限必须拒绝"
        );
        assert!(
            build(&caps, &policy(30), false).is_err(),
            "设备 feature 位未开启时必须拒绝"
        );
        assert_eq!(
            build(&caps, &policy(30), true)
                .expect("合法周期应成立")
                .intra_refresh_cycle(),
            Some(30)
        );
    }

    /// GOP < 2 不成参考链，必须在构造期拒绝
    #[test]
    fn fixed_gop_rejects_too_short() {
        let caps = h264_caps();
        let policy = EncodePolicy {
            gop: GopPolicy::Fixed(1),
            ..Default::default()
        };
        assert!(build(&caps, &policy, false).is_err());
    }

    /// level 门禁按内容推导；`validate()` 的拒绝仍先于门禁
    #[test]
    fn level_gate_rejects_above_driver_limit() {
        let mut caps = h264_caps();
        if let CodecSpecific::H264(specific) = &mut caps.specific {
            specific.max_level_idc = 11; // 4.1 装不下 1080p60
        }
        let error =
            build(&caps, &EncodePolicy::default(), false).expect_err("1080p60 超过 4.1 必须拒绝");
        assert!(error.contains("超过驱动声明的上限"), "{error}");

        caps.queue_family = None;
        let error =
            build(&caps, &EncodePolicy::default(), false).expect_err("没有编码队列族必须拒绝");
        assert!(
            error.contains("没有可用的编码队列族"),
            "validate() 必须先于 level 门禁拒绝: {error}"
        );
    }

    /// 运行期码控只在值真的变化时置脏，钳制与 applied 快照同步
    #[test]
    fn coding_control_marks_dirty_only_on_change() {
        let config = build(&h264_caps(), &EncodePolicy::default(), false).expect("配置应成立");
        let mut coding = config.coding_control(60);
        assert_eq!(
            coding.applied(),
            None,
            "首帧前没有已配置状态，begin 不能带码控"
        );

        coding.mark_applied();
        assert_eq!(
            coding.applied(),
            Some(AppliedRateControl {
                bitrate_bps: 8_000_000,
                fps: 60
            })
        );
        assert!(!coding.needs_config());

        coding.set_bitrate(25_000_000);
        assert_eq!(coding.bitrate_bps(), 20_000_000, "运行期改码率同样钳制");
        assert!(coding.needs_config());

        coding.mark_applied();
        coding.set_bitrate(25_000_000);
        assert_eq!(coding.bitrate_bps(), 20_000_000);
        assert!(!coding.needs_config(), "钳制到当前值不应重发码控");
        coding.set_bitrate(20_000_000);
        assert!(!coding.needs_config(), "值未变化不应重发码控");

        coding.set_fps(30);
        assert!(coding.needs_config());
        assert_eq!(
            coding.applied().map(|a| a.fps),
            Some(60),
            "applied 记录的是上次真正下发的值"
        );
    }

    /// 量化范围按 codec 分域，QP 上限再夹到 H.26x 的 51
    #[test]
    fn quant_range_clamps_by_codec_domain() {
        let qp = QuantRange::Qp { min: 10, max: 40 };
        assert_eq!(qp.qp_range(0), (10, 40));
        assert_eq!(qp.qp_range(45), (40, 40));
        assert_eq!(QuantRange::Qp { min: 10, max: 60 }.qp_range(55), (55, 55));

        let q_index = QuantRange::QIndex { min: 2, max: 7 };
        assert_eq!(q_index.q_index_range(), (2, 7));
    }
}

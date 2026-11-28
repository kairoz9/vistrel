//! codec 专属实现的收口点
//!
//! 对共享层只暴露枚举与 dispatch：能力快照与探测（`caps`）、会话配置（`config`）、
//! 参数集对象（`parameters`）、运行期码控（`rate_control`）都在本目录；序列头/参数集
//! 构造、profile 取值、设备 feature 与逐帧状态按 codec 收在 `h264` / `h265` / `av1`。
//! 本文件只做变体定义、跨 codec 再导出与调度。

use ash::vk::{self, TaggedStructure};

use self::av1::caps::Av1Caps;
use self::h26x::H26xCaps;
use crate::encode::VideoCodec;

pub(in crate::encode::hardware::vulkan) mod av1;
pub(crate) mod caps;
pub(in crate::encode::hardware::vulkan) mod config;
pub(in crate::encode::hardware::vulkan) mod h264;
pub(in crate::encode::hardware::vulkan) mod h265;
pub(in crate::encode::hardware::vulkan) mod h26x;
pub(in crate::encode::hardware::vulkan) mod parameters;
pub(in crate::encode::hardware::vulkan) mod rate_control;

pub(in crate::encode::hardware::vulkan) use self::av1::SequenceConfig as Av1SequenceConfig;
pub(in crate::encode::hardware::vulkan) use self::h264::SequenceConfig as H264SequenceConfig;
pub(in crate::encode::hardware::vulkan) use self::h265::SequenceConfig as H265SequenceConfig;
pub(in crate::encode::hardware::vulkan) use self::parameters::VideoSessionParameters;
pub(in crate::encode::hardware::vulkan) use self::rate_control::{
    CodecGopRemaining, CodecRateControl, QuantRange,
};

/// 建 `VkDevice` 时需要链进 `VkDeviceCreateInfo` 的 codec feature
///
/// 目前只有 AV1 有必须显式开启的 feature 结构；H.264 / H.265 在本 ash rev 里没有生成
/// 对应结构，且实测缺该位也能用。保留空变体是为了让 device 建设路径对 codec 完全无感。
pub(in crate::encode::hardware::vulkan) enum DeviceFeatures {
    H264,
    H265,
    Av1(self::av1::device::Features),
}

impl DeviceFeatures {
    pub(in crate::encode::hardware::vulkan) fn new(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::H264 => Self::H264,
            VideoCodec::H265 => Self::H265,
            VideoCodec::Av1 => Self::Av1(self::av1::device::Features::new()),
        }
    }

    /// 把 codec feature 结构挂进物理设备查询链
    pub(in crate::encode::hardware::vulkan) fn push_query<'a>(
        &'a mut self,
        chain: vk::PhysicalDeviceFeatures2<'a>,
    ) -> vk::PhysicalDeviceFeatures2<'a> {
        match self {
            Self::Av1(features) => features.push_query(chain),
            Self::H264 | Self::H265 => chain,
        }
    }

    /// 查询后的收敛与准入判定
    pub(in crate::encode::hardware::vulkan) fn finish(&mut self) {
        if let Self::Av1(features) = self {
            features.finish();
        }
    }

    /// 把真正支持的 feature 位挂进设备创建链
    pub(in crate::encode::hardware::vulkan) fn push_enable<'a>(
        &'a mut self,
        info: vk::DeviceCreateInfo<'a>,
    ) -> vk::DeviceCreateInfo<'a> {
        match self {
            Self::Av1(features) => features.push_enable(info),
            Self::H264 | Self::H265 => info,
        }
    }
}

/// codec 专属的静态序列头参数
#[derive(Debug, Clone, Copy)]
pub(in crate::encode::hardware::vulkan) enum SequenceConfig {
    H264(H264SequenceConfig),
    H265(H265SequenceConfig),
    Av1(Av1SequenceConfig),
}

impl SequenceConfig {
    pub(in crate::encode::hardware::vulkan) fn codec(&self) -> VideoCodec {
        match self {
            Self::H264(_) => VideoCodec::H264,
            Self::H265(_) => VideoCodec::H265,
            Self::Av1(_) => VideoCodec::Av1,
        }
    }
}

/// codec 专属的 profile 子结构
pub(in crate::encode::hardware::vulkan) enum ProfilePart {
    H264(vk::VideoEncodeH264ProfileInfoKHR<'static>),
    H265(vk::VideoEncodeH265ProfileInfoKHR<'static>),
    Av1(vk::VideoEncodeAV1ProfileInfoKHR<'static>),
}

impl ProfilePart {
    /// 按 codec 取对应的 profile 子结构
    ///
    /// profile 取值必须与码流侧一致：H.264 High（100）、H.265 Main（1）、AV1 Main（0）。
    pub(in crate::encode::hardware::vulkan) fn new(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::H264 => Self::H264(h264::profile_info()),
            VideoCodec::H265 => Self::H265(h265::profile_info()),
            VideoCodec::Av1 => Self::Av1(av1::profile_info()),
        }
    }

    /// 把 codec 子结构挂到 profile 链上
    pub(in crate::encode::hardware::vulkan) fn attach<'a>(
        &'a mut self,
        profile: vk::VideoProfileInfoKHR<'a>,
    ) -> vk::VideoProfileInfoKHR<'a> {
        match self {
            Self::H264(part) => profile.push(part),
            Self::H265(part) => profile.push(part),
            Self::Av1(part) => profile.push(part),
        }
    }
}

/// 4:2:0 / 8 bit 的 profile 基底
///
/// 三个 codec 只有编码操作位不同，色度抽样与位深一致。
///
/// 返回类型故意不写 `'static`：`attach` 要求 profile 与 `&mut self` 共用同一个生命周期，
/// 写成 `'static` 会逼着编译器去找 `&'static mut ProfilePart`，而那是拿不到的。
pub(in crate::encode::hardware::vulkan) fn base_profile<'a>(
    codec: VideoCodec,
) -> vk::VideoProfileInfoKHR<'a> {
    vk::VideoProfileInfoKHR::default()
        .video_codec_operation(codec_encode_operation(codec))
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
}

/// 项目 codec 到 Vulkan 编码操作位的映射
pub(in crate::encode::hardware::vulkan) fn codec_encode_operation(
    codec: VideoCodec,
) -> vk::VideoCodecOperationFlagsKHR {
    match codec {
        VideoCodec::H264 => vk::VideoCodecOperationFlagsKHR::ENCODE_H264,
        VideoCodec::H265 => vk::VideoCodecOperationFlagsKHR::ENCODE_H265,
        VideoCodec::Av1 => vk::VideoCodecOperationFlagsKHR::ENCODE_AV1,
    }
}

/// 项目 codec 对应的设备扩展名
pub(in crate::encode::hardware::vulkan) fn codec_encode_extension(
    codec: VideoCodec,
) -> &'static str {
    match codec {
        VideoCodec::H264 => "VK_KHR_video_encode_h264",
        VideoCodec::H265 => "VK_KHR_video_encode_h265",
        VideoCodec::Av1 => "VK_KHR_video_encode_av1",
    }
}

/// codec 专属能力：互斥字段按 codec 分家，靠类型排除错配
#[derive(Debug, Clone)]
pub(in crate::encode::hardware::vulkan) enum CodecSpecific {
    H264(H26xCaps),
    H265(H26xCaps),
    Av1(Av1Caps),
}

impl Default for CodecSpecific {
    fn default() -> Self {
        Self::H264(H26xCaps::default())
    }
}

impl CodecSpecific {
    /// 本快照对应的 codec
    pub(in crate::encode::hardware::vulkan) fn codec(&self) -> VideoCodec {
        match self {
            Self::H264(_) => VideoCodec::H264,
            Self::H265(_) => VideoCodec::H265,
            Self::Av1(_) => VideoCodec::Av1,
        }
    }

    /// 建会话前的 codec 专属准入校验
    pub(in crate::encode::hardware::vulkan) fn validate(&self) -> Result<(), String> {
        match self {
            Self::H264(caps) => caps.validate(VideoCodec::H264),
            Self::H265(caps) => caps.validate(VideoCodec::H265),
            Self::Av1(caps) => caps.validate(),
        }
    }

    /// 驱动是否要求或偏好 begin 时给出 GOP 剩余帧数
    pub(in crate::encode::hardware::vulkan) fn wants_gop_remaining_frames(&self) -> bool {
        match self {
            Self::H264(caps) | Self::H265(caps) => caps.wants_gop_remaining_frames(),
            Self::Av1(caps) => caps.wants_gop_remaining_frames(),
        }
    }

    /// 驱动声明的最高 level，与 `codec::level_enum` 的输出同域
    pub(in crate::encode::hardware::vulkan) fn max_level(&self) -> u32 {
        match self {
            Self::H264(caps) | Self::H265(caps) => caps.max_level_idc,
            Self::Av1(caps) => caps.max_level,
        }
    }

    /// AV1 的 coded picture 对齐下限；H.26x 没有这个额外约束
    pub(in crate::encode::hardware::vulkan) fn coded_alignment(&self) -> vk::Extent2D {
        match self {
            Self::H264(_) | Self::H265(_) => vk::Extent2D::default(),
            Self::Av1(caps) => caps.coded_picture_alignment,
        }
    }
}

impl CodecSpecific {
    /// 按 codec 生成静态序列头配置
    pub(in crate::encode::hardware::vulkan) fn sequence_config(
        &self,
        level: u32,
        gop_length: u32,
    ) -> SequenceConfig {
        let frame_num_bits = self::h26x::frame_num_bits(gop_length);
        match self {
            Self::H264(_) => SequenceConfig::H264(h264::SequenceConfig::new(level, frame_num_bits)),
            Self::H265(_) => SequenceConfig::H265(h265::SequenceConfig::new(level, frame_num_bits)),
            Self::Av1(caps) => SequenceConfig::Av1(av1::SequenceConfig::from_caps(level, caps)),
        }
    }

    /// 当前 codec 的量化范围
    pub(in crate::encode::hardware::vulkan) fn quant_range(&self) -> QuantRange {
        match self {
            Self::H264(caps) | Self::H265(caps) => QuantRange::Qp {
                min: caps.min_qp,
                max: caps.max_qp,
            },
            Self::Av1(caps) => QuantRange::QIndex {
                min: caps.min_q_index,
                max: caps.max_q_index,
            },
        }
    }

    /// DPB 槽位与活跃参考数：AV1 声明 8 个逻辑槽，H.26x 只要当前帧 + 1 参考
    pub(in crate::encode::hardware::vulkan) fn dpb_limits(
        &self,
        max_slots: u32,
        max_active: u32,
    ) -> (u32, u32) {
        match self {
            Self::H264(caps) | Self::H265(caps) => caps.dpb_limits(max_slots, max_active),
            Self::Av1(caps) => caps.dpb_limits(max_slots, max_active),
        }
    }
}

/// 把推导出的 level 换算成与 `CodecCaps::max_level` 相同的枚举域
///
/// H.264/H.265 的 std 字段是枚举，AV1 的 `maxLevel` 与 `seq_level_idx` 同域；
/// 三个映射在这里收口，`SessionConfig::build` 不必按 codec 分支。
pub(super) fn level_enum(codec: VideoCodec, level: u32) -> Result<u32, String> {
    match codec {
        VideoCodec::H264 => self::h264::level_to_vk(level),
        VideoCodec::H265 => self::h265::level_enum(level),
        VideoCodec::Av1 => Ok(level),
    }
}

/// 调试落盘：只有 AV1 的 IVF fourcc 被支持
///
/// 其它 codec 不落盘也不报错；写失败只告警，不影响编码。
pub(in crate::encode::hardware::vulkan) fn dump_ivf(
    codec: VideoCodec,
    path: &str,
    width: u32,
    height: u32,
    frame: &[u8],
) {
    if codec == VideoCodec::Av1 {
        av1::ivf::dump(path, width, height, frame);
    }
}

/// 按 codec 分派一帧编码
pub(in crate::encode::hardware::vulkan) fn encode_prepared(
    encoder: &mut super::encoder::VulkanEncoder,
    force_idr: bool,
) -> Result<(Vec<u8>, bool), String> {
    match encoder.codec() {
        VideoCodec::H264 => encoder.encode_prepared_h264(force_idr),
        VideoCodec::H265 => encoder.encode_prepared_h265(force_idr),
        VideoCodec::Av1 => encoder.encode_prepared_av1(force_idr),
    }
}

/// 本帧要挂进 begin 的 GOP 剩余帧数结构
pub(in crate::encode::hardware::vulkan) fn gop_remaining<'a>(
    encoder: &super::encoder::VulkanEncoder,
    frame_num: u32,
    is_key: bool,
) -> Option<CodecGopRemaining<'a>> {
    if !encoder.gop_remaining_frames {
        return None;
    }
    let (remaining_i, remaining_p) =
        self::rate_control::gop_remaining_counts(encoder.gop_length, frame_num, is_key);
    Some(match encoder.codec() {
        VideoCodec::H264 => CodecGopRemaining::H264(
            vk::VideoEncodeH264GopRemainingFrameInfoKHR::default()
                .use_gop_remaining_frames(true)
                .gop_remaining_i(remaining_i)
                .gop_remaining_p(remaining_p)
                .gop_remaining_b(0),
        ),
        VideoCodec::H265 => CodecGopRemaining::H265(
            vk::VideoEncodeH265GopRemainingFrameInfoKHR::default()
                .use_gop_remaining_frames(true)
                .gop_remaining_i(remaining_i)
                .gop_remaining_p(remaining_p)
                .gop_remaining_b(0),
        ),
        VideoCodec::Av1 => CodecGopRemaining::Av1(
            vk::VideoEncodeAV1GopRemainingFrameInfoKHR::default()
                .use_gop_remaining_frames(true)
                .gop_remaining_intra(remaining_i)
                .gop_remaining_predictive(remaining_p)
                .gop_remaining_bipredictive(0),
        ),
    })
}

/// codec 专属的逐帧状态
///
/// H.264 的 `idr_pic_id` 与 H.26x 参考槽的「上一帧是否为 IDR」都是格式语义，由各 codec
/// 自己保存，不放进共享外壳。AV1 没有跨帧累积的语法状态。
#[derive(Debug, Clone, Copy)]
pub(in crate::encode::hardware::vulkan) enum CodecState {
    H264(h264::State),
    H265(h265::State),
    Av1,
}

impl CodecState {
    pub(in crate::encode::hardware::vulkan) fn new(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::H264 => Self::H264(h264::State::new()),
            VideoCodec::H265 => Self::H265(h265::State::new()),
            VideoCodec::Av1 => Self::Av1,
        }
    }

    /// 上一帧是否为 IDR；AV1 没有这个参考槽语义
    pub(in crate::encode::hardware::vulkan) fn last_was_idr(&self) -> bool {
        match self {
            Self::H264(state) => state.last_was_idr(),
            Self::H265(state) => state.last_was_idr(),
            Self::Av1 => false,
        }
    }

    /// 下一个 H.264 IDR 的 `idr_pic_id`；只由 H.264 路径调用
    pub(in crate::encode::hardware::vulkan) fn next_h264_idr_pic_id(&mut self) -> u16 {
        match self {
            Self::H264(state) => state.next_idr_pic_id(),
            Self::H265(_) | Self::Av1 => unreachable!("只有 H.264 读取 idr_pic_id"),
        }
    }

    /// 一帧完成后的状态推进
    pub(in crate::encode::hardware::vulkan) fn finish_frame(&mut self, is_idr: bool) {
        match self {
            Self::H264(state) => state.finish_frame(is_idr),
            Self::H265(state) => state.finish_frame(is_idr),
            Self::Av1 => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codec_state_advances_per_codec() {
        let mut h264 = CodecState::new(VideoCodec::H264);
        assert!(!h264.last_was_idr());
        assert_eq!(h264.next_h264_idr_pic_id(), 1);
        h264.finish_frame(true);
        assert!(h264.last_was_idr());
        assert_eq!(h264.next_h264_idr_pic_id(), 0);

        let mut h265 = CodecState::new(VideoCodec::H265);
        h265.finish_frame(true);
        assert!(h265.last_was_idr());
        h265.finish_frame(false);
        assert!(!h265.last_was_idr());

        let mut av1 = CodecState::new(VideoCodec::Av1);
        av1.finish_frame(true);
        assert!(!av1.last_was_idr(), "AV1 不读参考槽的上一帧类型");
    }

    /// 推导 level 必须换算到与 `CodecCaps::max_level` 相同的枚举域
    #[test]
    fn test_level_enum_domains() {
        // H.264 语法值 42（4.2）在 std 里是枚举 12
        assert_eq!(level_enum(VideoCodec::H264, 42), Ok(12));
        // H.265 语法值 123（4.1 × 30）是枚举 6
        assert_eq!(level_enum(VideoCodec::H265, 123), Ok(6));
        // AV1 的 seq_level_idx 与 StdVideoAV1Level 同域
        assert_eq!(level_enum(VideoCodec::Av1, 9), Ok(9));
        assert!(
            level_enum(VideoCodec::H264, 43).is_err(),
            "非法语法值必须拒绝"
        );
    }
}

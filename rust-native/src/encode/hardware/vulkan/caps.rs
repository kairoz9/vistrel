//! Vulkan Video 能力探测 (Video Capability Probe)
//!
//! 输出**结构化**的 per-codec 能力快照，而不是给人看的字符串——S1 起会话时每一个数值
//! （DPB 槽位、码流缓冲对齐、码控模式、图片粒度）都必须从这里取，任何一处写死都等于
//! 假设所有人用同一块卡。
//!
//! 同时提供 [`CodecCaps::validate`]：建会话前的准入校验，不满足就 fail-closed。
//!
//! 「向驱动声明 codec 形态」的 profile 链词汇（[`ProfilePart`] / [`base_profile`]）也托管在
//! 本模块：探测必须先构造 profile 链，会话与码流缓冲的创建沿用它。
//!
//! 全程只读，不创建 `VkDevice`，不分配显存。

// `push` 是 `TaggedStructure` trait 的方法，不导入就无法链 pNext
use ash::vk::{self, TaggedStructure};

use super::context::VulkanInstance;
use crate::encode::VideoCodec;

pub(super) const INTRA_REFRESH_EXTENSION: &str = "VK_KHR_video_encode_intra_refresh";

/// codec 专属的 profile 子结构
pub(super) enum ProfilePart {
    H264(vk::VideoEncodeH264ProfileInfoKHR<'static>),
    H265(vk::VideoEncodeH265ProfileInfoKHR<'static>),
    Av1(vk::VideoEncodeAV1ProfileInfoKHR<'static>),
}

impl ProfilePart {
    /// 按 codec 取对应的 profile 子结构
    ///
    /// profile 取值必须与码流侧一致：H.264 High（100）、H.265 Main（1）、AV1 Main（0）。
    pub(super) fn new(codec: VideoCodec) -> Self {
        match codec {
            // High profile（100）：与 NVENC 侧一致的通用取值
            VideoCodec::H264 => {
                Self::H264(vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(100))
            }
            // Main profile（1）。这个 ash 版本的 profile info 没有 level 字段
            VideoCodec::H265 => {
                Self::H265(vk::VideoEncodeH265ProfileInfoKHR::default().std_profile_idc(1))
            }
            // Main profile（0）：4:2:0 8bit，与 `av1` 里的序列头一致
            VideoCodec::Av1 => {
                Self::Av1(vk::VideoEncodeAV1ProfileInfoKHR::default().std_profile(0))
            }
        }
    }

    /// 把 codec 子结构挂到 profile 链上
    pub(super) fn attach<'a>(
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
pub(super) fn base_profile<'a>(codec: VideoCodec) -> vk::VideoProfileInfoKHR<'a> {
    vk::VideoProfileInfoKHR::default()
        .video_codec_operation(codec_encode_operation(codec))
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
}

/// 项目 codec 到 Vulkan 编码操作位的映射
pub(super) fn codec_encode_operation(codec: VideoCodec) -> vk::VideoCodecOperationFlagsKHR {
    match codec {
        VideoCodec::H264 => vk::VideoCodecOperationFlagsKHR::ENCODE_H264,
        VideoCodec::H265 => vk::VideoCodecOperationFlagsKHR::ENCODE_H265,
        VideoCodec::Av1 => vk::VideoCodecOperationFlagsKHR::ENCODE_AV1,
    }
}

/// 项目 codec 对应的设备扩展名
pub(super) fn codec_encode_extension(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "VK_KHR_video_encode_h264",
        VideoCodec::H265 => "VK_KHR_video_encode_h265",
        VideoCodec::Av1 => "VK_KHR_video_encode_av1",
    }
}

/// 单个 codec 的能力快照——建会话所需的全部驱动参数
#[derive(Debug, Clone, Default)]
pub struct CodecCaps {
    /// 该 codec 的编码队列族（按 codec 分别查询，不同 codec 可能落在不同族）
    pub queue_family: Option<u32>,
    pub max_coded_extent: vk::Extent2D,
    /// 图像分配上限（`VkPhysicalDeviceLimits::maxImageDimension2D`），两轴相同
    ///
    /// `max_coded_extent` 只约束 picture resource 的 `codedExtent`，图像本身可以更大；
    /// 0 表示未探测，`input_pad` 退回 `max_coded_extent`。
    pub max_image_dimension: u32,
    /// 图片访问粒度：图像布局的最小对齐
    pub picture_access_granularity: vk::Extent2D,
    /// 编码器实际读取输入的粒度；规范明确不产生 VU，只用于 [`Self::input_pad`]
    pub encode_input_picture_granularity: vk::Extent2D,
    /// 码流缓冲的大小对齐，分配时按它向上取整（偏移固定为 0，不需要另一份偏移对齐）
    pub min_bitstream_size_alignment: u64,
    pub max_dpb_slots: u32,
    pub max_active_reference_pictures: u32,
    /// 驱动声明的码率上限；请求超出属于非法用法，`FrameEncoder` 按它钳制
    pub max_bitrate: u64,
    pub rate_control_modes: vk::VideoEncodeRateControlModeFlagsKHR,
    /// codec 专属能力：同一时刻只有当前 codec 的字段存在
    pub specific: CodecSpecific,
    pub feedback_flags: vk::VideoEncodeFeedbackFlagsKHR,
    /// 驱动是否要求 begin 时给出 GOP 剩余帧数
    pub requires_gop_remaining_frames: bool,
    /// 驱动是否偏好 begin 时给出 GOP 剩余帧数
    pub prefers_gop_remaining_frames: bool,
    /// 会话创建必须原样回传的 std 头版本
    pub std_header_version: vk::ExtensionProperties,
    /// Intra-Refresh 模式（`Debug` 文本，排障用）
    ///
    /// 不存数值：这个 flags 类型在本 ash rev 上没有 `bits()`，而具体类型名随版本变动，
    /// 用 `Debug` 文本既能打印又不绑死类型。
    pub intra_refresh_modes: String,
    /// 驱动是否声明了至少一种 intra-refresh 模式
    pub intra_refresh_modes_present: bool,
    /// 支持的最大刷新周期（帧）。0 表示不支持
    pub max_intra_refresh_cycle_duration: u32,
    /// 刷新期间允许的最大活跃参考帧数
    pub max_intra_refresh_active_reference_pictures: u32,
    /// 驱动返回的原始字段转储，仅测试报告消费；生产探测不为此 format 大字符串
    #[cfg(test)]
    pub raw: String,
}

/// codec 专属能力：互斥字段按 codec 分家，靠类型排除错配
#[derive(Debug, Clone)]
pub enum CodecSpecific {
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
    /// 建会话前的 codec 专属准入校验
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::H264(caps) => caps.validate(VideoCodec::H264),
            Self::H265(caps) => caps.validate(VideoCodec::H265),
            Self::Av1(caps) => caps.validate(),
        }
    }
}

/// H.264 / H.265 共用的专属能力
#[derive(Debug, Clone, Default)]
pub struct H26xCaps {
    /// 驱动支持的最高 level（std 枚举值）；码流 level 按档位推导，换算后不得高于它
    pub max_level_idc: u32,
    /// H.26x 的 QP 范围
    pub min_qp: i32,
    pub max_qp: i32,
    /// P 帧 L0 参考上限；0 表示不能编 P 帧
    pub max_p_picture_l0_references: u32,
    /// 单帧最大 slice 数
    pub max_slice_count: u32,
}

impl H26xCaps {
    /// 本实现只发 IPPP：P 帧必须有 L0 参考，单 slice 必须被驱动接受
    fn validate(&self, codec: VideoCodec) -> Result<(), String> {
        if self.max_p_picture_l0_references == 0 {
            return Err(format!(
                "{codec:?} 驱动不支持 P 帧 L0 参考（maxPPictureL0ReferenceCount=0）"
            ));
        }
        if self.max_slice_count == 0 {
            return Err(format!("{codec:?} 驱动不支持任何 slice"));
        }
        Ok(())
    }
}

/// AV1 专属能力
#[derive(Debug, Clone, Default)]
pub struct Av1Caps {
    /// 驱动支持的最高 level（`maxLevel`，与 `seq_level_idx` 同域）
    pub max_level: u32,
    /// 码控的量化索引上下限（`minQIndex` / `maxQIndex`）
    ///
    /// CBR 下驱动自己算量化步长，夹取范围与驱动声明不符会被判参数非法。
    pub min_q_index: u32,
    pub max_q_index: u32,
    /// coded picture 对齐；驱动据此改写码流分辨率
    pub coded_picture_alignment: vk::Extent2D,
    /// 单参考上限
    pub max_single_reference_count: u32,
    /// 允许的单参考名掩码
    pub single_reference_name_mask: u32,
    /// operating point 上限
    pub max_operating_points: u32,
    /// 超级块能力
    pub superblock_sizes: vk::VideoEncodeAV1SuperblockSizeFlagsKHR,
}

impl Av1Caps {
    /// 单参考名必须落在掩码里（VUID 10328/10329），超级块 64/128 至少有一个
    ///
    /// `maxOperatingPoints == 0` 不是拒绝条件（VUID 10280），见 `new_av1`。
    fn validate(&self) -> Result<(), String> {
        if self.max_single_reference_count == 0 || self.single_reference_name_mask == 0 {
            return Err(format!(
                "{:?} 驱动不支持单参考预测（maxSingleReferenceCount={}，掩码={:#x}）",
                VideoCodec::Av1,
                self.max_single_reference_count,
                self.single_reference_name_mask
            ));
        }
        if !self.superblock_sizes.intersects(
            vk::VideoEncodeAV1SuperblockSizeFlagsKHR::TYPE_64
                | vk::VideoEncodeAV1SuperblockSizeFlagsKHR::TYPE_128,
        ) {
            return Err(format!(
                "{:?} 驱动未声明任何超级块尺寸（64x64 / 128x128 均不可用）",
                VideoCodec::Av1
            ));
        }
        Ok(())
    }

    /// AV1 序列头是否用 128x128 超级块：驱动支持 128 时用 128，否则用 64
    ///
    /// 与 FFmpeg `vulkan_encode_av1.c` 的 `use_128x128_superblock` 取法一致；
    /// 两个都支持时选 128（更大的块减少分区开销）。
    pub fn av1_use_128x128_superblock(&self) -> bool {
        self.superblock_sizes
            .contains(vk::VideoEncodeAV1SuperblockSizeFlagsKHR::TYPE_128)
    }
}

/// 输入图 padding 的结果
///
/// `extent` 是实际分配尺寸；`dropped` 表示至少一个轴的 padding 因超过图像上限被放弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputPad {
    pub extent: vk::Extent2D,
    pub dropped: bool,
}

impl CodecCaps {
    fn read_video(&mut self, caps: &vk::VideoCapabilitiesKHR<'_>) {
        self.max_coded_extent = caps.max_coded_extent;
        self.picture_access_granularity = caps.picture_access_granularity;
        self.min_bitstream_size_alignment = caps.min_bitstream_buffer_size_alignment;
        self.max_dpb_slots = caps.max_dpb_slots;
        self.max_active_reference_pictures = caps.max_active_reference_pictures;
        self.std_header_version = caps.std_header_version;
    }

    fn read_encode(&mut self, caps: &vk::VideoEncodeCapabilitiesKHR<'_>) {
        self.max_bitrate = caps.max_bitrate;
        self.rate_control_modes = caps.rate_control_modes;
        self.feedback_flags = caps.supported_encode_feedback_flags;
        self.encode_input_picture_granularity = caps.encode_input_picture_granularity;
    }

    /// 本快照对应的 codec
    pub fn codec(&self) -> VideoCodec {
        match &self.specific {
            CodecSpecific::H264(_) => VideoCodec::H264,
            CodecSpecific::H265(_) => VideoCodec::H265,
            CodecSpecific::Av1(_) => VideoCodec::Av1,
        }
    }

    /// 驱动声明的最高 level，与 `frame_encoder::level_enum` 的输出同域
    ///
    /// H.26x 是 `maxLevelIdc` 枚举，AV1 是 `maxLevel`；两者数值域不同，但都只与
    /// 本 codec 推导出的 level 比较，因此在这里收成一个访问器。
    pub fn max_level(&self) -> u32 {
        match &self.specific {
            CodecSpecific::H264(caps) | CodecSpecific::H265(caps) => caps.max_level_idc,
            CodecSpecific::Av1(caps) => caps.max_level,
        }
    }

    /// 建会话前的准入校验：不满足就 fail-closed
    ///
    /// 校验 CBR、`BITSTREAM_BYTES_WRITTEN`、DPB/参考帧，再交给 codec 专属能力校验
    /// （H.26x 的 P 帧与 slice、AV1 单参考与超级块）——缺任何一项都会在别人的卡上
    /// 变成运行时故障。`requiresGopRemainingFrames`、`maxOperatingPoints=0` 不在这里
    /// 拒绝：前者由 begin 的 pNext 结构满足，后者按 VUID 10280 以 count=0 表达。
    pub fn validate(&self) -> Result<(), String> {
        let codec = self.codec();
        if self.queue_family.is_none() {
            return Err(format!("{codec:?} 没有可用的编码队列族"));
        }
        if !self
            .rate_control_modes
            .contains(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
        {
            return Err(format!(
                "{codec:?} 驱动未声明 CBR 码控模式（支持: {:?}）",
                self.rate_control_modes
            ));
        }
        if !self
            .feedback_flags
            .contains(vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN)
        {
            return Err(format!(
                "{codec:?} 驱动未声明 BITSTREAM_BYTES_WRITTEN，无法读取码流长度"
            ));
        }
        if self.max_dpb_slots < 2 {
            return Err(format!(
                "{codec:?} 最大 DPB 槽位为 {}，不足 2（IDR + 1 参考帧）",
                self.max_dpb_slots
            ));
        }
        if self.max_active_reference_pictures < 1 {
            return Err(format!("{codec:?} 最大活动参考帧为 0"));
        }
        self.specific.validate()
    }

    /// Intra-Refresh 是否真的可用
    ///
    /// 依据不是扩展名，而是能力查询给出的三项：
    ///
    /// - `intra_refresh_modes` 非空：驱动声明了至少一种刷新模式；
    /// - `max_intra_refresh_cycle_duration >= 2`：周期小于 2 构不成渐进刷新；
    /// - **`max_intra_refresh_active_reference_pictures != 0`**：按规范，该字段为 0
    ///   表示实现**不支持** intra refresh。只查前两项会漏掉这一档，于是对一个不支持
    ///   的驱动照常下发 `VkVideoEncodeIntraRefreshInfoKHR`——驱动静默忽略，每帧退化成
    ///   普通 P 帧，而 GOP 又是按「有渐进刷新」选的超长值，结果是任何一次解码错误都
    ///   长期不恢复（花屏）。
    pub fn supports_intra_refresh(&self) -> bool {
        self.intra_refresh_modes_present
            && self.max_intra_refresh_cycle_duration >= 2
            && self.max_intra_refresh_active_reference_pictures != 0
    }

    /// coded extent 的硬对齐粒度
    ///
    /// H.26x 取 `pictureAccessGranularity`（图像布局）；AV1 再与
    /// `codedPictureAlignment`（驱动据此改写码流分辨率）逐轴取大。不满足会导致
    /// 会话创建失败或码流分辨率与声明不一致。
    pub fn coded_granularity(&self) -> (u32, u32) {
        let width = self.picture_access_granularity.width.max(1);
        let height = self.picture_access_granularity.height.max(1);
        match &self.specific {
            CodecSpecific::Av1(caps) => (
                width.max(caps.coded_picture_alignment.width),
                height.max(caps.coded_picture_alignment.height),
            ),
            CodecSpecific::H264(_) | CodecSpecific::H265(_) => (width, height),
        }
    }

    /// 编码输入图的分配粒度：coded 粒度与 `encodeInputPictureGranularity` 取大
    ///
    /// 只用于 [`Self::input_pad`]，不改变 coded extent 的硬对齐。
    pub fn input_granularity(&self) -> (u32, u32) {
        let (cw, ch) = self.coded_granularity();
        (
            cw.max(self.encode_input_picture_granularity.width.max(1)),
            ch.max(self.encode_input_picture_granularity.height.max(1)),
        )
    }

    /// 把请求的几何按 [`Self::coded_granularity`] 对齐
    ///
    /// 粒度由驱动声明，**不是固定 16**：1080p 在 16 粒度下是 1920x1088，1440p 在
    /// NVIDIA 的 64 粒度下是 2560x1472。不补齐会话创建直接失败；结果还必须与采集侧
    /// 算的一致，见 `encode::hardware::coded_align`。
    pub fn align_extent(&self, width: u32, height: u32) -> Result<vk::Extent2D, String> {
        let (gw, gh) = self.coded_granularity();
        let aligned = vk::Extent2D {
            width: width.next_multiple_of(gw),
            height: height.next_multiple_of(gh),
        };
        if aligned.width > self.max_coded_extent.width
            || aligned.height > self.max_coded_extent.height
        {
            return Err(format!(
                "{:?} 请求 {}x{} 对齐后为 {}x{}，超过驱动上限 {}x{}",
                self.codec(),
                width,
                height,
                aligned.width,
                aligned.height,
                self.max_coded_extent.width,
                self.max_coded_extent.height
            ));
        }
        Ok(aligned)
    }

    /// 输入图分配尺寸：coded extent 再按 [`Self::input_granularity`] 补齐
    ///
    /// 该粒度是 advisory（不产生 VU）；超过图像上限的轴退回 coded extent，
    /// 不让整个后端起不来。
    pub fn input_pad(&self, coded: vk::Extent2D) -> InputPad {
        let (gw, gh) = self.input_granularity();
        let padded = vk::Extent2D {
            width: coded.width.next_multiple_of(gw),
            height: coded.height.next_multiple_of(gh),
        };
        // 未探测到 `maxImageDimension2D` 时退回 coded 上限，两轴分别比较
        let (limit_width, limit_height) = if self.max_image_dimension == 0 {
            (self.max_coded_extent.width, self.max_coded_extent.height)
        } else {
            (self.max_image_dimension, self.max_image_dimension)
        };
        let width_dropped = padded.width != coded.width && padded.width > limit_width;
        let height_dropped = padded.height != coded.height && padded.height > limit_height;
        InputPad {
            extent: vk::Extent2D {
                width: if width_dropped {
                    coded.width
                } else {
                    padded.width
                },
                height: if height_dropped {
                    coded.height
                } else {
                    padded.height
                },
            },
            dropped: width_dropped || height_dropped,
        }
    }
}

/// 探测单个 codec 的能力，复用调用方已有的 instance
///
/// 不自己建 instance：建会话时已经有一个了，没必要再来一个（整机探测报告在
/// `test_support`，测试自己建）。
pub(crate) fn probe_codec(ctx: &VulkanInstance, codec: VideoCodec) -> Result<CodecCaps, String> {
    // `vkGetPhysicalDeviceVideoCapabilitiesKHR` 是 `VK_KHR_video_queue` 的实例级命令。
    // 扩展没声明时命令表里是空指针：构造不报错，调用即崩，所以必须先挡在这里。
    if !ctx.supports_device_extension("VK_KHR_video_queue") {
        return Err("设备未声明 VK_KHR_video_queue".to_string());
    }
    let video = video_capabilities_fn(ctx);
    let pd = ctx.physical_device();
    let family = ctx.video_encode_family(codec_encode_operation(codec));
    // 扩展未启用时不能把 intra-refresh 能力结构链进查询
    let intra_supported = ctx.supports_device_extension(INTRA_REFRESH_EXTENSION);

    let mut caps = match codec {
        VideoCodec::H264 => probe_h264(pd, &video, intra_supported),
        VideoCodec::H265 => probe_h265(pd, &video, intra_supported),
        VideoCodec::Av1 => probe_av1(pd, &video, intra_supported),
    }?;
    caps.queue_family = family;
    caps.max_image_dimension = ctx.properties().limits.max_image_dimension2_d;
    Ok(caps)
}

/// 查询完成后的公共抄写：encode 侧字段 + intra-refresh + 测试用原始转储
///
/// pNext 挂链的 bound 依赖 codec 能力结构的**具体类型**（ash 的 `Extends` /
/// `TaggedStructure` impl 按类型逐一生成），无法泛型化，因此各 codec 只保留
/// 「挂链 + 查询 + 取专属字段」，逐行相同的抄写与转储收在这里。
///
/// `read_video(&caps)` 必须留在调用点：`caps` 的 pNext 链可变借着 encode / codec /
/// intra 三个结构，先终止它的借用（最后一次使用），这里才能再不可变借用它们。
fn collect_caps<C: std::fmt::Debug>(
    out: &mut CodecCaps,
    encode_caps: &vk::VideoEncodeCapabilitiesKHR<'_>,
    intra_caps: &vk::VideoEncodeIntraRefreshCapabilitiesKHR<'_>,
    // 只被下面 cfg(test) 的原始转储读取；`expect` 让「某天生产代码也开始读它」时立刻报错
    #[cfg_attr(not(test), expect(unused_variables))] codec_caps: &C,
) {
    read_intra_refresh(out, intra_caps);
    out.read_encode(encode_caps);
    // 原始转储只被测试报告消费，生产不为此 format 大字符串
    #[cfg(test)]
    {
        out.raw = format!("{encode_caps:#?}\n{codec_caps:#?}\n");
    }
}

fn probe_h264(
    pd: vk::PhysicalDevice,
    video: &ash::khr::video_queue::Instance,
    intra_supported: bool,
) -> Result<CodecCaps, String> {
    let mut part = ProfilePart::new(VideoCodec::H264);
    let video_profile = part.attach(base_profile(VideoCodec::H264));

    let mut codec_caps = vk::VideoEncodeH264CapabilitiesKHR::default();
    let mut encode_caps = vk::VideoEncodeCapabilitiesKHR::default();
    let mut intra_caps = vk::VideoEncodeIntraRefreshCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default()
        .push(&mut encode_caps)
        .push(&mut codec_caps);
    // 扩展未启用时把该结构链进 pNext 是非法用法，必须按能力条件挂
    if intra_supported {
        caps = caps.push(&mut intra_caps);
    }

    // SAFETY: 四个结构体均在栈上且 pNext 链指向它们，调用期间全部存活
    unsafe { video.get_physical_device_video_capabilities(pd, &video_profile, &mut caps) }
        .map_err(|e| format!("{e:?}"))?;
    let mut out = CodecCaps::default();
    out.read_video(&caps);
    collect_caps(&mut out, &encode_caps, &intra_caps, &codec_caps);
    out.specific = CodecSpecific::H264(H26xCaps {
        max_level_idc: codec_caps.max_level_idc,
        min_qp: codec_caps.min_qp,
        max_qp: codec_caps.max_qp,
        max_p_picture_l0_references: codec_caps.max_p_picture_l0_reference_count,
        max_slice_count: codec_caps.max_slice_count,
    });
    out.requires_gop_remaining_frames = codec_caps.requires_gop_remaining_frames != 0;
    out.prefers_gop_remaining_frames = codec_caps.prefers_gop_remaining_frames != 0;
    Ok(out)
}

fn probe_h265(
    pd: vk::PhysicalDevice,
    video: &ash::khr::video_queue::Instance,
    intra_supported: bool,
) -> Result<CodecCaps, String> {
    let mut part = ProfilePart::new(VideoCodec::H265);
    let video_profile = part.attach(base_profile(VideoCodec::H265));

    let mut codec_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
    let mut encode_caps = vk::VideoEncodeCapabilitiesKHR::default();
    let mut intra_caps = vk::VideoEncodeIntraRefreshCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default()
        .push(&mut encode_caps)
        .push(&mut codec_caps);
    if intra_supported {
        caps = caps.push(&mut intra_caps);
    }

    // SAFETY: 同 probe_h264
    unsafe { video.get_physical_device_video_capabilities(pd, &video_profile, &mut caps) }
        .map_err(|e| format!("{e:?}"))?;
    let mut out = CodecCaps::default();
    out.read_video(&caps);
    collect_caps(&mut out, &encode_caps, &intra_caps, &codec_caps);
    out.specific = CodecSpecific::H265(H26xCaps {
        max_level_idc: codec_caps.max_level_idc,
        min_qp: codec_caps.min_qp,
        max_qp: codec_caps.max_qp,
        max_p_picture_l0_references: codec_caps.max_p_picture_l0_reference_count,
        max_slice_count: codec_caps.max_slice_segment_count,
    });
    out.requires_gop_remaining_frames = codec_caps.requires_gop_remaining_frames != 0;
    out.prefers_gop_remaining_frames = codec_caps.prefers_gop_remaining_frames != 0;
    Ok(out)
}

fn probe_av1(
    pd: vk::PhysicalDevice,
    video: &ash::khr::video_queue::Instance,
    intra_supported: bool,
) -> Result<CodecCaps, String> {
    let mut part = ProfilePart::new(VideoCodec::Av1);
    let video_profile = part.attach(base_profile(VideoCodec::Av1));

    let mut codec_caps = vk::VideoEncodeAV1CapabilitiesKHR::default();
    let mut encode_caps = vk::VideoEncodeCapabilitiesKHR::default();
    let mut intra_caps = vk::VideoEncodeIntraRefreshCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default()
        .push(&mut encode_caps)
        .push(&mut codec_caps);
    if intra_supported {
        caps = caps.push(&mut intra_caps);
    }

    // SAFETY: 同 probe_h264
    unsafe { video.get_physical_device_video_capabilities(pd, &video_profile, &mut caps) }
        .map_err(|e| format!("{e:?}"))?;
    let mut out = CodecCaps::default();
    out.read_video(&caps);
    collect_caps(&mut out, &encode_caps, &intra_caps, &codec_caps);
    // AV1 的能力字段叫 max_level，与 H.26x 的 max_level_idc 不同名；
    // 量化索引范围：不同驱动给的范围不同，写死会在别人的卡上被判非法
    out.specific = CodecSpecific::Av1(Av1Caps {
        max_level: codec_caps.max_level,
        min_q_index: codec_caps.min_q_index,
        max_q_index: codec_caps.max_q_index,
        coded_picture_alignment: codec_caps.coded_picture_alignment,
        max_single_reference_count: codec_caps.max_single_reference_count,
        single_reference_name_mask: codec_caps.single_reference_name_mask,
        max_operating_points: codec_caps.max_operating_points,
        superblock_sizes: codec_caps.superblock_sizes,
    });
    out.requires_gop_remaining_frames = codec_caps.requires_gop_remaining_frames != 0;
    out.prefers_gop_remaining_frames = codec_caps.prefers_gop_remaining_frames != 0;
    Ok(out)
}

/// 把 intra-refresh 能力抄进结构化快照
///
/// 只在扩展可用时才会被驱动填写；否则结构体保持默认零值，
/// `supports_intra_refresh()` 据此返回 false。
fn read_intra_refresh(out: &mut CodecCaps, caps: &vk::VideoEncodeIntraRefreshCapabilitiesKHR<'_>) {
    // 这个 flags 类型在本 ash rev 上没有 `bits()`，也不去猜它的访问器：
    // 「与空值比较」+ `Debug` 文本足以表达「有没有能力」和「排障看什么」
    out.intra_refresh_modes_present = caps.intra_refresh_modes != Default::default();
    out.intra_refresh_modes = format!("{:?}", caps.intra_refresh_modes);
    out.max_intra_refresh_cycle_duration = caps.max_intra_refresh_cycle_duration;
    out.max_intra_refresh_active_reference_pictures =
        caps.max_intra_refresh_active_reference_pictures;
}

/// `VK_KHR_video_queue` 的实例级命令表
///
/// 这里只用到 `get_physical_device_video_capabilities` 一条，但表是整块构造的。
/// 表里其余函数指针可能是空指针（本机没用到），只有真去调用才会成为问题，
/// 而到那时对应的扩展必然已启用。
fn video_capabilities_fn(ctx: &VulkanInstance) -> ash::khr::video_queue::Instance {
    ash::khr::video_queue::Instance::load(ctx.entry(), ctx.instance())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(width: u32, height: u32) -> vk::Extent2D {
        vk::Extent2D { width, height }
    }

    fn h264_caps() -> CodecCaps {
        CodecCaps {
            queue_family: Some(0),
            rate_control_modes: vk::VideoEncodeRateControlModeFlagsKHR::CBR,
            feedback_flags: vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
            max_dpb_slots: 8,
            max_active_reference_pictures: 1,
            specific: CodecSpecific::H264(H26xCaps {
                max_p_picture_l0_references: 4,
                max_slice_count: 1,
                ..H26xCaps::default()
            }),
            ..CodecCaps::default()
        }
    }

    fn av1_specific() -> Av1Caps {
        Av1Caps {
            max_single_reference_count: 7,
            single_reference_name_mask: 0x7f,
            superblock_sizes: vk::VideoEncodeAV1SuperblockSizeFlagsKHR::TYPE_64,
            ..Av1Caps::default()
        }
    }

    fn av1_caps() -> CodecCaps {
        CodecCaps {
            specific: CodecSpecific::Av1(av1_specific()),
            ..h264_caps()
        }
    }

    /// coded 对齐：H.26x 只取访问粒度，AV1 再与 codedPictureAlignment 取大；
    /// 输入读取粒度只影响图像分配
    #[test]
    fn test_coded_and_input_granularity() {
        let h264 = CodecCaps {
            picture_access_granularity: extent(2, 4),
            encode_input_picture_granularity: extent(16, 8),
            ..h264_caps()
        };
        assert_eq!(h264.coded_granularity(), (2, 4));
        assert_eq!(h264.input_granularity(), (16, 8));

        let av1 = CodecCaps {
            specific: CodecSpecific::Av1(Av1Caps {
                coded_picture_alignment: extent(8, 32),
                ..av1_specific()
            }),
            ..h264
        };
        assert_eq!(av1.coded_granularity(), (8, 32));
        assert_eq!(av1.input_granularity(), (16, 32));
    }

    /// 输入 padding 是 advisory：补齐超过图像分配上限时逐轴退回，
    /// 不把合法分辨率拒掉
    #[test]
    fn test_input_pad_falls_back_when_over_limit() {
        let caps = CodecCaps {
            picture_access_granularity: extent(2, 2),
            encode_input_picture_granularity: extent(64, 64),
            max_coded_extent: extent(1920, 1088),
            ..h264_caps()
        };
        let coded = extent(1920, 1088);
        let exact = caps.input_pad(coded);
        assert_eq!(
            exact.extent, coded,
            "1088 已整除 64 时 padding 与 coded 相同"
        );
        assert!(!exact.dropped);

        let tight = extent(1366, 768);
        let pad_caps = CodecCaps {
            max_coded_extent: tight,
            ..caps.clone()
        };
        let pad = pad_caps.input_pad(tight);
        assert_eq!(
            pad.extent, tight,
            "padding 超过图像上限时必须退回 coded，而不是报错"
        );
        assert!(pad.dropped, "宽轴 1408 > 1366，应报告 padding 被放弃");

        let roomy = CodecCaps {
            max_coded_extent: extent(1408, 768),
            ..pad_caps
        };
        assert_eq!(
            roomy.input_pad(tight).extent,
            extent(1408, 768),
            "有空间时按 encodeInputPictureGranularity 补齐"
        );

        // 图像分配上限优先于 maxCodedExtent：后者只约束 codedExtent
        let image_limited = CodecCaps {
            max_image_dimension: 768,
            ..caps
        };
        let limited = image_limited.input_pad(tight);
        assert_eq!(limited.extent, tight);
        assert!(limited.dropped);
    }

    /// AV1 超级块：64-only、128-only、两者都有都要通过；一个都没有才拒绝
    #[test]
    fn test_av1_superblock_accepts_64_or_128() {
        let both = Av1Caps {
            superblock_sizes: vk::VideoEncodeAV1SuperblockSizeFlagsKHR::TYPE_64
                | vk::VideoEncodeAV1SuperblockSizeFlagsKHR::TYPE_128,
            ..av1_specific()
        };
        assert!(both.validate().is_ok());
        assert!(both.av1_use_128x128_superblock(), "两者都支持时选 128");

        let only_64 = Av1Caps {
            superblock_sizes: vk::VideoEncodeAV1SuperblockSizeFlagsKHR::TYPE_64,
            ..av1_specific()
        };
        assert!(only_64.validate().is_ok());
        assert!(!only_64.av1_use_128x128_superblock());

        let only_128 = Av1Caps {
            superblock_sizes: vk::VideoEncodeAV1SuperblockSizeFlagsKHR::TYPE_128,
            ..av1_specific()
        };
        assert!(only_128.validate().is_ok(), "128-only 驱动必须可用");
        assert!(only_128.av1_use_128x128_superblock());

        let none = Av1Caps {
            superblock_sizes: vk::VideoEncodeAV1SuperblockSizeFlagsKHR::empty(),
            ..av1_specific()
        };
        assert!(none.validate().is_err());
    }

    /// maxOperatingPoints=0 是合法状态：VUID 10280 允许 count=0，
    /// 此时由调用方省略操作点数组
    #[test]
    fn test_av1_zero_operating_points_is_not_rejected() {
        let caps = CodecCaps {
            specific: CodecSpecific::Av1(Av1Caps {
                max_operating_points: 0,
                ..av1_specific()
            }),
            ..av1_caps()
        };
        assert!(caps.validate().is_ok());
    }
}

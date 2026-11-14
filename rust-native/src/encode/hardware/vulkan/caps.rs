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
    pub codec: VideoCodec,
    /// 该 codec 的编码队列族（按 codec 分别查询，不同 codec 可能落在不同族）
    pub queue_family: Option<u32>,
    pub max_coded_extent: vk::Extent2D,
    /// 图片访问粒度：编码输入图的长宽必须对齐到它
    pub picture_access_granularity: vk::Extent2D,
    /// 码流缓冲的大小对齐，分配时按它向上取整（偏移固定为 0，不需要另一份偏移对齐）
    pub min_bitstream_size_alignment: u64,
    pub max_dpb_slots: u32,
    pub max_active_reference_pictures: u32,
    /// 驱动声明的码率上限；请求超出属于非法用法，`FrameEncoder` 按它钳制
    pub max_bitrate: u64,
    pub rate_control_modes: vk::VideoEncodeRateControlModeFlagsKHR,
    /// 驱动支持的最高 level（枚举值，排障用；码流实际声明的 level 按档位推导，不读它）
    pub max_level_idc: u32,
    /// AV1 码控的量化索引上下限（`minQIndex` / `maxQIndex`）
    ///
    /// 只在 AV1 上被填写；H.26x 的 QP 范围是标准固定值（0..51），不走这两个字段。
    /// CBR 下驱动自己算量化步长，若给的夹取范围与驱动声明不符，驱动会判参数非法。
    pub min_q_index: u32,
    pub max_q_index: u32,
    pub feedback_flags: vk::VideoEncodeFeedbackFlagsKHR,
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
    }

    /// 建会话前的准入校验：不满足就 fail-closed
    ///
    /// 这些是「不同机器真的会不一样」的项，缺任何一项都会在别人的卡上变成运行时故障：
    /// - 没有 CBR：Followed Bitrate 无处落地
    /// - 没有 `BITSTREAM_BYTES_WRITTEN`：无法得知编码器写了多少字节
    /// - DPB 槽位 < 2 或活动参考帧 < 1：只能全 IDR，等于没有帧间压缩
    pub fn validate(&self) -> Result<(), String> {
        if self.queue_family.is_none() {
            return Err(format!("{:?} 没有可用的编码队列族", self.codec));
        }
        if !self
            .rate_control_modes
            .contains(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
        {
            return Err(format!(
                "{:?} 驱动未声明 CBR 码控模式（支持: {:?}）",
                self.codec, self.rate_control_modes
            ));
        }
        if !self
            .feedback_flags
            .contains(vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN)
        {
            return Err(format!(
                "{:?} 驱动未声明 BITSTREAM_BYTES_WRITTEN，无法读取码流长度",
                self.codec
            ));
        }
        if self.max_dpb_slots < 2 {
            return Err(format!(
                "{:?} 最大 DPB 槽位为 {}，不足 2（IDR + 1 参考帧）",
                self.codec, self.max_dpb_slots
            ));
        }
        if self.max_active_reference_pictures < 1 {
            return Err(format!("{:?} 最大活动参考帧为 0", self.codec));
        }
        Ok(())
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

    /// 把请求的几何对齐到该 codec 的图片访问粒度
    ///
    /// 粒度由驱动声明，**不是固定 16**：1080p 在 16 粒度下是 1920x1088，
    /// 而 1440p 在 NVIDIA 的 64 粒度下是 2560x1472（1440 能整除 16 却整除不了 64）。
    /// 不补齐的话编码会话创建会直接失败；对齐结果还必须与采集侧算的一致，
    /// 见 `encode::hardware::coded_align`。
    pub fn align_extent(&self, width: u32, height: u32) -> Result<vk::Extent2D, String> {
        let gw = self.picture_access_granularity.width.max(1);
        let gh = self.picture_access_granularity.height.max(1);
        let aligned = vk::Extent2D {
            width: width.next_multiple_of(gw),
            height: height.next_multiple_of(gh),
        };
        if aligned.width > self.max_coded_extent.width
            || aligned.height > self.max_coded_extent.height
        {
            return Err(format!(
                "{:?} 请求 {}x{} 对齐后为 {}x{}，超过驱动上限 {}x{}",
                self.codec,
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
    caps.codec = codec;
    caps.queue_family = family;
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
    out.max_level_idc = codec_caps.max_level_idc;
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
    out.max_level_idc = codec_caps.max_level_idc;
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
    out.max_level_idc = codec_caps.max_level;
    out.min_q_index = codec_caps.min_q_index;
    out.max_q_index = codec_caps.max_q_index;
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

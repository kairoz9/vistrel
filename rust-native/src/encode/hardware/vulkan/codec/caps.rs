//! Vulkan Video 能力探测 (Video Capability Probe)
//!
//! 输出**结构化**的 per-codec 能力快照，而不是给人看的字符串——S1 起会话时每一个数值
//! （DPB 槽位、码流缓冲对齐、码控模式、图片粒度）都必须从这里取，任何一处写死都等于
//! 假设所有人用同一块卡。
//!
//! 同时提供 [`CodecCaps::validate`]：建会话前的准入校验，不满足就 fail-closed。
//!
//! 各 codec 的专属能力结构与 probe 在 `h264/caps.rs` / `h265/caps.rs` /
//! `av1/caps.rs`；本模块持有共享快照、通用校验与探测入口。
//!
//! 全程只读，不创建 `VkDevice`，不分配显存。

// `push` 是 `TaggedStructure` trait 的方法，不导入就无法链 pNext
use ash::vk;

use super::super::context::VulkanInstance;
use super::CodecSpecific;
use crate::encode::VideoCodec;

pub(super) const INTRA_REFRESH_EXTENSION: &str = "VK_KHR_video_encode_intra_refresh";

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
    /// 驱动声明的码率上限；请求超出属于非法用法，`SessionConfig::build` 按它钳制
    pub max_bitrate: u64,
    pub rate_control_modes: vk::VideoEncodeRateControlModeFlagsKHR,
    /// codec 专属能力：同一时刻只有当前 codec 的字段存在
    pub(in crate::encode::hardware::vulkan) specific: CodecSpecific,
    pub feedback_flags: vk::VideoEncodeFeedbackFlagsKHR,
    /// 会话创建必须原样回传的 std 头版本
    pub std_header_version: vk::ExtensionProperties,
    /// intra-refresh 能力
    pub intra_refresh: IntraRefreshCaps,
    /// 驱动返回的原始字段转储，仅测试报告消费；生产探测不为此 format 大字符串
    #[cfg(test)]
    pub raw: String,
}

/// intra-refresh 能力
#[derive(Debug, Clone, Default)]
pub struct IntraRefreshCaps {
    /// 驱动声明的刷新模式；空表示扩展可用但驱动未声明任何模式
    pub modes: vk::VideoEncodeIntraRefreshModeFlagsKHR,
    /// 支持的最大刷新周期（帧）。0 表示不支持
    pub max_cycle_duration: u32,
    /// 刷新期间允许的最大活跃参考帧数
    pub max_active_reference_pictures: u32,
}

impl IntraRefreshCaps {
    /// 三项同时成立才真正可用
    ///
    /// `modes` 非空、`max_cycle_duration >= 2`，且 `max_active_reference_pictures != 0`
    /// （规范中 0 表示不支持）。漏掉最后一项会让驱动静默忽略 intra-refresh，而 GOP
    /// 仍按渐进刷新选超长值，解码错误长期不恢复。
    pub fn supported(&self) -> bool {
        !self.modes.is_empty()
            && self.max_cycle_duration >= 2
            && self.max_active_reference_pictures != 0
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
    pub(in crate::encode::hardware::vulkan) fn read_video(
        &mut self,
        caps: &vk::VideoCapabilitiesKHR<'_>,
    ) {
        self.max_coded_extent = caps.max_coded_extent;
        self.picture_access_granularity = caps.picture_access_granularity;
        self.min_bitstream_size_alignment = caps.min_bitstream_buffer_size_alignment;
        self.max_dpb_slots = caps.max_dpb_slots;
        self.max_active_reference_pictures = caps.max_active_reference_pictures;
        self.std_header_version = caps.std_header_version;
    }

    pub(in crate::encode::hardware::vulkan) fn read_encode(
        &mut self,
        caps: &vk::VideoEncodeCapabilitiesKHR<'_>,
    ) {
        self.max_bitrate = caps.max_bitrate;
        self.rate_control_modes = caps.rate_control_modes;
        self.feedback_flags = caps.supported_encode_feedback_flags;
        self.encode_input_picture_granularity = caps.encode_input_picture_granularity;
    }

    /// 本快照对应的 codec
    pub fn codec(&self) -> VideoCodec {
        self.specific.codec()
    }

    /// 驱动声明的最高 level，与 `codec::level_to_vk` 的输出同域
    pub fn max_level(&self) -> u32 {
        self.specific.max_level()
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

    /// 驱动是否要求或偏好 begin 时给出 GOP 剩余帧数
    pub fn wants_gop_remaining_frames(&self) -> bool {
        self.specific.wants_gop_remaining_frames()
    }

    /// Intra-Refresh 是否真的可用，判定见 [`IntraRefreshCaps::supported`]
    pub fn supports_intra_refresh(&self) -> bool {
        self.intra_refresh.supported()
    }

    /// coded extent 的硬对齐粒度
    ///
    /// H.26x 取 `pictureAccessGranularity`（图像布局）；AV1 再与
    /// `codedPictureAlignment`（驱动据此改写码流分辨率）逐轴取大。不满足会导致
    /// 会话创建失败或码流分辨率与声明不一致。
    pub fn coded_granularity(&self) -> (u32, u32) {
        let width = self.picture_access_granularity.width.max(1);
        let height = self.picture_access_granularity.height.max(1);
        let extra = self.specific.coded_alignment();
        (width.max(extra.width), height.max(extra.height))
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
/// 不自己建 instance：建会话时已经有一个了，没必要再来一个。
pub(in crate::encode::hardware) fn probe_codec(
    ctx: &VulkanInstance,
    codec: VideoCodec,
) -> Result<CodecCaps, String> {
    // `vkGetPhysicalDeviceVideoCapabilitiesKHR` 是 `VK_KHR_video_queue` 的实例级命令。
    // 扩展没声明时命令表里是空指针：构造不报错，调用即崩，所以必须先挡在这里。
    if !ctx.supports_device_extension("VK_KHR_video_queue") {
        return Err("设备未声明 VK_KHR_video_queue".to_string());
    }
    let video = video_capabilities_fn(ctx);
    let pd = ctx.physical_device();
    let family = ctx.video_encode_family(super::codec_encode_operation(codec));
    // 扩展未启用时不能把 intra-refresh 能力结构链进查询
    let intra_supported = ctx.supports_device_extension(INTRA_REFRESH_EXTENSION);

    let mut caps = match codec {
        VideoCodec::H264 => super::h264::caps::probe(pd, &video, intra_supported),
        VideoCodec::H265 => super::h265::caps::probe(pd, &video, intra_supported),
        VideoCodec::Av1 => super::av1::caps::probe(pd, &video, intra_supported),
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
pub(in crate::encode::hardware::vulkan) fn collect_caps<C: std::fmt::Debug>(
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

/// 把 intra-refresh 能力抄进结构化快照
///
/// 只在扩展可用时才会被驱动填写；否则结构体保持默认零值，
/// [`CodecCaps::supports_intra_refresh`] 据此返回 false。
fn read_intra_refresh(out: &mut CodecCaps, caps: &vk::VideoEncodeIntraRefreshCapabilitiesKHR<'_>) {
    out.intra_refresh = IntraRefreshCaps {
        modes: caps.intra_refresh_modes,
        max_cycle_duration: caps.max_intra_refresh_cycle_duration,
        max_active_reference_pictures: caps.max_intra_refresh_active_reference_pictures,
    };
}

/// `VK_KHR_video_queue` 的实例级命令表
///
/// 这里只用到 `get_physical_device_video_capabilities` 一条，但表是整块构造的。
/// 表里其余函数指针可能是空指针（本机没用到），只有真去调用才会成为问题，
/// 而到那时对应的扩展必然已启用。
pub(in crate::encode::hardware::vulkan) fn video_capabilities_fn(
    ctx: &VulkanInstance,
) -> ash::khr::video_queue::Instance {
    ash::khr::video_queue::Instance::load(ctx.entry(), ctx.instance())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::hardware::vulkan::codec::av1::caps::Av1Caps;
    use crate::encode::hardware::vulkan::codec::h26x::H26xCaps;

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

    /// intra-refresh 三项缺一不可：模式为空、周期 < 2、活跃参考为 0 都要拒绝
    #[test]
    fn test_intra_refresh_requires_all_three_capabilities() {
        let ok = IntraRefreshCaps {
            modes: vk::VideoEncodeIntraRefreshModeFlagsKHR::PER_PICTURE_PARTITION,
            max_cycle_duration: 2,
            max_active_reference_pictures: 1,
        };
        assert!(ok.supported());

        let no_modes = IntraRefreshCaps {
            modes: vk::VideoEncodeIntraRefreshModeFlagsKHR::empty(),
            ..ok.clone()
        };
        assert!(!no_modes.supported());

        let short_cycle = IntraRefreshCaps {
            max_cycle_duration: 1,
            ..ok.clone()
        };
        assert!(!short_cycle.supported());

        let no_active_refs = IntraRefreshCaps {
            max_active_reference_pictures: 0,
            ..ok
        };
        assert!(!no_active_refs.supported());
    }
}

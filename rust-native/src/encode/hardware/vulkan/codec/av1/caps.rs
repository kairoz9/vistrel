//! AV1 能力探测

use ash::vk::{self, TaggedStructure};

use crate::encode::VideoCodec;
use crate::encode::hardware::vulkan::codec::caps::{CodecCaps, collect_caps};
use crate::encode::hardware::vulkan::codec::{CodecSpecific, ProfilePart, base_profile};

/// AV1 专属能力
#[derive(Debug, Clone, Default)]
pub(in crate::encode::hardware::vulkan) struct Av1Caps {
    /// 驱动支持的最高 level（`maxLevel`，与 `seq_level_idx` 同域）
    pub(in crate::encode::hardware::vulkan) max_level: u32,
    /// 码控的量化索引上下限（`minQIndex` / `maxQIndex`）
    ///
    /// CBR 下驱动自己算量化步长，夹取范围与驱动声明不符会被判参数非法。
    pub(in crate::encode::hardware::vulkan) min_q_index: u32,
    pub(in crate::encode::hardware::vulkan) max_q_index: u32,
    /// coded picture 对齐；驱动据此改写码流分辨率
    pub(in crate::encode::hardware::vulkan) coded_picture_alignment: vk::Extent2D,
    /// 单参考上限
    pub(in crate::encode::hardware::vulkan) max_single_reference_count: u32,
    /// 允许的单参考名掩码
    pub(in crate::encode::hardware::vulkan) single_reference_name_mask: u32,
    /// operating point 上限
    pub(in crate::encode::hardware::vulkan) max_operating_points: u32,
    /// 超级块能力
    pub(in crate::encode::hardware::vulkan) superblock_sizes:
        vk::VideoEncodeAV1SuperblockSizeFlagsKHR,
    /// 驱动是否要求 begin 时给出 GOP 剩余帧数
    pub(in crate::encode::hardware::vulkan) requires_gop_remaining_frames: bool,
    /// 驱动是否偏好 begin 时给出 GOP 剩余帧数
    pub(in crate::encode::hardware::vulkan) prefers_gop_remaining_frames: bool,
}

impl Av1Caps {
    /// 单参考名必须落在掩码里（VUID 10328/10329），超级块 64/128 至少有一个
    ///
    /// `maxOperatingPoints == 0` 不是拒绝条件（VUID 10280），见 `new_av1`。
    pub(in crate::encode::hardware::vulkan) fn validate(&self) -> Result<(), String> {
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
    pub(in crate::encode::hardware::vulkan) fn av1_use_128x128_superblock(&self) -> bool {
        self.superblock_sizes
            .contains(vk::VideoEncodeAV1SuperblockSizeFlagsKHR::TYPE_128)
    }

    /// AV1 声明 8 个逻辑 DPB 槽位
    pub(in crate::encode::hardware::vulkan) fn dpb_limits(
        &self,
        max_slots: u32,
        max_active: u32,
    ) -> (u32, u32) {
        let slots = max_slots.min(8);
        (slots, max_active.min(slots))
    }

    /// 驱动是否要求或偏好 begin 时给出 GOP 剩余帧数
    pub(in crate::encode::hardware::vulkan) fn wants_gop_remaining_frames(&self) -> bool {
        self.requires_gop_remaining_frames || self.prefers_gop_remaining_frames
    }
}

/// 探测 AV1 能力
pub(in crate::encode::hardware::vulkan) fn probe(
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
        requires_gop_remaining_frames: codec_caps.requires_gop_remaining_frames != 0,
        prefers_gop_remaining_frames: codec_caps.prefers_gop_remaining_frames != 0,
    });
    Ok(out)
}

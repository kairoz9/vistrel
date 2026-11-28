//! H.265 能力探测

use ash::vk::{self, TaggedStructure};

use super::super::H26xCaps;
use crate::encode::VideoCodec;
use crate::encode::hardware::vulkan::codec::caps::{CodecCaps, collect_caps};
use crate::encode::hardware::vulkan::codec::{CodecSpecific, ProfilePart, base_profile};

/// 探测 H.265 能力
pub(in crate::encode::hardware::vulkan) fn probe(
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
        requires_gop_remaining_frames: codec_caps.requires_gop_remaining_frames != 0,
        prefers_gop_remaining_frames: codec_caps.prefers_gop_remaining_frames != 0,
    });
    Ok(out)
}

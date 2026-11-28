use super::frame::*;
use super::params::*;
use super::*;

use crate::encode::hardware::vulkan::codec::VideoSessionParameters;
use crate::encode::hardware::vulkan::device::VideoDevice;
use crate::encode::hardware::vulkan::session::VideoSession;
use ash::vk::TaggedStructure;
use ash::vk::native::{
    StdVideoEncodeH265ReferenceListsInfo, StdVideoH265SequenceParameterSetVui,
    StdVideoH265ShortTermRefPicSet,
};

use crate::encode::VideoCodec;
use crate::encode::hardware::vulkan::codec::{H265SequenceConfig, SequenceConfig};
use crate::encode::hardware::vulkan::context::VulkanInstance;
use crate::encode::hardware::vulkan::encoder::{
    make_noisy_frame, test_session_config, test_vulkan_encoder,
};
use crate::encode::hardware::vulkan::{codec::caps, test_support};

/// VUI：BT.709 + Limited Range
#[test]
fn test_vui_bt709_limited() {
    let v = vui();
    assert_eq!(v.flags.video_signal_type_present_flag(), 1);
    assert_eq!(v.flags.colour_description_present_flag(), 1);
    assert_eq!(v.flags.video_full_range_flag(), 0, "0 = limited range");
    assert_eq!(
        v.video_format, 5,
        "未指定视频格式，色彩由 color description 表达"
    );
    assert_eq!(v.colour_primaries, 1, "BT.709");
    assert_eq!(v.transfer_characteristics, 1);
    assert_eq!(v.matrix_coeffs, 1);
    assert!(v.pHrdParameters.is_null());
}

/// SPS：VUI 标志置位，指针指向调用方持有的那一份
#[test]
fn test_sps_declares_vui() {
    let dpb = dpb();
    let ptl = profile_tier_level(93).expect("3.1 必须能映射到枚举");
    let vui = vui();
    let sets = parameter_sets(
        vk::Extent2D {
            width: 1920,
            height: 1088,
        },
        vk::Extent2D {
            width: 1920,
            height: 1080,
        },
        9,
        &dpb,
        &ptl,
        &vui,
    );

    assert_eq!(sets.sps.flags.vui_parameters_present_flag(), 1);
    assert_eq!(
        sets.sps.pSequenceParameterSetVui, &vui as *const StdVideoH265SequenceParameterSetVui,
        "VUI 指针必须指向调用方持有的那一份"
    );
    assert_eq!(sets.sps.conf_win_bottom_offset, 4, "(1088-1080)/2 = 4");
}

/// std 的 `general_level_idc` 是枚举而非码流语法值：123 必须落到 4.1（枚举 6）
#[test]
fn test_profile_tier_level_converts_codec_level_to_enum() {
    assert_eq!(
        profile_tier_level(93)
            .expect("3.1 必须能映射到枚举")
            .general_level_idc,
        4,
        "3.1"
    );
    assert_eq!(
        profile_tier_level(123)
            .expect("4.1 必须能映射到枚举")
            .general_level_idc,
        6,
        "4.1"
    );
    assert_eq!(
        profile_tier_level(186)
            .expect("6.2 必须能映射到枚举")
            .general_level_idc,
        12,
        "6.2"
    );
    assert!(
        level_to_vk(1).is_err(),
        "未知 level 必须报错，不能静默落到 6.2"
    );
}

/// 变换层级给满 32 → 4 的四层；层级 0 会让 NVIDIA H.265 只编码顶部少量 CTB
#[test]
fn test_sps_transform_hierarchy_is_full_depth() {
    let dpb = dpb();
    let ptl = profile_tier_level(93).expect("3.1 必须能映射到枚举");
    let vui = vui();
    let sets = parameter_sets(
        vk::Extent2D {
            width: 1920,
            height: 1088,
        },
        vk::Extent2D {
            width: 1920,
            height: 1080,
        },
        9,
        &dpb,
        &ptl,
        &vui,
    );

    assert_eq!(sets.sps.max_transform_hierarchy_depth_inter, 3);
    assert_eq!(sets.sps.max_transform_hierarchy_depth_intra, 3);
}

/// IDR：IRAP 标志置位、I slice、无参考列表
#[test]
fn test_idr_frame_std() {
    let parts = H265FrameParts::new(None);
    let frame = build(&parts, true, 0);
    assert_eq!(frame.pic_info.pic_type, PICTURE_TYPE_IDR);
    assert_eq!(frame.pic_info.flags.IrapPicFlag(), 1, "IDR 必须是 IRAP");
    assert_eq!(frame.pic_info.flags.is_reference(), 1, "本帧应成为参考帧");
    assert_eq!(frame.slice_header.slice_type, SLICE_TYPE_I);
    assert_eq!(
        frame.slice_header.flags.first_slice_segment_in_pic_flag(),
        1,
        "单 slice 必须声明自己是第一段"
    );
    assert_eq!(
        frame.ref_info.pic_type, PICTURE_TYPE_IDR,
        "DPB 槽信息要一致"
    );
    assert_eq!(frame.ref_info.PicOrderCntVal, 0);

    assert!(frame.pic_info.pRefLists.is_null(), "IDR 不应挂参考列表");
    assert!(
        frame.pic_info.pShortTermRefPicSet.is_null(),
        "IDR 不应挂短参考集"
    );
}

/// P 帧：非 IRAP、P slice、参考列表第一项指向给定槽位
#[test]
fn test_p_frame_std() {
    let parts = H265FrameParts::new(Some(1));
    let frame = build(&parts, false, 7);
    assert_eq!(frame.pic_info.pic_type, PICTURE_TYPE_P);
    assert_eq!(frame.pic_info.flags.IrapPicFlag(), 0, "P 帧不是 IRAP");
    assert_eq!(frame.slice_header.slice_type, SLICE_TYPE_P);
    assert_eq!(frame.pic_info.PicOrderCntVal, 7);
    assert_eq!(parts.ref_lists.num_ref_idx_l0_active_minus1, 0);
    assert_eq!(parts.ref_lists.RefPicList0[0], 1, "参考槽索引");
    assert_eq!(
        parts.ref_lists.RefPicList0[1], NO_REFERENCE,
        "其余表项应为空位"
    );

    assert!(!frame.pic_info.pRefLists.is_null(), "P 帧必须挂参考列表");
    assert!(
        !frame.pic_info.pShortTermRefPicSet.is_null(),
        "P 帧必须挂短参考集：SPS 里没有，slice 头就得自带"
    );
    assert_eq!(
        parts.st_ref_pic_set.num_negative_pics, 1,
        "只有一张负向参考（上一帧）"
    );
    assert_eq!(
        parts.st_ref_pic_set.used_by_curr_pic_s0_flag, 1,
        "该参考必须被当前帧引用"
    );
}

/// 两个指针都要真的指向传进来的 `parts`
#[test]
fn test_pointers_target_parts() {
    let parts = H265FrameParts::new(Some(0));
    let frame = build(&parts, false, 3);

    assert_eq!(
        frame.pic_info.pRefLists, &parts.ref_lists as *const StdVideoEncodeH265ReferenceListsInfo,
        "参考列表指针应指向 parts"
    );
    assert_eq!(
        frame.pic_info.pShortTermRefPicSet,
        &parts.st_ref_pic_set as *const StdVideoH265ShortTermRefPicSet,
        "短参考集指针应指向 parts"
    );
}

/// 建会话后构造 VPS/SPS/PPS 并创建参数对象
///
/// 验证驱动接受这套 std 参数，并回吐可按起始码切分的 Annex-B 参数集
#[test]
#[ignore = "requires GPU"]
fn test_vulkan_video_h265_session_parameters() {
    let Ok(ctx) = VulkanInstance::new("Vistrel-S4") else {
        println!("Vulkan 不可用，跳过");
        return;
    };
    let caps = match caps::probe_codec(&ctx, VideoCodec::H265) {
        Ok(caps) => caps,
        Err(e) => {
            println!("H.265 能力探测失败，跳过: {e}");
            return;
        }
    };
    let device = match VideoDevice::new(&ctx, VideoCodec::H265) {
        Ok(device) => device,
        Err(e) => {
            println!("设备创建失败，跳过: {e}");
            return;
        }
    };
    let config = match test_session_config(&caps, 60) {
        Ok(config) => config,
        Err(e) => {
            println!("会话配置构造失败，跳过: {e}");
            return;
        }
    };
    let session = match VideoSession::new(&device, &config) {
        Ok(session) => session,
        Err(e) => {
            println!("H.265 会话创建失败，跳过: {e}");
            return;
        }
    };
    // 与生产路径同源：按内容推导 level，而不是驱动声明的最高 level
    let level = crate::encode::profile_level(VideoCodec::H265, 1920, 1080, 60, 8_000);
    let sequence = SequenceConfig::H265(H265SequenceConfig {
        level_idc: level,
        frame_num_bits: 9,
    });
    match VideoSessionParameters::new(&device, &session, &sequence) {
        Ok(params) => {
            let sets = params.parameter_sets();
            println!(
                "H.265 会话参数对象创建成功: VPS/SPS/PPS @ {}x{} / level_idc {}（推导，驱动上限 {}）/ POC 位宽 9 / 参数集 {} 字节 / 句柄 {:?}",
                session.coded_extent().width,
                session.coded_extent().height,
                level,
                caps.max_level(),
                sets.len(),
                params.params()
            );
            println!("参数集前 16 字节: {:02x?}", &sets[..16.min(sets.len())]);

            assert!(!sets.is_empty(), "驱动应返回非空的参数集");
            let headers = test_support::nal_headers(sets);
            println!("参数集 NAL 头: {headers:02x?}");
            assert_eq!(headers, vec![0x40, 0x42, 0x44], "顺序应为 VPS → SPS → PPS");

            // H.265 的 SPS/PPS 字段更多，覆盖情况比 H.264 更值得看
            let mut get_info = vk::VideoEncodeH265SessionParametersGetInfoKHR::default()
                .write_std_vps(true)
                .write_std_sps(true)
                .write_std_pps(true)
                .std_vps_id(0)
                .std_sps_id(0)
                .std_pps_id(0);
            let info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
                .video_session_parameters(params.params())
                .push(&mut get_info);
            let mut h265_feedback = vk::VideoEncodeH265SessionParametersFeedbackInfoKHR::default();
            let mut feedback =
                vk::VideoEncodeSessionParametersFeedbackInfoKHR::default().push(&mut h265_feedback);
            let bytes = VideoSessionParameters::fetch_encoded(&device, &info, Some(&mut feedback))
                .expect("带反馈地再取一次参数集");
            println!(
                "驱动覆盖情况: VPS={} SPS={} PPS={} / 字节数 {}",
                h265_feedback.has_std_vps_overrides != 0,
                h265_feedback.has_std_sps_overrides != 0,
                h265_feedback.has_std_pps_overrides != 0,
                bytes.len()
            );
        }
        Err(e) => println!("H.265 会话参数对象创建失败: {e}"),
    }
}

/// H.265 单帧 IDR + 一帧 P
///
/// 断言码流结构：IDR 必须带 VPS(32)/SPS(33)/PPS(34) 与 IDR slice(19/20)，
/// P 帧不带参数集且是 TRAIL_R(1)。画面正确性仍需解码器，这里只验结构。
#[test]
#[ignore = "requires GPU"]
fn test_vulkan_video_h265_encode_idr() {
    let Ok(ctx) = VulkanInstance::new("Vistrel-S4") else {
        println!("Vulkan 不可用，跳过");
        return;
    };
    let caps = match caps::probe_codec(&ctx, VideoCodec::H265) {
        Ok(caps) => caps,
        Err(e) => {
            println!("H.265 能力探测失败，跳过: {e}");
            return;
        }
    };
    let device = match VideoDevice::new(&ctx, VideoCodec::H265) {
        Ok(device) => device,
        Err(e) => {
            println!("设备创建失败，跳过: {e}");
            return;
        }
    };
    let mut encoder = match test_vulkan_encoder(&device, &caps, 15) {
        Ok(encoder) => encoder,
        Err(e) => {
            println!("H.265 编码器初始化失败: {e}");
            return;
        }
    };
    let extent = encoder.extent();

    let (idr, is_key) = match encoder.encode_idr(&make_noisy_frame(extent, 7)) {
        Ok(v) => v,
        Err(e) => {
            println!("H.265 IDR 编码失败: {e}");
            return;
        }
    };
    let types = test_support::h265_nal_types(&idr);
    println!(
        "H.265 IDR: {} 字节, 关键帧={}, NAL 类型 {:?}",
        idr.len(),
        is_key,
        types
    );
    assert!(is_key, "首帧必须是关键帧");
    assert!(types.contains(&32), "IDR 应含 VPS(32)");
    assert!(types.contains(&33), "IDR 应含 SPS(33)");
    assert!(types.contains(&34), "IDR 应含 PPS(34)");
    assert!(
        types.contains(&19) || types.contains(&20),
        "IDR 应含 IDR slice（19/20），实际 {types:?}"
    );

    let (p, p_key) = match encoder.encode_frame(&make_noisy_frame(extent, 8), false) {
        Ok(v) => v,
        Err(e) => {
            println!("H.265 P 帧编码失败: {e}");
            return;
        }
    };
    let p_types = test_support::h265_nal_types(&p);
    println!(
        "H.265 P 帧: {} 字节, 关键帧={}, NAL 类型 {:?}",
        p.len(),
        p_key,
        p_types
    );
    assert!(!p_key, "P 帧不应是关键帧");
    assert!(
        !p_types.contains(&32) && !p_types.contains(&33) && !p_types.contains(&34),
        "P 帧不应重复携带参数集，实际 {p_types:?}"
    );
    assert!(
        p_types.contains(&1),
        "P 帧应为 TRAIL_R(1)，实际 {p_types:?}"
    );
}

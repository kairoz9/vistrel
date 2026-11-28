//! H.264 单元与 GPU 集成测试

use super::frame::*;
use super::params::*;
use super::*;

use crate::encode::hardware::vulkan::codec::VideoSessionParameters;
use crate::encode::hardware::vulkan::device::VideoDevice;
use crate::encode::hardware::vulkan::frame_encoder::FrameEncoder;
use crate::encode::hardware::vulkan::session::VideoSession;
use ash::vk::TaggedStructure;
use ash::vk::native::{StdVideoEncodeH264ReferenceListsInfo, StdVideoH264SequenceParameterSetVui};

use crate::encode::VideoCodec;
use crate::encode::hardware::vulkan::codec::config::{
    EncodePolicy, GopPolicy, IntraRefreshPolicy, SessionConfig,
};
use crate::encode::hardware::vulkan::codec::{H264SequenceConfig, SequenceConfig};
use crate::encode::hardware::vulkan::context::VulkanInstance;
use crate::encode::hardware::vulkan::frame_encoder::test_session_config_with;
use crate::encode::hardware::vulkan::frame_encoder::{
    make_noisy_frame, test_frame_encoder, test_profile, test_session_config,
};
use crate::encode::hardware::vulkan::{codec::caps, test_support};

/// IDR：IdrPicFlag 置位、I slice、无参考列表、参考槽信息与自身类型一致
#[test]
fn test_idr_frame_std() {
    let parts = H264FrameParts::new(None);
    let frame = build(&parts, true, 0, 1);

    assert_eq!(frame.pic_info.flags.IdrPicFlag(), 1);
    assert_eq!(frame.pic_info.primary_pic_type, PICTURE_TYPE_IDR);
    assert_eq!(frame.slice_header.slice_type, SLICE_TYPE_I);
    assert!(frame.pic_info.pRefLists.is_null(), "IDR 不挂参考列表");
    assert_eq!(frame.pic_info.flags.is_reference(), 1, "本帧应成为参考帧");
    assert_eq!(frame.pic_info.idr_pic_id, 1, "相邻 IDR 必须互异");
    assert_eq!(frame.ref_info.primary_pic_type, PICTURE_TYPE_IDR);
    assert_eq!(frame.ref_info.FrameNum, 0);
}

/// P 帧：参考列表首项指向给定槽位、其余为空位；frame_num 与 POC 同源
#[test]
fn test_p_frame_std() {
    let parts = H264FrameParts::new(Some(1));
    let frame = build(&parts, false, 7, 0);

    assert_eq!(frame.pic_info.flags.IdrPicFlag(), 0);
    assert_eq!(frame.pic_info.frame_num, 7);
    assert_eq!(frame.pic_info.PicOrderCnt, 7);
    assert_eq!(frame.slice_header.slice_type, SLICE_TYPE_P);
    // 0 = 启用去块（字段名是「禁用」，写 1 才是关闭）
    assert_eq!(frame.slice_header.disable_deblocking_filter_idc, 0);
    assert!(!frame.pic_info.pRefLists.is_null(), "P 帧必须挂参考列表");
    assert_eq!(parts.ref_lists.RefPicList0[0], 1, "参考槽索引");
    assert_eq!(
        parts.ref_lists.RefPicList0[1], NO_REFERENCE_PICTURE,
        "其余表项应为空位"
    );
    assert_eq!(
        parts.ref_lists.RefPicList1[0], NO_REFERENCE_PICTURE,
        "未使用的 L1 也必须全部是空位"
    );
    assert_eq!(parts.ref_lists.num_ref_idx_l0_active_minus1, 0);
}

/// 指针必须指向传进来的 `parts`
#[test]
fn test_pointers_target_parts() {
    let parts = H264FrameParts::new(Some(0));
    let frame = build(&parts, false, 3, 0);

    assert_eq!(
        frame.pic_info.pRefLists, &parts.ref_lists as *const StdVideoEncodeH264ReferenceListsInfo,
        "参考列表指针应指向 parts"
    );
}

/// 上一帧的参考槽信息必须用它**自己的**类型与帧号，不是当前帧的
#[test]
fn test_reference_info_uses_own_type() {
    let idr_ref = reference_info(0, true);
    assert_eq!(idr_ref.primary_pic_type, PICTURE_TYPE_IDR);
    assert_eq!(idr_ref.FrameNum, 0);
    assert_eq!(idr_ref.PicOrderCnt, 0);

    let p_ref = reference_info(6, false);
    assert_eq!(p_ref.primary_pic_type, PICTURE_TYPE_P);
    assert_eq!(p_ref.FrameNum, 6);
    assert_eq!(p_ref.PicOrderCnt, 6);
}

/// SPS：1080→1088 只裁底部（差值一半）、位宽夹取、宏块数按 16 换算
#[test]
fn test_sps_fields() {
    let vui = vui();
    let coded = vk::Extent2D {
        width: 1920,
        height: 1088,
    };
    let display = vk::Extent2D {
        width: 1920,
        height: 1080,
    };
    let s = sps(coded, display, 12, 12, &vui);

    assert_eq!(s.profile_idc, H264_PROFILE_HIGH);
    assert_eq!(
        s.level_idc, 12,
        "SPS 的 level 是枚举：4.2 = 12，不是语法值 42"
    );
    assert_eq!(s.chroma_format_idc, H264_CHROMA_420);
    assert_eq!(s.max_num_ref_frames, MAX_NUM_REF_FRAMES);
    assert_eq!(s.flags.frame_cropping_flag(), 1);
    assert_eq!(s.frame_crop_right_offset, 0, "宽相同不裁右侧");
    assert_eq!(s.frame_crop_bottom_offset, 4, "(1088-1080)/2 = 4");
    // 12 位夹取：两个位宽字段都是 12-4
    assert_eq!(s.log2_max_frame_num_minus4, 8);
    assert_eq!(s.log2_max_pic_order_cnt_lsb_minus4, 8);
    assert_eq!(s.pic_width_in_mbs_minus1, 1920 / MB_SIZE - 1);
    assert_eq!(s.pic_height_in_map_units_minus1, 1088 / MB_SIZE - 1);
    assert_eq!(
        s.pSequenceParameterSetVui, &vui as *const StdVideoH264SequenceParameterSetVui,
        "VUI 指针必须指向调用方持有的那一份"
    );
}

/// level_idc 的语法值 → 枚举映射；4.2 是 12，越界值必须拒绝
#[test]
fn test_level_to_vk() {
    assert_eq!(level_to_vk(42), Ok(12));
    assert_eq!(level_to_vk(52), Ok(15));
    assert_eq!(level_to_vk(62), Ok(18));
    assert!(level_to_vk(43).is_err(), "不存在的语法值必须拒绝");
}

/// VUI：BT.709 + Limited Range——缺了它解码器按 BT.601 猜，整片偏色
#[test]
fn test_vui_bt709_limited() {
    let v = vui();
    assert_eq!(v.flags.video_signal_type_present_flag(), 1);
    assert_eq!(v.flags.color_description_present_flag(), 1);
    assert_eq!(v.flags.video_full_range_flag(), 0, "0 = limited range");
    assert_eq!(v.colour_primaries, 1, "BT.709");
    assert_eq!(v.transfer_characteristics, 1);
    assert_eq!(v.matrix_coefficients, 1);
}

/// PPS：CABAC 开（同画质更低码率）
#[test]
fn test_pps_cabac() {
    let p = pps();
    assert_eq!(p.flags.entropy_coding_mode_flag(), 1);
    assert_eq!(p.flags.deblocking_filter_control_present_flag(), 1);
}

/// 建会话后构造 SPS/PPS 并创建参数对象
///
/// 验三件事：驱动接受了这套 std 参数、它回吐的参数集能按 Annex-B 切出 SPS → PPS，
/// 以及它有没有覆盖我们给的 std 参数（覆盖为真就意味着手写序列化器那条路是错的）。
#[test]
#[ignore = "requires GPU"]
fn test_vulkan_video_h264_session_parameters() {
    let Ok(ctx) = VulkanInstance::new("Vistrel-S1d") else {
        println!("Vulkan 不可用，跳过");
        return;
    };
    let caps = match caps::probe_codec(&ctx, VideoCodec::H264) {
        Ok(caps) => caps,
        Err(e) => {
            println!("H.264 能力探测失败，跳过: {e}");
            return;
        }
    };
    let device = match VideoDevice::new(&ctx, VideoCodec::H264) {
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
            println!("会话创建失败，跳过: {e}");
            return;
        }
    };
    // 与生产路径同源：按内容推导 level，而不是驱动声明的最高 level
    let level = u32::from(crate::encode::h264_level_idc(1920, 1080, 60, 8_000));
    let level_vk = level_to_vk(level).expect("推导出的 level 必须能映射到枚举");
    let sequence = SequenceConfig::H264(H264SequenceConfig {
        level_idc: level,
        frame_num_bits: 4,
    });
    match VideoSessionParameters::new(&device, &session, &sequence) {
        Ok(params) => {
            let sets = params.parameter_sets();
            println!(
                "H.264 会话参数对象创建成功: SPS/PPS @ {}x{} / level 语法值 {} → 枚举 {}（驱动上限 {}）/ frame_num 位宽 4 / 参数集 {} 字节 / 句柄 {:?}",
                session.coded_extent().width,
                session.coded_extent().height,
                level,
                level_vk,
                caps.max_level(),
                sets.len(),
                params.params()
            );
            println!("参数集前 16 字节: {:02x?}", &sets[..16.min(sets.len())]);

            assert!(!sets.is_empty(), "驱动应返回非空的参数集");
            let headers = test_support::nal_headers(sets);
            println!("参数集 NAL 头: {headers:02x?}");
            assert_eq!(headers, vec![0x67, 0x68], "顺序应为 SPS → PPS");

            // 再取一次，这次挂上反馈结构：驱动若覆盖了 std 参数，手写序列化器那条路
            // 产出的 SPS/PPS 就与它实际编码所依据的不一致
            let mut get_info = vk::VideoEncodeH264SessionParametersGetInfoKHR::default()
                .write_std_sps(true)
                .write_std_pps(true)
                .std_sps_id(0)
                .std_pps_id(0);
            let info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
                .video_session_parameters(params.params())
                .push(&mut get_info);
            let mut h264_feedback = vk::VideoEncodeH264SessionParametersFeedbackInfoKHR::default();
            let mut feedback =
                vk::VideoEncodeSessionParametersFeedbackInfoKHR::default().push(&mut h264_feedback);
            let bytes = VideoSessionParameters::fetch_encoded(
                &device,
                &info,
                &mut feedback as *mut _ as *mut std::ffi::c_void,
            )
            .expect("带反馈地再取一次参数集");
            println!(
                "驱动覆盖情况: SPS={} PPS={} / 字节数 {}",
                h264_feedback.has_std_sps_overrides != 0,
                h264_feedback.has_std_pps_overrides != 0,
                bytes.len()
            );
            if h264_feedback.has_std_sps_overrides == 0 {
                let sps_level = bytes
                    .windows(5)
                    .position(|w| w == [0, 0, 0, 1, 0x67])
                    .map(|i| bytes[i + 7]);
                assert_eq!(
                    sps_level,
                    Some(0x2A),
                    "SPS 第 4 个字节是 level_idc，4.2 应写成 0x2A（不是 0x5F）"
                );
            }
        }
        Err(e) => println!("会话参数对象创建失败: {e}"),
    }
}

/// 编码一帧 IDR，检查产出的码流以起始码开头且含 SPS/PPS/IDR
#[test]
#[ignore = "requires GPU"]
fn test_vulkan_video_h264_encode_idr() {
    let Ok(ctx) = VulkanInstance::new("Vistrel-S1d2") else {
        println!("Vulkan 不可用，跳过");
        return;
    };
    let caps = match caps::probe_codec(&ctx, VideoCodec::H264) {
        Ok(caps) => caps,
        Err(e) => {
            println!("H.264 能力探测失败，跳过: {e}");
            return;
        }
    };
    let device = match VideoDevice::new(&ctx, VideoCodec::H264) {
        Ok(device) => device,
        Err(e) => {
            println!("设备创建失败，跳过: {e}");
            return;
        }
    };
    let mut encoder = match test_frame_encoder(&device, &caps, 15) {
        Ok(encoder) => encoder,
        Err(e) => {
            println!("编码器初始化失败: {e}");
            return;
        }
    };
    // 分阶段打印：SIGSEGV 时靠最后一条标记定位崩在哪一步
    println!(
        "编码器就绪, 开始造帧 (HAS_OVERRIDES={})",
        encoder.feedback_has_overrides
    );

    let extent = encoder.extent();
    let pixels = extent.width as usize * extent.height as usize;
    let mut nv12 = vec![0u8; pixels + pixels / 2];
    for (i, value) in nv12[..pixels].iter_mut().enumerate() {
        *value = (i % 200 + 16) as u8;
    }
    nv12[pixels..].fill(128);

    println!("开始编码 IDR");
    let (idr, is_key) = match encoder.encode_idr(&nv12) {
        Ok(v) => v,
        Err(e) => {
            println!("IDR 编码失败: {e}");
            return;
        }
    };
    let idr_types = test_support::h264_nal_types(&idr);
    println!(
        "IDR: {} 字节, 关键帧={}, NAL 类型 {:?}",
        idr.len(),
        is_key,
        idr_types
    );
    assert!(is_key, "首帧必须是关键帧");
    assert!(idr_types.contains(&7), "IDR 码流应包含 SPS(7)");
    assert!(idr_types.contains(&8), "IDR 码流应包含 PPS(8)");
    assert!(idr_types.contains(&5), "IDR 码流应包含 IDR slice(5)");

    let mut p_sizes = Vec::new();
    for n in 1..=2 {
        let (p, p_key) = match encoder.encode_frame(&nv12, false) {
            Ok(v) => v,
            Err(e) => {
                println!("P 帧 {n} 编码失败: {e}");
                return;
            }
        };
        let types = test_support::h264_nal_types(&p);
        println!(
            "P 帧 {n}: {} 字节, 关键帧={}, NAL 类型 {:?}",
            p.len(),
            p_key,
            types
        );
        assert!(!p_key, "P 帧不应是关键帧");
        assert!(!types.contains(&7), "P 帧不应重复携带 SPS");
        p_sizes.push(p.len());
    }
    assert!(
        p_sizes[0] < idr.len(),
        "静态内容下 P 帧应显著小于 IDR（IDR {} 字节, P {} 字节）",
        idr.len(),
        p_sizes[0]
    );

    // ---- 阶段二：高熵内容 + 运行期改码率 ----
    // 静态内容全是 skip，码率控制没有可调空间；换成逐帧不同的随机纹理，
    // 每帧都有真实残差，CBR 才会真的动 QP。
    let mut high_sizes = Vec::new();
    for n in 0..8 {
        let frame = make_noisy_frame(extent, 100 + n);
        match encoder.encode_frame(&frame, false) {
            Ok((p, _)) => high_sizes.push(p.len()),
            Err(e) => {
                println!("8Mbps 段第 {n} 帧失败: {e}");
                return;
            }
        }
    }

    encoder.set_bitrate(2_000_000);
    assert_eq!(encoder.bitrate(), 2_000_000, "码率应已更新");
    let mut low_sizes = Vec::new();
    for n in 0..8 {
        let frame = make_noisy_frame(extent, 200 + n);
        match encoder.encode_frame(&frame, false) {
            Ok((p, _)) => low_sizes.push(p.len()),
            Err(e) => {
                println!("2Mbps 段第 {n} 帧失败: {e}");
                return;
            }
        }
    }

    let high_total: usize = high_sizes.iter().sum();
    let low_total: usize = low_sizes.iter().sum();
    println!("8Mbps 段合计 {high_total} 字节 / 2Mbps 段合计 {low_total} 字节（各 8 帧）");
    assert!(
        low_total < high_total,
        "降码率后同样 8 帧的总字节数应下降（8Mbps {high_total} vs 2Mbps {low_total}）"
    );
}

/// GOP 剩余帧数：即使驱动没有声明 requires/prefers，begin 带上该结构也必须被接受
///
/// 本机 caps 两项都是 false，正常路径不会挂这个结构。这里手动把 prefers 置真，
/// 覆盖 `record_and_submit` 的 pNext 链与 I/P 计数字段。
#[test]
#[ignore = "requires GPU"]
fn test_vulkan_video_h264_gop_remaining_frames() {
    let Ok(ctx) = VulkanInstance::new("Vistrel-S1e") else {
        println!("Vulkan 不可用，跳过");
        return;
    };
    let mut caps = match caps::probe_codec(&ctx, VideoCodec::H264) {
        Ok(caps) => caps,
        Err(e) => {
            println!("H.264 能力探测失败，跳过: {e}");
            return;
        }
    };
    if let crate::encode::hardware::vulkan::codec::CodecSpecific::H264(specific) =
        &mut caps.specific
    {
        specific.prefers_gop_remaining_frames = true;
    }
    let device = match VideoDevice::new(&ctx, VideoCodec::H264) {
        Ok(device) => device,
        Err(e) => {
            println!("设备创建失败，跳过: {e}");
            return;
        }
    };
    let mut encoder = match test_frame_encoder(&device, &caps, 15) {
        Ok(encoder) => encoder,
        Err(e) => {
            println!("编码器初始化失败: {e}");
            return;
        }
    };
    let extent = encoder.extent();
    let (_, key) = match encoder.encode_idr(&make_noisy_frame(extent, 1)) {
        Ok(v) => v,
        Err(e) => {
            println!("IDR 编码失败（GOP 剩余帧数结构可能不合法）: {e}");
            return;
        }
    };
    assert!(key, "首帧必须是关键帧");
    // 同一 GOP 内的 P 帧，剩余 P 帧数逐帧递减
    for n in 0..3 {
        match encoder.encode_frame(&make_noisy_frame(extent, 10 + n), false) {
            Ok((_, false)) => {}
            Ok((_, true)) => panic!("第 {n} 个 P 帧不应是关键帧"),
            Err(e) => panic!("第 {n} 个 P 帧编码失败: {e}"),
        }
    }
    println!("GOP 剩余帧数结构已被驱动接受: IDR + 3 P 帧");
}

/// Intra-Refresh：长 GOP + 渐进刷新，不产出周期性 IDR
///
/// 没有解码器，无法验证「画面确实逐块刷新干净」；这条能验三件事：
/// 驱动接受参数、长 GOP 下 frame_num 位宽正确、以及**不再出现周期性 IDR**。
#[test]
#[ignore = "requires GPU"]
fn test_vulkan_video_h264_intra_refresh() {
    let Ok(ctx) = VulkanInstance::new("Vistrel-S5") else {
        println!("Vulkan 不可用，跳过");
        return;
    };
    let caps = match caps::probe_codec(&ctx, VideoCodec::H264) {
        Ok(caps) => caps,
        Err(e) => {
            println!("H.264 能力探测失败，跳过: {e}");
            return;
        }
    };
    if !caps.supports_intra_refresh() {
        println!(
            "驱动未声明 intra-refresh 能力，跳过: 模式 [{:?}] / 最大周期 {}",
            caps.intra_refresh.modes, caps.intra_refresh.max_cycle_duration
        );
        return;
    }
    let device = match VideoDevice::new(&ctx, VideoCodec::H264) {
        Ok(device) => device,
        Err(e) => {
            println!("设备创建失败，跳过: {e}");
            return;
        }
    };
    // 能力查询说支持、但 feature 位没链进 create_info 时设备侧仍是关闭状态
    if !device.supports_intra_refresh() {
        println!("设备未开启 intra-refresh feature 位，跳过");
        return;
    }

    // 越界策略必须在构造期被拒，不能静默截断
    let max_cycle = caps.intra_refresh.max_cycle_duration;
    let too_small = EncodePolicy {
        intra_refresh: IntraRefreshPolicy::Fixed(1),
        ..Default::default()
    };
    assert!(
        SessionConfig::build(&caps, &test_profile(), &too_small, true).is_err(),
        "周期 < 2 必须被拒绝"
    );
    if max_cycle < u32::MAX {
        let too_large = EncodePolicy {
            intra_refresh: IntraRefreshPolicy::Fixed(max_cycle + 1),
            ..Default::default()
        };
        assert!(
            SessionConfig::build(&caps, &test_profile(), &too_large, true).is_err(),
            "超过驱动上限必须被拒绝"
        );
    }

    // GOP 取 300 帧：frame_num 位宽应升到 9，否则参考链会在 GOP 中途断掉
    let policy = EncodePolicy {
        gop: GopPolicy::Fixed(300),
        ..Default::default()
    };
    let config = match test_session_config_with(&caps, &policy, true) {
        Ok(config) => config,
        Err(e) => {
            println!("会话配置构造失败，跳过: {e}");
            return;
        }
    };
    let cycle = config
        .intra_refresh_cycle()
        .expect("能力与 feature 位都成立时必须启用 intra-refresh");
    let coding = config.coding_control(60);
    let mut encoder = match FrameEncoder::new(&device, config, coding) {
        Ok(encoder) => encoder,
        Err(e) => {
            println!("编码器初始化失败: {e}");
            return;
        }
    };

    let extent = encoder.extent();
    let (idr, is_key) = match encoder.encode_idr(&make_noisy_frame(extent, 7)) {
        Ok(v) => v,
        Err(e) => {
            println!("IDR 编码失败: {e}");
            return;
        }
    };
    assert!(is_key, "首帧必须是关键帧");
    assert!(
        test_support::h264_nal_types(&idr).contains(&5),
        "首帧应含 IDR slice(5)"
    );

    let mut sizes = Vec::new();
    for n in 0..20 {
        let (p, key) = match encoder.encode_frame(&make_noisy_frame(extent, 100 + n), false) {
            Ok(v) => v,
            Err(e) => {
                println!("第 {n} 帧编码失败: {e}");
                return;
            }
        };
        let types = test_support::h264_nal_types(&p);
        assert!(!key, "第 {n} 帧不该是关键帧：长 GOP 下不应出现周期性 IDR");
        assert!(!types.contains(&5), "第 {n} 帧不该是 IDR slice");
        assert!(!types.contains(&7), "第 {n} 帧不该重复携带 SPS");
        sizes.push(p.len());
    }
    let total: usize = sizes.iter().sum();
    println!("intra-refresh: 周期 {cycle}（上限 {max_cycle}）/ GOP 300 / 20 帧合计 {total} 字节");
}

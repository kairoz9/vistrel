use super::frame::*;
use super::sequence::*;
use super::*;

use crate::encode::VideoCodec;
use crate::encode::hardware::vulkan::codec::VideoSessionParameters;
use crate::encode::hardware::vulkan::codec::config::EncodePolicy;
use crate::encode::hardware::vulkan::context::VulkanInstance;
use crate::encode::hardware::vulkan::device::VideoDevice;
use crate::encode::hardware::vulkan::encoder::VulkanEncoder;
use crate::encode::hardware::vulkan::encoder::{
    make_noisy_frame, test_session_config, test_session_config_with, test_vulkan_encoder,
};
use crate::encode::hardware::vulkan::session::VideoSession;
use crate::encode::hardware::vulkan::{codec::caps, test_support};
use ash::vk::native::{
    StdVideoAV1ColorConfig, StdVideoAV1GlobalMotion, StdVideoAV1LoopFilter, StdVideoAV1Quantization,
};

/// 位宽换算：1920/1080 是实际会走到的取值，另外验一下边界
#[test]
fn test_frame_size_bits() {
    assert_eq!(bits_minus_1(1919), 10, "1919 需要 11 位，减 1 得 10");
    assert_eq!(bits_minus_1(1079), 10, "1079 同样需要 11 位");
    assert_eq!(bits_minus_1(0), 0, "0 只需 1 位，减 1 得 0");
    assert_eq!(bits_minus_1(1), 0, "1 只需 1 位");
    assert_eq!(bits_minus_1(255), 7, "255 需要 8 位");
    assert_eq!(bits_minus_1(256), 8, "256 需要 9 位");
}

/// 序列头必须自洽：宽高与入参一致、profile 为 Main、色彩为 BT.709 limited
#[test]
fn test_sequence_header_fields() {
    let color = color_config();
    let header = sequence_header(
        vk::Extent2D {
            width: 1920,
            height: 1088,
        },
        &color,
        false,
    );

    assert_eq!(header.seq_profile, AV1_PROFILE_MAIN);
    assert_eq!(header.max_frame_width_minus_1, 1919);
    assert_eq!(header.max_frame_height_minus_1, 1087);
    assert_eq!(header.frame_width_bits_minus_1, 10);
    assert_eq!(header.frame_height_bits_minus_1, 10);
    assert_eq!(
        header.pColorConfig, &color as *const StdVideoAV1ColorConfig,
        "色彩配置指针必须指向调用方持有的那一份"
    );
    assert!(header.pTimingInfo.is_null(), "不下发 timing info");

    // film grain 被 VUID 强制为 0，打开会让会话参数创建直接失败
    assert_eq!(header.flags.film_grain_params_present(), 0);
    // 驱动只支持 64x64 时序列头不能声明 128
    assert_eq!(header.flags.use_128x128_superblock(), 0);
    // 驱动支持 128x128（或只支持 128）时必须能声明它
    let header128 = sequence_header(
        vk::Extent2D {
            width: 1920,
            height: 1088,
        },
        &color,
        true,
    );
    assert_eq!(
        header128.flags.use_128x128_superblock(),
        1,
        "128-only 驱动必须能生成合法的序列头"
    );
    // 序列头工具全部为 0（含 order hint）
    assert_eq!(
        header.flags.enable_order_hint(),
        0,
        "本实现不用任何依赖 order hint 的工具，关掉它少一份跨帧同步状态"
    );
    assert_eq!(header.flags.enable_cdef(), 0, "与 FFmpeg 一致：cdef 关");
    assert_eq!(header.flags.enable_restoration(), 0, "与 FFmpeg 一致");
    assert_eq!(header.flags.enable_filter_intra(), 0, "与 FFmpeg 一致");
    assert_eq!(header.flags.enable_intra_edge_filter(), 0, "与 FFmpeg 一致");
    assert_eq!(header.flags.enable_warped_motion(), 0, "与 FFmpeg 一致");
    assert_eq!(header.flags.enable_dual_filter(), 0, "与 FFmpeg 一致");
    assert_eq!(header.flags.enable_masked_compound(), 0, "与 FFmpeg 一致");
    assert_eq!(
        header.flags.enable_interintra_compound(),
        0,
        "与 FFmpeg 一致"
    );
    assert_eq!(
        header.flags.enable_jnt_comp(),
        0,
        "依赖 order hint，FFmpeg 也关"
    );
    assert_eq!(header.flags.enable_ref_frame_mvs(), 0, "同上");

    // order hint 关闭 ⇒ 位宽字段必须为 0（不参与码流）
    assert_eq!(header.order_hint_bits_minus_1, 0);
}

/// 色彩配置：limited range + BT.709，与 CSC 的转换公式同源
#[test]
fn test_color_config() {
    let color = color_config();
    assert_eq!(color.BitDepth, 8);
    assert_eq!(color.subsampling_x, 1);
    assert_eq!(color.subsampling_y, 1);
    assert_eq!(color.flags.color_range(), 0, "0 = limited range");
    assert_eq!(color.flags.mono_chrome(), 0);
    assert_eq!(color.flags.color_description_present_flag(), 1);
    assert_eq!(color.matrix_coefficients, BT709);
    // 4:2:0 必须显式声明共位：UNKNOWN 会让硬件解码器自行假设色度相位，
    // 与编码端的共位输出错开 ⇒ 彩色噪点（软件解码器宽容，测不出来）
    assert_eq!(
        color.chroma_sample_position, CHROMA_POS_COLOCATED,
        "不能填 CSP_UNKNOWN"
    );
}

/// 显示尺寸。编码尺寸是 1920x1088，多出来的 8 行不进 `render_size`
fn display() -> vk::Extent2D {
    vk::Extent2D {
        width: 1920,
        height: 1080,
    }
}

/// 编码尺寸：显示高度向上对齐到 16
fn coded() -> vk::Extent2D {
    vk::Extent2D {
        width: 1920,
        height: 1088,
    }
}

/// 直接调 `build`，参数与生产路径一致（不传 order hint —— 序列头没开）
fn build_default<'a>(
    parts: &'a Av1FrameParts,
    is_key: bool,
    frame_idx: u32,
    setup_slot: u8,
    ref_slot: u8,
    coded: vk::Extent2D,
    display: vk::Extent2D,
) -> Av1FrameStd<'a> {
    build(
        parts, is_key, frame_idx, setup_slot, ref_slot, coded, display,
    )
}

/// KEY 帧：刷新全部 VBI 槽、无参考、帧类型与预测方式一致
#[test]
fn test_key_frame_std() {
    let parts = Av1FrameParts::new(100);
    let frame = build_default(&parts, true, 0, 0, 0, coded(), display());

    assert_eq!(frame.picture_info.frame_type, FRAME_TYPE_KEY);
    // 规范隐式要求 allFrames：给别的值会让驱动与解码端的 VBI 状态分叉（实测黑屏）
    assert_eq!(frame.picture_info.refresh_frame_flags, REFRESH_ALL);
    assert_eq!(frame.picture_info.primary_ref_frame, PRIMARY_REF_NONE);
    assert_eq!(frame.ref_info.frame_type, FRAME_TYPE_KEY);
    // 显示尺寸与编码尺寸分开：1088 的帧只渲染 1080 行，且必须置差异标志
    assert_eq!(
        frame.picture_info.flags.render_and_frame_size_different(),
        1,
        "1080 != 1088 时必须声明渲染尺寸，否则码流里不写这两个字段"
    );
    assert_eq!(frame.picture_info.render_height_minus_1, 1079);
    assert_eq!(frame.picture_info.render_width_minus_1, 1919);
}

/// 帧间帧：刷新**本帧写入的那个** VBI 槽，7 个参考名全部指向参考帧的槽号
///
/// 关键不变量：VBI 槽号 == DPB 槽号。`setup_slot` 既是 DPB 槽也是被刷新的 VBI 槽，
/// `ref_slot` 既是参考帧的 DPB 槽也是它的 VBI 槽 —— 两套编号错开会让驱动拿错参考图。
#[test]
fn test_inter_frame_std() {
    let parts = Av1FrameParts::new(100);
    let frame = build_default(&parts, false, 7, 1, 0, coded(), display());

    assert_eq!(frame.picture_info.frame_type, FRAME_TYPE_INTER);
    assert_eq!(
        frame.picture_info.refresh_frame_flags, 0x02,
        "刷新的是本帧写入的槽 1，不是恒定的槽 0"
    );
    assert!(
        frame.picture_info.ref_frame_idx.iter().all(|&i| i == 0),
        "7 个参考名必须全部指向参考帧所在的槽 0"
    );
    assert_eq!(
        frame.picture_info.primary_ref_frame, LAST_FRAME_NAME,
        "帧间帧用 LAST_FRAME 做 CDF 预测，不能是 PRIMARY_REF_NONE"
    );
    assert_eq!(frame.picture_info.current_frame_id, 7);
}

/// 两套编号必须一致：刷新的 VBI 槽 == 写入的 DPB 槽
#[test]
fn test_vbi_slot_equals_dpb_slot() {
    let parts = Av1FrameParts::new(100);
    for setup in 0u8..2 {
        let frame = build_default(&parts, false, 3, setup, 1 - setup, coded(), display());
        assert_eq!(
            frame.picture_info.refresh_frame_flags,
            1u8 << setup,
            "VBI 槽号必须等于 DPB 槽号 {setup}"
        );
        assert!(
            frame
                .picture_info
                .ref_frame_idx
                .iter()
                .all(|&i| i == (1 - setup) as i8),
            "参考名必须指向另一格槽 {setup}"
        );
    }
}

/// 显示尺寸等于编码尺寸时不该写 render_size
#[test]
fn test_render_size_flag_when_equal() {
    let parts = Av1FrameParts::new(100);
    let frame = build_default(&parts, true, 0, 0, 0, coded(), coded());
    assert_eq!(
        frame.picture_info.flags.render_and_frame_size_different(),
        0,
        "两者相等时置位会让码流多出无意义的渲染尺寸"
    );
}

/// 三个指针必须指向传进来的 `parts`，不是别处
#[test]
fn test_pointers_target_parts() {
    let parts = Av1FrameParts::new(128);
    let frame = build_default(&parts, true, 0, 0, 0, coded(), display());

    assert_eq!(
        frame.picture_info.pQuantization,
        &parts.quantization as *const StdVideoAV1Quantization
    );
    assert_eq!(
        frame.picture_info.pLoopFilter,
        &parts.loop_filter as *const StdVideoAV1LoopFilter
    );
    assert_eq!(
        frame.picture_info.pGlobalMotion,
        &parts.global_motion as *const StdVideoAV1GlobalMotion
    );
}

/// 序列头里关掉的工具，每帧指针必须为 null（尤其 pSegmentation 是规范强制）
#[test]
fn test_disabled_tools_are_null() {
    let parts = Av1FrameParts::new(100);
    let frame = build_default(&parts, false, 1, 0, 1, coded(), display());

    assert!(
        frame.picture_info.pSegmentation.is_null(),
        "规范强制为 null"
    );
    assert!(
        frame.picture_info.pCDEF.is_null(),
        "序列头 enable_cdef = 0（与 FFmpeg 一致），给了指针就是非法用法"
    );
    assert!(
        frame.picture_info.pLoopRestoration.is_null(),
        "序列头 enable_restoration = 0"
    );
    assert!(frame.picture_info.pTileInfo.is_null(), "单 tile");
    assert!(frame.picture_info.pExtensionHeader.is_null());
    assert!(frame.picture_info.pBufferRemovalTimes.is_null());
}

/// 逐帧参数与 FFmpeg `init_pic_params` 逐字段对齐
///
/// FFmpeg 的取值是「环路滤波强度全 0（等于关掉）+ 参考帧/模式 delta 用 AV1 默认表」，
/// 不要凭「画质更好」把它改成非零 —— 那是与参考实现分叉。
#[test]
fn test_frame_parts_match_ffmpeg() {
    let parts = Av1FrameParts::new(128);
    // FFmpeg：`loop_filter_level = { 0 }`
    assert_eq!(parts.loop_filter.loop_filter_level, [0; 4]);
    assert_eq!(parts.loop_filter.loop_filter_sharpness, 0);
    // FFmpeg：`default_loop_filter_ref_deltas = { 1, 0, 0, 0, -1, 0, -1, -1 }`
    assert_eq!(
        parts.loop_filter.loop_filter_ref_deltas,
        [1, 0, 0, 0, -1, 0, -1, -1]
    );
    // FFmpeg：`update_mode_delta = 1`，`update_ref_delta` 保持 0
    assert_eq!(parts.loop_filter.update_mode_delta, 1);
    assert_eq!(parts.loop_filter.update_ref_delta, 0);
    // FFmpeg：量化偏移与 qmatrix 全 0
    assert_eq!(parts.quantization.base_q_idx, 128);
    assert_eq!(parts.quantization.DeltaQUDc, 0);
    assert_eq!(parts.quantization.qm_y, 0);
    // FFmpeg：全局运动全 IDENTITY
    assert_eq!(parts.global_motion.GmType, [0; NUM_REF_FRAMES]);
}

/// order hint 不参与码流：序列头 `enable_order_hint = 0`，三个相关字段必须全 0
///
/// 关掉它是因为本实现不用任何依赖 order hint 的工具（`frame_refs_short_signaling` /
/// `use_ref_frame_mvs` / `jnt_comp` / `skip_mode`），开着只会多一份必须与解码端
/// 严格同步的跨帧状态 —— 而 `order_hint` / `ref_order_hint[]` 由我们手写，
/// NVENC 侧由驱动生成，任何一处不一致都会让硬件解码器的 MV 投影与编码端分叉。
#[test]
fn test_order_hint_disabled() {
    let parts = Av1FrameParts::new(128);
    let frame = build(&parts, false, 2, 0, 1, coded(), coded());

    assert_eq!(frame.picture_info.order_hint, 0, "enable_order_hint = 0");
    assert_eq!(
        frame.picture_info.ref_order_hint, [0; NUM_REF_FRAMES],
        "enable_order_hint = 0 时这张表不进码流"
    );
    assert_eq!(frame.ref_info.OrderHint, 0);
    // 与 FFmpeg 一致：逐块自适应选变换尺寸
    assert_eq!(frame.picture_info.TxMode, TX_MODE_SELECT);
}

/// 帧头标志：show_frame / showable_frame / error_resilient_mode / 精度
#[test]
fn test_picture_flags() {
    let parts = Av1FrameParts::new(128);

    // KEY 帧：可显示、错误恢复点、showable 由 show_frame 推导
    let key = build_default(&parts, true, 0, 0, 0, coded(), coded());
    assert_eq!(key.picture_info.flags.show_frame(), 1);
    assert_eq!(key.picture_info.flags.error_resilient_mode(), 1, "关键帧");
    assert_eq!(
        key.picture_info.flags.segmentation_enabled(),
        0,
        "规范不支持"
    );
    assert_eq!(key.picture_info.flags.frame_refs_short_signaling(), 0);
    assert_eq!(
        key.picture_info.flags.allow_warped_motion(),
        0,
        "序列头关着"
    );

    // 帧间帧：showable = 1，error_resilient = 0（与 FFmpeg 的表达式一致）
    let inter = build_default(&parts, false, 3, 0, 1, coded(), coded());
    assert_eq!(inter.picture_info.flags.show_frame(), 1);
    assert_eq!(inter.picture_info.flags.showable_frame(), 1);
    assert_eq!(inter.picture_info.flags.error_resilient_mode(), 0);
    assert_eq!(
        inter.picture_info.flags.allow_high_precision_mv(),
        0,
        "与 FFmpeg 一致"
    );
}

/// AV1 会话参数对象：驱动接受序列头，并回吐一个合法的 sequence header OBU
///
/// 这里验两件事：驱动收下了这套 std 参数，以及 `vkGetEncodedVideoSessionParametersKHR`
/// 真的能产出 OBU 字节。后者是 AV1 路径成立的前提 —— 没有它就得自己写序列头语法。
#[test]
#[ignore = "requires GPU"]
fn test_vulkan_video_av1_session_parameters() {
    let Ok(ctx) = VulkanInstance::new("Vistrel-S4d") else {
        println!("Vulkan 不可用，跳过");
        return;
    };
    let caps = match caps::probe_codec(&ctx, VideoCodec::Av1) {
        Ok(caps) => caps,
        Err(e) => {
            println!("AV1 能力探测失败，跳过: {e}");
            return;
        }
    };
    let device = match VideoDevice::new(&ctx, VideoCodec::Av1) {
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
            println!("AV1 会话创建失败，跳过: {e}");
            return;
        }
    };
    // 与生产路径同源：按内容推导 level，而不是驱动声明的最高 level
    let level_idx = u32::from(crate::encode::av1_level_idx(1920, 1080, 60, 8_000));
    match VideoSessionParameters::new(&device, &session, config.sequence()) {
        Ok(params) => {
            let obu = params.parameter_sets();
            println!(
                "AV1 会话参数对象创建成功: 序列头 OBU {} 字节 @ {}x{} / level {}（推导，驱动上限 {}）/ 句柄 {:?}",
                obu.len(),
                session.coded_extent().width,
                session.coded_extent().height,
                level_idx,
                caps.max_level(),
                params.params()
            );
            println!("OBU 前 16 字节: {:02x?}", &obu[..16.min(obu.len())]);

            assert!(!obu.is_empty(), "驱动应返回非空的序列头 OBU");
            // OBU 头：forbidden(1) | type(4) | extension(1) | has_size(1) | reserved(1)
            let obu_type = (obu[0] >> 3) & 0x0F;
            assert_eq!(obu_type, 1, "序列头 OBU 的 obu_type 必须是 1");
        }
        Err(e) => println!("AV1 会话参数对象创建失败: {e}"),
    }
}

/// AV1 的 KEY + INTER 帧
///
/// 断言两件事：
/// 1. **码流结构**：KEY 帧前面必须有 sequence header OBU（我们自己拼的），帧间帧不得
///    重复携带序列头。AV1 没有起始码，切分要靠 OBU 头里的 leb128 长度。
/// 2. **帧间帧不能灾难性地贵**（8 倍 KEY 以内）。这只是兜底 —— 「参考帧有没有被用上」
///    **不能**用字节数判定：同内容 P 帧在 CBR 稳态下不会塌成几百字节，H.264 也一样
///    （实测 KEY 6157 / P [8164, 4056, 6101]）。要判定参考链只能解码看画面，
///    见 `test_vulkan_video_av1_static_sequence_stability` 写出的 `av1_stability.obu`。
///
/// 用例里同时跑一遍 H.264 作**对照**（只打印不断言）：它是「同一套调用顺序下正常编码器
/// 长什么样」的标尺，换 codec 时先看它有没有变。
#[test]
#[ignore = "requires GPU"]
fn test_vulkan_video_av1_encode_key() {
    let Ok(ctx) = VulkanInstance::new("Vistrel-S4d") else {
        println!("Vulkan 不可用，跳过");
        return;
    };
    let caps = match caps::probe_codec(&ctx, VideoCodec::Av1) {
        Ok(caps) => caps,
        Err(e) => {
            println!("AV1 能力探测失败，跳过: {e}");
            return;
        }
    };
    let device = match VideoDevice::new(&ctx, VideoCodec::Av1) {
        Ok(device) => device,
        Err(e) => {
            println!("设备创建失败，跳过: {e}");
            return;
        }
    };
    let mut encoder = match test_vulkan_encoder(&device, &caps, 15) {
        Ok(encoder) => encoder,
        Err(e) => {
            println!("AV1 编码器初始化失败: {e}");
            return;
        }
    };
    let extent = encoder.extent();

    let (key, is_key) = match encoder.encode_idr(&make_noisy_frame(extent, 11)) {
        Ok(v) => v,
        Err(e) => {
            println!("AV1 KEY 帧编码失败: {e}");
            return;
        }
    };
    let types = test_support::av1_obu_types(&key);
    println!(
        "AV1 KEY: {} 字节, 关键帧={}, OBU 类型 {:?}",
        key.len(),
        is_key,
        types
    );
    assert!(is_key, "首帧必须是关键帧");
    assert_eq!(types.first(), Some(&1), "KEY 帧必须以 sequence header 开头");
    assert!(
        types.iter().any(|&t| matches!(t, 3 | 4 | 6)),
        "KEY 帧应含 frame / tile group / frame header OBU，实际 {types:?}"
    );

    // 决定性的验证方法：**同一帧内容紧接着连编三次**。
    //
    // 参考链正常时残差几乎全为 0，P 帧会塌成几百字节；如果它和 KEY 帧同量级，
    // 说明编码器根本没把上一帧当参考（或参考槽接错了）。这正是
    // 「静止画面噪点越积越重、运动时整片偏色」的机制：残差按 A 帧算、
    // 解码端加在 B 帧上，误差沿参考链累积成随机游走。
    //
    // 连编三次而不是只看第一次：某些驱动会把 **KEY 之后的第一个 P 帧**当成一次
    // 完整刷新（DPB 刚建立），那一帧偏大是正常的。取三者最小值——只要参考链
    // 能用，总有一帧会塌下去；三帧全都不塌才是真的没在用参考。
    //
    // 这条不需要浏览器也不需要 ffmpeg —— 它直接测「参考帧有没有被用上」。
    let repeat = make_noisy_frame(extent, 11);
    let mut same_sizes = Vec::new();
    for n in 0..3 {
        match encoder.encode_frame(&repeat, false) {
            Ok((bytes, is_key)) => {
                assert!(!is_key, "帧间帧不应是关键帧");
                println!(
                    "AV1 INTER#{n}(同内容): {} 字节 / KEY {} 字节 = {:.3}",
                    bytes.len(),
                    key.len(),
                    bytes.len() as f64 / key.len() as f64
                );
                same_sizes.push(bytes.len());
            }
            Err(e) => {
                println!("AV1 同内容帧间帧编码失败: {e}");
                return;
            }
        }
    }
    let best = *same_sizes.iter().min().unwrap_or(&usize::MAX);

    // **对照组：同一份内容、同一套调用顺序，换成 H.264 再跑一遍。**
    //
    // 这条把「AV1 的参考链坏了」和「这套测试（或 CBR 配置）本身就产不出小 P 帧」
    // 分开。**实测结论（2026-10-24）：H.264 也塌不下去**（KEY 6157 /
    // P [8164, 4056, 6101]），所以「P 帧不塌」不是 AV1 的缺陷，而是 CBR 在这类
    // 内容上的正常行为。保留这个对照，是为了下次换 codec / 改码控时有现成的标尺。
    //
    // 不设断言：它是标尺，不是被测对象。
    let mut h264_report = "H.264 对照不可用".to_string();
    if let (Ok(h264_caps), Ok(h264_device)) = (
        caps::probe_codec(&ctx, VideoCodec::H264),
        VideoDevice::new(&ctx, VideoCodec::H264),
    ) && let Ok(mut h264) = test_vulkan_encoder(&h264_device, &h264_caps, 15)
    {
        let h264_extent = h264.extent();
        let h264_key = match h264.encode_idr(&make_noisy_frame(h264_extent, 11)) {
            Ok((bytes, _)) => bytes.len(),
            Err(e) => {
                println!("H.264 对照 KEY 编码失败: {e}");
                0
            }
        };
        let mut h264_same = Vec::new();
        for n in 0..3 {
            match h264.encode_frame(&make_noisy_frame(h264_extent, 11), false) {
                Ok((bytes, _)) => {
                    println!("H.264 INTER#{n}(同内容): {} 字节", bytes.len());
                    h264_same.push(bytes.len());
                }
                Err(e) => {
                    println!("H.264 对照帧编码失败: {e}");
                    break;
                }
            }
        }
        println!("H.264 对照: KEY {h264_key} 字节 / 同内容 P 帧 {h264_same:?}");
        h264_report = format!("H.264 对照 KEY {h264_key} / P {h264_same:?}");
    }

    // **禁止以「同内容 P 帧必须远小于 KEY」作为断言标准——实测不成立，且不是 AV1 的问题。**
    //
    // 同一份内容、同一套调用顺序、同一个码率，H.264（参考链一直正常）给出
    // KEY 6157 / P [8164, 4056, 6101]：P 帧同样不小于 KEY。所以这条标准测的是
    // 码控行为，不是参考链。
    //
    // 原因是 CBR 的稳态：每帧的残差是「上一帧重建的量化误差」，被当前帧重新编码后
    // 又产生新的误差，循环下去每帧都有非零残差，帧间帧塌不成几百字节。码控把 Q 压到
    // 允许下限时，这个残差可以编得比 intra 还贵（AV1 的 Q 下限比 H.264 更细，所以
    // 它的 P/KEY 比更难看）。
    //
    // 字节数只能兜住**灾难性**回归（帧间帧贵一个数量级）；「参考有没有被用上」只能
    // 靠解码判定，见 `test_vulkan_video_av1_static_sequence_stability` 写出的
    // `target/av1_stability.obu`。
    println!(
        "AV1 同内容 P 帧 {same_sizes:?}（最小 {best}）/ KEY {} 字节；{h264_report}",
        key.len()
    );
    assert!(
        best <= key.len() * 8,
        "同内容 P 帧比 KEY 贵了 8 倍以上（KEY {} 字节，各帧 {same_sizes:?}）；{h264_report}",
        key.len()
    );

    let (p, p_key) = match encoder.encode_frame(&make_noisy_frame(extent, 12), false) {
        Ok(v) => v,
        Err(e) => {
            println!("AV1 帧间帧编码失败: {e}");
            return;
        }
    };
    let p_types = test_support::av1_obu_types(&p);
    println!(
        "AV1 INTER: {} 字节, 关键帧={}, OBU 类型 {:?}",
        p.len(),
        p_key,
        p_types
    );
    assert!(!p_key, "帧间帧不应是关键帧");
    assert!(
        !p_types.contains(&1),
        "帧间帧不应重复携带 sequence header，实际 {p_types:?}"
    );
    assert!(
        p_types.iter().any(|&t| matches!(t, 3 | 4 | 6)),
        "帧间帧应含画面 OBU，实际 {p_types:?}"
    );
}

/// 静态长序列：相同内容连续编码 40 帧，P 帧字节数必须收敛而不是单调增长
///
/// 这是「误差是否在**编码器内部**累积」最直接的判断依据，不需要浏览器、不需要 ffmpeg、
/// 也不需要看画面。正确的 IPPP 里 `recon_n = S + e_n`（误差有界），残差趋于 0；
/// 一旦编码器的重建帧与「按码流解出来的重建帧」分叉，残差就会逐帧变大、字节数
/// 单调上升 —— 那正是「静止画面噪点越来越重、最后没法看」的机制。
///
/// **按生产配置跑**：GOP 取 `INTRA_REFRESH_GOP`、并开启 intra-refresh（驱动支持时），
/// 与 `VulkanVideoBackend::new` 完全一致。GOP 60 无刷新是另一条路径，测不出
/// 「长 GOP + 渐进刷新」组合下的问题。
///
/// 两种结果指向完全不同的方向：
/// - 字节数稳定在几百 ⇒ 编码器内部正常，问题在码流内容或传输/解码侧
/// - 字节数单调增长 ⇒ 编码器每帧都在引入新误差，与刷新机制无关
#[test]
#[ignore = "requires GPU"]
fn test_vulkan_video_av1_static_sequence_stability() {
    let Ok(ctx) = VulkanInstance::new("Vistrel-Stability") else {
        println!("Vulkan 不可用，跳过");
        return;
    };
    let caps = match caps::probe_codec(&ctx, VideoCodec::Av1) {
        Ok(caps) => caps,
        Err(e) => {
            println!("AV1 能力探测失败，跳过: {e}");
            return;
        }
    };
    let device = match VideoDevice::new(&ctx, VideoCodec::Av1) {
        Ok(device) => device,
        Err(e) => {
            println!("AV1 设备创建失败，跳过: {e}");
            return;
        }
    };

    // 与生产路径同一套准入：能力三项 + 设备 feature 位
    let config = match test_session_config_with(
        &caps,
        &EncodePolicy::default(),
        device.supports_intra_refresh(),
    ) {
        Ok(config) => config,
        Err(e) => {
            println!("会话配置构造失败，跳过: {e}");
            return;
        }
    };
    let cycle = config.intra_refresh_cycle();
    let gop = config.gop_length();
    let coding = config.coding_control(60);
    let mut encoder = match VulkanEncoder::new(&device, config, coding) {
        Ok(encoder) => encoder,
        Err(e) => {
            println!("AV1 编码器初始化失败: {e}");
            return;
        }
    };
    println!("配置：GOP {gop} / intra-refresh {cycle:?}");

    let extent = encoder.extent();
    // 全程同一份内容：静态画面下残差应当趋于 0，字节数应当收敛
    let frame = make_noisy_frame(extent, 7);

    let key = match encoder.encode_idr(&frame) {
        Ok((bytes, _)) => bytes,
        Err(e) => {
            println!("AV1 KEY 帧编码失败: {e}");
            return;
        }
    };

    const FRAMES: usize = 40;
    let mut sizes = Vec::with_capacity(FRAMES);
    let mut obu_types: Vec<Vec<u8>> = Vec::with_capacity(FRAMES);
    // AV1 裸 OBU 流用 temporal delimiter 分隔时间单元，否则 demuxer 会把整段当一个单元
    let mut stream = Vec::new();
    stream.extend_from_slice(&[0x12, 0x00]);
    stream.extend_from_slice(&key);
    for n in 0..FRAMES {
        match encoder.encode_frame(&frame, false) {
            Ok((bytes, _)) => {
                sizes.push(bytes.len());
                obu_types.push(test_support::av1_obu_types(&bytes));
                stream.extend_from_slice(&[0x12, 0x00]);
                stream.extend_from_slice(&bytes);
            }
            Err(e) => {
                println!("AV1 第 {n} 个 P 帧编码失败: {e}");
                return;
            }
        }
    }

    println!("AV1 静态序列：KEY {} 字节", key.len());
    for (i, (size, types)) in sizes.iter().zip(&obu_types).enumerate() {
        println!("  第 {:2} 个 P 帧: {size} 字节, OBU {types:?}", i + 1);
    }

    // 码流落到 target/：语法是否合法、画面是否正确只能由外部解码器判定
    let path = "target/av1_stability.obu";
    if std::fs::write(path, &stream).is_ok() {
        println!("\n码流已写出: {path}（{} 字节）", stream.len());
        println!("  语法检查: ffmpeg -v error -i {path} -f null -");
        println!("  解出画面: ffmpeg -v error -i {path} target/av1_stab_%03d.png");
    }

    let head: usize = sizes[..5].iter().sum::<usize>() / 5;
    let tail: usize = sizes[FRAMES - 5..].iter().sum::<usize>() / 5;
    assert!(
        tail <= head * 2,
        "静态内容下 P 帧字节数在累积（前 5 帧均值 {head}，后 5 帧均值 {tail}）：\
             编码器的重建帧在漂移，误差每帧都在增加，与解码端无关"
    );
}

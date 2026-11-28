//! H.264 会话级码流参数：SPS / PPS / VUI 与参数集装配

use ash::vk::native::{
    StdVideoH264PictureParameterSet, StdVideoH264PpsFlags, StdVideoH264SequenceParameterSet,
    StdVideoH264SequenceParameterSetVui, StdVideoH264SpsFlags, StdVideoH264SpsVuiFlags,
};
use ash::vk::{self, TaggedStructure};

use crate::encode::hardware::vulkan::codec::VideoSessionParameters;
use crate::encode::hardware::vulkan::device::VideoDevice;
use crate::encode::hardware::vulkan::session::VideoSession;

// ---- 会话级：SPS / PPS / VUI ----

/// H.264 的宏块边长
pub(super) const MB_SIZE: u32 = 16;
/// High profile：与建会话时声明的 profile 一致，不能各说各话
pub(super) const H264_PROFILE_HIGH: u32 = 100;
/// 4:2:0
pub(super) const H264_CHROMA_420: u32 = 1;
/// 单参考帧：IPPP，够用且让 DPB 需求最小
pub(super) const MAX_NUM_REF_FRAMES: u8 = 1;

/// H.264 码流语法 `level_idc`（10..62）→ `StdVideoH264LevelIdc` 枚举
///
/// 两者数值域不同（语法 4.2 = 42，枚举 4.2 = 12）；直接填语法值会被驱动按越界
/// 枚举序列化。SDP 的 `profile-level-id` 仍用语法值，转换只发生在 std 边界。
pub(in crate::encode::hardware::vulkan) fn level_to_vk(raw: u32) -> Result<u32, String> {
    match raw {
        10 => Ok(0),
        11 => Ok(1),
        12 => Ok(2),
        13 => Ok(3),
        20 => Ok(4),
        21 => Ok(5),
        22 => Ok(6),
        30 => Ok(7),
        31 => Ok(8),
        32 => Ok(9),
        40 => Ok(10),
        41 => Ok(11),
        42 => Ok(12),
        50 => Ok(13),
        51 => Ok(14),
        52 => Ok(15),
        60 => Ok(16),
        61 => Ok(17),
        62 => Ok(18),
        _ => Err(format!(
            "H.264 level_idc 语法值 {raw} 没有对应的 StdVideoH264LevelIdc 枚举"
        )),
    }
}

/// 序列参数集的 VUI：只声明色彩空间，不声明时序与 HRD
///
/// **为什么必须有**：CSC 输出的是 BT.709 + Limited Range，而 VUI 是码流里唯一
/// 说明这件事的地方。缺了它，解码器只能按默认值猜——多数实现默认 BT.601，
/// 而 BT.601 与 BT.709 的色度系数差 10% 量级，表现为**整片偏色**。
/// NVENC 后端不受影响，是因为它的 SPS 由驱动生成、自带完整 VUI；这是两个后端
/// 在码流元数据层面唯一的差异，也是「NVENC 不花、Vulkan 花」的直接来源之一。
///
/// 时序与 HRD 一律不声明：`timing_info_present_flag = 0` 时对应字段被解码器忽略，
/// 而声明错的 `time_scale` 反而会误导播放器做帧率推算。
pub(super) fn vui() -> StdVideoH264SequenceParameterSetVui {
    // SAFETY: 纯位域 POD，全零位模式合法（所有 present_flag 为 0）
    let mut flags = unsafe { std::mem::zeroed::<StdVideoH264SpsVuiFlags>() };
    // video_signal_type_present_flag 打开后才读 video_format / video_full_range_flag
    flags.set_video_signal_type_present_flag(1);
    // color_description_present_flag 打开后才读三个色彩字段
    flags.set_color_description_present_flag(1);
    // video_full_range_flag 保持 0：CSC 输出 Limited Range（Y 16..235 / UV 16..240）

    StdVideoH264SequenceParameterSetVui {
        flags,
        // 不声明 aspect_ratio_info_present_flag，以下三个字段被忽略
        aspect_ratio_idc: 0,
        sar_width: 0,
        sar_height: 0,
        video_format: 5,
        colour_primaries: 1,
        transfer_characteristics: 1,
        matrix_coefficients: 1,
        // timing_info_present_flag = 0，以下两个字段被忽略
        num_units_in_tick: 0,
        time_scale: 0,
        // bitstream_restriction_flag = 0，以下两个字段被忽略
        max_num_reorder_frames: 0,
        max_dec_frame_buffering: 0,
        // chroma_loc_info_present_flag = 0，以下两个字段被忽略
        chroma_sample_loc_type_top_field: 0,
        chroma_sample_loc_type_bottom_field: 0,
        reserved1: 0,
        pHrdParameters: std::ptr::null(),
    }
}

/// 序列参数集
///
/// `level_idc` 是 `StdVideoH264LevelIdc` 枚举值，必须先经 [`level_to_vk`] 转换；
/// 不用 `H26xCaps::max_level_idc`：那是驱动能力上限，比内容实际需要的高。
///
/// `frame_num_bits` 是 `log2(MaxFrameNum)`，取值 4..=12。它由 GOP 长度决定：
/// 一个 GOP 内的 `frame_num` 必须不重复，否则参考链会错乱。intra-refresh 需要长 GOP，
/// 位宽必须跟着抬，不能再写死成 16 帧。
///
/// 上限 12（`MaxFrameNum = 4096`）是兼容性决定：16 位的 `frame_num` +
/// `log2_max_pic_order_cnt_lsb` 组合在浏览器硬件解码器上支持无保证，实测会花屏。
///
/// `vui` 由调用方持有：SPS 里只存它的地址，生命周期必须覆盖
/// `vkCreateVideoSessionParametersKHR`。
pub(super) fn sps(
    extent: vk::Extent2D,
    display: vk::Extent2D,
    level_idc: u32,
    frame_num_bits: u32,
    vui: &StdVideoH264SequenceParameterSetVui,
) -> StdVideoH264SequenceParameterSet {
    // 位宽上限 12：`log2_max_frame_num_minus4` 语法元素取值 0..=12 虽允许 16 位，
    // 但 16 位 `frame_num` 在解码器侧兼容性差，见 `codec::sequence::frame_num_bits`
    let bits = frame_num_bits.clamp(4, 12);
    // SAFETY: `StdVideoH264SpsFlags` 是 `#[repr(C)]` 的纯位域 POD，bindgen 不派生
    // `Default`；全零位模式合法（所有标志位为 0），随后由 setter 逐位打开需要的项
    let mut flags = unsafe { std::mem::zeroed::<StdVideoH264SpsFlags>() };
    // 逐行扫描、无场编码
    flags.set_frame_mbs_only_flag(1);
    // frame_mbs_only_flag=1 时该标志必须为 1
    flags.set_direct_8x8_inference_flag(1);
    // 裁剪窗口：编码尺寸按 16 对齐后可能大于显示尺寸（1080 → 1088）。4:2:0 逐行图像的
    // CropUnitX / CropUnitY 都是 2，所以偏移量是差值的一半；只裁右侧与底部，左上角保持原点
    let crop_x = extent.width.saturating_sub(display.width) / 2;
    let crop_y = extent.height.saturating_sub(display.height) / 2;
    if crop_x != 0 || crop_y != 0 {
        flags.set_frame_cropping_flag(1);
    }
    // VUI 必须声明：没有它解码器只能猜色彩矩阵
    flags.set_vui_parameters_present_flag(1);

    StdVideoH264SequenceParameterSet {
        flags,
        profile_idc: H264_PROFILE_HIGH,
        level_idc,
        chroma_format_idc: H264_CHROMA_420,
        seq_parameter_set_id: 0,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        // frame_num 上限 = 2^bits，覆盖一个 GOP 且不重复（bindgen 里该字段是 u8）
        log2_max_frame_num_minus4: (bits - 4) as u8,
        // POC type 0：只依赖 pic_order_cnt_lsb
        pic_order_cnt_type: 0,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        // POC LSB 与 frame_num 同宽：本实现里 poc == frame_num，同宽即不会回绕
        log2_max_pic_order_cnt_lsb_minus4: (bits - 4) as u8,
        num_ref_frames_in_pic_order_cnt_cycle: 0,
        max_num_ref_frames: MAX_NUM_REF_FRAMES,
        reserved1: 0,
        pic_width_in_mbs_minus1: extent.width / MB_SIZE - 1,
        pic_height_in_map_units_minus1: extent.height / MB_SIZE - 1,
        frame_crop_left_offset: 0,
        frame_crop_right_offset: crop_x,
        frame_crop_top_offset: 0,
        frame_crop_bottom_offset: crop_y,
        reserved2: 0,
        pOffsetForRefFrame: std::ptr::null(),
        pScalingLists: std::ptr::null(),
        // 生命周期由调用方保证（见 `sps` 的文档）
        pSequenceParameterSetVui: vui,
    }
}

/// 图像参数集
pub(super) fn pps() -> StdVideoH264PictureParameterSet {
    // SAFETY: 同 SPS 的位域结构体，全零位模式合法
    let mut flags = unsafe { std::mem::zeroed::<StdVideoH264PpsFlags>() };
    // CABAC：同等画质下码率更低，驱动侧两个方向都声明支持
    flags.set_entropy_coding_mode_flag(1);
    flags.set_deblocking_filter_control_present_flag(1);

    StdVideoH264PictureParameterSet {
        flags,
        seq_parameter_set_id: 0,
        pic_parameter_set_id: 0,
        num_ref_idx_l0_default_active_minus1: 0,
        num_ref_idx_l1_default_active_minus1: 0,
        weighted_bipred_idc: 0,
        pic_init_qp_minus26: 0,
        pic_init_qs_minus26: 0,
        chroma_qp_index_offset: 0,
        second_chroma_qp_index_offset: 0,
        pScalingLists: std::ptr::null(),
    }
}

// ---- 会话参数装配 ----

impl VideoSessionParameters {
    /// 按会话的编码几何构造 SPS/PPS 并提交给驱动
    pub(in crate::encode::hardware::vulkan) fn new_h264(
        device: &VideoDevice,
        session: &VideoSession,
        level_idc: u32,
        frame_num_bits: u32,
    ) -> Result<Self, String> {
        let extent = session.coded_extent();
        if !extent.width.is_multiple_of(MB_SIZE) || !extent.height.is_multiple_of(MB_SIZE) {
            return Err(format!(
                "编码尺寸 {}x{} 未按 {} 对齐，无法用宏块表达",
                extent.width, extent.height, MB_SIZE
            ));
        }

        let vui = vui();
        // SPS 要枚举，SDP 要语法值：转换只发生在 Vulkan std 边界
        let level_idc = level_to_vk(level_idc)?;
        let sps = sps(
            extent,
            session.display_extent(),
            level_idc,
            frame_num_bits,
            &vui,
        );
        let pps = pps();

        // 参数集以数组形式交给驱动：数量与指针成对出现
        let add_info = vk::VideoEncodeH264SessionParametersAddInfoKHR::default()
            .std_sp_ss(std::slice::from_ref(&sps))
            .std_pp_ss(std::slice::from_ref(&pps));

        // 会话参数对象本身只需要知道「最多容纳几套 SPS/PPS」
        let mut h264_params = vk::VideoEncodeH264SessionParametersCreateInfoKHR::default()
            .max_std_sps_count(1)
            .max_std_pps_count(1)
            .parameters_add_info(&add_info);

        let create_info = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(session.session())
            .push(&mut h264_params);

        // SAFETY: create_info 及其引用的 h264_params → add_info → sps/pps 链在调用期间
        // 全部存活；返回的 params 在后续失败路径上会被显式销毁
        let params = unsafe {
            device
                .video_queue_table()
                .create_video_session_parameters(&create_info, None)
        }
        .map_err(|e| format!("创建会话参数对象失败: {e:?}"))?;

        // 字节由驱动序列化：它可能已对 std 参数施加覆盖，只有这份与它写出的 slice NAL 同源
        let mut get_info = vk::VideoEncodeH264SessionParametersGetInfoKHR::default()
            .write_std_sps(true)
            .write_std_pps(true)
            .std_sps_id(sps.seq_parameter_set_id as u32)
            .std_pps_id(pps.pic_parameter_set_id as u32);
        let info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
            .video_session_parameters(params)
            .push(&mut get_info);
        // 取 feedback：驱动有没有改写我们给的 SPS/PPS。
        //
        // 这是唯一能知道「我们填的 std 参数与实现期望是否一致」的通道，传 null 就永远
        // 不知道。覆盖为真意味着码流里那份参数集不是我们写的那份——排查码流问题时
        // 必须先排除这一项，否则所有「按代码推断字段值」的结论都不成立。
        let mut h264_feedback = vk::VideoEncodeH264SessionParametersFeedbackInfoKHR::default();
        // `pFeedbackInfo` 的顶层类型固定是 `VideoEncodeSessionParametersFeedbackInfoKHR`，
        // codec 专属反馈挂在它的 pNext 链上；直接传 codec 专属结构会违反 VUID
        let mut feedback =
            vk::VideoEncodeSessionParametersFeedbackInfoKHR::default().push(&mut h264_feedback);
        let this = Self::assemble(
            device,
            params,
            &info,
            &mut feedback as *mut _ as *mut std::ffi::c_void,
        )?;
        if h264_feedback.has_std_sps_overrides != 0 || h264_feedback.has_std_pps_overrides != 0 {
            log::debug!(
                "H.264 参数集被驱动覆写: SPS={} PPS={}；码流以驱动回吐的字节为准",
                h264_feedback.has_std_sps_overrides != 0,
                h264_feedback.has_std_pps_overrides != 0,
            );
        }
        Ok(this)
    }
}

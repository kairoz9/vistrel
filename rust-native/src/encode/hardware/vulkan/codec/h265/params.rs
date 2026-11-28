//! H.265 会话级码流参数：VPS / SPS / PPS / VUI 与参数集装配
//!
//! **参数集的生命周期约束**：`pDecPicBufMgr` / `pProfileTierLevel` /
//! `pSequenceParameterSetVui` 是裸指针。[`parameter_sets`] 只存调用方给的引用，
//! 不拥有它们；调用方必须保证这三个结构体活到参数集用完为止。

use ash::vk::native::{
    StdVideoH265DecPicBufMgr, StdVideoH265PictureParameterSet, StdVideoH265PpsFlags,
    StdVideoH265ProfileTierLevel, StdVideoH265ProfileTierLevelFlags,
    StdVideoH265SequenceParameterSet, StdVideoH265SequenceParameterSetVui, StdVideoH265SpsFlags,
    StdVideoH265SpsVuiFlags, StdVideoH265VideoParameterSet, StdVideoH265VpsFlags,
};
use ash::vk::{self, TaggedStructure};

use crate::encode::hardware::vulkan::codec::VideoSessionParameters;
use crate::encode::hardware::vulkan::device::VideoDevice;
use crate::encode::hardware::vulkan::session::VideoSession;

// ---- 会话级：VPS / SPS / PPS ----

/// `STD_VIDEO_H265_CHROMA_FORMAT_IDC_420`
const CHROMA_FORMAT_420: u32 = 1;
/// Main profile（`STD_VIDEO_H265_PROFILE_IDC_MAIN`）
const PROFILE_MAIN: u32 = 1;

/// `STD_VIDEO_H265_CHROMA_QP_OFFSET_LIST_SIZE`
const CHROMA_QP_OFFSET_LIST_SIZE: usize = 6;
/// `STD_VIDEO_H265_CHROMA_QP_OFFSET_TILE_COLS_LIST_SIZE`
const TILE_COLS_LIST_SIZE: usize = 19;
/// `STD_VIDEO_H265_CHROMA_QP_OFFSET_TILE_ROWS_LIST_SIZE`
const TILE_ROWS_LIST_SIZE: usize = 21;

/// 一组参数集
///
/// 内含指向 `dpb` / `ptl` / `vui` 的裸指针，见模块文档的生命周期约束。
pub struct H265ParameterSets {
    pub vps: StdVideoH265VideoParameterSet,
    pub sps: StdVideoH265SequenceParameterSet,
    pub pps: StdVideoH265PictureParameterSet,
}

/// 解码图像缓冲区：2 张（当前帧 + 1 参考帧），无重排序
pub fn dpb() -> StdVideoH265DecPicBufMgr {
    // SAFETY: `#[repr(C)]` 纯 POD，全零位模式合法
    let mut dpb = unsafe { std::mem::zeroed::<StdVideoH265DecPicBufMgr>() };
    dpb.max_dec_pic_buffering_minus1[0] = 1;
    dpb.max_num_reorder_pics[0] = 0;
    dpb.max_latency_increase_plus1[0] = 0;
    dpb
}

/// 序列参数集 VUI：BT.709 + Limited Range
///
/// 与 CSC 输出一致；不声明时序、HRD、宽高比与默认显示窗口（对应 present flag 保持 0）。
pub fn vui() -> StdVideoH265SequenceParameterSetVui {
    // SAFETY: 纯位域 POD，全零位模式合法（所有 present_flag 为 0）
    let mut flags = unsafe { std::mem::zeroed::<StdVideoH265SpsVuiFlags>() };
    // 打开后解码器才会读 video_format / video_full_range_flag / 三个色彩字段
    flags.set_video_signal_type_present_flag(1);
    flags.set_colour_description_present_flag(1);
    // video_full_range_flag 保持 0：CSC 输出 Limited Range（Y 16..235 / UV 16..240）

    StdVideoH265SequenceParameterSetVui {
        flags,
        // aspect_ratio_info_present_flag = 0，以下三个字段被忽略
        aspect_ratio_idc: 0,
        sar_width: 0,
        sar_height: 0,
        video_format: 5,
        colour_primaries: 1,
        transfer_characteristics: 1,
        matrix_coeffs: 1,
        // chroma_loc_info_present_flag = 0，以下两个字段被忽略
        chroma_sample_loc_type_top_field: 0,
        chroma_sample_loc_type_bottom_field: 0,
        reserved1: 0,
        reserved2: 0,
        // default_display_window_flag = 0，以下四个字段被忽略
        def_disp_win_left_offset: 0,
        def_disp_win_right_offset: 0,
        def_disp_win_top_offset: 0,
        def_disp_win_bottom_offset: 0,
        // timing_info_present_flag = 0，以下三个字段被忽略
        vui_num_units_in_tick: 0,
        vui_time_scale: 0,
        vui_num_ticks_poc_diff_one_minus1: 0,
        // bitstream_restriction_flag = 0，以下字段被忽略
        min_spatial_segmentation_idc: 0,
        reserved3: 0,
        max_bytes_per_pic_denom: 0,
        max_bits_per_min_cu_denom: 0,
        log2_max_mv_length_horizontal: 0,
        log2_max_mv_length_vertical: 0,
        pHrdParameters: std::ptr::null(),
    }
}

/// 码流 `general_level_idc`（level × 30）→ `StdVideoH265LevelIdc` 枚举
///
/// std 字段是枚举（0 = 1.0 … 12 = 6.2），不是语法元素；直接填 123 这类值会被驱动
/// 当成越界枚举序列化。未知值必须报错——调用方还要拿它和驱动上限比较。
/// 可见性限定在 `encode::hardware::vulkan`：`SessionConfig::build` 的 level 门禁复用同一映射。
pub(in crate::encode::hardware::vulkan) fn level_to_vk(
    general_level_idc: u32,
) -> Result<u32, String> {
    match general_level_idc {
        30 => Ok(0),   // 1.0
        60 => Ok(1),   // 2.0
        63 => Ok(2),   // 2.1
        90 => Ok(3),   // 3.0
        93 => Ok(4),   // 3.1
        120 => Ok(5),  // 4.0
        123 => Ok(6),  // 4.1
        150 => Ok(7),  // 5.0
        153 => Ok(8),  // 5.1
        156 => Ok(9),  // 5.2
        180 => Ok(10), // 6.0
        183 => Ok(11), // 6.1
        186 => Ok(12), // 6.2
        _ => Err(format!(
            "H.265 general_level_idc {general_level_idc} 没有对应的 StdVideoH265LevelIdc 枚举"
        )),
    }
}

/// profile / tier / level
///
/// `general_level_idc` 是 level × 30 的语法值；枚举转换见 [`level_to_vk`]。
pub fn profile_tier_level(general_level_idc: u32) -> Result<StdVideoH265ProfileTierLevel, String> {
    // SAFETY: 位域 POD，全零位模式合法
    let mut flags = unsafe { std::mem::zeroed::<StdVideoH265ProfileTierLevelFlags>() };
    // 逐行扫描、仅帧编码
    flags.set_general_progressive_source_flag(1);
    flags.set_general_frame_only_constraint_flag(1);
    Ok(StdVideoH265ProfileTierLevel {
        flags,
        general_profile_idc: PROFILE_MAIN,
        general_level_idc: level_to_vk(general_level_idc)?,
    })
}

/// 构造 VPS / SPS / PPS
///
/// `frame_num_bits` 是 `log2(MaxPocLsb)`，由 GOP 长度决定（H.265 的 SPS 里不再有 frame_num，
/// 只有 POC LSB 的位宽）。上限 12 与 H.264 一致：`log2_max_pic_order_cnt_lsb_minus4`
/// 语法元素本身允许 0..=12，但 16 位 POC LSB 在解码器侧兼容性无保证。
///
/// `dpb` / `ptl` / `vui` 由调用方持有：参数集只存它们的地址，不拥有它们。
pub fn parameter_sets(
    extent: ash::vk::Extent2D,
    display: ash::vk::Extent2D,
    frame_num_bits: u32,
    dpb: &StdVideoH265DecPicBufMgr,
    ptl: &StdVideoH265ProfileTierLevel,
    vui: &StdVideoH265SequenceParameterSetVui,
) -> H265ParameterSets {
    let bits = frame_num_bits.clamp(4, 12) as u8;

    // ---- VPS ----
    // SAFETY: 同上，位域 POD 全零起步
    let mut vps_flags = unsafe { std::mem::zeroed::<StdVideoH265VpsFlags>() };
    vps_flags.set_vps_temporal_id_nesting_flag(1);
    let vps = StdVideoH265VideoParameterSet {
        flags: vps_flags,
        vps_video_parameter_set_id: 0,
        // 单时域层
        vps_max_sub_layers_minus1: 0,
        reserved1: 0,
        reserved2: 0,
        // 不给定时信息：时序由上层（RTP 时间戳）负责
        vps_num_units_in_tick: 0,
        vps_time_scale: 0,
        vps_num_ticks_poc_diff_one_minus1: 0,
        reserved3: 0,
        pDecPicBufMgr: dpb,
        pHrdParameters: std::ptr::null(),
        pProfileTierLevel: ptl,
    };

    // ---- SPS ----
    // SAFETY: 同上
    let mut sps_flags = unsafe { std::mem::zeroed::<StdVideoH265SpsFlags>() };
    sps_flags.set_sps_temporal_id_nesting_flag(1);
    // VUI 声明 BT.709 + Limited；没有它解码器只能按默认色域猜
    sps_flags.set_vui_parameters_present_flag(1);
    // 裁剪窗口：编码尺寸按 16 对齐后可能大于显示尺寸（1080 → 1088）。H.265 的 conf_win
    // 偏移以 4:2:0 的色度样本为单位（SubWidthC / SubHeightC = 2），所以是差值的一半；
    // 只裁右侧与底部，左上角保持原点
    let crop_x = extent.width.saturating_sub(display.width) / 2;
    let crop_y = extent.height.saturating_sub(display.height) / 2;
    if crop_x != 0 || crop_y != 0 {
        sps_flags.set_conformance_window_flag(1);
    }
    let sps = StdVideoH265SequenceParameterSet {
        flags: sps_flags,
        chroma_format_idc: CHROMA_FORMAT_420,
        pic_width_in_luma_samples: extent.width,
        pic_height_in_luma_samples: extent.height,
        sps_video_parameter_set_id: 0,
        sps_max_sub_layers_minus1: 0,
        sps_seq_parameter_set_id: 0,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        log2_max_pic_order_cnt_lsb_minus4: bits - 4,
        // CTB：最小 8x8（minus3 = 0）、最大 32x32（diff = 2）
        log2_min_luma_coding_block_size_minus3: 0,
        log2_diff_max_min_luma_coding_block_size: 2,
        // 变换块：最小 4x4（minus2 = 0）、最大 32x32（diff = 3）。
        // 层级必须给满 3（32 → 16 → 8 → 4）：层级 0 时 NVIDIA 的 H.265 编码器
        // 只写画面顶部的少量 CTB 行，其余解码为黑；这是驱动行为，不是码流约束。
        log2_min_luma_transform_block_size_minus2: 0,
        log2_diff_max_min_luma_transform_block_size: 3,
        max_transform_hierarchy_depth_inter: 3,
        max_transform_hierarchy_depth_intra: 3,
        // 不预设短/长期参考集：参考列表由每帧的 std 结构给出
        num_short_term_ref_pic_sets: 0,
        num_long_term_ref_pics_sps: 0,
        pcm_sample_bit_depth_luma_minus1: 0,
        pcm_sample_bit_depth_chroma_minus1: 0,
        log2_min_pcm_luma_coding_block_size_minus3: 0,
        log2_diff_max_min_pcm_luma_coding_block_size: 0,
        reserved1: 0,
        reserved2: 0,
        palette_max_size: 0,
        delta_palette_max_predictor_size: 0,
        motion_vector_resolution_control_idc: 0,
        sps_num_palette_predictor_initializers_minus1: 0,
        // 只裁右侧与底部，左上角保持原点
        conf_win_left_offset: 0,
        conf_win_right_offset: crop_x,
        conf_win_top_offset: 0,
        conf_win_bottom_offset: crop_y,
        pProfileTierLevel: ptl,
        pDecPicBufMgr: dpb,
        pScalingLists: std::ptr::null(),
        pShortTermRefPicSet: std::ptr::null(),
        pLongTermRefPicsSps: std::ptr::null(),
        pSequenceParameterSetVui: vui,
        pPredictorPaletteEntries: std::ptr::null(),
    };

    // ---- PPS ----
    // SAFETY: 同上
    let mut pps_flags = unsafe { std::mem::zeroed::<StdVideoH265PpsFlags>() };
    // 去块滤波：交给驱动默认行为，不额外覆盖
    pps_flags.set_deblocking_filter_control_present_flag(1);
    let pps = StdVideoH265PictureParameterSet {
        flags: pps_flags,
        pps_pic_parameter_set_id: 0,
        pps_seq_parameter_set_id: 0,
        sps_video_parameter_set_id: 0,
        num_extra_slice_header_bits: 0,
        num_ref_idx_l0_default_active_minus1: 0,
        num_ref_idx_l1_default_active_minus1: 0,
        init_qp_minus26: 0,
        diff_cu_qp_delta_depth: 0,
        pps_cb_qp_offset: 0,
        pps_cr_qp_offset: 0,
        pps_beta_offset_div2: 0,
        pps_tc_offset_div2: 0,
        log2_parallel_merge_level_minus2: 0,
        log2_max_transform_skip_block_size_minus2: 0,
        diff_cu_chroma_qp_offset_depth: 0,
        chroma_qp_offset_list_len_minus1: 0,
        cb_qp_offset_list: [0; CHROMA_QP_OFFSET_LIST_SIZE],
        cr_qp_offset_list: [0; CHROMA_QP_OFFSET_LIST_SIZE],
        log2_sao_offset_scale_luma: 0,
        log2_sao_offset_scale_chroma: 0,
        pps_act_y_qp_offset_plus5: 0,
        pps_act_cb_qp_offset_plus5: 0,
        pps_act_cr_qp_offset_plus3: 0,
        pps_num_palette_predictor_initializers: 0,
        luma_bit_depth_entry_minus8: 0,
        chroma_bit_depth_entry_minus8: 0,
        // 不分 tile：整帧一个 tile，避免引入 tile 相关的额外约束
        num_tile_columns_minus1: 0,
        num_tile_rows_minus1: 0,
        reserved1: 0,
        reserved2: 0,
        column_width_minus1: [0; TILE_COLS_LIST_SIZE],
        row_height_minus1: [0; TILE_ROWS_LIST_SIZE],
        reserved3: 0,
        pScalingLists: std::ptr::null(),
        pPredictorPaletteEntries: std::ptr::null(),
    };

    H265ParameterSets { vps, sps, pps }
}

// ---- 会话参数装配 ----

impl VideoSessionParameters {
    /// 建 H.265 的会话参数对象（VPS + SPS + PPS）
    ///
    /// `dpb` / `ptl` / `vui` 必须是本函数的局部变量：`parameter_sets` 只把它们的地址
    /// 存进参数集，不拥有它们。挪到返回值里就会悬垂。
    pub(in crate::encode::hardware::vulkan) fn new_h265(
        device: &VideoDevice,
        session: &VideoSession,
        level_idc: u32,
        frame_num_bits: u32,
    ) -> Result<Self, String> {
        let extent = session.coded_extent();
        let dpb = dpb();
        let ptl = profile_tier_level(level_idc)?;
        let vui = vui();
        let sets = parameter_sets(
            extent,
            session.display_extent(),
            frame_num_bits,
            &dpb,
            &ptl,
            &vui,
        );

        let add_info = vk::VideoEncodeH265SessionParametersAddInfoKHR::default()
            .std_vp_ss(std::slice::from_ref(&sets.vps))
            .std_sp_ss(std::slice::from_ref(&sets.sps))
            .std_pp_ss(std::slice::from_ref(&sets.pps));

        let mut h265_params = vk::VideoEncodeH265SessionParametersCreateInfoKHR::default()
            .max_std_vps_count(1)
            .max_std_sps_count(1)
            .max_std_pps_count(1)
            .parameters_add_info(&add_info);

        let create_info = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(session.session())
            .push(&mut h265_params);

        // SAFETY: create_info 及其引用的 h265_params → add_info → vps/sps/pps 链在调用
        // 期间全部存活；返回的 params 在后续失败路径上会被显式销毁
        let params = unsafe {
            device
                .video_queue_table()
                .create_video_session_parameters(&create_info, None)
        }
        .map_err(|e| format!("创建 H.265 会话参数对象失败: {e:?}"))?;

        // 三个参数集一次取回，规范保证顺序是 VPS → SPS → PPS
        let mut get_info = vk::VideoEncodeH265SessionParametersGetInfoKHR::default()
            .write_std_vps(true)
            .write_std_sps(true)
            .write_std_pps(true)
            .std_vps_id(sets.vps.vps_video_parameter_set_id as u32)
            .std_sps_id(sets.sps.sps_seq_parameter_set_id as u32)
            .std_pps_id(sets.pps.pps_pic_parameter_set_id as u32);
        let info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
            .video_session_parameters(params)
            .push(&mut get_info);
        // H.265 的 SPS/PPS 字段更多，驱动是否覆盖我们给的参数更值得关注——
        // 与 H.264 同一套反馈通道
        let mut h265_feedback = vk::VideoEncodeH265SessionParametersFeedbackInfoKHR::default();
        // `pFeedbackInfo` 的顶层类型固定是 `VideoEncodeSessionParametersFeedbackInfoKHR`，
        // codec 专属反馈挂在它的 pNext 链上；直接传 codec 专属结构会违反 VUID
        let mut feedback =
            vk::VideoEncodeSessionParametersFeedbackInfoKHR::default().push(&mut h265_feedback);
        let this = Self::assemble(device, params, &info, Some(&mut feedback))?;
        if h265_feedback.has_std_vps_overrides != 0
            || h265_feedback.has_std_sps_overrides != 0
            || h265_feedback.has_std_pps_overrides != 0
        {
            log::debug!(
                "H.265 参数集被驱动覆写: VPS={} SPS={} PPS={}；码流以驱动回吐的字节为准",
                h265_feedback.has_std_vps_overrides != 0,
                h265_feedback.has_std_sps_overrides != 0,
                h265_feedback.has_std_pps_overrides != 0,
            );
        }
        Ok(this)
    }
}

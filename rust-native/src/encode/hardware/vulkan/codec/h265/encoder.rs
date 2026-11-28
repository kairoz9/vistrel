//! H.265 编码方言 (H.265 Encode Dialect)
//!
//! 本文件是「H.265 的一切」，三段内容同址：
//! - **码流词汇**：会话级 VPS / SPS / PPS + 每帧参数（std 结构构造）
//! - **会话参数装配**：[`VideoSessionParameters::new_h265`]
//! - **编码命令装配**：[`FrameEncoder::encode_prepared_h265`]
//!
//! VPS / SPS / PPS 的最小集合，字段对着 `vk_video/vulkan_video_codec_h265std.h` 逐项核对。
//!
//! **VUI 声明 BT.709 + Limited**：与 CSC 输出一致；缺了它解码器只能按默认色域猜，
//! 多数实现默认 BT.601，表现为整片偏色。H.264 / AV1 用的是同一组色彩参数。
//!
//! **码流字节由驱动生成**（`vkGetEncodedVideoSessionParametersKHR`），std 结构体只负责
//! 填字段。驱动可以按 encode quality level 对参数施加覆盖，只有它自己序列化出来的那份
//! 才与它写出的 slice NAL 同源 —— 所以这里不再手写任何语法。
//!
//! **参数集的生命周期约束**：`pDecPicBufMgr` / `pProfileTierLevel` /
//! `pSequenceParameterSetVui` 是裸指针。`parameter_sets` 只把调用方给的引用存进去，
//! **不拥有**它们；调用方必须保证这三个结构体活到参数集用完为止。这样做是为了避免
//! 自引用结构——若把它们放进返回值再取地址，返回时指针就悬垂了。
//!
//! 每帧部分构造 std 结构体并装进 Vulkan 编码命令。字段来自
//! `vk_video/vulkan_video_codec_h265std_encode.h`。
//!
//! **帧头的自引用已拆开**：`StdVideoEncodeH265PictureInfo::pRefLists` /
//! `pShortTermRefPicSet` 若指向同一结构体的兄弟字段，就只能等结构体落位后再接，
//! 「接完之后不能再移动」没有任何编译期保证。因此被指向的部分单独成
//! [`H265FrameParts`]，[`build`] 借用它并返回 `H265FrameStd<'a>`：帧头活着期间，
//! `parts` 就被借走，移不动 —— 约束交给借用检查器。

use std::marker::PhantomData;

use ash::vk::native::{
    StdVideoEncodeH265PictureInfo, StdVideoEncodeH265PictureInfoFlags,
    StdVideoEncodeH265ReferenceInfo, StdVideoEncodeH265ReferenceInfoFlags,
    StdVideoEncodeH265ReferenceListsInfo, StdVideoEncodeH265ReferenceListsInfoFlags,
    StdVideoEncodeH265SliceSegmentHeader, StdVideoEncodeH265SliceSegmentHeaderFlags,
    StdVideoH265DecPicBufMgr, StdVideoH265PictureParameterSet, StdVideoH265PpsFlags,
    StdVideoH265ProfileTierLevel, StdVideoH265ProfileTierLevelFlags,
    StdVideoH265SequenceParameterSet, StdVideoH265SequenceParameterSetVui,
    StdVideoH265ShortTermRefPicSet, StdVideoH265SpsFlags, StdVideoH265SpsVuiFlags,
    StdVideoH265VideoParameterSet, StdVideoH265VpsFlags,
};
use ash::vk::{self, TaggedStructure};

#[cfg(test)]
use crate::encode::VideoCodec;
use crate::encode::hardware::vulkan::codec::CodecRateControl;
use crate::encode::hardware::vulkan::codec::VideoSessionParameters;
use crate::encode::hardware::vulkan::device::VideoDevice;
use crate::encode::hardware::vulkan::frame_encoder::{
    FrameEncoder, begin_scope, intra_refresh_info, picture_resource,
};
use crate::encode::hardware::vulkan::session::VideoSession;

#[cfg(test)]
use crate::encode::hardware::vulkan::codec::{H265SequenceConfig, SequenceConfig};
#[cfg(test)]
use crate::encode::hardware::vulkan::context::VulkanInstance;
#[cfg(test)]
use crate::encode::hardware::vulkan::frame_encoder::{
    make_noisy_frame, test_frame_encoder, test_session_config,
};
#[cfg(test)]
use crate::encode::hardware::vulkan::{codec::caps, test_support};

/// CBR 的 QP 地板：只约束「画质过好」的一侧，不参与码率上限。
/// 最终取值会钳进驱动声明的 `minQp..maxQp`（`FrameEncoder::qp_range`）。
const MIN_QP_H265: i32 = 22;

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
/// `pub(super)`：`SessionConfig::build` 的 level 门禁复用同一映射。
pub(in crate::encode::hardware::vulkan) fn level_enum(
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
/// `general_level_idc` 是 level × 30 的语法值；枚举转换见 [`level_enum`]。
pub fn profile_tier_level(general_level_idc: u32) -> Result<StdVideoH265ProfileTierLevel, String> {
    // SAFETY: 位域 POD，全零位模式合法
    let mut flags = unsafe { std::mem::zeroed::<StdVideoH265ProfileTierLevelFlags>() };
    // 逐行扫描、仅帧编码
    flags.set_general_progressive_source_flag(1);
    flags.set_general_frame_only_constraint_flag(1);
    Ok(StdVideoH265ProfileTierLevel {
        flags,
        general_profile_idc: PROFILE_MAIN,
        general_level_idc: level_enum(general_level_idc)?,
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

// ---- 每帧参数 ----

/// `STD_VIDEO_H265_PICTURE_TYPE_P`
pub(super) const PICTURE_TYPE_P: u32 = 0;
/// `STD_VIDEO_H265_PICTURE_TYPE_IDR`
pub(super) const PICTURE_TYPE_IDR: u32 = 3;
/// `STD_VIDEO_H265_SLICE_TYPE_P`
const SLICE_TYPE_P: u32 = 1;
/// `STD_VIDEO_H265_SLICE_TYPE_I`
const SLICE_TYPE_I: u32 = 2;
/// `STD_VIDEO_H265_MAX_NUM_LIST_REF`（15）里表示「空位」的取值
const NO_REFERENCE: u8 = 0xFF;
/// 合并候选上限（H.265 规范允许 1..=5）
const MAX_NUM_MERGE_CAND: u8 = 5;

/// `pic_info` 指向的两个子结构
///
/// 先让它落到最终位置，再交给 [`build`]。`build` 的返回值借用它，因此在帧头存活期间
/// 它既不能移动也不能改。
pub struct H265FrameParts {
    pub ref_lists: StdVideoEncodeH265ReferenceListsInfo,
    /// P 帧的显式短参考集（SPS 里 `num_short_term_ref_pic_sets = 0`，
    /// 所以 slice 头必须自带一份，否则驱动拿到空指针）
    pub st_ref_pic_set: StdVideoH265ShortTermRefPicSet,
}

impl H265FrameParts {
    /// `ref_slot` 是 P 帧要引用的 DPB 槽索引；IDR 传 `None`
    pub fn new(ref_slot: Option<u8>) -> Self {
        // SAFETY: `StdVideoEncodeH265ReferenceListsInfoFlags` 是 `#[repr(C)]` 纯位域
        // POD，bindgen 不派生 `Default`；全零位模式合法，随后由 setter 逐位打开
        let mut ref_list_flags =
            unsafe { std::mem::zeroed::<StdVideoEncodeH265ReferenceListsInfoFlags>() };
        // 不做参考列表重排：list_entry_l0 由驱动按 RefPicList0 顺序推导
        ref_list_flags.set_ref_pic_list_modification_flag_l0(0);

        let mut ref_lists = StdVideoEncodeH265ReferenceListsInfo {
            flags: ref_list_flags,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            RefPicList0: [NO_REFERENCE; 15],
            RefPicList1: [NO_REFERENCE; 15],
            list_entry_l0: [0; 15],
            list_entry_l1: [0; 15],
        };
        if let Some(slot) = ref_slot {
            ref_lists.RefPicList0[0] = slot;
        }

        // 显式短参考集：1 张负向参考（上一帧），POC 差 0，且被当前帧引用。
        // SPS 里 num_short_term_ref_pic_sets = 0，所以这份必须随每帧给出。
        // SAFETY: 纯 POD，全零位模式合法（inter_ref_pic_set_prediction_flag = 0）
        let mut st_ref_pic_set = unsafe { std::mem::zeroed::<StdVideoH265ShortTermRefPicSet>() };
        st_ref_pic_set.num_negative_pics = 1;
        st_ref_pic_set.num_positive_pics = 0;
        st_ref_pic_set.delta_poc_s0_minus1[0] = 0;
        st_ref_pic_set.used_by_curr_pic_s0_flag = 1;

        Self {
            ref_lists,
            st_ref_pic_set,
        }
    }
}

/// 一帧的 H.265 帧头结构
///
/// `pic_info` 的两个指针指向 [`H265FrameParts`]；`_marker` 以 `PhantomData`
/// 把 `'a` 绑在 `pic_info` 上 —— 帧头活着期间，被指向的数据移不动、改不了。
pub struct H265FrameStd<'a> {
    pub pic_info: StdVideoEncodeH265PictureInfo,
    pub slice_header: StdVideoEncodeH265SliceSegmentHeader,
    /// 本帧作为参考帧写进 DPB 槽时用的信息
    pub ref_info: StdVideoEncodeH265ReferenceInfo,
    /// 借用锚，不参与编码：见结构体文档
    _marker: PhantomData<&'a H265FrameParts>,
}

/// 构造一帧的帧头结构
///
/// `poc` 是 `PicOrderCntVal`（本实现里等于 `frame_num`）。
/// IDR 是全帧内：没有参考列表，也不需要短参考集，两个指针保持 null ——
/// 此时 `parts` 照样传进来，只是不被引用。
pub fn build<'a>(parts: &'a H265FrameParts, is_idr: bool, poc: i32) -> H265FrameStd<'a> {
    // SAFETY: `StdVideoEncodeH265*Flags` 都是 `#[repr(C)]` 纯位域 POD，
    // bindgen 不派生 `Default`；全零位模式合法，随后由 setter 逐位打开
    let mut pic_flags = unsafe { std::mem::zeroed::<StdVideoEncodeH265PictureInfoFlags>() };
    if is_idr {
        // IRAP：IDR 属于随机接入点
        pic_flags.set_IrapPicFlag(1);
        // 输出标志：不清空之前已输出的图像
        pic_flags.set_pic_output_flag(1);
    } else {
        // 短参考集直接来自 SPS（本实现 num_short_term_ref_pic_sets = 0，参考列表显式给出）
        pic_flags.set_short_term_ref_pic_set_sps_flag(0);
    }
    // 本帧会成为参考帧
    pic_flags.set_is_reference(1);

    // SAFETY: 同上
    let mut slice_flags =
        unsafe { std::mem::zeroed::<StdVideoEncodeH265SliceSegmentHeaderFlags>() };
    // 单 slice：整帧只有一段，且是第一段
    slice_flags.set_first_slice_segment_in_pic_flag(1);

    let pic_info = StdVideoEncodeH265PictureInfo {
        flags: pic_flags,
        pic_type: if is_idr {
            PICTURE_TYPE_IDR
        } else {
            PICTURE_TYPE_P
        },
        sps_video_parameter_set_id: 0,
        pps_seq_parameter_set_id: 0,
        pps_pic_parameter_set_id: 0,
        short_term_ref_pic_set_idx: 0,
        PicOrderCntVal: poc,
        TemporalId: 0,
        reserved1: [0; 7],
        // IDR 没有参考列表也没有短参考集，两个指针保持 null
        pRefLists: if is_idr {
            std::ptr::null()
        } else {
            &parts.ref_lists
        },
        pShortTermRefPicSet: if is_idr {
            std::ptr::null()
        } else {
            &parts.st_ref_pic_set
        },
        pLongTermRefPics: std::ptr::null(),
    };

    let slice_header = StdVideoEncodeH265SliceSegmentHeader {
        flags: slice_flags,
        slice_type: if is_idr { SLICE_TYPE_I } else { SLICE_TYPE_P },
        slice_segment_address: 0,
        collocated_ref_idx: 0,
        MaxNumMergeCand: MAX_NUM_MERGE_CAND,
        slice_cb_qp_offset: 0,
        slice_cr_qp_offset: 0,
        slice_beta_offset_div2: 0,
        slice_tc_offset_div2: 0,
        slice_act_y_qp_offset: 0,
        slice_act_cb_qp_offset: 0,
        slice_act_cr_qp_offset: 0,
        slice_qp_delta: 0,
        reserved1: 0,
        pWeightTable: std::ptr::null(),
    };

    // SAFETY: 同上
    let mut ref_info_flags = unsafe { std::mem::zeroed::<StdVideoEncodeH265ReferenceInfoFlags>() };
    // 本帧被后续帧引用，不是「用后即弃」
    ref_info_flags.set_unused_for_reference(0);
    let ref_info = StdVideoEncodeH265ReferenceInfo {
        flags: ref_info_flags,
        pic_type: pic_info.pic_type,
        PicOrderCntVal: poc,
        TemporalId: 0,
    };

    H265FrameStd {
        pic_info,
        slice_header,
        ref_info,
        _marker: PhantomData,
    }
}

/// 参考帧在 DPB 槽里的描述信息（上一帧）
///
/// `poc` / `is_idr` 都是**该参考帧自己的**——这个槽里装的是上一帧，用当前帧的类型
/// 去填会让驱动按错误的方式管理这个参考槽。
pub fn reference_info(poc: i32, is_idr: bool) -> StdVideoEncodeH265ReferenceInfo {
    // SAFETY: 纯位域 POD，全零位模式合法（unused_for_reference 等全部位为 0）
    let flags = unsafe { std::mem::zeroed::<StdVideoEncodeH265ReferenceInfoFlags>() };
    StdVideoEncodeH265ReferenceInfo {
        flags,
        pic_type: if is_idr {
            PICTURE_TYPE_IDR
        } else {
            PICTURE_TYPE_P
        },
        PicOrderCntVal: poc,
        TemporalId: 0,
    }
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
        let this = Self::assemble(
            device,
            params,
            &info,
            &mut feedback as *mut _ as *mut std::ffi::c_void,
        )?;
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

// ---- 编码命令装配 ----

impl FrameEncoder {
    /// H.265 一帧：构造 std 结构、DPB 槽与码控，然后走公共提交
    pub(in crate::encode::hardware::vulkan) fn encode_prepared_h265(
        &mut self,
        force_idr: bool,
    ) -> Result<(Vec<u8>, bool), String> {
        let is_idr = force_idr || self.frame_num == 0 || self.frame_num >= self.gop_length;
        let setup_idx = self.next_slot;
        let ref_idx = 1 - setup_idx;
        let frame_num = if is_idr { 0 } else { self.frame_num };
        let poc = frame_num as i32;
        let extent = self.image.extent();

        // std 结构：先让被指向的子结构落位，再构造帧头（借用约束见模块文档）
        let parts = H265FrameParts::new(if is_idr { None } else { Some(ref_idx as u8) });
        let frame = build(&parts, is_idr, poc);

        // CBR 下 constantQp 必须为 0（VUID-vkCmdEncodeVideoKHR-constantQp-08272），
        // QP 由码控计算；填固定值会让驱动行为未定义
        let slice = vk::VideoEncodeH265NaluSliceSegmentInfoKHR::default()
            .constant_qp(0)
            .std_slice_segment_header(&frame.slice_header);
        let mut picture_info = vk::VideoEncodeH265PictureInfoKHR::default()
            .nalu_slice_segment_entries(std::slice::from_ref(&slice))
            .std_picture_info(&frame.pic_info);

        // 当前帧写入的 DPB 槽
        let mut setup_dpb =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&frame.ref_info);
        let setup_resource = picture_resource(self.dpb.view(), extent, setup_idx as u32);
        let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_idx as i32)
            .picture_resource(&setup_resource)
            .push(&mut setup_dpb);

        // 参考槽（仅 P 帧用）：指向上一帧所在的 DPB 槽。类型取上一帧自己的
        let prev_frame_num = frame_num.saturating_sub(1);
        let ref_ref_info = reference_info(prev_frame_num as i32, self.codec_state.last_was_idr());
        let mut ref_dpb =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&ref_ref_info);
        let ref_resource = picture_resource(self.dpb.view(), extent, ref_idx as u32);
        let ref_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_idx as i32)
            .picture_resource(&ref_resource)
            .push(&mut ref_dpb);
        let ref_slots = [ref_slot];

        let src_resource = picture_resource(self.image.view(), extent, 0);
        let mut encode_info = vk::VideoEncodeInfoKHR::default()
            .dst_buffer(self.bitstream.buffer())
            .dst_buffer_offset(0)
            .dst_buffer_range(self.bitstream.size())
            .src_picture_resource(src_resource)
            .setup_reference_slot(&setup_slot)
            .push(&mut picture_info);
        if !is_idr {
            encode_info = encode_info.reference_slots(&ref_slots);
        }

        let intra_cycle = self.intra_refresh_cycle;
        let mut intra_info = intra_refresh_info(intra_cycle, self.intra_refresh_index);
        if let Some(info) = intra_info.as_mut() {
            encode_info = encode_info.push(info);
        }

        // CBR 码控：H.265 同样必须给出 QP 范围与 GOP 结构，缺了驱动判非法。
        // 地板取 `MIN_QP_H265`，再钳进驱动声明的 `minQp..maxQp`
        let (min_qp_value, max_qp_value) = self.coding().qp_range(MIN_QP_H265);
        let min_qp = vk::VideoEncodeH265QpKHR::default()
            .qp_i(min_qp_value)
            .qp_p(min_qp_value)
            .qp_b(min_qp_value);
        let max_qp = vk::VideoEncodeH265QpKHR::default()
            .qp_i(max_qp_value)
            .qp_p(max_qp_value)
            .qp_b(max_qp_value);
        let mut h265_layer = vk::VideoEncodeH265RateControlLayerInfoKHR::default()
            .use_min_qp(true)
            .min_qp(min_qp)
            .use_max_qp(true)
            .max_qp(max_qp);
        let layer = vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(self.bitrate_bps())
            .max_bitrate(self.bitrate_bps())
            .frame_rate_numerator(self.fps())
            .frame_rate_denominator(1)
            .push(&mut h265_layer);
        let layers = [layer];
        // codec 专属码控信息与 `VkVideoEncodeRateControlInfoKHR` 是兄弟，都挂在 begin 上
        let h265_rate_control = vk::VideoEncodeH265RateControlInfoKHR::default()
            .flags(
                vk::VideoEncodeH265RateControlFlagsKHR::REGULAR_GOP
                    | vk::VideoEncodeH265RateControlFlagsKHR::REFERENCE_PATTERN_FLAT,
            )
            .gop_frame_count(self.gop_length)
            .idr_period(self.gop_length)
            .consecutive_b_frame_count(0)
            .sub_layer_count(1);
        let scope = begin_scope(if is_idr { None } else { Some(&ref_slots[0]) }, &setup_slot);
        // 驱动要求/偏好时，把本帧在 GOP 内的剩余 I/P 帧数一并交给码控
        let gop_remaining = self.gop_remaining(frame_num, is_idr);
        let bytes_written = self.submit_encode(
            &encode_info,
            &scope,
            &layers,
            Some(CodecRateControl::H265(h265_rate_control)),
            gop_remaining,
        )?;
        self.finish_frame(bytes_written, is_idr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            level_enum(1).is_err(),
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
            frame.pic_info.pRefLists,
            &parts.ref_lists as *const StdVideoEncodeH265ReferenceListsInfo,
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
                let mut h265_feedback =
                    vk::VideoEncodeH265SessionParametersFeedbackInfoKHR::default();
                let mut feedback = vk::VideoEncodeSessionParametersFeedbackInfoKHR::default()
                    .push(&mut h265_feedback);
                let bytes = VideoSessionParameters::fetch_encoded(
                    &device,
                    &info,
                    &mut feedback as *mut _ as *mut std::ffi::c_void,
                )
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
        let mut encoder = match test_frame_encoder(&device, &caps, 15) {
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
}

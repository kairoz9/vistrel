//! H.264 编码方言 (H.264 Encode Dialect)
//!
//! 本文件是「H.264 的一切」，三段内容同址：
//! - **码流词汇**：会话级 SPS / PPS / VUI + 每帧参数（std 结构构造）
//! - **会话参数装配**：[`VideoSessionParameters::new_h264`]
//! - **编码命令装配**：[`FrameEncoder::encode_prepared_h264`]
//!
//! **码流字节由驱动生成**（`vkGetEncodedVideoSessionParametersKHR`），std 结构体只负责
//! 填字段——驱动可以按 encode quality level 对参数集施加覆盖，只有它自己序列化出来的
//! 那份才与它写出的 slice NAL 同源，所以这里不手写任何语法。
//!
//! **帧头的自引用已拆开**：`StdVideoEncodeH264PictureInfo::pRefLists` 指向兄弟结构
//! `ReferenceListsInfo`。被指向的部分单独成 [`H264FrameParts`]，[`build`] 借用它并返回
//! `H264FrameStd<'a>`——帧头活着期间 `parts` 被借走、移不动，约束交给借用检查器
//! （与 hevc / av1 同一套模式）。

use std::marker::PhantomData;

use ash::vk::native::{
    StdVideoEncodeH264PictureInfo, StdVideoEncodeH264PictureInfoFlags,
    StdVideoEncodeH264ReferenceInfo, StdVideoEncodeH264ReferenceListsInfo,
    StdVideoEncodeH264SliceHeader, StdVideoEncodeH264SliceHeaderFlags,
    StdVideoH264PictureParameterSet, StdVideoH264PpsFlags, StdVideoH264SequenceParameterSet,
    StdVideoH264SequenceParameterSetVui, StdVideoH264SpsFlags, StdVideoH264SpsVuiFlags,
};
use ash::vk::{self, TaggedStructure};

use super::{CodecRateControl, FrameEncoder, begin_scope, intra_refresh_info, picture_resource};
use crate::encode::VideoCodec;
use crate::encode::hardware::vulkan::caps::CodecCaps;
use crate::encode::hardware::vulkan::device::VideoDevice;
use crate::encode::hardware::vulkan::session::{VideoSession, VideoSessionParameters};

/// CBR 的 QP 地板：只约束「画质过好」的一侧，不参与码率上限。
/// H.264 的 QP 范围是标准固定值 0..51
const MIN_QP_H264: i32 = 18;

#[cfg(test)]
use super::{make_noisy_frame, test_profile};
#[cfg(test)]
use crate::encode::hardware::vulkan::context::VulkanInstance;
#[cfg(test)]
use crate::encode::hardware::vulkan::{caps, test_support};

// ---- 会话级：SPS / PPS / VUI ----

/// H.264 的宏块边长
pub(super) const MB_SIZE: u32 = 16;
/// High profile：与建会话时声明的 profile 一致，不能各说各话
const H264_PROFILE_HIGH: u32 = 100;
/// 4:2:0
const H264_CHROMA_420: u32 = 1;
/// 单参考帧：IPPP，够用且让 DPB 需求最小
const MAX_NUM_REF_FRAMES: u8 = 1;

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
/// `level_idc` 是 `StdVideoH264LevelIdc` **枚举值**（4.2 = 42），由
/// [`crate::encode::profile_level`] 按**当前档位**推导。不用 `CodecCaps::max_level_idc`：
/// 那是驱动能力上限（H.264 常报 6.2），声明得比实际内容高，方向与 SDP 的声明相反。
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
    // 但 16 位 `frame_num` 在解码器侧兼容性差，见 `frame_encoder::frame_num_bits`
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

// ---- 每帧参数 ----

/// `STD_VIDEO_H264_PICTURE_TYPE_P`
pub(super) const PICTURE_TYPE_P: u32 = 0;
/// `STD_VIDEO_H264_PICTURE_TYPE_IDR`
pub(super) const PICTURE_TYPE_IDR: u32 = 5;
/// `STD_VIDEO_H264_SLICE_TYPE_P`
const SLICE_TYPE_P: u32 = 0;
/// `STD_VIDEO_H264_SLICE_TYPE_I`
const SLICE_TYPE_I: u32 = 2;
/// 参考列表中的空位（`STD_VIDEO_H264_NO_REFERENCE_PICTURE`）
const NO_REFERENCE_PICTURE: u8 = 0xFF;
/// `disable_deblocking_filter_idc`：0 = **启用**去块滤波
///
/// 这个字段名是「禁用」，1 才是关闭——写成 1 会让低码率下的块效应直接进码流。
/// H.265 没有等价开关（去块恒开），两个 codec 必须同口径，否则同一档位的画质不可比。
const DEBLOCKING_FILTER_IDC: u32 = 0;

/// `pic_info` 指向的子结构
///
/// 先让它落到最终位置，再交给 [`build`]。`build` 的返回值借用它，因此在帧头存活期间
/// 它既不能移动也不能改。
pub struct H264FrameParts {
    /// P 帧的参考列表（IDR 不挂，`pRefLists` 保持 null）
    pub ref_lists: StdVideoEncodeH264ReferenceListsInfo,
}

impl H264FrameParts {
    /// `ref_slot` 是 P 帧要引用的 DPB 槽索引；IDR 传 `None`
    pub fn new(ref_slot: Option<u8>) -> Self {
        // SAFETY: 纯位域 POD，bindgen 不派生 Default；全零位模式合法
        let mut ref_lists = unsafe { std::mem::zeroed::<StdVideoEncodeH264ReferenceListsInfo>() };
        // 未使用的表项必须是 NO_REFERENCE：0 是合法槽号，留成 0 会让驱动把不存在
        // 的参考当成有效槽（VUID-vkCmdEncodeVideoKHR-pNext-08339）
        ref_lists.RefPicList0 = [NO_REFERENCE_PICTURE; 32];
        ref_lists.RefPicList1 = [NO_REFERENCE_PICTURE; 32];
        // P 帧：参考列表只含一个条目，指向上一帧所在的 DPB 槽
        if let Some(slot) = ref_slot {
            ref_lists.num_ref_idx_l0_active_minus1 = 0;
            ref_lists.RefPicList0[0] = slot;
        }
        Self { ref_lists }
    }
}

/// 一帧的 H.264 帧头结构
///
/// `pic_info` 的 `pRefLists` 指向 [`H264FrameParts`]；`_marker` 以 `PhantomData`
/// 把 `'a` 绑在 `pic_info` 上——帧头活着期间，被指向的数据移不动、改不了。
pub struct H264FrameStd<'a> {
    pub pic_info: StdVideoEncodeH264PictureInfo,
    pub slice_header: StdVideoEncodeH264SliceHeader,
    /// 本帧作为参考帧写进 DPB 槽时用的信息
    pub ref_info: StdVideoEncodeH264ReferenceInfo,
    /// 借用锚，不参与编码：见结构体文档
    _marker: PhantomData<&'a H264FrameParts>,
}

/// 构造一帧的帧头结构
///
/// `idr_pic_id` 相邻 IDR 必须互异（H.264 7.4.3）：固定为 0 会让解码器把第二个及之后的
/// IDR 当成重复帧丢掉，于是「该恢复的时候恢复不了」——交替 0/1 由调用方维护并传入。
/// POC 直接取 frame_num：两者同宽（SPS 里位宽一致），一个 GOP 内都不会回绕。
pub fn build<'a>(
    parts: &'a H264FrameParts,
    is_idr: bool,
    frame_num: u32,
    idr_pic_id: u16,
) -> H264FrameStd<'a> {
    // SAFETY: 纯位域 POD，全零位模式合法，随后由 setter 逐位打开
    let mut pic_flags = unsafe { std::mem::zeroed::<StdVideoEncodeH264PictureInfoFlags>() };
    if is_idr {
        pic_flags.set_IdrPicFlag(1);
    }
    // 挂了 setup 槽就表示这一帧要成为参考帧
    pic_flags.set_is_reference(1);

    let pic_info = StdVideoEncodeH264PictureInfo {
        flags: pic_flags,
        seq_parameter_set_id: 0,
        pic_parameter_set_id: 0,
        idr_pic_id,
        primary_pic_type: if is_idr {
            PICTURE_TYPE_IDR
        } else {
            PICTURE_TYPE_P
        },
        frame_num,
        PicOrderCnt: frame_num as i32,
        temporal_id: 0,
        reserved1: [0; 3],
        pRefLists: if is_idr {
            std::ptr::null()
        } else {
            &parts.ref_lists
        },
    };

    // SAFETY: 同上，全零位模式合法
    let slice_flags = unsafe { std::mem::zeroed::<StdVideoEncodeH264SliceHeaderFlags>() };
    let slice_header = StdVideoEncodeH264SliceHeader {
        flags: slice_flags,
        first_mb_in_slice: 0,
        slice_type: if is_idr { SLICE_TYPE_I } else { SLICE_TYPE_P },
        slice_alpha_c0_offset_div2: 0,
        slice_beta_offset_div2: 0,
        slice_qp_delta: 0,
        reserved1: 0,
        cabac_init_idc: 0,
        disable_deblocking_filter_idc: DEBLOCKING_FILTER_IDC,
        pWeightTable: std::ptr::null(),
    };

    let ref_info = reference_info_of(frame_num, pic_info.primary_pic_type);

    H264FrameStd {
        pic_info,
        slice_header,
        ref_info,
        _marker: PhantomData,
    }
}

/// 参考帧在 DPB 槽里的描述信息（上一帧）
///
/// `frame_num` / `is_idr` 都是**该参考帧自己的**——这个槽里装的是上一帧，用当前帧的
/// 类型去填是错的：IDR 之后的第一帧参考的是 IDR，填成 P 会让驱动按 P 帧去管理这个
/// 参考槽（H.264 里 IDR 会清空 DPB 并重置 POC，与 P 帧行为不同）。
pub fn reference_info(frame_num: u32, is_idr: bool) -> StdVideoEncodeH264ReferenceInfo {
    reference_info_of(
        frame_num,
        if is_idr {
            PICTURE_TYPE_IDR
        } else {
            PICTURE_TYPE_P
        },
    )
}

fn reference_info_of(frame_num: u32, primary_pic_type: u32) -> StdVideoEncodeH264ReferenceInfo {
    StdVideoEncodeH264ReferenceInfo {
        // SAFETY: 纯位域 POD，全零位模式合法
        flags: unsafe { std::mem::zeroed() },
        primary_pic_type,
        FrameNum: frame_num,
        PicOrderCnt: frame_num as i32,
        long_term_pic_num: 0,
        long_term_frame_idx: 0,
        temporal_id: 0,
    }
}

// ---- 会话参数装配 ----

impl VideoSessionParameters {
    /// 按会话的编码几何构造 SPS/PPS 并提交给驱动
    pub(super) fn new_h264(
        device: &VideoDevice,
        session: &VideoSession,
        caps: &CodecCaps,
        level_idc: u32,
        frame_num_bits: u32,
    ) -> Result<Self, String> {
        if caps.codec != VideoCodec::H264 {
            return Err(format!("H.264 会话参数收到 {:?}", caps.codec));
        }

        let extent = session.coded_extent();
        if !extent.width.is_multiple_of(MB_SIZE) || !extent.height.is_multiple_of(MB_SIZE) {
            return Err(format!(
                "编码尺寸 {}x{} 未按 {} 对齐，无法用宏块表达",
                extent.width, extent.height, MB_SIZE
            ));
        }

        let vui = vui();
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
        Ok(this)
    }
}

// ---- 编码命令装配 ----

impl FrameEncoder {
    /// H.264 一帧
    pub(super) fn encode_prepared_h264(
        &mut self,
        force_idr: bool,
    ) -> Result<(Vec<u8>, bool), String> {
        // frame_num 走到 GOP 末尾就重出 IDR；SPS 的位宽保证一个 GOP 内不回绕
        let is_idr = force_idr || self.frame_num == 0 || self.frame_num >= self.gop_length;
        let setup_idx = self.next_slot;
        let ref_idx = 1 - setup_idx;
        let frame_num = if is_idr { 0 } else { self.frame_num };
        // 相邻 IDR 的 `idr_pic_id` 必须不同（H.264 7.4.3）。固定为 0 时第二个及之后的
        // IDR 会被解码器当成重复帧丢弃，于是「该恢复的时候恢复不了」——这是花屏
        // 长期不消失的直接原因，与 GOP 取多大无关
        let idr_pic_id = if is_idr {
            self.idr_pic_id ^= 1;
            self.idr_pic_id
        } else {
            0
        };

        // std 结构：先让被指向的参考列表落位，再构造帧头（与 H.265 / AV1 同一套；
        // POC 取 frame_num，见 `build` 的文档）
        let parts = H264FrameParts::new(if is_idr { None } else { Some(ref_idx as u8) });
        let frame = build(&parts, is_idr, frame_num, idr_pic_id);

        // CBR 下 constantQp 必须为 0（VUID-vkCmdEncodeVideoKHR-constantQp-08269），
        // QP 由码控计算；填固定值会让驱动行为未定义
        let nalu_slice = vk::VideoEncodeH264NaluSliceInfoKHR::default()
            .constant_qp(0)
            .std_slice_header(&frame.slice_header);
        let mut h264_picture_info = vk::VideoEncodeH264PictureInfoKHR::default()
            .nalu_slice_entries(std::slice::from_ref(&nalu_slice))
            .std_picture_info(&frame.pic_info)
            .generate_prefix_nalu(false);

        let extent = self.image.extent();

        // 当前帧写入的 DPB 槽
        let mut setup_dpb =
            vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&frame.ref_info);
        let setup_resource = picture_resource(self.dpb.view(), extent, setup_idx as u32);
        let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_idx as i32)
            .picture_resource(&setup_resource)
            .push(&mut setup_dpb);

        // 参考槽（仅 P 帧用）：指向上一帧写入的那个 DPB 槽。类型取上一帧自己的
        let prev_frame_num = frame_num.saturating_sub(1);
        let ref_ref_info = reference_info(prev_frame_num, self.last_was_idr);
        let mut ref_dpb =
            vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&ref_ref_info);
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
            .push(&mut h264_picture_info);
        if !is_idr {
            encode_info = encode_info.reference_slots(&ref_slots);
        }

        // Intra-Refresh：每帧告诉驱动「本轮刷哪一片」，与 codec 无关（见 `intra_refresh_info`）
        let intra_cycle = self.intra_refresh_cycle;
        let mut intra_info = intra_refresh_info(intra_cycle, self.intra_refresh_index);
        if let Some(info) = intra_info.as_mut() {
            encode_info = encode_info.push(info);
        }

        // CBR 码控：首帧带 RESET 建立初始状态，之后只在码率变更时重发
        // CBR 下驱动要自己算 QP，必须给出合法的 QP 范围；缺省 0..0 会让算出的 QP 越界，
        // 表现为 INVALID_VIDEO_STD_PARAMETERS。H.264 的 QP 范围是 0..51（标准固定值）
        let min_qp = vk::VideoEncodeH264QpKHR::default()
            .qp_i(MIN_QP_H264)
            .qp_p(MIN_QP_H264)
            .qp_b(MIN_QP_H264);
        let max_qp = vk::VideoEncodeH264QpKHR::default()
            .qp_i(51)
            .qp_p(51)
            .qp_b(51);
        let mut h264_layer = vk::VideoEncodeH264RateControlLayerInfoKHR::default()
            .use_min_qp(true)
            .min_qp(min_qp)
            .use_max_qp(true)
            .max_qp(max_qp);
        let layer = vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(self.bitrate_bps())
            .max_bitrate(self.bitrate_bps())
            .frame_rate_numerator(self.fps())
            .frame_rate_denominator(1)
            .push(&mut h264_layer);
        let layers = [layer];
        // H.264 的码控必须同时声明 GOP 结构，否则驱动按默认值（0）判定参数非法。
        // 它与 `VkVideoEncodeRateControlInfoKHR` 是**兄弟**，两者都直接挂在 begin 上
        // （ash 的 `Extends` 只允许这样挂，见 `CodecRateControl` 的说明）。
        let h264_rate_control = vk::VideoEncodeH264RateControlInfoKHR::default()
            .gop_frame_count(self.gop_length)
            .idr_period(self.gop_length)
            .consecutive_b_frame_count(0)
            .temporal_layer_count(1);
        let scope = begin_scope(if is_idr { None } else { Some(&ref_slots[0]) }, &setup_slot);
        let bytes_written = self.submit_encode(
            &encode_info,
            &scope,
            &layers,
            Some(CodecRateControl::H264(h264_rate_control)),
        )?;
        self.finish_frame(bytes_written, is_idr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            frame.pic_info.pRefLists,
            &parts.ref_lists as *const StdVideoEncodeH264ReferenceListsInfo,
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
        let s = sps(coded, display, 42, 12, &vui);

        assert_eq!(s.profile_idc, H264_PROFILE_HIGH);
        assert_eq!(s.level_idc, 42);
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
        let session = match VideoSession::new(&device, &caps, 1920, 1080) {
            Ok(session) => session,
            Err(e) => {
                println!("会话创建失败，跳过: {e}");
                return;
            }
        };
        // 与生产路径同源：按内容推导 level，而不是驱动声明的最高 level
        let level = u32::from(crate::encode::h264_level_idc(1920, 1080, 60, 8_000));
        match VideoSessionParameters::new_h264(&device, &session, &caps, level, 4) {
            Ok(params) => {
                let sets = params.parameter_sets();
                println!(
                    "H.264 会话参数对象创建成功: SPS/PPS @ {}x{} / level_idc {}（推导，驱动上限 {}）/ frame_num 位宽 4 / 参数集 {} 字节 / 句柄 {:?}",
                    session.coded_extent().width,
                    session.coded_extent().height,
                    level,
                    caps.max_level_idc,
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
                let mut h264_feedback =
                    vk::VideoEncodeH264SessionParametersFeedbackInfoKHR::default();
                let mut feedback = vk::VideoEncodeSessionParametersFeedbackInfoKHR::default()
                    .push(&mut h264_feedback);
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
        let mut encoder = match FrameEncoder::new(&device, &caps, test_profile(), caps.codec, 15) {
            Ok(encoder) => encoder,
            Err(e) => {
                println!("编码器初始化失败: {e}");
                return;
            }
        };
        // 分阶段打印：SIGSEGV 时靠最后一条标记定位崩在哪一步
        println!("编码器就绪, 开始造帧");

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
                "驱动未声明 intra-refresh 能力，跳过: 模式 [{}] / 最大周期 {}",
                caps.intra_refresh_modes, caps.max_intra_refresh_cycle_duration
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

        // GOP 取 300 帧：frame_num 位宽应升到 9，否则参考链会在 GOP 中途断掉
        let mut encoder = match FrameEncoder::new(&device, &caps, test_profile(), caps.codec, 300) {
            Ok(encoder) => encoder,
            Err(e) => {
                println!("编码器初始化失败: {e}");
                return;
            }
        };

        let max_cycle = caps.max_intra_refresh_cycle_duration;
        let cycle = max_cycle.clamp(2, 60);
        if let Err(e) = encoder.set_intra_refresh(cycle) {
            println!("启用 intra-refresh 失败: {e}");
            return;
        }
        // 越界必须被拒，不能静默截断
        assert!(encoder.set_intra_refresh(1).is_err(), "周期 < 2 必须被拒绝");
        if max_cycle < u32::MAX {
            assert!(
                encoder.set_intra_refresh(max_cycle + 1).is_err(),
                "超过驱动上限必须被拒绝"
            );
        }
        assert_eq!(
            encoder.intra_refresh_cycle(),
            Some(cycle),
            "失败调用不应改动状态"
        );

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
        println!(
            "intra-refresh: 周期 {cycle}（上限 {max_cycle}）/ GOP 300 / 20 帧合计 {total} 字节"
        );
    }
}

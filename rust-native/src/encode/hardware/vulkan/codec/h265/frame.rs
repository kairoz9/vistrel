//! H.265 每帧参数与编码命令装配
//!
//! **帧头的自引用已拆开**：`StdVideoEncodeH265PictureInfo` 的指针字段指向
//! [`H265FrameParts`]；[`build`] 借用 `parts` 并返回 [`H265FrameStd<'a>`]，帧头存活期间
//! `parts` 移不动、改不了，约束交给借用检查器。

use std::marker::PhantomData;

use ash::vk::native::{
    StdVideoEncodeH265PictureInfo, StdVideoEncodeH265PictureInfoFlags,
    StdVideoEncodeH265ReferenceInfo, StdVideoEncodeH265ReferenceInfoFlags,
    StdVideoEncodeH265ReferenceListsInfo, StdVideoEncodeH265ReferenceListsInfoFlags,
    StdVideoEncodeH265SliceSegmentHeader, StdVideoEncodeH265SliceSegmentHeaderFlags,
    StdVideoH265ShortTermRefPicSet,
};
use ash::vk::{self, TaggedStructure};

use crate::encode::hardware::vulkan::codec::CodecRateControl;
use crate::encode::hardware::vulkan::encoder::{
    VulkanEncoder, begin_scope, intra_refresh_info, picture_resource,
};

/// CBR 的 QP 地板：只约束「画质过好」的一侧，不参与码率上限。
/// 最终取值会钳进驱动声明的 `minQp..maxQp`（`VulkanEncoder::qp_range`）。
const MIN_QP_H265: i32 = 22;

// ---- 每帧参数 ----

/// `STD_VIDEO_H265_PICTURE_TYPE_P`
pub(super) const PICTURE_TYPE_P: u32 = 0;
/// `STD_VIDEO_H265_PICTURE_TYPE_IDR`
pub(super) const PICTURE_TYPE_IDR: u32 = 3;
/// `STD_VIDEO_H265_SLICE_TYPE_P`
pub(super) const SLICE_TYPE_P: u32 = 1;
/// `STD_VIDEO_H265_SLICE_TYPE_I`
pub(super) const SLICE_TYPE_I: u32 = 2;
/// `STD_VIDEO_H265_MAX_NUM_LIST_REF`（15）里表示「空位」的取值
pub(super) const NO_REFERENCE: u8 = 0xFF;
/// 合并候选上限（H.265 规范允许 1..=5）
pub(super) const MAX_NUM_MERGE_CAND: u8 = 5;

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

// ---- 编码命令装配 ----

impl VulkanEncoder {
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

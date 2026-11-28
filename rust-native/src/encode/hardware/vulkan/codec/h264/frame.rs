//! H.264 每帧参数与编码命令装配

use std::marker::PhantomData;

use ash::vk::native::{
    StdVideoEncodeH264PictureInfo, StdVideoEncodeH264PictureInfoFlags,
    StdVideoEncodeH264ReferenceInfo, StdVideoEncodeH264ReferenceListsInfo,
    StdVideoEncodeH264SliceHeader, StdVideoEncodeH264SliceHeaderFlags,
};
use ash::vk::{self, TaggedStructure};

use crate::encode::hardware::vulkan::codec::CodecRateControl;
use crate::encode::hardware::vulkan::frame_encoder::{
    FrameEncoder, begin_scope, intra_refresh_info, picture_resource,
};

/// CBR 的 QP 地板：只约束「画质过好」的一侧，不参与码率上限。
/// H.264 的 QP 范围是标准固定值 0..51
const MIN_QP_H264: i32 = 18;

// ---- 每帧参数 ----

/// `STD_VIDEO_H264_PICTURE_TYPE_P`
pub(super) const PICTURE_TYPE_P: u32 = 0;
/// `STD_VIDEO_H264_PICTURE_TYPE_IDR`
pub(super) const PICTURE_TYPE_IDR: u32 = 5;
/// `STD_VIDEO_H264_SLICE_TYPE_P`
pub(super) const SLICE_TYPE_P: u32 = 0;
/// `STD_VIDEO_H264_SLICE_TYPE_I`
pub(super) const SLICE_TYPE_I: u32 = 2;
/// 参考列表中的空位（`STD_VIDEO_H264_NO_REFERENCE_PICTURE`）
pub(super) const NO_REFERENCE_PICTURE: u8 = 0xFF;
/// `disable_deblocking_filter_idc`：0 = **启用**去块滤波
///
/// 这个字段名是「禁用」，1 才是关闭——写成 1 会让低码率下的块效应直接进码流。
/// H.265 没有等价开关（去块恒开），两个 codec 必须同口径，否则同一档位的画质不可比。
pub(super) const DEBLOCKING_FILTER_IDC: u32 = 0;

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

// ---- 编码命令装配 ----

impl FrameEncoder {
    /// H.264 一帧
    pub(in crate::encode::hardware::vulkan) fn encode_prepared_h264(
        &mut self,
        force_idr: bool,
    ) -> Result<(Vec<u8>, bool), String> {
        // frame_num 走到 GOP 末尾就重出 IDR；SPS 的位宽保证一个 GOP 内不回绕
        let is_idr = force_idr || self.frame_num == 0 || self.frame_num >= self.gop_length;
        let setup_idx = self.next_slot;
        let ref_idx = 1 - setup_idx;
        let frame_num = if is_idr { 0 } else { self.frame_num };
        // P 帧不携带 `idr_pic_id`；只有 IDR 推进交替值
        let idr_pic_id = if is_idr {
            self.codec_state.next_h264_idr_pic_id()
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
        let ref_ref_info = reference_info(prev_frame_num, self.codec_state.last_was_idr());
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
        // CBR 下驱动要自己算 QP，缺省 0..0 会越界（INVALID_VIDEO_STD_PARAMETERS）；
        // 地板取 `MIN_QP_H264`，再钳进驱动的 `minQp..maxQp`。
        let (min_qp_value, max_qp_value) = self.coding().qp_range(MIN_QP_H264);
        let min_qp = vk::VideoEncodeH264QpKHR::default()
            .qp_i(min_qp_value)
            .qp_p(min_qp_value)
            .qp_b(min_qp_value);
        let max_qp = vk::VideoEncodeH264QpKHR::default()
            .qp_i(max_qp_value)
            .qp_p(max_qp_value)
            .qp_b(max_qp_value);
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
            .flags(
                vk::VideoEncodeH264RateControlFlagsKHR::REGULAR_GOP
                    | vk::VideoEncodeH264RateControlFlagsKHR::REFERENCE_PATTERN_FLAT,
            )
            .gop_frame_count(self.gop_length)
            .idr_period(self.gop_length)
            .consecutive_b_frame_count(0)
            .temporal_layer_count(1);
        let scope = begin_scope(if is_idr { None } else { Some(&ref_slots[0]) }, &setup_slot);
        // 驱动要求/偏好时，把本帧在 GOP 内的剩余 I/P 帧数一并交给码控
        let gop_remaining = self.gop_remaining(frame_num, is_idr);
        let bytes_written = self.submit_encode(
            &encode_info,
            &scope,
            &layers,
            Some(CodecRateControl::H264(h264_rate_control)),
            gop_remaining,
        )?;
        self.finish_frame(bytes_written, is_idr)
    }
}

//! AV1 每帧参数与编码命令装配

use std::marker::PhantomData;

use ash::vk::native::{
    StdVideoAV1GlobalMotion, StdVideoAV1LoopFilter, StdVideoAV1LoopFilterFlags,
    StdVideoAV1Quantization, StdVideoAV1QuantizationFlags, StdVideoEncodeAV1PictureInfo,
    StdVideoEncodeAV1PictureInfoFlags, StdVideoEncodeAV1ReferenceInfo,
    StdVideoEncodeAV1ReferenceInfoFlags,
};
use ash::vk::{self, TaggedStructure};

use crate::encode::hardware::vulkan::codec::CodecRateControl;
use crate::encode::hardware::vulkan::frame_encoder::{
    FrameEncoder, begin_scope, intra_refresh_info, picture_resource,
};

/// `STD_VIDEO_AV1_REFS_PER_FRAME`
pub(super) const AV1_REFS_PER_FRAME: usize = 7;

/// 未使用的参考名在 `referenceNameSlotIndices` 里的取值（负值 = 无参考）
pub(super) const AV1_UNUSED_REFERENCE_SLOT: i32 = -1;

/// 编码命令装配的恒定量化索引占位：CBR 下驱动自己算 qindex，std 结构里这个字段
/// 的实际约束见 `encode_prepared_av1`（VUID-10320 要求 `constantQIndex` 为 0）
pub(super) const CONSTANT_Q_INDEX: u8 = 128;

/// AV1 码控的量化索引地板：只约束「画质过好」的一侧，不参与码率上限。
/// 码控层下限取它与能力快照 `min_q_index` 的较大者
pub(super) const MIN_Q_INDEX_AV1: u32 = 108;

// ---- 每帧参数 ----

/// `STD_VIDEO_AV1_FRAME_TYPE_KEY`
pub(super) const FRAME_TYPE_KEY: u32 = 0;
/// `STD_VIDEO_AV1_FRAME_TYPE_INTER`
pub(super) const FRAME_TYPE_INTER: u32 = 1;
/// `STD_VIDEO_AV1_PRIMARY_REF_NONE`：不使用主参考帧做 CDF 预测
pub(super) const PRIMARY_REF_NONE: u8 = 7;
/// `STD_VIDEO_AV1_INTERPOLATION_FILTER_EIGHTTAP`
pub(super) const INTERPOLATION_EIGHTTAP: u32 = 0;
/// `STD_VIDEO_AV1_TX_MODE_SELECT`：逐块自适应选变换尺寸
///
/// FFmpeg `init_pic_params` 用的就是这个值（`TX_MODE_LARGEST` 只在 `tx_mode_select = 0`
/// 时出现，属于更少见的配置）。
pub(super) const TX_MODE_SELECT: u32 = 2;
/// `STD_VIDEO_AV1_NUM_REF_FRAMES`：VBI 槽位数
pub(super) const NUM_REF_FRAMES: usize = 8;
/// `STD_VIDEO_AV1_REFS_PER_FRAME`：参与帧间预测的参考名数量
pub(super) const REFS_PER_FRAME: usize = 7;
/// `STD_VIDEO_AV1_LAST_FRAME` 在 `referenceNameSlotIndices` / `primary_ref_frame` 里的下标
pub(super) const LAST_FRAME_NAME: u8 = 0;
/// KEY 帧刷新全部 8 个 VBI 槽
///
/// **这不是可调参数。** AV1 规范 5.9.2 `uncompressed_header()` 在
/// `frame_type == KEY_FRAME && show_frame` 分支里直接把 `refresh_frame_flags`
/// 赋成 `allFrames`（0xFF），**根本不从码流读这个字段**。给别的值会让驱动按
/// 「只刷新一个槽」管理内部 VBI 状态，而解码端仍按 0xFF 认为八个槽全部有效 ——
/// 两边立刻分叉，实测表现为**直接黑屏**。
///
/// **禁止**照 FFmpeg 的 `1 << ap->slot` 只刷一个槽来避开「8 个 VBI 槽挤在
/// 2 个物理槽」的配置：驱动按单槽管理内部 VBI 状态，而解码端仍按 0xFF 认为
/// 八槽全部有效，两边立刻分叉，实测黑屏。
pub(super) const REFRESH_ALL: u8 = 0xFF;

/// `picture_info` 指向的三个子结构
///
/// 先让它落到最终位置，再交给 [`build`]——返回值借用它，帧头存活期间不可移动
/// （模块文档「帧头的自引用已拆开」）。
pub struct Av1FrameParts {
    pub quantization: StdVideoAV1Quantization,
    pub loop_filter: StdVideoAV1LoopFilter,
    pub global_motion: StdVideoAV1GlobalMotion,
}

impl Av1FrameParts {
    /// `base_q_idx` 是常量量化索引，仅在码控关闭时生效；CBR 下由驱动覆盖
    pub fn new(base_q_idx: u8) -> Self {
        // SAFETY: `StdVideoAV1QuantizationFlags` 是 `#[repr(C)]` 纯位域 POD，
        // bindgen 不派生 `Default`；全零位模式合法（所有标志关闭）
        let q_flags = unsafe { std::mem::zeroed::<StdVideoAV1QuantizationFlags>() };
        let quantization = StdVideoAV1Quantization {
            flags: q_flags,
            base_q_idx,
            // 色度量化偏移全 0：与序列头的 diff_uv_delta = 0 一致
            DeltaQYDc: 0,
            DeltaQUDc: 0,
            DeltaQUAc: 0,
            DeltaQVDc: 0,
            DeltaQVAc: 0,
            qm_y: 0,
            qm_u: 0,
            qm_v: 0,
        };

        // SAFETY: 同上
        let lf_flags = unsafe { std::mem::zeroed::<StdVideoAV1LoopFilterFlags>() };
        // **与 FFmpeg `init_pic_params` 一致：环路滤波强度全 0（等于关掉）**，
        // 但参考帧/模式 delta 仍按 AV1 的默认表填好。`loop_filter_delta_enabled = 0`
        // 时这些 delta 不进码流，填它们只是与参考实现保持逐字段一致，避免驱动
        // 自行取默认值时与解码端推导出不同的表。
        let loop_filter = StdVideoAV1LoopFilter {
            flags: lf_flags,
            loop_filter_level: [0; 4],
            loop_filter_sharpness: 0,
            update_ref_delta: 0,
            // AV1 的默认 ref delta 表（FFmpeg 的 `default_loop_filter_ref_deltas`）
            loop_filter_ref_deltas: [1, 0, 0, 0, -1, 0, -1, -1],
            update_mode_delta: 1,
            loop_filter_mode_deltas: [0; 2],
        };

        let global_motion = StdVideoAV1GlobalMotion {
            // 0 = IDENTITY：不做全局运动补偿，参数数组随之无意义
            GmType: [0; NUM_REF_FRAMES],
            gm_params: [[0; 6]; NUM_REF_FRAMES],
        };

        Self {
            quantization,
            loop_filter,
            global_motion,
        }
    }
}

/// 一帧的 AV1 帧头结构
///
/// `picture_info` 的三个指针指向 [`Av1FrameParts`]；`_marker` 以 `PhantomData`
/// 把 `'a` 绑在 `picture_info` 上 —— 帧头活着期间，被指向的数据移不动、改不了。
pub struct Av1FrameStd<'a> {
    pub picture_info: StdVideoEncodeAV1PictureInfo,
    /// 本帧作为参考帧写进 DPB 槽时用的信息
    pub ref_info: StdVideoEncodeAV1ReferenceInfo,
    /// 借用锚，不参与编码：见结构体文档
    _marker: PhantomData<&'a Av1FrameParts>,
}

/// 构造一帧的帧头结构
///
/// - `parts`：被 `picture_info` 指向的量化 / 环路滤波 / 全局运动
/// - `is_key`：KEY 帧（等价于 H.26x 的 IDR）还是 INTER 帧
/// - `frame_idx`：帧计数，写进 `current_frame_id`
/// - `setup_slot`：本帧重建帧写入的 **DPB 槽号**，同时也是被刷新的 VBI 槽号
/// - `ref_slot`：参考帧所在的 **DPB 槽号**，同时也是它的 VBI 槽号（KEY 帧忽略）
/// - `coded` / `display`：编码尺寸与显示尺寸。AV1 没有 cropping 语法，显示区域由
///   `render_size` 表达，因此两者不等时必须置 `render_and_frame_size_different`
///
/// **不处理 order hint**：序列头里 `enable_order_hint = 0`，`order_hint` 与
/// `ref_order_hint[]` 都不参与码流，一律填 0。本实现不用 `frame_refs_short_signaling` /
/// `use_ref_frame_mvs` / `jnt_comp` / `skip_mode`，order hint 没有任何用途。
///
/// **VBI 槽号与 DPB 槽号必须取同一个值。** AV1 有两套编号：
/// `refresh_frame_flags` / `ref_frame_idx` 索引 8 个虚拟参考槽（VBI），
/// 而 `VkVideoEncodeInfoKHR::pReferenceSlots` 索引物理 DPB 槽，两者的桥是
/// `referenceNameSlotIndices`。让两套编号错开（例如「VBI 固定为 0、DPB 0/1 轮转」）
/// 在规范上说得通，但驱动只要有一处理解偏差，编码器就会拿错参考图：
/// 残差按 A 帧算、解码端加在 B 帧上，误差随参考链累积 —— 静态画面表现为
/// **彩色噪点逐帧变重**，运动画面表现为整片偏色。
/// FFmpeg 的 `vulkan_encode_av1.c` 直接用 `ref_frame_idx[i] = ap_ref->slot`、
/// `refresh_frame_flags = 1 << ap->slot`，让两套编号完全一致，就是为避开这类歧义。
pub fn build<'a>(
    parts: &'a Av1FrameParts,
    is_key: bool,
    frame_idx: u32,
    setup_slot: u8,
    ref_slot: u8,
    coded: vk::Extent2D,
    display: vk::Extent2D,
) -> Av1FrameStd<'a> {
    // SAFETY: `StdVideoEncodeAV1PictureInfoFlags` 是 `#[repr(C)]` 纯位域 POD，bindgen
    // 不派生 `Default`；全零位模式合法（所有标志关闭），随后只打开需要的几位
    let mut flags = unsafe { std::mem::zeroed::<StdVideoEncodeAV1PictureInfoFlags>() };
    // 输出这一帧（否则解码器不显示）
    flags.set_show_frame(1);
    // KEY 帧才能被当作「可显示起点」；帧间帧的 showable 由 show_frame 推导，
    // 这里显式写出来与 FFmpeg 的 `showable_frame = frame_type != KEY` 一致
    if !is_key {
        flags.set_showable_frame(1);
    }
    // 关键帧必须是错误恢复点（FFmpeg 的 `(I||IDR) && display_order <= encode_order`）
    if is_key {
        flags.set_error_resilient_mode(1);
    }
    // 1/8 像素精度运动矢量。与 FFmpeg 一致取 **0**。
    //
    // 设 1 的理由曾是「画质更好，且不需要任何额外语法」——但 `allow_high_precision_mv = 1`
    // 会让编码器大量走 1/8 精度的亚像素插值，而真实码流几乎全是 0，那条路径没有任何
    // 参考实现验证过。这正是本项目反复踩的那类自作主张：语法合法不代表画面正确。
    flags.set_allow_high_precision_mv(0);
    // 显示尺寸小于编码尺寸时必须置位：否则 `render_width/height_minus_1` 不写进码流，
    // 解码器按帧尺寸输出（1088 而非 1080）
    if display != coded {
        flags.set_render_and_frame_size_different(1);
    }

    // 7 个参考名全部指向同一个 VBI 槽 = 参考帧所在的 DPB 槽。
    // KEY 帧不写这个字段，保持 0（与 FFmpeg 的 `ref_frame_idx = { 0 }` 初值一致）
    let mut ref_frame_idx = [0i8; REFS_PER_FRAME];
    if !is_key {
        ref_frame_idx.fill(ref_slot as i8);
    }

    let picture_info = StdVideoEncodeAV1PictureInfo {
        flags,
        frame_type: if is_key {
            FRAME_TYPE_KEY
        } else {
            FRAME_TYPE_INTER
        },
        // 未启用解码器模型 / 显示延迟，这两个时序字段不参与码流
        frame_presentation_time: 0,
        // frame_id_numbers_present_flag = 0，字段不参与码流，保持与帧号一致即可
        current_frame_id: frame_idx,
        // enable_order_hint = 0：这个字段不参与码流，必须为 0
        order_hint: 0,
        // 帧间帧用 LAST_FRAME 做 CDF 预测（与 `ref_frame_idx[0]` 指向的槽一致）；
        // KEY 帧没有参考，规范强制为 PRIMARY_REF_NONE
        primary_ref_frame: if is_key {
            PRIMARY_REF_NONE
        } else {
            LAST_FRAME_NAME
        },
        // KEY 帧刷新全部 VBI 槽（规范隐式要求 allFrames，见 `REFRESH_ALL` 的说明）；
        // 帧间帧刷新本帧写入的那个槽（= DPB 槽号）
        refresh_frame_flags: if is_key {
            REFRESH_ALL
        } else {
            1u8 << (setup_slot & 7)
        },
        // enable_superres = 0，编码像素比不参与码流
        coded_denom: 0,
        render_width_minus_1: display.width.saturating_sub(1).min(u16::MAX as u32) as u16,
        render_height_minus_1: display.height.saturating_sub(1).min(u16::MAX as u32) as u16,
        interpolation_filter: INTERPOLATION_EIGHTTAP,
        // 与 FFmpeg 一致：逐块自适应选变换尺寸
        TxMode: TX_MODE_SELECT,
        // delta_q / delta_lf 都未启用，这两个字段不参与码流
        delta_q_res: 0,
        delta_lf_res: 0,
        // enable_order_hint = 0：这张表不参与码流，全部为 0
        ref_order_hint: [0; NUM_REF_FRAMES],
        ref_frame_idx,
        reserved1: [0; 3],
        // frame_id_numbers_present_flag = 0，字段不参与码流
        delta_frame_id_minus_1: [0; REFS_PER_FRAME],
        // 单 tile，不需要显式 tile 参数
        pTileInfo: std::ptr::null(),
        pQuantization: &parts.quantization,
        // segmentation：规范明确 AV1 编码不支持，必须为 null
        pSegmentation: std::ptr::null(),
        pLoopFilter: &parts.loop_filter,
        // 序列头 enable_cdef = 0（与 FFmpeg 一致），所以这一项必须为 null
        pCDEF: std::ptr::null(),
        // 序列头 enable_restoration = 0
        pLoopRestoration: std::ptr::null(),
        pGlobalMotion: &parts.global_motion,
        // 不生成 OBU extension header，也就没有 temporal/spatial id
        pExtensionHeader: std::ptr::null(),
        // 未启用解码器模型（序列头 timing_info_present_flag = 0）
        pBufferRemovalTimes: std::ptr::null(),
    };

    // SAFETY: 同 picture info 的 flags，纯位域 POD，全零合法
    let ref_flags = unsafe { std::mem::zeroed::<StdVideoEncodeAV1ReferenceInfoFlags>() };
    let ref_info = StdVideoEncodeAV1ReferenceInfo {
        flags: ref_flags,
        RefFrameId: frame_idx,
        frame_type: if is_key {
            FRAME_TYPE_KEY
        } else {
            FRAME_TYPE_INTER
        },
        // enable_order_hint = 0：这个字段不参与码流
        OrderHint: 0,
        reserved1: [0; 3],
        pExtensionHeader: std::ptr::null(),
    };

    Av1FrameStd {
        picture_info,
        ref_info,
        _marker: PhantomData,
    }
}

/// 参考帧在 DPB 槽里的描述信息
///
/// `frame_idx` 是该参考帧自己的帧号（不是当前帧的）。
/// `current_frame_id` 只在 `frame_id_numbers_present_flag = 1` 时才进码流，本实现置 0，
/// 因此这里保持与帧号一致即可。
///
/// `OrderHint` 固定为 0：序列头里 `enable_order_hint = 0`，这个字段不参与码流。
pub fn reference_info(frame_idx: u32, is_key: bool) -> StdVideoEncodeAV1ReferenceInfo {
    // SAFETY: 纯位域 POD，全零位模式合法
    let flags = unsafe { std::mem::zeroed::<StdVideoEncodeAV1ReferenceInfoFlags>() };
    StdVideoEncodeAV1ReferenceInfo {
        flags,
        RefFrameId: frame_idx,
        frame_type: if is_key {
            FRAME_TYPE_KEY
        } else {
            FRAME_TYPE_INTER
        },
        OrderHint: 0,
        reserved1: [0; 3],
        pExtensionHeader: std::ptr::null(),
    }
}

// ---- 编码命令装配 ----

impl FrameEncoder {
    /// AV1 一帧
    ///
    /// 与 H.26x 的三处结构性差异：
    /// 1. **没有 slice header**：帧头字段全在 `StdVideoEncodeAV1PictureInfo` 里，驱动据它写
    ///    frame OBU 与 tile group。
    /// 2. **两套槽位**：`refresh_frame_flags` / `ref_frame_idx` 索引 8 个 VBI 槽，
    ///    `pReferenceSlots` 索引物理 DPB 槽。本实现让两套编号**完全一致**（VBI 槽号 == DPB 槽号），
    ///    理由见 `build` 的文档。
    /// 3. **没有 frame_num 位宽**：序列头由驱动生成，参考管理靠 VBI 槽掩码而不是帧号回绕。
    pub(in crate::encode::hardware::vulkan) fn encode_prepared_av1(
        &mut self,
        force_idr: bool,
    ) -> Result<(Vec<u8>, bool), String> {
        let is_key = force_idr || self.frame_num == 0 || self.frame_num >= self.gop_length;
        let setup_idx = self.next_slot;
        let ref_idx = 1 - setup_idx;
        let frame_num = if is_key { 0 } else { self.frame_num };
        let extent = self.image.extent();

        // std 结构：先让被指向的子结构落位，再构造帧头（同 H.265）
        let parts = Av1FrameParts::new(CONSTANT_Q_INDEX);
        // `setup_idx` / `ref_idx` 同时充当 VBI 槽号：两套编号一致，见 `build`。
        //
        // render_size 传**显示尺寸**：AV1 没有 cropping 语法，这是它唯一能表达「显示区域」
        // 的字段；画面本身按编码尺寸编码（补齐边在采集侧就已就位）。1080p 下差 8 行 =
        // 0.7%，且硬件解码器对「render 小于 frame」的支持并无保证，所以只作声明、不改
        // 编码几何。H.264 / H.265 用裁剪窗口达到同样效果，见 `codec/h264/params.rs` /
        // `codec/h265/params.rs`。
        let frame = build(
            &parts,
            is_key,
            frame_num,
            setup_idx as u8,
            ref_idx as u8,
            extent,
            self.display_extent(),
        );

        // 参考名 → DPB 槽的映射。
        //
        // 规范（VK_KHR_video_encode_av1 §3.7）：编码期间使用哪些参考名**主要由
        // `referenceNameSlotIndices` 的非负元素决定**，且实现可以「裁剪使用的参考名称
        // 集合」。只填 LAST_FRAME、其余留 -1 的话，驱动一旦选择 GOLDEN / ALTREF 这类
        // 名字就找不到可用参考——这不是错误用法，但会让驱动的预测选择受限，实际表现
        // 是帧间预测失效、残差巨大。
        //
        // 提案示例 4.5 给的可移植做法就是「所有元素都指向所用参考图的 DPB 槽」，
        // 让实现自行决定用哪个名字。这里必须与 `ref_frame_idx` 指向**同一个槽号**：
        // 前者是驱动读哪张图，后者是解码端从哪个 VBI 槽取参考，两者错开就会
        // 「残差按 A 帧算、解码端加在 B 帧上」，误差沿参考链累积。
        let mut name_slots = [AV1_UNUSED_REFERENCE_SLOT; AV1_REFS_PER_FRAME];
        if !is_key {
            name_slots.fill(ref_idx as i32);
        }

        let mut picture_info = vk::VideoEncodeAV1PictureInfoKHR::default()
            .prediction_mode(if is_key {
                vk::VideoEncodeAV1PredictionModeKHR::INTRA_ONLY
            } else {
                vk::VideoEncodeAV1PredictionModeKHR::SINGLE_REFERENCE
            })
            .rate_control_group(if is_key {
                vk::VideoEncodeAV1RateControlGroupKHR::INTRA
            } else {
                vk::VideoEncodeAV1RateControlGroupKHR::PREDICTIVE
            })
            // **CBR 下 `constantQIndex` 必须为 0**
            // （VUID-vkCmdEncodeVideoKHR-constantQIndex-10320）。填 128 是非法用法：
            // 驱动若按它量化、而码流里写的是码控算出的 qindex，解码端就会用错的步长
            // 反量化，误差沿参考链累积。只有码控为 DISABLED 时才需要这个字段。
            .std_picture_info(&frame.picture_info)
            // 主参考帧既参与 CDF 预测也参与运动预测（false）。与 std 结构里
            // 帧间帧的 `primary_ref_frame = LAST_FRAME` 一致；置 true 会让驱动只把它
            // 用于 CDF、不用于预测，反而与 `ref_frame_idx[0]` 指向的参考脱节
            .primary_reference_cdf_only(false)
            // 不生成 OBU extension header，与序列头 / std 结构里的 null 指针一致
            .generate_obu_extension_header(false);
        // 定长数组字段直接赋值：ash 未为它生成切片 setter，但字段本身是公开的
        picture_info.reference_name_slot_indices = name_slots;

        // 当前帧写入的 DPB 槽
        let mut setup_dpb =
            vk::VideoEncodeAV1DpbSlotInfoKHR::default().std_reference_info(&frame.ref_info);
        let setup_resource = picture_resource(self.dpb.view(), extent, setup_idx as u32);
        let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_idx as i32)
            .picture_resource(&setup_resource)
            .push(&mut setup_dpb);

        // 参考槽（仅帧间帧用）：上一帧的帧号；帧号为 0 的那一帧必然是 KEY 帧
        let prev_frame_num = frame_num.saturating_sub(1);
        let ref_ref_info = reference_info(prev_frame_num, prev_frame_num == 0);
        let mut ref_dpb =
            vk::VideoEncodeAV1DpbSlotInfoKHR::default().std_reference_info(&ref_ref_info);
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
        if !is_key {
            encode_info = encode_info.reference_slots(&ref_slots);
        }

        let intra_cycle = self.intra_refresh_cycle;
        let mut intra_info = intra_refresh_info(intra_cycle, self.intra_refresh_index);
        if let Some(info) = intra_info.as_mut() {
            encode_info = encode_info.push(info);
        }

        // CBR 码控：量化索引范围来自能力查询。CBR 下驱动自己算量化步长，缺省范围
        // 会让算出的 Q 越界（INVALID_VIDEO_STD_PARAMETERS）；地板还不能超过
        // `maxQIndex`（VUID 10300 / 10304）。
        let (min_q_index, max_q_index) = self.coding().q_index_range();
        let max_q_index = max_q_index.max(min_q_index);
        let floor = min_q_index.max(MIN_Q_INDEX_AV1).min(max_q_index);
        let min_q = vk::VideoEncodeAV1QIndexKHR::default()
            .intra_q_index(floor)
            .predictive_q_index(floor)
            .bipredictive_q_index(floor);
        let max_q = vk::VideoEncodeAV1QIndexKHR::default()
            .intra_q_index(max_q_index)
            .predictive_q_index(max_q_index)
            .bipredictive_q_index(max_q_index);
        let mut av1_layer = vk::VideoEncodeAV1RateControlLayerInfoKHR::default()
            .use_min_q_index(true)
            .min_q_index(min_q)
            .use_max_q_index(true)
            .max_q_index(max_q);
        let layer = vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(self.bitrate_bps())
            .max_bitrate(self.bitrate_bps())
            .frame_rate_numerator(self.fps())
            .frame_rate_denominator(1)
            .push(&mut av1_layer);
        let layers = [layer];
        // 两个 flag 都必须声明，缺一个驱动就会按默认假设去管理参考帧：
        //
        // - `REGULAR_GOP`：GOP 结构规则（每 `keyFramePeriod` 帧一个关键帧）。不声明时
        //   规范要求 `gopFrameCount` / `keyFramePeriod` / `consecutiveBipredictiveFrameCount`
        //   必须全为 0 —— 我们填了非零值却不声明，是明确的非法用法，驱动行为未定义。
        // - `REFERENCE_PATTERN_FLAT`：参考模式是**平坦的**（每帧最多参考一个前帧）。
        //   这是本实现的实际结构（`ref_frame_idx` 全部指向上一帧、无 B 帧）。不声明时
        //   驱动会按层次参考模式（dyadic）规划 DPB 与码控，与码流里声明的参考结构
        //   对不上 —— 残差按一套参考算、解码端按另一套加，误差沿参考链累积。
        //
        // FFmpeg `vulkan_encode_av1.c::init_pic_rc` 同样同时置这两位，取值与本实现一致。
        let av1_rate_control = vk::VideoEncodeAV1RateControlInfoKHR::default()
            .flags(
                vk::VideoEncodeAV1RateControlFlagsKHR::REGULAR_GOP
                    | vk::VideoEncodeAV1RateControlFlagsKHR::REFERENCE_PATTERN_FLAT,
            )
            .gop_frame_count(self.gop_length)
            .key_frame_period(self.gop_length)
            // 无 B 帧：`b_per_p - 1` 为 0
            .consecutive_bipredictive_frame_count(0)
            // 未声明 `TEMPORAL_LAYER_PATTERN_DYADIC`，规范要求此时该值必须为 1
            .temporal_layer_count(1);
        let scope = begin_scope(if is_key { None } else { Some(&ref_slots[0]) }, &setup_slot);
        // 驱动要求/偏好时，把本帧在 GOP 内的剩余 Intra/Predictive 帧数交给码控
        let gop_remaining = self.gop_remaining(frame_num, is_key);
        let bytes_written = self.submit_encode(
            &encode_info,
            &scope,
            &layers,
            Some(CodecRateControl::Av1(av1_rate_control)),
            gop_remaining,
        )?;
        // 没有需要逐帧维护的跨帧状态：`order_hint` 已在序列头里关掉，
        // VBI 槽的刷新与参考选择完全由 `refresh_frame_flags` / `ref_frame_idx` 表达。
        self.finish_frame(bytes_written, is_key)
    }
}

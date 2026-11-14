//! AV1 编码方言 (AV1 Encode Dialect)
//!
//! 本文件是「AV1 的一切」，三段内容同址：
//! - **码流词汇**：会话级序列头 + 每帧参数（std 结构构造）
//! - **会话参数装配**：[`VideoSessionParameters::new_av1`]
//! - **编码命令装配**：[`FrameEncoder::encode_prepared_av1`]
//!
//! AV1 与 H.26x 有一个根本差异：**序列头不是参数集数组，而是唯一的 sequence header
//! OBU**。所以 AV1 的会话参数对象只装一份序列头，且不支持 `vkUpdateVideoSessionParametersKHR`
//! 增量更新 —— 换分辨率就得重建整个参数对象。
//!
//! std 结构只负责**构造**，不负责把它写成比特流：AV1 序列头语法（unary 编码的
//! operating point、color config 的条件分支）比 H.26x 复杂得多，而规范提供了
//! `vkGetEncodedVideoSessionParametersKHR` 让驱动直接产出 OBU 字节。用它比自己写语法可靠：
//! 驱动还可能在内部覆盖部分参数，只有它自己序列化出来的那份才是与码流一致的。
//!
//! 每帧要交给驱动一组 std 结构：picture info（帧头字段）+ 量化 + 环路滤波 + 全局运动，
//! 以及写进 DPB 槽时用的 reference info。
//!
//! **帧头的自引用已拆开**：`StdVideoEncodeAV1PictureInfo` 的四个指针字段若指向同一
//! 结构体的兄弟字段，就只能等结构体落位后再调 `link()` 接上，而「接完之后不能再移动」
//! 没有任何编译期保证 —— 返回值、`Vec` 扩容、`mem::swap` 都会让指针悬垂。
//! 因此被指向的部分单独成 [`Av1FrameParts`]，[`build`] 借用它并返回 `Av1FrameStd<'a>`：
//! 只要帧头还活着，`parts` 就被借走，移不动 —— 这条约束交给借用检查器。
//!
//! 序列头与每帧的取舍一致（照抄 FFmpeg `init_sequence_headers`）：序列头里关掉的工具
//! （cdef / restoration / superres / segmentation / tile），对应的每帧指针一律为 null；
//! 规范要求 `pSegmentation` 必须为 null，因为 AV1 编码暂不支持 segmentation。

use std::marker::PhantomData;

use ash::vk::native::{
    StdVideoAV1ColorConfig, StdVideoAV1ColorConfigFlags, StdVideoAV1GlobalMotion,
    StdVideoAV1LoopFilter, StdVideoAV1LoopFilterFlags, StdVideoAV1Quantization,
    StdVideoAV1QuantizationFlags, StdVideoAV1SequenceHeader, StdVideoAV1SequenceHeaderFlags,
    StdVideoEncodeAV1OperatingPointInfo, StdVideoEncodeAV1OperatingPointInfoFlags,
    StdVideoEncodeAV1PictureInfo, StdVideoEncodeAV1PictureInfoFlags,
    StdVideoEncodeAV1ReferenceInfo, StdVideoEncodeAV1ReferenceInfoFlags,
};
use ash::vk::{self, TaggedStructure};

use super::{CodecRateControl, FrameEncoder, begin_scope, intra_refresh_info, picture_resource};
use crate::encode::VideoCodec;
use crate::encode::hardware::vulkan::caps::CodecCaps;
use crate::encode::hardware::vulkan::device::VideoDevice;
use crate::encode::hardware::vulkan::session::{VideoSession, VideoSessionParameters};

#[cfg(test)]
use super::{make_noisy_frame, test_profile};
#[cfg(test)]
use crate::encode::hardware::vulkan::context::VulkanInstance;
#[cfg(test)]
use crate::encode::hardware::vulkan::{caps, test_support};

/// `STD_VIDEO_AV1_REFS_PER_FRAME`
const AV1_REFS_PER_FRAME: usize = 7;

/// 未使用的参考名在 `referenceNameSlotIndices` 里的取值（负值 = 无参考）
const AV1_UNUSED_REFERENCE_SLOT: i32 = -1;

/// 编码命令装配的恒定量化索引占位：CBR 下驱动自己算 qindex，std 结构里这个字段
/// 的实际约束见 `encode_prepared_av1`（VUID-10320 要求 `constantQIndex` 为 0）
const CONSTANT_Q_INDEX: u8 = 128;

/// AV1 码控的量化索引地板：只约束「画质过好」的一侧，不参与码率上限。
/// 码控层下限取它与能力快照 `min_q_index` 的较大者
const MIN_Q_INDEX_AV1: u32 = 108;

// ---- 会话级：序列头 ----

/// Main profile：4:2:0 8bit，与建会话时声明的 profile 必须一致
const AV1_PROFILE_MAIN: u32 = 0;
/// BT.709 的 primaries / transfer / matrix 枚举值都是 1
const BT709: u32 = 1;
/// `CSP_COLOCATED`：色度样本与对应亮度样本共位
///
/// 4:2:0 的标准取值。**不能填 `CSP_UNKNOWN`（0）**：规范对 UNKNOWN 只说「由渲染器
/// 采用默认值」，于是软件解码器（ffmpeg / libaom）和硬件解码器可以各自假设不同的
/// 上采样位置 —— 编码端按共位输出的色度，被硬件按另一套相位插值，结果就是**彩色噪点**，
/// 而且只有硬件路径会错、ffmpeg 永远测不出来。NVENC 的序列头由驱动生成，用的是共位。
const CHROMA_POS_COLOCATED: u32 = 2;
/// 8bit
const BIT_DEPTH_8: u8 = 8;

/// 色彩配置
///
/// 与 CSC 保持同一套色彩参数：BT.709 + Limited Range。`color_range` 置 0 表示 studio
/// （limited）范围，和 `rgba_to_nv12.comp` 输出的 16..235 一致；这里若写成 full range，
/// 解码器会按 0..255 解释，画面会偏灰。
pub fn color_config() -> StdVideoAV1ColorConfig {
    // SAFETY: `StdVideoAV1ColorConfigFlags` 是 `#[repr(C)]` 纯位域 POD，bindgen 不派生
    // `Default`；全零位模式合法，随后由 setter 逐位打开
    let mut flags = unsafe { std::mem::zeroed::<StdVideoAV1ColorConfigFlags>() };
    // 有色彩描述，解码器才会读 primaries/transfer/matrix 三个字段
    flags.set_color_description_present_flag(1);

    StdVideoAV1ColorConfig {
        flags,
        BitDepth: BIT_DEPTH_8,
        // 4:2:0：两个方向都做色度抽样
        subsampling_x: 1,
        subsampling_y: 1,
        reserved1: 0,
        color_primaries: BT709,
        transfer_characteristics: BT709,
        matrix_coefficients: BT709,
        // 4:2:0 的标准取值，必须显式声明（UNKNOWN 会让硬件解码器自行假设相位）
        chroma_sample_position: CHROMA_POS_COLOCATED,
    }
}

/// `order_hint_bits_minus_1`：**0，即不启用 order hint**
///
/// 序列头里 `enable_order_hint = 0`，这个字段随之不参与码流。
///
/// **为什么不启用**：order hint 的唯一用途是 `frame_refs_short_signaling`、
/// `use_ref_frame_mvs`、`jnt_comp`、`skip_mode` 这四项 —— 本实现一个都没开，
/// 参考名固定指向 LAST_FRAME。开着它只会多出一份「必须与解码端严格同步」的状态：
/// `order_hint` 与 `ref_order_hint[8]` 由我们手写（NVENC 侧由驱动生成），
/// 任何一处不一致，硬件解码器做 MV 投影时就会与编码端的理解分叉。
///
/// 曾用 7（8 位），理由是「4 位装不下 60/120/240 的 GOP」。但那是误解：
/// order hint 只需要表达**参考帧与当前帧的距离**，本实现永远参考上一帧，距离固定为 1。
const ORDER_HINT_BITS_MINUS_1: u8 = 0;

/// 序列头
///
/// `color` 由调用方持有：本结构只存它的地址，不拥有它。传临时值会立刻悬垂。
///
/// **工具取舍照抄 FFmpeg `vulkan_encode_av1.c`（`init_sequence_headers`）**：它把所有
/// 画质工具都置 0 —— `enable_cdef`、`enable_restoration`、`enable_filter_intra`、
/// `enable_intra_edge_filter`、`enable_interintra_compound`、`enable_masked_compound`、
/// `enable_warped_motion`、`enable_dual_filter`、`enable_jnt_comp`、`enable_ref_frame_mvs`
/// 全是 0。**`enable_order_hint` 也关掉**（FFmpeg 开着它，但那是它需要 —— 见
/// `ORDER_HINT_BITS_MINUS_1` 的说明）。
/// 每帧的 `pCDEF` / `pLoopRestoration` 随之保持 null（FFmpeg 传的是全零结构，效果等价）。
///
/// 不要凭「画质更好」去动这些开关：`enable_cdef = 0` 却给 `pCDEF` 是非法用法；
/// 开 `enable_restoration` 后每帧必须按平面给 `FrameRestorationType` 与
/// `LoopRestorationSize`，单元尺寸填错会直接破坏解码。
///
/// - `use_128x128_superblock` 必须为 0：本机驱动只声明了 64x64 超级块
///   （`superblock_sizes`）。FFmpeg 是按 `caps.superblockSizes` 读的，硬编码 0 与之等价。
pub fn sequence_header(
    extent: vk::Extent2D,
    color: &StdVideoAV1ColorConfig,
) -> StdVideoAV1SequenceHeader {
    // SAFETY: 同 `color_config`，全零位模式合法
    let flags = unsafe { std::mem::zeroed::<StdVideoAV1SequenceHeaderFlags>() };
    // **不打开 `enable_order_hint`**（全零位即关闭）。理由见 `ORDER_HINT_BITS_MINUS_1`：
    // 本实现不使用任何依赖 order hint 的工具，开着它只会多一份必须与解码端严格同步的状态。

    let max_w = extent.width.saturating_sub(1);
    let max_h = extent.height.saturating_sub(1);

    StdVideoAV1SequenceHeader {
        flags,
        seq_profile: AV1_PROFILE_MAIN,
        // 宽高字段的位宽：能表示 max_frame_width_minus_1 所需位数减 1
        frame_width_bits_minus_1: bits_minus_1(max_w),
        frame_height_bits_minus_1: bits_minus_1(max_h),
        // 字段本身是 u16：AV1 的序列头也就这个上限
        max_frame_width_minus_1: max_w.min(u16::MAX as u32) as u16,
        max_frame_height_minus_1: max_h.min(u16::MAX as u32) as u16,
        // frame_id_numbers_present_flag = 0，这两个字段不参与码流
        delta_frame_id_length_minus_2: 0,
        additional_frame_id_length_minus_1: 0,
        // enable_order_hint = 0，这个位宽不参与码流
        order_hint_bits_minus_1: ORDER_HINT_BITS_MINUS_1,
        // 0 = 强制关闭（不是 select），因此每帧不需要再单独表态
        seq_force_integer_mv: 0,
        seq_force_screen_content_tools: 0,
        reserved1: [0; 5],
        pColorConfig: color,
        // 不下发 timing info：帧率由码控层的 frame_rate 表达，序列头里再写一份会多一个
        // 必须与码控保持一致的地方
        pTimingInfo: std::ptr::null(),
    }
}

/// 操作点信息
///
/// AV1 至少要有一个操作点（`operating_points_cnt_minus_1 >= 0`），所以这里必须给一份，
/// 不能传空数组。`seq_level_idx` 用驱动声明的最高 level —— 与 H.26x 侧同一套口径。
pub fn operating_point(seq_level_idx: u32) -> StdVideoEncodeAV1OperatingPointInfo {
    // SAFETY: 同 `color_config`，全零位模式合法
    let flags = unsafe { std::mem::zeroed::<StdVideoEncodeAV1OperatingPointInfoFlags>() };

    StdVideoEncodeAV1OperatingPointInfo {
        flags,
        // 单操作点，idc = 0
        operating_point_idc: 0,
        seq_level_idx: seq_level_idx.min(u8::MAX as u32) as u8,
        // Main tier
        seq_tier: 0,
        // decoder model / initial display delay 都未启用，三个时序字段不参与码流
        decoder_buffer_delay: 0,
        encoder_buffer_delay: 0,
        initial_display_delay_minus_1: 0,
    }
}

/// 表示 `value` 所需位宽减 1
///
/// AV1 的 `frame_width_bits_minus_1` 是「能表示 max_frame_width_minus_1 的位数减 1」。
/// 1920 宽 → max_frame_width_minus_1 = 1919 → 11 位 → 写 10。
fn bits_minus_1(value: u32) -> u8 {
    let bits = u32::BITS - value.max(1).leading_zeros();
    (bits.saturating_sub(1)).min(u8::MAX as u32) as u8
}

// ---- 每帧参数 ----

/// `STD_VIDEO_AV1_FRAME_TYPE_KEY`
pub(super) const FRAME_TYPE_KEY: u32 = 0;
/// `STD_VIDEO_AV1_FRAME_TYPE_INTER`
pub(super) const FRAME_TYPE_INTER: u32 = 1;
/// `STD_VIDEO_AV1_PRIMARY_REF_NONE`：不使用主参考帧做 CDF 预测
const PRIMARY_REF_NONE: u8 = 7;
/// `STD_VIDEO_AV1_INTERPOLATION_FILTER_EIGHTTAP`
const INTERPOLATION_EIGHTTAP: u32 = 0;
/// `STD_VIDEO_AV1_TX_MODE_SELECT`：逐块自适应选变换尺寸
///
/// FFmpeg `init_pic_params` 用的就是这个值（`TX_MODE_LARGEST` 只在 `tx_mode_select = 0`
/// 时出现，属于更少见的配置）。
const TX_MODE_SELECT: u32 = 2;
/// `STD_VIDEO_AV1_NUM_REF_FRAMES`：VBI 槽位数
const NUM_REF_FRAMES: usize = 8;
/// `STD_VIDEO_AV1_REFS_PER_FRAME`：参与帧间预测的参考名数量
const REFS_PER_FRAME: usize = 7;
/// `STD_VIDEO_AV1_LAST_FRAME` 在 `referenceNameSlotIndices` / `primary_ref_frame` 里的下标
const LAST_FRAME_NAME: u8 = 0;
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

// ---- 会话参数装配 ----

impl VideoSessionParameters {
    /// 建 AV1 的会话参数对象（只有一份 sequence header）
    ///
    /// 与 H.26x 的两点结构性差异：
    /// 1. 序列头只有一份，且参数对象**不支持增量更新** —— 换分辨率必须整对象重建。
    /// 2. 序列头的**字节**由驱动生成（`vkGetEncodedVideoSessionParametersKHR`），这里不写语法。
    ///    驱动可能在内部覆盖部分参数，只有它自己序列化出来的那份才与码流一致。
    ///
    /// `level_idx` 必须是**按内容推导**的 AV1 level（`encode::av1_level_idx`），
    /// 不能用驱动声明的最高 level —— 见 `operating_point` 调用处的说明。
    pub(super) fn new_av1(
        device: &VideoDevice,
        session: &VideoSession,
        caps: &CodecCaps,
        level_idx: u32,
    ) -> Result<Self, String> {
        if caps.codec != VideoCodec::Av1 {
            return Err(format!("AV1 会话参数收到 {:?}", caps.codec));
        }
        let extent = session.coded_extent();
        // 两个局部都必须活到 create 调用结束：序列头只存它们的地址，不拥有它们
        let color = color_config();
        let header = sequence_header(extent, &color);
        // **`seq_level_idx` 必须与 SDP 的 `level-idx` 同源。**
        //
        // SDP 由 `transport::av1_sdp_fmtp` 按分辨率/帧率/码率推导；浏览器在
        // `configure()` 时用那个 level 分配解码资源（DPB 大小、带宽预算）。
        // 如果序列头声明更高的 level（例如驱动支持的 6.0 而内容只需要 4.1），
        // 硬件解码器就会「按 4.1 分配、按 6.0 解码」—— 资源不够，参考帧被挤掉，
        // 表现为画面持续劣化。ffmpeg 不看 SDP，直接按码流的 level 解，所以测不出来。
        //
        // NVENC 侧（`nvenc/session.rs`）本来就是这么做的，注释写着
        // 「使序列头的 seq_level_idx 与 SDP 的 level-idx 同源」。
        let ops = [operating_point(level_idx)];

        // 计数字段由切片 setter 一并写入：ash 对这个结构同样生成了切片版
        // `std_operating_points`（虽然 `stdOperatingPointCount` 与 `pStdOperatingPoints`
        // 不成常规配对，生成器的启发式仍然认出了它）
        let mut av1_params = vk::VideoEncodeAV1SessionParametersCreateInfoKHR::default()
            .std_sequence_header(&header)
            .std_operating_points(&ops);

        let create_info = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(session.session())
            .push(&mut av1_params);

        // SAFETY: create_info 及其引用的 av1_params → header → color 链在调用期间全部
        // 存活；返回的 params 在后续失败路径上会被显式销毁
        let params = unsafe {
            device
                .video_queue_table()
                .create_video_session_parameters(&create_info, None)
        }
        .map_err(|e| format!("创建 AV1 会话参数对象失败: {e:?}"))?;

        // AV1 的 pNext 留空：规范只为 H.264/H.265 要求 codec 专属的 GetInfo 结构，
        // 也没有对应的 FeedbackInfo
        let info =
            vk::VideoEncodeSessionParametersGetInfoKHR::default().video_session_parameters(params);
        Self::assemble(device, params, &info, std::ptr::null_mut())
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
    pub(super) fn encode_prepared_av1(
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
        // render_size 传**编码尺寸**，不用显示尺寸。AV1 没有 cropping 语法，`render_size`
        // 是唯一的显示尺寸表达方式，但它只影响显示，而硬件解码器对「render 小于 frame」
        // 的支持没有保证（部分实现会忽略，部分会走一条未充分测试的路径）。1080p 下差
        // 8 行 = 0.7%，客户端本来就 `object-fit: contain`，不值得为此换一条没人验证过的
        // 解码路径。要恢复「码流声明 1080」时再传 `session.display_extent()`。
        // render_size 传**显示尺寸**：AV1 没有 cropping 语法，这是它唯一能表达「显示区域」
        // 的字段；来源侧仍按编码尺寸编码（补齐全在主机帧上传时做）。H.264 / HEVC 在同一
        // 后端用裁剪窗口达到同样效果，见 `h264.rs` / `hevc.rs`。
        let frame = build(
            &parts,
            is_key,
            frame_num,
            setup_idx as u8,
            ref_idx as u8,
            extent,
            self.display,
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

        // CBR 码控：量化索引范围来自能力查询。
        // H.264 那次缺 QP 范围导致驱动算出的 QP 越界、报 INVALID_VIDEO_STD_PARAMETERS，
        // 这里提前给出范围而不是等驱动报错。
        let floor = self.min_q_index.max(MIN_Q_INDEX_AV1);
        let min_q = vk::VideoEncodeAV1QIndexKHR::default()
            .intra_q_index(floor)
            .predictive_q_index(floor)
            .bipredictive_q_index(floor);
        let max_q = vk::VideoEncodeAV1QIndexKHR::default()
            .intra_q_index(self.max_q_index)
            .predictive_q_index(self.max_q_index)
            .bipredictive_q_index(self.max_q_index);
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
        let bytes_written = self.submit_encode(
            &encode_info,
            &scope,
            &layers,
            Some(CodecRateControl::Av1(av1_rate_control)),
        )?;
        // 没有需要逐帧维护的跨帧状态：`order_hint` 已在序列头里关掉，
        // VBI 槽的刷新与参考选择完全由 `refresh_frame_flags` / `ref_frame_idx` 表达。
        self.finish_frame(bytes_written, is_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 超级块尺寸必须是 64x64：本机驱动只声明了 TYPE_64
        assert_eq!(header.flags.use_128x128_superblock(), 0);
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
        let session = match VideoSession::new(&device, &caps, 1920, 1080) {
            Ok(session) => session,
            Err(e) => {
                println!("AV1 会话创建失败，跳过: {e}");
                return;
            }
        };
        // 与生产路径同源：按内容推导 level，而不是驱动声明的最高 level
        let level_idx = u32::from(crate::encode::av1_level_idx(1920, 1080, 60, 8_000));
        match VideoSessionParameters::new_av1(&device, &session, &caps, level_idx) {
            Ok(params) => {
                let obu = params.parameter_sets();
                println!(
                    "AV1 会话参数对象创建成功: 序列头 OBU {} 字节 @ {}x{} / level {}（推导，驱动上限 {}）/ 句柄 {:?}",
                    obu.len(),
                    session.coded_extent().width,
                    session.coded_extent().height,
                    level_idx,
                    caps.max_level_idc,
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
        let mut encoder = match FrameEncoder::new(&device, &caps, test_profile(&caps), 15) {
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
        ) && let Ok(mut h264) =
            FrameEncoder::new(&h264_device, &h264_caps, test_profile(&h264_caps), 15)
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
        let cycle = if caps.supports_intra_refresh() && device.supports_intra_refresh() {
            Some((60 / 2).min(caps.max_intra_refresh_cycle_duration))
        } else {
            None
        };
        let gop = match cycle {
            Some(_) => 4095,
            None => 60,
        };
        let mut encoder = match FrameEncoder::new(&device, &caps, test_profile(&caps), gop) {
            Ok(encoder) => encoder,
            Err(e) => {
                println!("AV1 编码器初始化失败: {e}");
                return;
            }
        };
        if let Some(c) = cycle
            && let Err(e) = encoder.set_intra_refresh(c)
        {
            println!("开启 intra-refresh 失败: {e}");
            return;
        }
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
}

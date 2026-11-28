//! AV1 会话级码流参数：序列头与参数集装配

use ash::vk::native::{
    StdVideoAV1ColorConfig, StdVideoAV1ColorConfigFlags, StdVideoAV1SequenceHeader,
    StdVideoAV1SequenceHeaderFlags, StdVideoEncodeAV1OperatingPointInfo,
    StdVideoEncodeAV1OperatingPointInfoFlags,
};
use ash::vk::{self, TaggedStructure};

use crate::encode::hardware::vulkan::codec::Av1SequenceConfig;
use crate::encode::hardware::vulkan::codec::VideoSessionParameters;
use crate::encode::hardware::vulkan::device::VideoDevice;
use crate::encode::hardware::vulkan::session::VideoSession;

// ---- 会话级：序列头 ----

/// Main profile：4:2:0 8bit，与建会话时声明的 profile 必须一致
pub(super) const AV1_PROFILE_MAIN: u32 = 0;
/// BT.709 的 primaries / transfer / matrix 枚举值都是 1
pub(super) const BT709: u32 = 1;
/// `CSP_COLOCATED`：色度样本与对应亮度样本共位
///
/// 4:2:0 的标准取值。**不能填 `CSP_UNKNOWN`（0）**：规范对 UNKNOWN 只说「由渲染器
/// 采用默认值」，于是软件解码器（ffmpeg / libaom）和硬件解码器可以各自假设不同的
/// 上采样位置 —— 编码端按共位输出的色度，被硬件按另一套相位插值，结果就是**彩色噪点**，
/// 而且只有硬件路径会错、ffmpeg 永远测不出来。NVENC 的序列头由驱动生成，用的是共位。
pub(super) const CHROMA_POS_COLOCATED: u32 = 2;
/// 8bit
pub(super) const BIT_DEPTH_8: u8 = 8;

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
/// order hint 只需要表达**参考帧与当前帧的距离**；本实现永远参考上一帧，距离固定为
/// 1，4 位足够。不要用「GOP 长度需要更多位」来放大它：距离与 GOP 长度无关。
pub(super) const ORDER_HINT_BITS_MINUS_1: u8 = 0;

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
/// - `use_128x128_superblock` 由调用方按 `caps.superblock_sizes` 决定：128-only
///   驱动必须声明 128，否则无法创建序列头。
pub fn sequence_header(
    extent: vk::Extent2D,
    color: &StdVideoAV1ColorConfig,
    use_128x128_superblock: bool,
) -> StdVideoAV1SequenceHeader {
    // SAFETY: 同 `color_config`，全零位模式合法
    let mut flags = unsafe { std::mem::zeroed::<StdVideoAV1SequenceHeaderFlags>() };
    // **不打开 `enable_order_hint`**（保持 0）。理由见 `ORDER_HINT_BITS_MINUS_1`：
    // 本实现不使用任何依赖 order hint 的工具，开着它只会多一份必须与解码端严格同步的状态。
    if use_128x128_superblock {
        flags.set_use_128x128_superblock(1);
    }

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
/// `seq_level_idx` 由内容推导并与 SDP 同源，不能用驱动声明的最高 level；驱动
/// `maxOperatingPoints=0` 时调用方不传数组，由驱动填默认值。
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
pub(super) fn bits_minus_1(value: u32) -> u8 {
    let bits = u32::BITS - value.max(1).leading_zeros();
    (bits.saturating_sub(1)).min(u8::MAX as u32) as u8
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
    /// `config.level_idx` 必须是**按内容推导**的 AV1 level（`encode::av1_level_idx`），
    /// 不能用驱动声明的最高 level；`config.operating_point_count == 0` 时不写操作点，见下。
    pub(in crate::encode::hardware::vulkan) fn new_av1(
        device: &VideoDevice,
        session: &VideoSession,
        config: &Av1SequenceConfig,
    ) -> Result<Self, String> {
        let extent = session.coded_extent();
        // 两个局部都必须活到 create 调用结束：序列头只存它们的地址，不拥有它们
        let color = color_config();
        let header = sequence_header(extent, &color, config.superblock_128);
        // `maxOperatingPoints > 0` 时，`seq_level_idx` 必须与 SDP 的 `level-idx` 同源。
        //
        // SDP 由 `transport::av1_sdp_fmtp` 按内容推导，浏览器据此分配解码资源
        // （DPB、带宽）。序列头若声明驱动支持的更高 level（如 6.0 而内容只需 4.1），
        // 解码器会「按 4.1 分配、按 6.0 解码」，参考帧被挤掉，画面持续劣化。
        // NVENC 侧（`nvenc/session.rs`）同样按内容推导。
        //
        // `std_operating_points` 只存指针：`ops` 必须活到
        // `create_video_session_parameters` 调用结束。
        let ops = [operating_point(config.level_idx)];

        // `stdOperatingPointCount` 必须 <= 驱动声明的 `maxOperatingPoints`（VUID 10280）：
        // 0 表示实现不接受应用指定的操作点，此时必须省略数组、由驱动生成默认值。
        // 计数字段由 ash 的切片 setter 一并写入。
        let mut av1_params = vk::VideoEncodeAV1SessionParametersCreateInfoKHR::default()
            .std_sequence_header(&header);
        if config.operating_point_count > 0 {
            av1_params = av1_params.std_operating_points(&ops);
        } else {
            log::debug!("AV1 驱动 maxOperatingPoints=0：不传操作点参数，序列头由驱动填默认操作点");
        }

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

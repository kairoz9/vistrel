pub(crate) mod actor;
pub mod audio;
pub(crate) mod control;
pub(crate) mod encoder;
pub mod frame;
pub mod hardware;
#[cfg(test)]
mod perf;
pub mod pipeline;
pub(crate) mod pool;
pub(crate) mod progress;
pub mod software;

use std::time::Duration;

pub use audio::{
    AUDIO_CHANNELS, AUDIO_FRAME_DURATION_MS, AUDIO_SAMPLE_RATE, AUDIO_TOTAL_SAMPLES,
    AudioEncodeError, AudioEncoder, DEFAULT_AUDIO_BITRATE_BPS,
};
pub use encoder::EncodeBackendKind;
pub(crate) use frame::FramePayload;
pub use hardware::{
    BackendCapability, bootstrap_hardware_caps, encoder_caps, encoder_caps_options_json,
    hardware_backends, is_hardware_caps_bootstrapped, is_selectable,
};
pub use pipeline::VideoPipeline;
pub(crate) use progress::{EncodeStage, Heartbeat, StageCell};
use serde::{Deserialize, Serialize};

/// 一个音频包：编码器产出一个音频帧的 Opus 码流，按 `Bytes` 零拷贝移交；
/// `duration` 为该帧的样本时长，接收端据此维持播放节奏
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub data: bytes::Bytes,
    pub duration: Duration,
}

/// 一个视频包：编码器产出一帧的完整码流，按 `Bytes` 零拷贝移交；上链时由轨道分包成 RTP 包
pub struct VideoPacket {
    pub nalu: bytes::Bytes,
    pub birth_nanos: u64,
    pub is_keyframe: bool,
}

/// 已入箱的视频包：随附入箱打点，排队过期看的是「这一帧等了多久」
pub(crate) struct StampedVideoPacket {
    /// 编码产物本体
    pub(crate) packet: VideoPacket,
    /// 入箱时刻（打点）
    pub(crate) posted_at: tokio::time::Instant,
}

impl StampedVideoPacket {
    /// 入箱即打点：过期判定用的是「这一帧等了多久」，不是它诞生了多久
    pub(crate) fn now(packet: VideoPacket) -> Self {
        Self {
            packet,
            posted_at: tokio::time::Instant::now(),
        }
    }
}

/// 支持的视频编码格式 (Video Codec)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VideoCodec {
    #[default]
    H264,
    H265,
    Av1,
}

impl serde::Serialize for VideoCodec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::H264 => serializer.serialize_str("h264"),
            Self::H265 => serializer.serialize_str("h265"),
            Self::Av1 => serializer.serialize_str("av1"),
        }
    }
}

impl<'de> serde::Deserialize<'de> for VideoCodec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_token(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown video codec: {}", s)))
    }
}

impl VideoCodec {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::H265 => "H265",
            Self::Av1 => "AV1",
        }
    }

    /// 令牌别名解析（serde 反序列化与信令文本共用的入口）：先去首尾空白再按
    /// 小写匹配——`h264`/`avc` → H264、`h265`/`hevc` → H265、`av1` → AV1；
    /// 未知令牌（含空串）返回 `None`，别名集合是行为契约，不得增删
    pub fn from_token(token: &str) -> Option<Self> {
        let lower = token.trim().to_lowercase();
        if lower == "h264" || lower == "avc" {
            Some(Self::H264)
        } else if lower == "h265" || lower == "hevc" {
            Some(Self::H265)
        } else if lower == "av1" {
            Some(Self::Av1)
        } else {
            None
        }
    }
}

impl std::fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// 码率计算基准常数：H.264 1080p@60fps bpp=0.1
const BASE_WIDTH: usize = 1920;
const BASE_HEIGHT: usize = 1080;
const BASE_FPS: usize = 60;
/// 画质档位（bits per pixel per frame）的可选区间
pub const MIN_BPP: f32 = 0.1;
pub const MAX_BPP: f32 = 0.3;
/// 分辨率缩放指数：0.75 次幂法则 (Power Law for Resolution Scaling)
const RESOLUTION_POWER: f32 = 0.75;

/// 一档视频档位：显示几何、帧率与码率的不可变事实快照
///
/// 字段私有，只能经 [`Self::new`]（事实构造：默认档、性能矩阵与用例）或
/// [`TierRequest::derive`]（唯一生产构造口）构造：分辨率是已缩入包络并偶对齐的
/// **显示尺寸**（编码尺寸由 [`Self::coded_extent`] 派生），帧率 ≥ 1，码率非零且在
/// u32 范围内（NVENC 侧按裸转换消费）。level 预算压帧与码率模型只在
/// [`TierRequest::derive`] 里生效。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamingProfile {
    width: usize,
    height: usize,
    fps: usize,
    bitrate: usize,
}

impl Default for StreamingProfile {
    /// 初始占位档位：1080p60 下限画质——Actor 软编打底与用例的起点，首个定档请求前生效
    fn default() -> Self {
        Self::new(
            1920,
            1080,
            60,
            calculate_dynamic_bitrate(1920, 1080, 60, MIN_BPP, VideoCodec::H264),
        )
    }
}

/// 定档请求：内容几何与画质偏好的组合，档位派生的唯一输入
///
/// 几何与偏好在派生前都是请求值：包络缩入、偶对齐、level 预算压帧与码率模型全部在
/// [`Self::derive`] 内统一生效。codec 不在请求里——它是协商定死的会话常量（换 codec
/// 走 WebRTC 重协商的 offer 路径），派生时作为上下文传入
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TierRequest {
    pub width: usize,
    pub height: usize,
    pub fps: usize,
    /// 画质偏好（bits per pixel per frame）：只在派生码率时消费，不落进档位
    pub bpp: f32,
}

impl TierRequest {
    /// 派生生效档位：分辨率先过 [`StreamingProfile::sanitize_dimensions`]（缩入包络、
    /// 偶对齐），bpp 钳到 `[MIN_BPP, MAX_BPP]` 并舍入到百分位（NaN/Inf 兜底为下限），
    /// 帧率再过 level 预算压一道，码率按最终几何、bpp 与 codec 现算——产出即合法档位，
    /// 无失败路径
    pub fn derive(self, codec: VideoCodec) -> StreamingProfile {
        let (w, h, f) = StreamingProfile::sanitize_dimensions(self.width, self.height, self.fps);
        let bpp = clamp_bpp(self.bpp);
        // 预算按实际 bpp 压帧率；包络内的合法输入必然有解，None 只是防御兜底
        let f = clamp_framerate_to_level_budget(w, h, f, bpp, codec).unwrap_or(1);
        let bitrate = calculate_dynamic_bitrate(w, h, f, bpp, codec);
        StreamingProfile {
            width: w,
            height: h,
            fps: f,
            bitrate,
        }
    }
}

/// 内容驱动档位的**包络上界**，同时是**会话的规格上界**
///
/// 任何实际档位都先等比缩进这个 16:9 包络（只缩不放），帧率再受 level 预算压一道；
/// SDP level 按「包络几何 + 预算封顶帧率」推导（[`profile_level`] 的 SDP 入口），
/// 必须 ≥ 任何实际码流的 level：offer/answer 只有一次。分辨率没有下限——小窗口
/// 按原生尺寸串流，放大发的是假细节。
pub const MAX_WIDTH: usize = 3840;
pub const MAX_HEIGHT: usize = 2160;
pub const MAX_FPS: usize = 240;

/// 编码尺寸对齐粒度的上界
///
/// 各后端自己声明 `pictureAccessGranularity`（最松的软编不要求对齐，多数 Vulkan 驱动
/// 声明 16，NVIDIA 在 4K 上实际声明 64）。主机帧池按最坏值一次算足，免得容量随后端漂移。
const MAX_CODED_ALIGN: usize = 64;

/// 显示几何的公共对齐：各后端**输入**要求的并集（宽 %4、高 %2）
///
/// NVENC 的 NV12 主机输入按 `pitch = width` 登记，驱动要求 pitch 为 4 的倍数、高度为偶数；
/// 软编与 Vulkan Video 对显示几何无要求（Vulkan 的 coded 补齐走 `coded_align`/`coded_extent`）。
///
/// 取并集而不是随当前后端走：运行期后端会变（NVENC 失败回退软编、探测后端与实建后端不一致），
/// 几何跟着变就得重新协商档位、重建采集管线并逼一个 IDR，代价远大于最多 3px 的余量。
/// 新后端若要求更严（如 16），只改这一处。
const DISPLAY_ALIGN: (usize, usize) = (4, 2);

/// 公共 level 预算的硬约束：H.264 Level 6.0 的 MaxFS / MaxMBPS / MaxBR（High 列）
///
/// 按 codec 分预算暂不需要：HEVC/AV1 的码率模型系数更低、包络几何相同，H.264 这组
/// 是三者中公共的上界，统一用它做「超了就降帧率」的依据。
const LEVEL_BUDGET_MAX_FS_MB: u64 = 139_264;
const LEVEL_BUDGET_MAX_MBPS: u64 = 4_177_920;
const LEVEL_BUDGET_MAX_BR_KBPS: u32 = 300_000;

/// SDP 声明的顶档帧率：包络几何 + 最高 bpp 过一遍 level 预算（4K 包络 → 120）
///
/// SDP 只协商一次，声明输入必须覆盖任何实际档位；sanitize 的预算保证实际帧率
/// 不超过这里，因此声明自动跟进、永不低报。
pub fn sdp_ceiling_fps() -> usize {
    clamp_framerate_to_level_budget(MAX_WIDTH, MAX_HEIGHT, MAX_FPS, MAX_BPP, VideoCodec::H264)
        .unwrap_or(MAX_FPS)
}

/// SDP 声明输入的顶档码率：按包络顶档（预算封顶帧率）+ 最高 bpp 用现成码率模型现算，
/// 不设手填常量——码率模型或预算变更时声明自动跟进；且按 codec 分列，避免共用一个值把
/// AV1 抬到其码率模型到不了的更高档
pub fn stream_max_bitrate_kbps(codec: VideoCodec) -> u32 {
    (calculate_dynamic_bitrate(MAX_WIDTH, MAX_HEIGHT, sdp_ceiling_fps(), MAX_BPP, codec) / 1000)
        as u32
}

impl StreamingProfile {
    /// 统一校验与合法化：
    /// 1. 等比缩入 4K 包络（`3840x2160`），只缩不放，宽高比不变（5120×1440 → 3840×1080）
    /// 2. 像素偶数对齐 (& !1)：4:2:0 的硬性要求
    /// 3. 帧率钳到 `1..=MAX_FPS`，任意整数均合法——离散档位已退役；
    ///    level 预算在构造路径按实际 bpp 再压一道（[`clamp_framerate_to_level_budget`]）
    ///
    /// 0×0 不是需要「修正」的非法输入而是**暂停信号**：原样保留、不捏造最小值，
    /// ask 路径必须先行拒绝。
    ///
    /// 这里存的是**显示尺寸**（用户看到的 1080）。编码尺寸由 [`Self::coded_extent`] 派生，
    /// 两者之差由码流里的裁剪窗口抹掉。
    pub fn sanitize_dimensions(width: usize, height: usize, fps: usize) -> (usize, usize, usize) {
        let (w, h) = scale_into_envelope(width, height, MAX_WIDTH, MAX_HEIGHT);
        // 宽高已偶数对齐，这里再补到后端输入要求的并集；0×0 是暂停信号，保持原样
        let aligned = |v: usize, align: usize| {
            if v == 0 { 0 } else { v.next_multiple_of(align) }
        };
        (
            aligned(w, DISPLAY_ALIGN.0),
            aligned(h, DISPLAY_ALIGN.1),
            fps.clamp(1, MAX_FPS),
        )
    }

    /// 编码尺寸：宽高按 `align` 向上对齐
    ///
    /// 显示几何已由 [`Self::sanitize_dimensions`] 按后端输入并集取整，
    /// 软编与 NVENC 传 `(1, 1)` 即得该对齐后的显示尺寸。
    pub fn coded_extent(&self, align: (usize, usize)) -> (usize, usize) {
        let (aw, ah) = (align.0.max(1), align.1.max(1));
        (
            self.width.next_multiple_of(aw),
            self.height.next_multiple_of(ah),
        )
    }

    /// 事实构造（默认档、性能矩阵与用例）：几何过 sanitize，码率兜底非零。
    /// level 预算与码率模型不在这里生效——生产路径一律经 [`TierRequest::derive`]
    pub fn new(width: usize, height: usize, fps: usize, bitrate: usize) -> Self {
        let (w, h, f) = Self::sanitize_dimensions(width, height, fps);
        Self {
            width: w,
            height: h,
            fps: f,
            bitrate: bitrate.max(1),
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn fps(&self) -> usize {
        self.fps
    }

    pub fn bitrate(&self) -> usize {
        self.bitrate
    }

    /// 直写码率（bps）。不变量：非零且落在 u32 范围内——NVENC 侧按裸转换消费
    /// （构造器公式、带宽控制器的 u32 输出天然满足，零值在此兜底为 1）
    pub fn set_bitrate(&mut self, bitrate: usize) {
        self.bitrate = bitrate.max(1);
    }

    /// 直写帧率（钳到 `1..=MAX_FPS`），**不重算码率**——运行期码控改帧率的行为：
    /// level 预算由 ask 路径在源头保证，这里不做二次推演。
    pub fn set_fps(&mut self, fps: usize) {
        self.fps = fps.clamp(1, MAX_FPS);
    }
}

/// 动态综合码率计算：
pub fn calculate_dynamic_bitrate(
    width: usize,
    height: usize,
    fps: usize,
    bpp: f32,
    codec: VideoCodec,
) -> usize {
    if width == 0 || height == 0 || fps == 0 || bpp == 0.0 {
        return 100_000;
    }

    // 1. 基准码率 (bps)
    let base_pixels = (BASE_WIDTH * BASE_HEIGHT) as f32;
    let base_bps = base_pixels * BASE_FPS as f32 * bpp;

    // 2. 分辨率 0.75 次幂法则: (current_pixels / base_pixels) ^ 0.75
    let current_pixels = (width * height) as f32;
    let res_factor = (current_pixels / base_pixels).powf(RESOLUTION_POWER);

    // 3. 帧率系数: 线性比例 fps / BASE_FPS
    let fps_factor = fps as f32 / BASE_FPS as f32;

    // 4. Codec 因子: 相对 H.264 的实时低延迟硬件编码等效画质码率系数
    let codec_factor = match codec {
        VideoCodec::H264 => 1.0,
        VideoCodec::H265 => 0.70,
        VideoCodec::Av1 => 0.60,
    };

    let raw_bps = base_bps * res_factor * fps_factor * codec_factor;

    // 四舍五入到最近的 0.1 Mbps (100_000 bps)，至少 0.1 Mbps
    let rounded = ((raw_bps / 100_000.0).round() * 100_000.0) as usize;
    rounded.max(100_000)
}

/// 等比缩入 `max_w×max_h` 包络：只缩不放，取宽高两边中更紧的约束
///
/// 独立钳制宽高会拉伸变形（5120×1440 各自夹会变 3840×1440），必须按比例整体缩。
/// 用交叉乘比较避免乘法溢出；整数除法向下取整后偶对齐，结果保证不超出包络。
/// 0×0 是暂停信号，原样返回、绝不捏造尺寸。
fn scale_into_envelope(width: usize, height: usize, max_w: usize, max_h: usize) -> (usize, usize) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let (w, h) = if width <= max_w && height <= max_h {
        (width, height)
    } else if width as u64 * max_h as u64 > max_w as u64 * height as u64 {
        // 宽度是更紧的一边：按包络宽缩，高等比跟进
        (max_w, height * max_w / width)
    } else {
        (width * max_h / height, max_h)
    };
    (w & !1, h & !1)
}

/// 把帧率压进公共 level 预算：几何与 bpp 都不动，只降帧率
///
/// MaxFS 看几何、MaxMBPS 看几何×帧率、MaxBR 看该 bpp 下码率模型的实际产出。码率对
/// 帧率单调不减，从上限往下扫到的第一个可行值就是最高可用帧率。预算触发超限时向下
/// 取整到 10 的倍数（4K 包络的 MaxMBPS 上限 128.9 → 120），档位语义可预期；未触顶的
/// 任意整数帧率原样保留。返回 `None` 表示连 1fps 都放不进预算，调用方必须 fail closed，
/// 绝不允许静默回落到旧 level 或低报 SDP。
pub(crate) fn clamp_framerate_to_level_budget(
    width: usize,
    height: usize,
    fps: usize,
    bpp: f32,
    codec: VideoCodec,
) -> Option<usize> {
    if width == 0 || height == 0 || fps == 0 {
        return None;
    }
    // 宏块是 16×16，宽高不是 16 的倍数时向上取整（1080 高 → 68 行而非 67）
    let mb_frame = width.div_ceil(16) as u64 * height.div_ceil(16) as u64;
    if mb_frame > LEVEL_BUDGET_MAX_FS_MB {
        return None;
    }
    let mbps_cap = (LEVEL_BUDGET_MAX_MBPS / mb_frame) as usize;
    let mut fitting = mbps_cap.min(fps);
    while fitting >= 1 {
        let kbps = (calculate_dynamic_bitrate(width, height, fitting, bpp, codec) / 1000) as u32;
        if kbps <= LEVEL_BUDGET_MAX_BR_KBPS {
            break;
        }
        fitting -= 1;
    }
    if fitting == 0 {
        return None;
    }
    if fitting < fps {
        // 预算真的压低了请求帧率：取整到 10 的倍数，档位语义可预期。
        // 只降不升，预算约束必然仍满足；未触顶的任意整数（73、141）原样保留
        fitting = (fitting / 10) * 10;
        if fitting == 0 {
            return None;
        }
    }
    Some(fitting)
}

/// 软编回退顶格：`min(Content Resolution, 1080p) @ 60`
///
/// 只把大内容等比缩进 1080p 包络并压帧率，**绝不把小内容放大**——800×600 保持原样，
/// 抬到 1080p 发的是假细节。
pub(crate) fn clamp_to_software_ceiling(
    width: usize,
    height: usize,
    fps: usize,
) -> (usize, usize, usize) {
    let (w, h) = scale_into_envelope(width, height, 1920, 1080);
    (w, h, fps.clamp(1, 60))
}

/// 非有限值回落到最低档，区间外夹到端点，并量化到 0.01 步长
///
/// 量化步长必须与前端滑杆一致，否则回包值落在两档之间，滑杆就无法回写实际生效值。
pub fn clamp_bpp(bpp: f32) -> f32 {
    if !bpp.is_finite() {
        return MIN_BPP;
    }
    let clamped = bpp.clamp(MIN_BPP, MAX_BPP);
    ((clamped * 100.0).round() / 100.0).clamp(MIN_BPP, MAX_BPP)
}

/// 在升序档位表中选出第一个同时满足三项约束的档位
///
/// 三个编码器的 Level 体系各不相同（H.264 按 16×16 宏块、HEVC/AV1 按亮度样本，IDC 编码
/// 也不同），但判定规则同构：**帧尺寸、帧率×尺寸、码率**三项都不越界。表按档位升序排列，
/// 返回首个匹配项；全部不满足时返回最后一档（表内最大能力），由调用方决定是否接受。
fn first_satisfying<T: Copy>(
    specs: &[(T, u64, u64, u32)],
    frame_units: u64,
    rate_units: u64,
    bitrate_kbps: u32,
) -> T {
    for (idc, max_frame, max_rate, max_br) in specs {
        if frame_units <= *max_frame && rate_units <= *max_rate && bitrate_kbps <= *max_br {
            return *idc;
        }
    }
    specs[specs.len() - 1].0
}

/// 依据分辨率、帧率与码率推导 H.264 Level IDC
///
/// 编码器（NVENC 显式写入 SPS 的 level）与 SDP 的 `profile-level-id` 共用本函数，
/// 避免协商层宣称的 Level 与码流实际 Level 不一致。
///
/// `bitrate_kbps` 单位为 kbit/s。
pub fn h264_level_idc(width: usize, height: usize, fps: usize, bitrate_kbps: u32) -> u8 {
    // (Level IDC, MaxFS 宏块/帧, MaxMBPS 宏块/秒, MaxBR kbps @ High profile)
    // 数值取自 ITU-T H.264 Table A-1。MaxBR 必须取 High 列（= Baseline/Main 的 1.25 倍）
    const LEVELS: [(u8, u64, u64, u32); 9] = [
        (31, 3_600, 108_000, 17_500),
        (32, 5_120, 216_000, 25_000),
        (40, 8_192, 245_760, 25_000),
        (41, 8_192, 245_760, 62_500),
        (42, 8_704, 522_240, 62_500),
        (50, 22_080, 589_824, 168_750),
        (51, 36_864, 983_040, 300_000),
        (52, 36_864, 2_073_600, 300_000),
        (60, 139_264, 4_177_920, 300_000),
    ];

    // 宏块是 16×16，宽高不是 16 的倍数时必须向上取整（1080 高 → 68 行而非 67）
    let mb_frame = width.div_ceil(16) as u64 * height.div_ceil(16) as u64;
    first_satisfying(&LEVELS, mb_frame, mb_frame * fps as u64, bitrate_kbps)
}

/// 依据分辨率、帧率与码率推导 HEVC Level IDC
///
/// RFC 7798 的 `level-id` 就是 `general_level_idc`（30 × level），与写进 SPS 的值同源。
/// `bitrate_kbps` 单位为 kbit/s。
pub fn hevc_level_idc(width: usize, height: usize, fps: usize, bitrate_kbps: u32) -> u8 {
    // (Level IDC, MaxLumaPs 亮度样本/帧, MaxLumaSr 样本/秒, MaxBR kbps @ Main tier)
    // 数值取自 ITU-T H.265 Table A.8。工程使用 Main profile，故取 Main tier 的码率上限；
    // High tier 只在 4 级及以上存在，且需要码流显式声明 tier_flag，本工程不声明。
    const LEVELS: [(u8, u64, u64, u32); 13] = [
        (30, 36_864, 552_960, 128),
        (60, 122_880, 3_686_400, 1_500),
        (63, 245_760, 7_372_800, 3_000),
        (90, 552_960, 16_588_800, 6_000),
        (93, 983_040, 33_177_600, 10_000),
        (120, 2_228_224, 66_846_720, 12_000),
        (123, 2_228_224, 133_693_440, 20_000),
        (150, 8_912_896, 267_386_880, 25_000),
        (153, 8_912_896, 534_773_760, 40_000),
        (156, 8_912_896, 1_069_547_520, 60_000),
        (180, 35_651_584, 1_069_547_520, 60_000),
        (183, 35_651_584, 2_139_095_040, 120_000),
        (186, 35_651_584, 4_278_190_080, 240_000),
    ];

    let luma = width as u64 * height as u64;
    first_satisfying(&LEVELS, luma, luma * fps as u64, bitrate_kbps)
}

/// 依据分辨率、帧率与码率推导 AV1 Level 索引
///
/// RFC 9585 的 `level-idx` 是 AV1 规范 Annex A 的 level 索引（0 = Level 2.0），
/// 与写进序列头 `seq_level_idx` 的取值同源。`bitrate_kbps` 单位为 kbit/s。
pub fn av1_level_idx(width: usize, height: usize, fps: usize, bitrate_kbps: u32) -> u8 {
    // (seq_level_idx, MaxPicSize 样本/帧, MaxDisplayRate 样本/秒, MaxBitrate kbps @ Main profile)
    // 数值取自 AV1 规范 Annex A Table A.3；只列已定义的级别，索引留出规范的空洞
    const LEVELS: [(u8, u64, u64, u32); 14] = [
        (0, 147_456, 4_423_680, 1_500),           // 2.0
        (1, 278_784, 8_363_520, 3_000),           // 2.1
        (4, 665_856, 19_975_680, 6_000),          // 3.0
        (5, 1_065_024, 31_950_720, 10_000),       // 3.1
        (8, 2_359_296, 70_778_880, 12_000),       // 4.0
        (9, 2_359_296, 141_557_760, 20_000),      // 4.1
        (12, 8_912_896, 267_386_880, 30_000),     // 5.0
        (13, 8_912_896, 534_773_760, 40_000),     // 5.1
        (14, 8_912_896, 1_069_547_520, 60_000),   // 5.2
        (15, 8_912_896, 1_069_547_520, 60_000),   // 5.3
        (16, 35_651_584, 1_069_547_520, 60_000),  // 6.0
        (17, 35_651_584, 2_139_095_040, 100_000), // 6.1
        (18, 35_651_584, 4_278_190_080, 160_000), // 6.2
        (19, 35_651_584, 4_278_190_080, 160_000), // 6.3
    ];

    let pic = width as u64 * height as u64;
    first_satisfying(&LEVELS, pic, pic * fps as u64, bitrate_kbps)
}

/// 按内容规格推导某个 codec 的 level —— **SDP 与码流的唯一入口**
///
/// 两个调用方的差别只在输入，不在函数：
/// - **码流**（SPS / VPS / 序列头）用**当前档位** —— 声明这一路流真正需要的 level
/// - **SDP**（fmtp）用「[`MAX_WIDTH`] 包络几何 + [`sdp_ceiling_fps`] 预算封顶帧率 +
///   [`stream_max_bitrate_kbps`] 顶档码率」—— 声明本会话可能达到的最高 level
///
/// 这个分工是硬约束：SDP 只有一次 offer/answer，写死之后改不了；而档位随时可能升。
/// 反过来把码流也按上限声明则是错的（声明远高于实际，浪费且无意义）。
///
/// 不要用 `CodecCaps::max_level_idc`：那是**驱动能力上限**（H.264 常报 6.2），
/// 拿它当码流 level 会让码流比 SDP 声明的高，方向正好反了。
pub fn profile_level(
    codec: VideoCodec,
    width: usize,
    height: usize,
    fps: usize,
    bitrate_kbps: u32,
) -> u32 {
    match codec {
        VideoCodec::H264 => u32::from(h264_level_idc(width, height, fps, bitrate_kbps)),
        VideoCodec::H265 => u32::from(hevc_level_idc(width, height, fps, bitrate_kbps)),
        VideoCodec::Av1 => u32::from(av1_level_idx(width, height, fps, bitrate_kbps)),
    }
}

// NV12 两平面的字节数：Y 平面 `width * height`，交织 UV 平面同面积一半
pub(crate) const fn nv12_len(width: usize, height: usize) -> usize {
    width * height * 3 / 2
}

/// 主机帧（NV12）在给定对齐粒度下的字节数：**编码尺寸** × 1.5
///
/// 跨语言约定：采集侧按编码尺寸出帧（尺寸由编码器回执给出）。生产路径用编码器自己的
/// `extent()`，本函数只供测试造载荷。
#[cfg(test)]
pub(crate) fn host_nv12_len(profile: &StreamingProfile, align: (usize, usize)) -> usize {
    let (width, height) = profile.coded_extent(align);
    nv12_len(width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 几何用例共用的事实构造：码率不在断言范围，占位即可
    fn fact_profile(w: usize, h: usize, fps: usize) -> StreamingProfile {
        StreamingProfile::new(w, h, fps, 4_000_000)
    }

    /// 显示几何按后端**输入**要求的并集向上取整：宽 %4、高 %2
    ///
    /// 回归目标：854×480 原样交给 NVENC 时按 `pitch = width` 登记，854 不是 4 的倍数，
    /// 驱动以 `NV_ENC_ERR_INVALID_PARAM` 拒绝 → 硬编回退软编、刷新后黑屏。
    #[test]
    fn test_sanitize_dimensions_aligns_to_display_input_union() {
        assert_eq!(
            StreamingProfile::sanitize_dimensions(854, 480, 60),
            (856, 480, 60),
            "854 的 pitch 非 4 的倍数，必须补到 856"
        );
        assert_eq!(
            StreamingProfile::sanitize_dimensions(1608, 904, 60),
            (1608, 904, 60),
            "已对齐的档位不得被无谓放大"
        );
        assert_eq!(
            StreamingProfile::sanitize_dimensions(851, 481, 60),
            (852, 480, 60),
            "奇宽奇高先落到偶数再补 4/2 对齐"
        );
        assert_eq!(
            StreamingProfile::sanitize_dimensions(MAX_WIDTH, MAX_HEIGHT, MAX_FPS),
            (MAX_WIDTH, MAX_HEIGHT, MAX_FPS),
            "对齐不得把包络顶档顶出包络"
        );
        assert_eq!(
            StreamingProfile::sanitize_dimensions(0, 0, 60),
            (0, 0, 60),
            "0×0 是暂停信号，不能捏造成 4×2"
        );
    }

    #[test]
    fn test_clamp_bpp_bounds_and_quantizes() {
        assert_eq!(clamp_bpp(0.0), MIN_BPP);
        assert_eq!(clamp_bpp(f32::NAN), MIN_BPP);
        assert_eq!(clamp_bpp(0.5), MAX_BPP);
        assert_eq!(clamp_bpp(0.126), 0.13);
        // 定档请求走同一钳位：越界的 bpp 不会把档位码率推到区间外
        let p = TierRequest {
            width: 1920,
            height: 1080,
            fps: 180,
            bpp: 0.5,
        }
        .derive(VideoCodec::H264);
        assert_eq!(
            p.bitrate(),
            calculate_dynamic_bitrate(1920, 1080, 180, MAX_BPP, VideoCodec::H264)
        );
    }

    #[test]
    fn test_streaming_profile_sanitization_and_clamps() {
        // 1. 奇数宽高应偶数对齐
        let p = fact_profile(1921, 1081, 60);
        assert_eq!(p.width(), 1920);
        assert_eq!(p.height(), 1080);
        assert_eq!(p.fps(), 60);

        // 2. 分辨率没有下限：小窗口按原生尺寸串流，放大发的是假细节
        let tiny = fact_profile(100, 100, 5);
        assert_eq!(tiny.width(), 100);
        assert_eq!(tiny.height(), 100);
        assert_eq!(tiny.fps(), 5);

        // 3. 800×600 原样保留（spec 钉死的用例）
        let small = fact_profile(800, 600, 60);
        assert_eq!((small.width(), small.height(), small.fps()), (800, 600, 60));

        // 4. 超宽内容等比缩入包络，不变形（spec 钉死的用例）
        let ultrawide = fact_profile(5120, 1440, 60);
        assert_eq!(ultrawide.width(), 3840);
        assert_eq!(ultrawide.height(), 1080);
    }

    /// 编码尺寸由显示尺寸派生，粒度决定结果
    ///
    /// 两列粒度都是真实存在的：`(1,1)` 对应 NVENC / 软编（不要求对齐），
    /// `(16,16)` 是多数 Vulkan 驱动的 `pictureAccessGranularity`，
    /// `(64,64)` 是 NVIDIA 在 1440p 上实际声明的值 —— 1440 能被 16 整除却不能被 64
    /// 整除，正是「写死 16 就会错」的那一格。
    #[test]
    fn test_coded_extent_follows_display_size() {
        for (w, h, align, cw, ch) in [
            (1280, 720, (1, 1), 1280, 720),
            (1280, 720, (16, 16), 1280, 720),
            (1280, 720, (64, 64), 1280, 768),
            (1600, 900, (16, 16), 1600, 912),
            (1600, 900, (64, 64), 1600, 960),
            (1920, 1080, (16, 16), 1920, 1088),
            (1920, 1080, (64, 64), 1920, 1088),
            (2560, 1440, (16, 16), 2560, 1440),
            (2560, 1440, (64, 64), 2560, 1472),
        ] {
            let p = fact_profile(w, h, 60);
            assert_eq!(
                (p.width(), p.height()),
                (w, h),
                "{w}x{h} 的显示尺寸不应被改动"
            );
            assert_eq!(
                p.coded_extent(align),
                (cw, ch),
                "{w}x{h} 在粒度 {align:?} 下的编码尺寸对齐错误"
            );
        }
    }

    /// Level 判定必须同时看帧尺寸、帧率与码率三项
    ///
    /// 断言值都取自各自规范的表格，作为「改表即失败」的护栏：这些数字没有运行时反馈，
    /// 只能靠测试把行为钉住。
    #[test]
    fn test_level_selection_covers_three_constraints() {
        // --- H.264 ---
        // 720p30 的宏块率 108_000 恰好等于 Level 3.1 上限
        assert_eq!(h264_level_idc(1280, 720, 30, 10_000), 31);
        // 帧率翻倍后宏块率 216_000 恰好等于 Level 3.2 上限
        assert_eq!(h264_level_idc(1280, 720, 60, 10_000), 32);
        // 1080 不是 16 的倍数，宏块行数必须向上取整到 68，否则会错判为 4.1
        assert_eq!(h264_level_idc(1920, 1080, 60, 20_000), 42);
        // 4.0 与 4.1 的宏块能力相同，只有码率能把两者区分开（30 Mbps > 4.0 的 25 Mbps）
        assert_eq!(h264_level_idc(1280, 720, 60, 30_000), 41);
        // 60 Mbps 仍在 High 列 4.2 的 62.5 Mbps 之内，不该再抬到 5.0
        assert_eq!(h264_level_idc(1920, 1080, 60, 60_000), 42);
        // 1080p240 的宏块率 1_958_400 超出 5.1 的 983_040，但仍在 5.2 的 2_073_600 之内
        assert_eq!(h264_level_idc(1920, 1080, 240, 100_000), 52);

        // --- HEVC（level_idc = 30 × level）---
        // 720p30 落在 Level 3.1 的亮度样本率上限内
        assert_eq!(hevc_level_idc(1280, 720, 30, 10_000), 93);
        // 1080p60 的样本率 124_416_000 超过 Level 4.0 的 66_846_720
        assert_eq!(hevc_level_idc(1920, 1080, 60, 20_000), 123);

        // --- AV1（level-idx 0 = Level 2.0，索引带空洞）---
        assert_eq!(av1_level_idx(1280, 720, 30, 10_000), 5); // 3.1
        assert_eq!(av1_level_idx(1920, 1080, 60, 20_000), 9); // 4.1

        // 回归：结果里不得出现未定义索引 6/7（3.2/3.3）与 10/11（4.2/4.3）。
        // 1080p120 的样本率 248_832_000 超出 4.1 的 141_557_760，应落到 5.0（12）；
        // 落到未定义索引会让 NVENC 直接回 NV_ENC_ERR_INVALID_PARAM。
        assert_eq!(av1_level_idx(1920, 1080, 120, 14_900), 12); // 5.0
        assert_eq!(av1_level_idx(1920, 1080, 180, 22_400), 13); // 5.1
        // 720p60 的样本率 55_296_000 超出 3.1 的 31_950_720，应落到 4.0（8）而非 3.2（6）
        assert_eq!(av1_level_idx(1280, 720, 60, 3_300), 8); // 4.0

        for (w, h, fps, kbps) in [
            (1280, 720, 30, 10_000),
            (1280, 720, 60, 3_300),
            (1920, 1080, 60, 7_500),
            (1920, 1080, 120, 14_900),
            (1920, 1080, 180, 22_400),
            (2560, 1440, 60, 13_300),
        ] {
            let idx = av1_level_idx(w, h, fps, kbps);
            assert!(
                matches!(idx, 0 | 1 | 4 | 5 | 8 | 9 | 12..=19),
                "{w}x{h}@{fps} 算出了 AV1 未定义的 level 索引 {idx}"
            );
        }
    }

    /// 超出表内最大档位时返回最后一档，而不是回绕到最小值
    #[test]
    fn test_level_selection_saturates_at_max() {
        assert_eq!(h264_level_idc(2560, 1440, 240, 240_000), 60);
        assert_eq!(hevc_level_idc(2560, 1440, 240, 240_000), 186);
        assert_eq!(av1_level_idx(2560, 1440, 240, 240_000), 19);
    }

    /// 令牌与显示：信令串只认小写 token（含 AVC/HEVC 别名，空白与大小写容错），
    /// 未知串必须 None；serde 线格式即全小写 token，往返无损
    #[test]
    fn test_video_codec_token_roundtrip() {
        assert_eq!(VideoCodec::from_token("h264"), Some(VideoCodec::H264));
        assert_eq!(
            VideoCodec::from_token(" AVC "),
            Some(VideoCodec::H264),
            "别名与空白容错"
        );
        assert_eq!(VideoCodec::from_token("HEVC"), Some(VideoCodec::H265));
        assert_eq!(VideoCodec::from_token("av1"), Some(VideoCodec::Av1));
        assert_eq!(VideoCodec::from_token("vp9"), None);
        assert_eq!(VideoCodec::from_token(""), None);

        assert_eq!(VideoCodec::H265.as_str(), "H265");
        assert_eq!(VideoCodec::Av1.to_string(), "AV1");

        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            let wire = serde_json::to_string(&codec).unwrap();
            assert_eq!(wire, format!("\"{}\"", codec.as_str().to_lowercase()));
            assert_eq!(serde_json::from_str::<VideoCodec>(&wire).unwrap(), codec);
        }
        assert!(serde_json::from_str::<VideoCodec>("\"mpeg2\"").is_err());
    }

    /// set_fps：任意整数原样保留（离散档位退役），且不重算码率——
    /// 码率归带宽估计器所有，帧率变化不得冲掉
    #[test]
    fn test_set_fps_keeps_bitrate() {
        let base = StreamingProfile::new(1920, 1080, 60, 12_400_000);
        let mut fps_only = base;
        fps_only.set_fps(73);
        assert_eq!(fps_only.fps(), 73, "任意整数帧率不再吸档");
        assert_eq!(fps_only.bitrate(), base.bitrate(), "set_fps 不重算码率");
        fps_only.set_fps(500);
        assert_eq!(fps_only.fps(), MAX_FPS, "帧率上限 240");
    }

    /// 派生口：码率模型按 codec 系数区分；包络缩入、预算压帧与 bpp 钳制在派生内统一生效。
    /// codec 是派生的上下文参数（会话常量），同一请求配不同 codec 各自成立
    #[test]
    fn test_tier_request_derivation() {
        // 同几何不同 codec：H265 系数更低，码率必须下降
        let request = TierRequest {
            width: 1920,
            height: 1080,
            fps: 60,
            bpp: 0.2,
        };
        let h264 = request.derive(VideoCodec::H264);
        let h265 = request.derive(VideoCodec::H265);
        assert!(
            h265.bitrate() < h264.bitrate(),
            "换 H265 后码率必须重算下降"
        );

        // 超大内容等比缩入 4K 包络；240fps 触 MaxMBPS 预算后取整到 120
        let huge = TierRequest {
            width: 7680,
            height: 4320,
            fps: 300,
            bpp: MIN_BPP,
        }
        .derive(VideoCodec::H264);
        assert_eq!((huge.width(), huge.height()), (MAX_WIDTH, MAX_HEIGHT));
        assert_eq!(huge.fps(), 120, "4K 预算压到 120");

        // bpp 越界钳到上限，码率按钳后值现算
        let clamped = TierRequest {
            width: 1920,
            height: 1080,
            fps: 180,
            bpp: 0.5,
        }
        .derive(VideoCodec::H264);
        assert_eq!(
            clamped.bitrate(),
            calculate_dynamic_bitrate(1920, 1080, 180, MAX_BPP, VideoCodec::H264)
        );
    }

    /// 4K 包络下的 level 预算：120 是 16:9 的顶（spec 钉死），几何不动
    #[test]
    fn test_level_budget_lowers_framerate_at_envelope() {
        // 4K120 恰好装进 Level 6.0：MaxMBPS 3,888,000 ≤ 4,177,920
        let p120 = TierRequest {
            width: 3840,
            height: 2160,
            fps: 120,
            bpp: MAX_BPP,
        }
        .derive(VideoCodec::H264);
        assert_eq!(p120.fps(), 120);
        assert_eq!((p120.width(), p120.height()), (3840, 2160));

        // 4K144 / 4K240 无 level 可覆盖：压到 120（MaxMBPS 上限 128.9，触顶取整到 10）
        for requested in [144, 240] {
            let p = TierRequest {
                width: 3840,
                height: 2160,
                fps: requested,
                bpp: MAX_BPP,
            }
            .derive(VideoCodec::H264);
            assert_eq!(p.fps(), 120, "4K{requested} 必须压到 120");
            assert_eq!(
                (p.width(), p.height()),
                (3840, 2160),
                "预算只降帧率，不动几何"
            );
        }

        // 高 bpp 下 MaxBR 也能压帧率：bpp=1.0 时码率模型在 51fps 处越过 300Mbps（→50）
        assert_eq!(
            clamp_framerate_to_level_budget(3840, 2160, 240, 1.0, VideoCodec::H264),
            Some(50)
        );

        // 未触顶的任意整数原样保留；1080p240 的宏块率与码率都在 6.0 之内，预算不生效
        assert_eq!(
            clamp_framerate_to_level_budget(1920, 1080, 73, MIN_BPP, VideoCodec::H264),
            Some(73)
        );
        assert_eq!(
            clamp_framerate_to_level_budget(1920, 1080, 240, MAX_BPP, VideoCodec::H264),
            Some(240)
        );
    }

    /// 预算 fail closed：0 尺寸与超出 MaxFS 的几何直接拒绝，绝不静默回落
    #[test]
    fn test_level_budget_fails_closed() {
        assert_eq!(
            clamp_framerate_to_level_budget(0, 0, 60, MIN_BPP, VideoCodec::H264),
            None
        );
        // 500×500 = 250,000 宏块 > MaxFS 139,264
        assert_eq!(
            clamp_framerate_to_level_budget(8000, 8000, 60, MIN_BPP, VideoCodec::H264),
            None
        );
    }

    /// 软编顶格：min(Content, 1080p) @ 60，只缩不放
    #[test]
    fn test_software_ceiling_clamps() {
        // 4K 缩进 1080p
        assert_eq!(clamp_to_software_ceiling(3840, 2160, 240), (1920, 1080, 60));
        // 小内容不放大
        assert_eq!(clamp_to_software_ceiling(800, 600, 144), (800, 600, 60));
        assert_eq!(clamp_to_software_ceiling(1600, 900, 60), (1600, 900, 60));
        // 超宽等比缩入 1080p 包络
        assert_eq!(clamp_to_software_ceiling(5120, 1440, 120), (1920, 540, 60));
        // 帧率只压不抬
        assert_eq!(clamp_to_software_ceiling(800, 600, 30), (800, 600, 30));
    }

    /// SDP 声明必须 ≥ 任何可达成档位的实际 level
    ///
    /// 声明输入 = 包络几何 + 预算封顶帧率 + 顶档码率；可达域 = 包络内几何 × 预算钳后
    /// 的帧率 × bpp 两端。level 对各输入单调，对可达域逐点扫描即可全覆盖。
    #[test]
    fn test_sdp_declared_level_covers_every_rung() {
        let ceiling = sdp_ceiling_fps();
        assert_eq!(ceiling, 120, "4K 包络 + 最高 bpp 的预算封顶帧率");
        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            let declared = profile_level(
                codec,
                MAX_WIDTH,
                MAX_HEIGHT,
                ceiling,
                stream_max_bitrate_kbps(codec),
            );
            // 顶档真实 level：H.264 6.0（idc 60）、HEVC 6.2（idc 186）、AV1 6.2（idx 18）
            // —— 码率维度按 bpp 顶格，顶档码率 HEVC 147.8M / AV1 126.7M 越过 6.1 桶
            let expected = match codec {
                VideoCodec::H264 => 60,
                VideoCodec::H265 => 186,
                VideoCodec::Av1 => 18,
            };
            assert_eq!(
                declared, expected,
                "SDP 声明档位漂移，检查包络顶档与码率模型"
            );
            for (raw_w, raw_h) in [
                (800, 600),
                (1920, 1080),
                (2560, 1440),
                (3840, 2160),
                (5120, 1440),
                (7680, 4320),
            ] {
                for &bpp in [MIN_BPP, MAX_BPP].iter() {
                    for &fps in [30usize, 60, 73, 120, 144, 240].iter() {
                        let (w, h, f) = StreamingProfile::sanitize_dimensions(raw_w, raw_h, fps);
                        let Some(budgeted) = clamp_framerate_to_level_budget(w, h, f, bpp, codec)
                        else {
                            panic!("包络内 {w}x{h}@{f} 必须有预算解");
                        };
                        let bitrate = calculate_dynamic_bitrate(w, h, budgeted, bpp, codec);
                        let actual = profile_level(codec, w, h, budgeted, (bitrate / 1000) as u32);
                        assert!(
                            actual <= declared,
                            "codec={codec:?} {w}x{h}@{budgeted} bpp={bpp} 实际 level {actual} 超过 SDP 声明 {declared}"
                        );
                    }
                }
            }
        }
    }
}

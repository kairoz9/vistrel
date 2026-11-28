//! codec 专属的码控：pNext 结构、量化范围、GOP 剩余帧数与运行期状态机

use ash::vk::{self, TaggedStructure};

/// 本帧在 GOP 内的剩余帧数（含当前帧，IDR 为 0）
///
/// `gopRemainingI/P/B` 表示「执行本次编码前码控应假设 GOP 还剩多少帧」。本实现
/// 是 IPPP：IDR 占 I，其余全是 P，B 恒 0；`frame_num` 在 IDR 上为 0，其余落在
/// 1..=gop_length-1。
pub(in crate::encode::hardware::vulkan) fn gop_remaining_counts(
    gop_length: u32,
    frame_num: u32,
    is_key: bool,
) -> (u32, u32) {
    let remaining_i = u32::from(is_key);
    let remaining_p = gop_length
        .saturating_sub(frame_num)
        .saturating_sub(u32::from(is_key));
    (remaining_i, remaining_p)
}

/// 码控的量化范围：H.26x 用 QP，AV1 用量化索引，两者数值域不同
#[derive(Debug, Clone, Copy)]
pub(in crate::encode::hardware::vulkan) enum QuantRange {
    Qp { min: i32, max: i32 },
    QIndex { min: u32, max: u32 },
}

impl QuantRange {
    /// 只由 H.264/H.265 路径调用：码控分派按 codec 走，AV1 不进这里
    pub(in crate::encode::hardware::vulkan) fn qp_range(&self, floor: i32) -> (i32, i32) {
        let (min, max) = match self {
            Self::Qp { min, max } => (*min, *max),
            Self::QIndex { .. } => unreachable!("AV1 的量化索引范围不适用 QP 门禁"),
        };
        let lo = min.min(max);
        let hi = min.max(max);
        let min = floor.clamp(lo, hi);
        let max = 51.min(hi).max(min);
        (min, max)
    }

    /// 只由 AV1 路径调用
    pub(in crate::encode::hardware::vulkan) fn q_index_range(&self) -> (u32, u32) {
        match self {
            Self::QIndex { min, max } => (*min, *max),
            Self::Qp { .. } => unreachable!("H.26x 的 QP 范围不适用 AV1 量化索引"),
        }
    }
}

/// 码控的静态上限：驱动码率上限与量化范围
#[derive(Debug, Clone, Copy)]
pub(in crate::encode::hardware::vulkan) struct RateControlLimits {
    max_bitrate: u64,
    quant: QuantRange,
    vbv_ms: u32,
}

impl RateControlLimits {
    pub(in crate::encode::hardware::vulkan) fn new(
        max_bitrate: u64,
        quant: QuantRange,
        vbv_ms: u32,
    ) -> Self {
        Self {
            max_bitrate,
            quant,
            vbv_ms,
        }
    }

    pub(in crate::encode::hardware::vulkan) fn max_bitrate(&self) -> u64 {
        self.max_bitrate
    }

    pub(in crate::encode::hardware::vulkan) fn quant(&self) -> QuantRange {
        self.quant
    }

    pub(in crate::encode::hardware::vulkan) fn vbv_ms(&self) -> u32 {
        self.vbv_ms
    }

    /// 0 表示驱动未设上限
    pub(in crate::encode::hardware::vulkan) fn clamp_bitrate(&self, bps: u64) -> u64 {
        if self.max_bitrate > 0 && bps > self.max_bitrate {
            self.max_bitrate
        } else {
            bps
        }
    }
}

/// 上一次真正配置进会话的码控值
///
/// begin 的 `VkVideoEncodeRateControlInfoKHR` 必须与已配置状态逐字段一致（08254），
/// 因此单独留存；当前值在作用域内再配进去。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::encode::hardware::vulkan) struct AppliedRateControl {
    pub bitrate_bps: u64,
    pub fps: u32,
}

/// 运行期码控状态
#[derive(Debug)]
pub(in crate::encode::hardware::vulkan) struct CodingControl {
    limits: RateControlLimits,
    bitrate_bps: u64,
    fps: u32,
    applied: Option<AppliedRateControl>,
    dirty: bool,
}

impl CodingControl {
    pub(in crate::encode::hardware::vulkan) fn new(
        limits: RateControlLimits,
        requested_bitrate: u64,
        fps: u32,
    ) -> Self {
        Self {
            limits,
            bitrate_bps: limits.clamp_bitrate(requested_bitrate),
            fps,
            applied: None,
            dirty: true,
        }
    }

    pub fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    pub(in crate::encode::hardware::vulkan) fn vbv_ms(&self) -> u32 {
        self.limits.vbv_ms()
    }

    pub fn qp_range(&self, floor: i32) -> (i32, i32) {
        self.limits.quant().qp_range(floor)
    }

    pub fn q_index_range(&self) -> (u32, u32) {
        self.limits.quant().q_index_range()
    }

    /// 运行期改码率：下一帧随码控命令重发，不需要重建会话
    ///
    /// 先钳制再比较：请求超出驱动上限、但落值等于当前值时不算变更，不重发码控。
    pub fn set_bitrate(&mut self, bps: u64) {
        if bps == 0 {
            return;
        }
        let bps = self.limits.clamp_bitrate(bps);
        if bps == self.bitrate_bps {
            return;
        }
        self.bitrate_bps = bps;
        self.dirty = true;
    }

    /// 运行期改帧率：码控每帧都带 `frame_rate_numerator`，改字段 + 置脏即可生效
    pub fn set_fps(&mut self, fps: u32) {
        if fps == 0 || fps == self.fps {
            return;
        }
        self.fps = fps;
        self.dirty = true;
    }

    /// 首帧或码率/帧率变更后需要重新下发码控
    pub fn needs_config(&self) -> bool {
        self.dirty || self.applied.is_none()
    }

    pub fn has_applied(&self) -> bool {
        self.applied.is_some()
    }

    pub fn applied(&self) -> Option<AppliedRateControl> {
        self.applied
    }

    /// 编码成功后记录本次真正配置进会话的值
    pub fn mark_applied(&mut self) {
        self.applied = Some(AppliedRateControl {
            bitrate_bps: self.bitrate_bps,
            fps: self.fps,
        });
        self.dirty = false;
    }
}

/// begin / coding control 的 pNext 上要挂的 codec 专属码控结构
///
/// **它是 `VkVideoEncodeRateControlInfoKHR` 的兄弟，不是子节点。** ash 的 `Extends`
/// 实现由 vk.xml 的 pNext 合法列表生成，只为它生成了 `VideoBeginCodingInfoKHR` /
/// `VideoCodingControlInfoKHR` 两个父类型。
///
/// 存**按值**的副本而不是 `&mut`：begin 与 coding control 两处都要挂它，而 ash 的 `push`
/// 要 `&mut`；按值存一份就能各拷一份，避开同一结构被两处同时可变借用。三个变体类型不同，
/// 只能用枚举统一。
#[derive(Clone, Copy)]
pub(in crate::encode::hardware::vulkan) enum CodecRateControl<'a> {
    H264(vk::VideoEncodeH264RateControlInfoKHR<'a>),
    H265(vk::VideoEncodeH265RateControlInfoKHR<'a>),
    Av1(vk::VideoEncodeAV1RateControlInfoKHR<'a>),
}

/// begin 时告诉码控「当前 GOP 还剩多少 I/P/B 帧」
///
/// `requiresGopRemainingFrames` 时必须在 begin 的 pNext 里带上（VUID 08256 / 10282），
/// `prefersGopRemainingFrames` 时能让 CBR 分配更准。三个结构都是
/// `VideoBeginCodingInfoKHR` 的直接扩展，与码控结构是兄弟。
///
/// 按值保存：三处 begin 各拷一份，避开同一结构被同时可变借用。
#[derive(Clone, Copy)]
pub(in crate::encode::hardware::vulkan) enum CodecGopRemaining<'a> {
    H264(vk::VideoEncodeH264GopRemainingFrameInfoKHR<'a>),
    H265(vk::VideoEncodeH265GopRemainingFrameInfoKHR<'a>),
    Av1(vk::VideoEncodeAV1GopRemainingFrameInfoKHR<'a>),
}

impl<'a> CodecRateControl<'a> {
    /// 把码控结构挂进 `vkCmdBeginVideoCodingKHR` 的 pNext 链
    pub(in crate::encode::hardware::vulkan) fn push_begin<'b>(
        &'b mut self,
        info: vk::VideoBeginCodingInfoKHR<'b>,
    ) -> vk::VideoBeginCodingInfoKHR<'b> {
        match self {
            Self::H264(caps) => info.push(caps),
            Self::H265(caps) => info.push(caps),
            Self::Av1(caps) => info.push(caps),
        }
    }

    /// 把码控结构挂进 `vkCmdControlVideoCodingKHR` 的 pNext 链
    pub(in crate::encode::hardware::vulkan) fn push_control<'b>(
        &'b mut self,
        info: vk::VideoCodingControlInfoKHR<'b>,
    ) -> vk::VideoCodingControlInfoKHR<'b> {
        match self {
            Self::H264(caps) => info.push(caps),
            Self::H265(caps) => info.push(caps),
            Self::Av1(caps) => info.push(caps),
        }
    }
}

impl<'a> CodecGopRemaining<'a> {
    /// 把 GOP 剩余帧数结构挂进 begin 的 pNext 链
    pub(in crate::encode::hardware::vulkan) fn push_begin<'b>(
        &'b mut self,
        info: vk::VideoBeginCodingInfoKHR<'b>,
    ) -> vk::VideoBeginCodingInfoKHR<'b> {
        match self {
            Self::H264(caps) => info.push(caps),
            Self::H265(caps) => info.push(caps),
            Self::Av1(caps) => info.push(caps),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GOP 剩余帧数：含当前帧，IDR 占 I，其余全是 P，B 恒 0
    #[test]
    fn test_gop_remaining_counts() {
        // 30 帧 GOP：IDR + 29 个 P
        assert_eq!(gop_remaining_counts(30, 0, true), (1, 29));
        assert_eq!(gop_remaining_counts(30, 1, false), (0, 29));
        assert_eq!(gop_remaining_counts(30, 2, false), (0, 28));
        assert_eq!(gop_remaining_counts(30, 29, false), (0, 1));
        // 最短 GOP：IDR + 1 个 P
        assert_eq!(gop_remaining_counts(2, 0, true), (1, 1));
        assert_eq!(gop_remaining_counts(2, 1, false), (0, 1));
    }
}

//! H.264 / H.265 共用的专属能力与 frame_num 位宽

use crate::encode::VideoCodec;

/// 让一个 GOP 内的 `frame_num` 不重复所需的最小位宽（`log2(MaxFrameNum)`）
pub(in crate::encode::hardware::vulkan) fn frame_num_bits(gop_length: u32) -> u32 {
    let needed = gop_length.saturating_add(1).max(16);
    // ceil_log2：needed 不是 2 的幂时也要装得下
    let bits = 32 - needed.saturating_sub(1).leading_zeros();
    bits.clamp(4, 12)
}

/// H.264 / H.265 共用的专属能力
#[derive(Debug, Clone, Default)]
pub(in crate::encode::hardware::vulkan) struct H26xCaps {
    /// 驱动支持的最高 level（std 枚举值）；码流 level 按档位推导，换算后不得高于它
    pub(in crate::encode::hardware::vulkan) max_level_idc: u32,
    /// H.26x 的 QP 范围
    pub(in crate::encode::hardware::vulkan) min_qp: i32,
    pub(in crate::encode::hardware::vulkan) max_qp: i32,
    /// P 帧 L0 参考上限；0 表示不能编 P 帧
    pub(in crate::encode::hardware::vulkan) max_p_picture_l0_references: u32,
    /// 单帧最大 slice 数
    pub(in crate::encode::hardware::vulkan) max_slice_count: u32,
    /// 驱动是否要求 begin 时给出 GOP 剩余帧数
    pub(in crate::encode::hardware::vulkan) requires_gop_remaining_frames: bool,
    /// 驱动是否偏好 begin 时给出 GOP 剩余帧数
    pub(in crate::encode::hardware::vulkan) prefers_gop_remaining_frames: bool,
}

impl H26xCaps {
    /// 本实现只发 IPPP：P 帧必须有 L0 参考，单 slice 必须被驱动接受
    pub(in crate::encode::hardware::vulkan) fn validate(
        &self,
        codec: VideoCodec,
    ) -> Result<(), String> {
        if self.max_p_picture_l0_references == 0 {
            return Err(format!(
                "{codec:?} 驱动不支持 P 帧 L0 参考（maxPPictureL0ReferenceCount=0）"
            ));
        }
        if self.max_slice_count == 0 {
            return Err(format!("{codec:?} 驱动不支持任何 slice"));
        }
        Ok(())
    }

    /// 本实现只需当前帧 + 1 参考帧
    pub(in crate::encode::hardware::vulkan) fn dpb_limits(
        &self,
        max_slots: u32,
        max_active: u32,
    ) -> (u32, u32) {
        (max_slots.min(2), max_active.min(1))
    }

    /// 驱动是否要求或偏好 begin 时给出 GOP 剩余帧数
    pub(in crate::encode::hardware::vulkan) fn wants_gop_remaining_frames(&self) -> bool {
        self.requires_gop_remaining_frames || self.prefers_gop_remaining_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `frame_num` 位宽换算：装得下 `gop_length + 1` 个不重复值
    ///
    /// 位宽算小了，参考链会在 GOP 中途断掉，而且不报错。
    #[test]
    fn test_frame_num_bits() {
        assert_eq!(frame_num_bits(2), 4, "MaxFrameNum 至少 16");
        assert_eq!(frame_num_bits(15), 4, "16 个值正好 4 位");
        assert_eq!(frame_num_bits(16), 5, "17 个值需要 32");
        assert_eq!(frame_num_bits(300), 9, "301 个值需要 512");
        assert_eq!(frame_num_bits(4095), 12, "4096 个值正好 12 位");
        // 12 位是上限：更长的 GOP 会截断到 12，`MaxFrameNum` 仍是 4096，
        // 由 `is_idr` 的 `frame_num >= gop_length` 分支保证到点出 IDR
        assert_eq!(frame_num_bits(65_535), 12, "超过 12 位一律截断");
        assert_eq!(frame_num_bits(65_536), 12, "超出一位也截断到 12");
        assert_eq!(frame_num_bits(u32::MAX), 12, "上限截断，不溢出");
    }
}

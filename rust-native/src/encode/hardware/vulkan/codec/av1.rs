//! AV1 编码模块
//!
//! - `caps` / `device`：能力探测与 `videoEncodeAV1` 设备特性
//! - `sequence`：序列头与会话参数装配
//! - `frame`：每帧参数与编码命令装配
//! - `ivf`：IVF 落盘

use ash::vk;

use self::caps::Av1Caps;
pub(in crate::encode::hardware::vulkan) mod caps;
pub(in crate::encode::hardware::vulkan) mod device;
mod frame;
pub(in crate::encode::hardware::vulkan) mod ivf;
mod sequence;
#[cfg(test)]
mod tests;

/// AV1 的静态序列头参数
#[derive(Debug, Clone, Copy)]
pub(in crate::encode::hardware::vulkan) struct SequenceConfig {
    pub(in crate::encode::hardware::vulkan) level_idx: u32,
    pub(in crate::encode::hardware::vulkan) superblock_128: bool,
    pub(in crate::encode::hardware::vulkan) operating_point_count: u32,
}

impl SequenceConfig {
    pub(in crate::encode::hardware::vulkan) fn from_caps(level_idx: u32, caps: &Av1Caps) -> Self {
        Self {
            level_idx,
            superblock_128: caps.av1_use_128x128_superblock(),
            operating_point_count: u32::from(caps.max_operating_points > 0),
        }
    }
}

/// 本 codec 的 Vulkan profile 子结构
///
/// Main profile（0）：4:2:0 8bit，与本模块序列头一致。
pub(in crate::encode::hardware::vulkan) fn profile_info()
-> vk::VideoEncodeAV1ProfileInfoKHR<'static> {
    vk::VideoEncodeAV1ProfileInfoKHR::default().std_profile(0)
}

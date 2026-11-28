//! H.264 编码模块
//!
//! - `caps`：能力探测与校验
//! - `params`：SPS / PPS / VUI 与会话参数装配
//! - `frame`：每帧参数与编码命令装配

use ash::vk;

pub(in crate::encode::hardware::vulkan) mod caps;
mod frame;
mod params;
#[cfg(test)]
mod tests;

pub(in crate::encode::hardware::vulkan) use self::params::level_to_vk;

/// H.264 的静态序列头参数
#[derive(Debug, Clone, Copy)]
pub(in crate::encode::hardware::vulkan) struct SequenceConfig {
    pub(in crate::encode::hardware::vulkan) level_idc: u32,
    pub(in crate::encode::hardware::vulkan) frame_num_bits: u32,
}

impl SequenceConfig {
    pub(in crate::encode::hardware::vulkan) fn new(level_idc: u32, frame_num_bits: u32) -> Self {
        Self {
            level_idc,
            frame_num_bits,
        }
    }
}

/// H.264 的逐帧状态
#[derive(Debug, Clone, Copy)]
pub(in crate::encode::hardware::vulkan) struct State {
    idr_pic_id: u16,
    last_was_idr: bool,
}

impl State {
    pub(in crate::encode::hardware::vulkan) fn new() -> Self {
        // 首帧 IDR 会先 `^= 1` 变成 1，与「初始 0」也不同，两个方向都安全
        Self {
            idr_pic_id: 0,
            last_was_idr: false,
        }
    }

    /// 上一帧是否为 IDR
    ///
    /// 参考槽里装的是**上一帧**，它的 `primary_pic_type` 必须反映那一帧的实际类型。
    /// 用当前帧的 `is_idr` 去填是错的：IDR 之后的第一帧参考的是 IDR，填成 P 会让驱动
    /// 按 P 帧去管理这个参考槽（H.264 里 IDR 会清空 DPB 并重置 POC，与 P 帧行为不同）。
    pub(in crate::encode::hardware::vulkan) fn last_was_idr(&self) -> bool {
        self.last_was_idr
    }

    /// 下一个 IDR 的 `idr_pic_id`
    ///
    /// H.264 7.4.3 要求**相邻两个 IDR 的 `idr_pic_id` 必须不同**。固定为 0 会让解码器
    /// 把第二个及之后的 IDR 当成「同一张图」丢掉，于是该恢复的时候恢复不了——这是花屏
    /// 长期不消失的直接原因，与 GOP 取多大无关。只有两个连续值需要互异，交替 0/1 就够。
    pub(in crate::encode::hardware::vulkan) fn next_idr_pic_id(&mut self) -> u16 {
        self.idr_pic_id ^= 1;
        self.idr_pic_id
    }

    pub(in crate::encode::hardware::vulkan) fn finish_frame(&mut self, is_idr: bool) {
        self.last_was_idr = is_idr;
    }
}

/// 本 codec 的 Vulkan profile 子结构
///
/// High profile（100）：与 NVENC 侧一致的通用取值。
pub(in crate::encode::hardware::vulkan) fn profile_info()
-> vk::VideoEncodeH264ProfileInfoKHR<'static> {
    vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(100)
}

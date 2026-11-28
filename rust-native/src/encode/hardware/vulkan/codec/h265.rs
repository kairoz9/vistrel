//! H.265 编码模块

use ash::vk;

pub(in crate::encode::hardware::vulkan) mod caps;
pub(in crate::encode::hardware::vulkan) mod encoder;

/// H.265 的静态序列头参数
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

/// H.265 的逐帧状态
#[derive(Debug, Clone, Copy)]
pub(in crate::encode::hardware::vulkan) struct State {
    last_was_idr: bool,
}

impl State {
    pub(in crate::encode::hardware::vulkan) fn new() -> Self {
        // 首帧一定是 IDR，但它不挂参考槽，这个初值只会影响「首帧之后」的判断
        Self {
            last_was_idr: false,
        }
    }

    /// 上一帧是否为 IDR
    ///
    /// 参考槽里装的是**上一帧**，它的 `primary_pic_type` 必须反映那一帧的实际类型。
    /// 用当前帧的 `is_idr` 去填是错的：IDR 之后的第一帧参考的是 IDR，填成 P 会让驱动
    /// 按 P 帧去管理这个参考槽。
    pub(in crate::encode::hardware::vulkan) fn last_was_idr(&self) -> bool {
        self.last_was_idr
    }

    pub(in crate::encode::hardware::vulkan) fn finish_frame(&mut self, is_idr: bool) {
        self.last_was_idr = is_idr;
    }
}

/// 本 codec 的 Vulkan profile 子结构
///
/// Main profile（1）。这个 ash 版本的 profile info 没有 level 字段。
pub(in crate::encode::hardware::vulkan) fn profile_info()
-> vk::VideoEncodeH265ProfileInfoKHR<'static> {
    vk::VideoEncodeH265ProfileInfoKHR::default().std_profile_idc(1)
}

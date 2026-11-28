//! AV1 编码的 device feature 位

use ash::vk::{self, TaggedStructure};

/// `videoEncodeAV1` 的查询 / 启用载体
///
/// 只声明 `videoCodecOperation = ENCODE_AV1` 而不开 `videoEncodeAV1`，驱动对这条路径的
/// 行为是**未定义的**（VUID-VkVideoSessionCreateInfoKHR-pVideoProfile-10269，以及
/// VkBufferCreateInfo / VkImageCreateInfo / VkQueryPoolCreateInfo 上同族三条）。
///
/// 结构体必须存活到 `vkCreateDevice` 返回，因此由调用方持有；查询后只保留驱动真正支持
/// 的那一位，避免顺手启用一堆没验证过的特性。
pub(in crate::encode::hardware::vulkan) struct Features {
    features: vk::PhysicalDeviceVideoEncodeAV1FeaturesKHR<'static>,
    supported: bool,
}

impl Features {
    pub(in crate::encode::hardware::vulkan) fn new() -> Self {
        Self {
            features: vk::PhysicalDeviceVideoEncodeAV1FeaturesKHR::default(),
            supported: false,
        }
    }

    /// 把 feature 结构挂进物理设备查询链
    pub(in crate::encode::hardware::vulkan) fn push_query<'a>(
        &'a mut self,
        chain: vk::PhysicalDeviceFeatures2<'a>,
    ) -> vk::PhysicalDeviceFeatures2<'a> {
        chain.push(&mut self.features)
    }

    /// 查询后的收敛：缺 feature 位只告警，不阻断
    ///
    /// 上层看到告警后可自行降级 NVENC；静默继续就是「已知的未知行为」。
    pub(in crate::encode::hardware::vulkan) fn finish(&mut self) {
        self.supported = self.features.video_encode_av1 == vk::TRUE;
        if !self.supported {
            log::warn!("设备未声明 AV1 编码 feature 位，Vulkan AV1 编码行为未定义");
        }
        self.features = vk::PhysicalDeviceVideoEncodeAV1FeaturesKHR::default();
        if self.supported {
            self.features.video_encode_av1 = vk::TRUE;
        }
    }

    /// 把已确认支持的 feature 位挂进设备创建链；不支持时原样返回
    pub(in crate::encode::hardware::vulkan) fn push_enable<'a>(
        &'a mut self,
        info: vk::DeviceCreateInfo<'a>,
    ) -> vk::DeviceCreateInfo<'a> {
        if self.supported {
            info.push(&mut self.features)
        } else {
            info
        }
    }
}

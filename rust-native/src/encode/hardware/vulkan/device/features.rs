//! 设备扩展与 push descriptor 方案探测

use ash::vk::{self, TaggedStructure};

use crate::encode::hardware::vulkan::context::VulkanInstance;

/// 显存直通所需的外部内存/信号量扩展（平台相关）
///
/// 这些是**可选**扩展：缺了只是没有显存直通，纯 CPU 上传路径照常工作。
/// 不能因为某台机器缺一个可选扩展就整个后端不可用。
#[cfg(target_os = "linux")]
pub(super) const EXTERNAL_MEMORY_EXTENSIONS: &[&str] = &[
    // `VK_KHR_external_memory` / `VK_KHR_external_semaphore` 本身是 1.1 核心，
    // 不需要再启用扩展名；平台专属的 `_fd` 形态没有核心版本，仍是扩展
    "VK_KHR_external_memory_fd",
    "VK_KHR_external_semaphore_fd",
];

#[cfg(target_os = "windows")]
pub(super) const EXTERNAL_MEMORY_EXTENSIONS: &[&str] = &[
    "VK_KHR_external_memory_win32",
    "VK_KHR_external_semaphore_win32",
];

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(super) const EXTERNAL_MEMORY_EXTENSIONS: &[&str] = &[];

/// Intra-Refresh 扩展
///
/// 只声明扩展还不够：真正的准入是 `VkPhysicalDeviceVideoEncodeIntraRefreshFeaturesKHR`
/// 的 feature 位。它和 1.2/1.3 的核心 feature 一样，**建设备之前**就能对物理设备查，
/// 而且必须在 `vkCreateDevice` 时链进 pNext 才算「开启」—— 物理设备支持不等于逻辑设备
/// 启用，只查不链的话后续下发 `VkVideoEncodeIntraRefreshInfoKHR` 是非法用法。
/// 不支持就必须 fail-closed，不能靠扩展名猜。
pub(super) const INTRA_REFRESH_EXTENSION: &str = "VK_KHR_video_encode_intra_refresh";

/// push descriptor 的扩展名
///
/// Vulkan 1.4 把它提升为核心，但**可选**：设备版本不到 1.4 时必须先确认驱动声明了它，
/// 否则塞进 `enabled_extension_names` 会让 `vkCreateDevice` 直接返回
/// `VK_ERROR_EXTENSION_NOT_PRESENT`——那是整个后端不可用，不是「CSC 降级」。
pub(super) const PUSH_DESCRIPTOR_EXTENSION: &str = "VK_KHR_push_descriptor";

/// 建设备**之前**就能确定的 push descriptor 通路与上限
#[derive(Debug, Clone, Copy)]
pub(super) enum PushDescriptorPlan {
    /// 设备版本 >= 1.4 且 `pushDescriptor` feature 位为真，走核心命令
    Core(u32),
    /// 设备不到 1.4 但声明了 `VK_KHR_push_descriptor`，走扩展表
    Khr(u32),
    /// 两条都不成立：没有 GPU 色彩转换，退回 CPU 换算
    Unavailable,
}

/// push descriptor 的运行时通路
///
/// 两条通路的参数与行为完全一致，差别只在入口名与「能力从哪来」。
pub(super) enum PushDescriptor {
    /// `vkCmdPushDescriptorSet`（Vulkan 1.4 核心）
    Core,
    /// `vkCmdPushDescriptorSetKHR`（扩展已显式启用）
    Khr(ash::khr::push_descriptor::Device),
    /// 两条通路都不可用
    Unavailable,
}

/// 决定走哪条 push descriptor 通路
///
/// 版本判定必须用 [`VulkanInstance::device_api_version`]——它是 min(应用声明版本,
/// 物理设备版本)。用物理设备的原始 `api_version` 会在「旧 loader + 新驱动」的机器上
/// 判定成 1.4，而应用实际只声明到 1.3，链 1.4 的 feature 结构体属于超版本使用。
pub(super) fn plan_push_descriptor(instance: &VulkanInstance) -> PushDescriptorPlan {
    let pd = instance.physical_device();

    if instance.device_api_version() >= vk::API_VERSION_1_4 {
        let mut features = vk::PhysicalDeviceVulkan14Features::default();
        {
            // 内层作用域：`push` 可变借走 features，查询结束后借用必须先释放才能读回字段
            let mut features2 = vk::PhysicalDeviceFeatures2::default().push(&mut features);
            // SAFETY: 纯查询，features2 及其 pNext 链在调用期间存活
            unsafe {
                instance
                    .instance()
                    .get_physical_device_features2(pd, &mut features2);
            }
        }
        if features.push_descriptor != vk::TRUE {
            // 1.4 设备按理都支持，但能力位是唯一可信来源，不按版本号推断
            return PushDescriptorPlan::Unavailable;
        }

        let mut props = vk::PhysicalDeviceVulkan14Properties::default();
        {
            let mut props2 = vk::PhysicalDeviceProperties2::default().push(&mut props);
            // SAFETY: 纯查询，props2 及其 pNext 链在调用期间存活
            unsafe {
                instance
                    .instance()
                    .get_physical_device_properties2(pd, &mut props2);
            }
        }
        return PushDescriptorPlan::Core(props.max_push_descriptors);
    }

    if !instance.supports_device_extension(PUSH_DESCRIPTOR_EXTENSION) {
        return PushDescriptorPlan::Unavailable;
    }
    let mut props = vk::PhysicalDevicePushDescriptorPropertiesKHR::default();
    {
        let mut props2 = vk::PhysicalDeviceProperties2::default().push(&mut props);
        // SAFETY: 纯查询，props2 及其 pNext 链在调用期间存活
        unsafe {
            instance
                .instance()
                .get_physical_device_properties2(pd, &mut props2);
        }
    }
    PushDescriptorPlan::Khr(props.max_push_descriptors)
}

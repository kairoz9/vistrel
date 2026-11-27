//! 零 SDK 依赖的 Vulkan 实例与物理设备选择 (Vulkan Instance Bootstrap)
//!
//! 只做探测与设备创建所需的公共底座：动态加载系统 loader、创建 instance、挑物理设备、
//! 快照每个队列族的能力。**不创建 `VkDevice`**——那是 `device` 模块的事。
//!
//! 队列族信息按族逐条保留，不合并成「一个视频族」。异构硬件上 H.264 与 AV1 可能落在
//! 不同族，任何「第一个编码族」的取法都是对某一块卡的过拟合。

use std::ffi::{CString, c_char};
use std::sync::Arc;

// `push` 是 `TaggedStructure` trait 的方法，不导入就无法链 pNext
use ash::vk::{self, TaggedStructure};

/// 需要向 instance 申请的扩展（均为设备级外部内存/信号量扩展的实例级伴生项）
const INSTANCE_EXTENSIONS: [&str; 2] = [
    "VK_KHR_external_memory_capabilities",
    "VK_KHR_external_semaphore_capabilities",
];

/// Khronos 校验层
///
/// **只在 debug 构建启用**：它把每条命令逐项对照规范核一遍，开销不能带进 release。
/// 层或扩展不存在时静默跳过——调试设施缺失不该让编码器起不来。
const VALIDATION_LAYER: &str = "VK_LAYER_KHRONOS_validation";
/// 校验层的消息通道
///
/// 没有它只能吃校验层默认的 stderr 输出，格式不可控、也进不了 `log`。
const DEBUG_UTILS_EXTENSION: &str = "VK_EXT_debug_utils";

/// 本后端要求的 Vulkan 版本下限
///
/// 三条硬依赖都在这个版本之下：
/// - `VK_KHR_synchronization2`（1.3 核心）：**所有**图像屏障都走 `vkCmdPipelineBarrier2`
/// - `VK_KHR_timeline_semaphore`（1.2 核心）：显存槽位的跨进程同步
/// - `vkGetPhysicalDeviceQueueFamilyProperties2`（1.1 核心）：队列族视频能力快照
///
/// 提供硬件视频编码的驱动都在 1.3 之上，这条下限不缩小实际可用的硬件面。
pub const MIN_API_VERSION: u32 = vk::API_VERSION_1_3;

/// 单个队列族的能力快照
#[derive(Debug, Clone, Copy)]
pub struct QueueFamilyInfo {
    pub index: u32,
    pub flags: vk::QueueFlags,
    /// 该族声明支持的视频编解码操作；非视频族为空
    pub video_codec_operations: vk::VideoCodecOperationFlagsKHR,
    /// 该族时间戳的有效位数（`timestampValidBits`）；0 表示该族不能写时间戳
    ///
    /// 只在测试构建里采集：GPU 时间戳探针是性能剖析设施，生产路径不发查询。
    #[cfg(test)]
    pub timestamp_valid_bits: u32,
}

impl QueueFamilyInfo {
    /// 该族能否用于视频编码（必须有编码标志，且至少声明一种编码 codec）
    pub fn supports_video_encode(&self) -> bool {
        self.flags.contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
            && !self.video_codec_operations.is_empty()
    }

    /// 该族是否支持指定的编码 codec
    pub fn supports_encode_of(&self, operation: vk::VideoCodecOperationFlagsKHR) -> bool {
        self.supports_video_encode() && self.video_codec_operations.contains(operation)
    }
}

/// 探测用的 Vulkan 实例与所选物理设备
///
/// 句柄本体放在 `Arc<InstanceInner>` 里，本结构只是共享句柄。**实例必须活得比设备久**，
/// 而持有设备句柄的资源（例如主机帧池里的 `HostBuffer`）可能活得比
/// `VulkanVideoBackend` 更久：后端重建时若实例随旧后端一起销毁，设备就成了「没有实例
/// 的设备」，之后任何设备命令都是未定义行为（`VUID-vkDestroyInstance-instance-00629`）。
/// [`super::device::VideoDevice`] 持有本结构的一份克隆，于是销毁次序自动落成
/// 「最后一个设备引用之后」，与谁先被 drop 无关。
#[derive(Clone)]
pub struct VulkanInstance {
    inner: Arc<InstanceInner>,
}

/// 实例句柄与派生快照；最后一个 `Arc` 引用释放时销毁 `VkInstance`
struct InstanceInner {
    entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    properties: vk::PhysicalDeviceProperties,
    queue_families: Vec<QueueFamilyInfo>,
    device_extensions: Vec<String>,
    /// 所选物理设备的版本。**建设备时的 feature 结构体只能按它判定** ——
    /// 实例版本与设备版本可以不同（loader 1.4 + 设备 1.2 是合法组合），
    /// 按实例版本链 `PhysicalDeviceVulkan13Features` 属于非法用法
    device_api_version: u32,
    /// 校验层消息通道（仅 debug 构建且层可用时存在）
    ///
    /// 销毁次序是硬约束：messenger 派生自 instance，必须先于 instance 销毁。
    /// `InstanceInner::drop` 里显式 `take()`，不依赖字段声明顺序。
    validation: Option<ValidationMessenger>,
    /// 测试并发闸（见 [`crate::testing`]）：Clone 共享同一闸位。
    /// 只被持有不被读取，死代码豁免是本字段的常态
    #[cfg(test)]
    #[allow(dead_code)]
    gpu_slot: Arc<crate::testing::GpuSlot>,
}

/// 校验层的消息通道：加载器与 messenger 必须成对销毁
struct ValidationMessenger {
    loader: ash::ext::debug_utils::Instance,
    messenger: vk::DebugUtilsMessengerEXT,
}

impl ValidationMessenger {
    /// 建立消息通道
    ///
    /// `load` 不做空指针检查，调用方必须先确认层与扩展都可用、且 instance 已创建成功。
    fn new(entry: &ash::Entry, instance: &ash::Instance) -> Result<Self, String> {
        // SAFETY: `load` 只取函数指针；本结构存活期间 instance 不会销毁（由
        // `VulkanInstance` 保证），因此这些指针始终可调用
        let loader = ash::ext::debug_utils::Instance::load(entry, instance);
        let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
            .message_severity(
                vk::DebugUtilsMessageSeverityFlagsEXT::ERROR
                    | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING,
            )
            .message_type(
                vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                    | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                    | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
            )
            .pfn_user_callback(Some(validation_callback));
        // SAFETY: info 已完整初始化，回调是 'static 函数；返回的 messenger
        // 由本结构持有，只在 Drop 里销毁一次
        let messenger = unsafe { loader.create_debug_utils_messenger(&info, None) }
            .map_err(|e| format!("创建校验层消息通道失败: {e:?}"))?;
        Ok(Self { loader, messenger })
    }
}

impl Drop for ValidationMessenger {
    fn drop(&mut self) {
        // SAFETY: messenger 由本结构在构造时创建、只在此处销毁一次；
        // 此时 instance 尚未销毁（由 `VulkanInstance::drop` 的调用次序保证）
        unsafe {
            self.loader
                .destroy_debug_utils_messenger(self.messenger, None);
        }
    }
}

/// 校验层消息回调
///
/// 只做日志，不改变任何行为，恒返回 `VK_FALSE`（表示不中断调用）。
///
/// # Safety
///
/// 由 Vulkan loader 在注册的 messenger 生命周期内调用：`data` 与
/// `data.p_message` 的有效性由校验层保证仅在回调期间成立；本函数只读取并
/// 立即格式化，不保存任何指针，返回后不留引用
unsafe extern "system" fn validation_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _types: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user_data: *mut std::ffi::c_void,
) -> vk::Bool32 {
    // SAFETY: `data` 与 `data.p_message` 由校验层保证在回调期间有效，
    // 后者为 NUL 结尾的 C 串。此处只读取并立即格式化，不保存任何指针。
    let text = if data.is_null() {
        std::borrow::Cow::Borrowed("<空消息>")
    } else {
        let message = unsafe { (*data).p_message };
        if message.is_null() {
            std::borrow::Cow::Borrowed("<空消息>")
        } else {
            unsafe { std::ffi::CStr::from_ptr(message) }.to_string_lossy()
        }
    };
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        log::error!("[Vulkan 校验] {text}");
    } else {
        log::warn!("[Vulkan 校验] {text}");
    }
    vk::FALSE
}

impl VulkanInstance {
    /// 探测底座构造：动态加载系统 loader → 建 instance → 挑物理设备并快照队列族能力
    ///
    /// loader 或设备低于 1.3 下限、无设备可选即报错（fail-fast，被挡设备逐条列出原因）；
    /// 校验层只在显式设置 `VISTREL_VULKAN_VALIDATION` 时启用，被要求却不可用会告警。
    /// 不创建 `VkDevice`——那是 `VideoDevice::new` 的事
    pub fn new(app_name: &str) -> Result<Self, String> {
        // SAFETY: Entry::load 动态加载系统 loader（Linux libvulkan.so.1 / Windows vulkan-1.dll），
        // 不引入编译期 SDK 依赖。返回的 Entry 由本结构持有，其生命周期覆盖所有派生函数指针的调用点，
        // 满足「Entry 存活期间才可调用其加载的函数」这一前置条件
        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| format!("加载系统 Vulkan loader 失败: {e}"))?;

        // SAFETY: 仅查询 loader 支持的实例版本，不创建任何对象
        let available = unsafe { entry.try_enumerate_instance_version() }
            .map_err(|e| format!("枚举实例版本失败: {e:?}"))?
            .unwrap_or(vk::API_VERSION_1_0);
        if available < MIN_API_VERSION {
            return Err(format!(
                "需要 Vulkan {} 及以上，当前 loader 仅到 {}",
                version_text(MIN_API_VERSION),
                version_text(available)
            ));
        }
        // 不主动索取高于 loader 能力的版本，避免 create_instance 直接失败
        let instance_api_version = available.min(vk::API_VERSION_1_4);

        let app_name_c = CString::new(app_name).map_err(|e| format!("应用名含 NUL: {e}"))?;
        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name_c)
            .api_version(instance_api_version);

        // 一次枚举全部实例扩展：每个名字各查一次会让驱动往返次数随名单长度增长
        let available_exts = instance_extension_names(&entry)?;
        let mut ext_names = Vec::new();
        for name in INSTANCE_EXTENSIONS {
            if available_exts.iter().any(|e| e == name) {
                ext_names.push(CString::new(name).map_err(|e| format!("扩展名含 NUL: {e}"))?);
            }
        }
        // 校验层：**默认关闭，只有显式设置 `VISTREL_VULKAN_VALIDATION` 才启用。**
        //
        // 绝不能在 debug 构建里默认开启：**它会让 Vulkan Video 的 AV1 编码路径直接黑屏**
        // （2026-10-24 实测，关掉后同一份代码画面立刻正常）。视频编码/解码在
        // `VK_LAYER_KHRONOS_validation` 里是相对年轻的部分，层自己会插桩命令、
        // 维护会话状态机，一旦它的状态机与驱动的实现有偏差，产出的就是空帧而不是报错 ——
        // 而且校验输出看起来「一切正常」，排查时会一路往应用层找，代价极大。
        //
        // 排查编码问题时再手动打开：
        //   `VISTREL_VULKAN_VALIDATION=1 RUST_LOG=info ./gradlew :26.3:runClient`
        // 打开后出现的 VUID 是「规范合规」问题，未必是画面问题的原因，要分开对待。
        //
        // **校验层不是驱动自带的**：它随 LunarG Vulkan SDK 安装，未安装时
        // `VK_LAYER_KHRONOS_validation` 根本不存在。这种情况下必须把原因说出来，
        // 否则「没装」会被误读成「装了但没报错」。
        let requested = std::env::var_os("VISTREL_VULKAN_VALIDATION").is_some();
        let layer_found = instance_layer_available(&entry, VALIDATION_LAYER);
        let debug_ext_found = available_exts.iter().any(|e| e == DEBUG_UTILS_EXTENSION);
        let want_validation = requested && layer_found && debug_ext_found;
        if want_validation {
            ext_names.push(
                CString::new(DEBUG_UTILS_EXTENSION).map_err(|e| format!("扩展名含 NUL: {e}"))?,
            );
            log::info!("Vulkan 校验层已启用（{VALIDATION_LAYER}）");
        } else if requested {
            // 显式要求了却没能启用，必须说出来 —— 否则「没装」会被误读成「装了但没报错」
            log::warn!(
                "Vulkan 校验层未启用（层存在={layer_found} / 消息通道扩展={debug_ext_found}）"
            );
        }
        let ext_ptrs: Vec<*const c_char> = ext_names.iter().map(|c| c.as_ptr()).collect();
        let layer_names: Vec<CString> = if want_validation {
            vec![CString::new(VALIDATION_LAYER).map_err(|e| format!("层名含 NUL: {e}"))?]
        } else {
            Vec::new()
        };
        let layer_ptrs: Vec<*const c_char> = layer_names.iter().map(|c| c.as_ptr()).collect();

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_layer_names(&layer_ptrs)
            .enabled_extension_names(&ext_ptrs);

        // SAFETY: create_info 及其引用的 app_info / 层名 / 扩展名数组在调用期间均存活且已完整初始化；
        // 出参 instance 由 `InstanceGuard` 持有，成功时移交给本结构并在 Drop 中销毁一次
        let instance = InstanceGuard::new(
            unsafe { entry.create_instance(&create_info, None) }
                .map_err(|e| format!("创建 Vulkan 实例失败: {e:?}"))?,
        );

        // 消息通道必须在 instance 之后建：它派生自 instance
        let validation = if want_validation {
            Some(ValidationMessenger::new(&entry, instance.instance())?)
        } else {
            None
        };

        // SAFETY: 仅枚举，不创建对象
        let devices = unsafe { instance.instance().enumerate_physical_devices() }
            .map_err(|e| format!("枚举物理设备失败: {e:?}"))?;
        if devices.is_empty() {
            return Err("未找到任何 Vulkan 物理设备".to_string());
        }

        let mut best: Option<SelectedDevice> = None;
        // 被版本下限挡掉的设备：全部挡掉时要把原因说清，而不是只报「未选出可用物理设备」
        let mut rejected: Vec<String> = Vec::new();
        for pd in devices {
            // SAFETY: pd 来自同一 instance 的枚举结果，在其销毁前始终有效
            let properties = unsafe { instance.instance().get_physical_device_properties(pd) };
            if properties.api_version < MIN_API_VERSION {
                rejected.push(format!(
                    "{}（Vulkan {}）",
                    device_name_of(&properties),
                    version_text(properties.api_version)
                ));
                continue;
            }
            // SAFETY: 队列族属性查询只读取驱动常量，不创建对象
            let families = unsafe { snapshot_queue_families(instance.instance(), pd) };

            // 有视频编码队列优先（+10），同级再按设备类型排
            let video_bonus = if families.iter().any(|f| f.supports_video_encode()) {
                10
            } else {
                0
            };
            let score = video_bonus
                + match properties.device_type {
                    vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                    vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                    vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                    _ => 0,
                };
            if best.as_ref().is_none_or(|b| score > b.score) {
                best = Some(SelectedDevice {
                    score,
                    physical_device: pd,
                    properties,
                    queue_families: families,
                });
            }
        }
        let selected = best.ok_or_else(|| {
            format!(
                "没有满足 Vulkan {} 的物理设备；已跳过: {}",
                version_text(MIN_API_VERSION),
                rejected.join(", ")
            )
        })?;

        // SAFETY: 仅枚举，不创建对象
        let raw_exts = unsafe {
            instance
                .instance()
                .enumerate_device_extension_properties(selected.physical_device)
        }
        .map_err(|e| format!("枚举设备扩展失败: {e:?}"))?;
        let device_extensions = raw_exts
            .iter()
            // `extension_name_as_c_str` 是 ash 提供的安全 getter：驱动漏写 NUL 时返回 Err，
            // 不会像 `CStr::from_ptr` 那样把越界读取变成 UB。名字不可用的扩展直接跳过
            .filter_map(|e| e.extension_name_as_c_str().ok())
            .map(|s| s.to_string_lossy().into_owned())
            .collect();

        Ok(Self {
            inner: Arc::new(InstanceInner {
                entry,
                instance: instance.into_inner(),
                physical_device: selected.physical_device,
                properties: selected.properties,
                queue_families: selected.queue_families,
                device_extensions,
                // 可用版本 = min(应用声明的版本, 物理设备版本)。
                //
                // 不能直接用 `properties.api_version`：规范在
                // `VkPhysicalDeviceProperties::apiVersion` 下写明「the application must not
                // use functionality that exceeds the version of Vulkan associated with a given
                // object」，并给了「应用声明 1.1、设备支持 1.2 时不得使用 1.2 功能」的例子。
                // 旧 loader（1.3）+ 新驱动（报 1.4）正是这种组合：物理设备报 1.4，但实例与
                // 应用只声明到 1.3，此时链 1.4 的 feature 结构体、调用 1.4 核心命令全是
                // 超版本使用。
                device_api_version: instance_api_version.min(selected.properties.api_version),
                validation,
                // 测试并发闸：建实例即占闸，随实例（含 Clone）最后一个引用归还
                #[cfg(test)]
                gpu_slot: crate::testing::GpuSlot::acquire(),
            }),
        })
    }

    /// 所选物理设备支持的版本
    ///
    /// **建设备时链哪个 feature 结构体只能看这个**，不能看实例版本。
    pub fn device_api_version(&self) -> u32 {
        self.inner.device_api_version
    }

    pub fn entry(&self) -> &ash::Entry {
        &self.inner.entry
    }

    pub fn instance(&self) -> &ash::Instance {
        &self.inner.instance
    }

    pub fn physical_device(&self) -> vk::PhysicalDevice {
        self.inner.physical_device
    }

    pub fn properties(&self) -> &vk::PhysicalDeviceProperties {
        &self.inner.properties
    }

    /// 设备名（驱动上报的字符串）
    pub fn device_name(&self) -> String {
        device_name_of(&self.inner.properties)
    }

    /// 支持指定编码 codec 的队列族；`None` 表示该设备无法编码这种 codec
    pub fn video_encode_family(&self, operation: vk::VideoCodecOperationFlagsKHR) -> Option<u32> {
        self.inner
            .queue_families
            .iter()
            .find(|f| f.supports_encode_of(operation))
            .map(|f| f.index)
    }

    /// 指定队列族的时间戳有效位数；未知族按 0（不支持）返回
    ///
    /// 只服务测试构建的 GPU 时间戳探针：`vkCmdWriteTimestamp` 要求命令池所属队列族的
    /// `timestampValidBits` 非零（VUID-vkCmdWriteTimestamp-timestampValidBits-00829）。
    #[cfg(test)]
    pub fn queue_family_timestamp_valid_bits(&self, index: u32) -> u32 {
        self.inner
            .queue_families
            .iter()
            .find(|f| f.index == index)
            .map_or(0, |f| f.timestamp_valid_bits)
    }

    /// 通用计算队列族：优先选同时带 GRAPHICS 的族（避免落到视频专用族上）
    pub fn compute_family(&self) -> Option<u32> {
        self.inner
            .queue_families
            .iter()
            .find(|f| {
                f.flags
                    .contains(vk::QueueFlags::COMPUTE | vk::QueueFlags::GRAPHICS)
            })
            .or_else(|| {
                self.inner
                    .queue_families
                    .iter()
                    .find(|f| f.flags.contains(vk::QueueFlags::COMPUTE))
            })
            .map(|f| f.index)
    }

    pub fn supports_device_extension(&self, name: &str) -> bool {
        self.inner.device_extensions.iter().any(|e| e == name)
    }
}

impl Drop for InstanceInner {
    fn drop(&mut self) {
        // messenger 派生自 instance，必须先销毁。这里显式 `take()` 而不是依赖字段声明
        // 顺序：字段的析构发生在 `drop` 返回之后，那时 instance 已经没了。
        drop(self.validation.take());
        // SAFETY: 本结构是实例句柄的唯一所有者，只在最后一个 `Arc` 引用释放时运行；
        // `VideoDevice` 持有本结构的一份克隆，因此走到这里时引用该实例的 `VkDevice`
        // 必然都已销毁，`vkDestroyInstance` 的「无存活子对象」前提成立
        unsafe {
            self.instance.destroy_instance(None);
        }
    }
}

/// 构造期实例守卫：`ash::Instance` 没有 `Drop`，成功交给 [`InstanceInner`] 之前
/// 的任何错误路径都必须显式销毁实例。
struct InstanceGuard(Option<ash::Instance>);

impl InstanceGuard {
    fn new(instance: ash::Instance) -> Self {
        Self(Some(instance))
    }

    fn instance(&self) -> &ash::Instance {
        self.0
            .as_ref()
            .expect("InstanceGuard 在 into_inner 前恒持有实例")
    }

    fn into_inner(mut self) -> ash::Instance {
        self.0.take().expect("InstanceGuard 的实例已被取走")
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Some(instance) = self.0.take() {
            // SAFETY: 守卫是实例的唯一所有者；`validation` 声明在它之后、先于它
            // 析构，且此时尚未创建 VkDevice
            unsafe {
                instance.destroy_instance(None);
            }
        }
    }
}

struct SelectedDevice {
    score: u32,
    physical_device: vk::PhysicalDevice,
    properties: vk::PhysicalDeviceProperties,
    queue_families: Vec<QueueFamilyInfo>,
}

/// 快照全部队列族的能力（含视频 codec 操作位）
// SAFETY: `instance` 与 `pd` 必须同源且 `instance` 仍存活
unsafe fn snapshot_queue_families(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
) -> Vec<QueueFamilyInfo> {
    // SAFETY: _len 查询只读驱动常量
    let count = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
    if count == 0 {
        return Vec::new();
    }

    let mut video_props = vec![vk::QueueFamilyVideoPropertiesKHR::default(); count];
    let mut props: Vec<vk::QueueFamilyProperties2<'_>> = video_props
        .iter_mut()
        .map(|v| vk::QueueFamilyProperties2::default().push(v))
        .collect();

    // SAFETY: props 长度等于同一驱动给出的 count，pNext 链指向等长的 video_props 元素
    unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut props) };

    // 先把标志抄出来再释放 props：push 借走了 video_props，两者不能同时读
    let flags: Vec<vk::QueueFlags> = props
        .iter()
        .map(|p| p.queue_family_properties.queue_flags)
        .collect();
    #[cfg(test)]
    let timestamp_valid_bits: Vec<u32> = props
        .iter()
        .map(|p| p.queue_family_properties.timestamp_valid_bits)
        .collect();
    drop(props);

    flags
        .into_iter()
        .enumerate()
        .map(|(index, flags)| QueueFamilyInfo {
            index: index as u32,
            flags,
            video_codec_operations: video_props[index].video_codec_operations,
            #[cfg(test)]
            timestamp_valid_bits: timestamp_valid_bits[index],
        })
        .collect()
}

/// 枚举 loader 提供的全部实例扩展名
///
/// 名字不可用（驱动没写 NUL）的项直接跳过：它本来也无法按名匹配。
fn instance_extension_names(entry: &ash::Entry) -> Result<Vec<String>, String> {
    // SAFETY: 仅枚举 loader 的实例扩展，不创建对象
    let exts = unsafe { entry.enumerate_instance_extension_properties(None) }
        .map_err(|e| format!("枚举实例扩展失败: {e:?}"))?;
    Ok(exts
        .iter()
        .filter_map(|e| e.extension_name_as_c_str().ok())
        .map(|s| s.to_string_lossy().into_owned())
        .collect())
}

/// loader 是否提供指定的实例层
///
/// 查不到就当没有：校验层是调试设施，缺了不该让编码器起不来。
fn instance_layer_available(entry: &ash::Entry, name: &str) -> bool {
    // SAFETY: 仅枚举 loader 的实例层，不创建任何对象
    let Ok(layers) = (unsafe { entry.enumerate_instance_layer_properties() }) else {
        return false;
    };
    layers
        .iter()
        .filter_map(|l| l.layer_name_as_c_str().ok())
        .any(|l| l.to_string_lossy() == name)
}

/// 从设备属性里取设备名
///
/// `device_name_as_c_str` 是 ash 提供的安全 getter：驱动漏写 NUL 时返回 Err，
/// 不会像 `CStr::from_ptr` 那样把越界读取变成 UB。
fn device_name_of(properties: &vk::PhysicalDeviceProperties) -> String {
    properties
        .device_name_as_c_str()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "<设备名不可用>".to_string())
}

pub(super) fn version_text(v: u32) -> String {
    format!(
        "{}.{}.{}",
        vk::api_version_major(v),
        vk::api_version_minor(v),
        vk::api_version_patch(v)
    )
}

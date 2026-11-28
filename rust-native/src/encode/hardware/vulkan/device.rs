//! Vulkan 设备与队列 (Video Device & Queues)
//!
//! 只做两件事：按**指定 codec** 建出可用的 `VkDevice` 与队列，以及提供显存 / 主机可见
//! 缓冲的分配原语。队列族按 codec 逐个查，不假设所有 codec 落在同一族——异构硬件上
//! H.264 与 AV1 分属不同族是合法布局。
//!
//! 句柄本体放在 `Arc<DeviceInner>` 里：编码会话、输入图、CSC 都必须在同一设备上，
//! 用引用共享比纠结「谁拥有设备」的所有权更容易写对，也不会写出自引用结构。

use std::ffi::{CString, c_char};
use std::ptr::NonNull;
use std::sync::Arc;

// `push` 是 `TaggedStructure` trait 的方法，不导入就无法链 pNext
use ash::vk::{self, TaggedStructure};

use super::codec::{DeviceFeatures, codec_encode_extension, codec_encode_operation};
use super::context::VulkanInstance;
use crate::encode::VideoCodec;

/// 显存直通所需的外部内存/信号量扩展（平台相关）
///
/// 这些是**可选**扩展：缺了只是没有显存直通，纯 CPU 上传路径照常工作。
/// 不能因为某台机器缺一个可选扩展就整个后端不可用。
#[cfg(target_os = "linux")]
const EXTERNAL_MEMORY_EXTENSIONS: &[&str] = &[
    // `VK_KHR_external_memory` / `VK_KHR_external_semaphore` 本身是 1.1 核心，
    // 不需要再启用扩展名；平台专属的 `_fd` 形态没有核心版本，仍是扩展
    "VK_KHR_external_memory_fd",
    "VK_KHR_external_semaphore_fd",
];

#[cfg(target_os = "windows")]
const EXTERNAL_MEMORY_EXTENSIONS: &[&str] = &[
    "VK_KHR_external_memory_win32",
    "VK_KHR_external_semaphore_win32",
];

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
const EXTERNAL_MEMORY_EXTENSIONS: &[&str] = &[];

/// Intra-Refresh 扩展
///
/// 只声明扩展还不够：真正的准入是 `VkPhysicalDeviceVideoEncodeIntraRefreshFeaturesKHR`
/// 的 feature 位。它和 1.2/1.3 的核心 feature 一样，**建设备之前**就能对物理设备查，
/// 而且必须在 `vkCreateDevice` 时链进 pNext 才算「开启」—— 物理设备支持不等于逻辑设备
/// 启用，只查不链的话后续下发 `VkVideoEncodeIntraRefreshInfoKHR` 是非法用法。
/// 不支持就必须 fail-closed，不能靠扩展名猜。
const INTRA_REFRESH_EXTENSION: &str = "VK_KHR_video_encode_intra_refresh";

/// push descriptor 的扩展名
///
/// Vulkan 1.4 把它提升为核心，但**可选**：设备版本不到 1.4 时必须先确认驱动声明了它，
/// 否则塞进 `enabled_extension_names` 会让 `vkCreateDevice` 直接返回
/// `VK_ERROR_EXTENSION_NOT_PRESENT`——那是整个后端不可用，不是「CSC 降级」。
const PUSH_DESCRIPTOR_EXTENSION: &str = "VK_KHR_push_descriptor";

/// 建设备**之前**就能确定的 push descriptor 通路与上限
#[derive(Debug, Clone, Copy)]
enum PushDescriptorPlan {
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
enum PushDescriptor {
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
fn plan_push_descriptor(instance: &VulkanInstance) -> PushDescriptorPlan {
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

/// 计算着色器**输出平面**的格式是否支持 `STORAGE_IMAGE`
///
/// 源图已改成 sampled image（RGBA8 / BGRA8 的采样支持是规范保证的，不需要查），
/// 所以只剩两个输出平面要准入。这是「不同机器真的会不一样」的项，必须运行时查、
/// 不支持就 fail-closed。
#[derive(Debug, Clone, Copy, Default)]
pub struct StorageFormatSupport {
    /// Y 平面 R8（着色器输出）
    pub r8: bool,
    /// UV 平面 R8G8（着色器输出）
    pub r8g8: bool,
}

impl StorageFormatSupport {
    /// 两个输出平面都支持才算可用
    pub fn all(&self) -> bool {
        self.r8 && self.r8g8
    }
}

/// 设备句柄与其派生状态；最后一个 `Arc` 引用释放时销毁 `VkDevice`
struct DeviceInner {
    device: ash::Device,
    /// 实例的**所有权**。`VkDevice` 必须早于 `VkInstance` 销毁，而持有设备句柄的对象
    /// （主机帧池里的 `HostBuffer` 等）可能比后端活得久，所以实例不能只由后端持有。
    /// 字段顺序同时是兜底：`Drop` 里先 `destroy_device`，随后本字段释放最后一个引用。
    instance: VulkanInstance,
    /// `VK_KHR_video_queue` 的设备级命令表（会话、参数对象、begin/control/end）
    ///
    /// 表在建设备时一次构造：`Device::new` 走 `vkGetDeviceProcAddr` 填满整张表，
    /// **不检查空指针**。因此它的成立前提是上面已确认两个视频扩展都已启用。
    video_queue_table: ash::khr::video_queue::Device,
    /// `VK_KHR_video_encode_queue` 的设备级命令表（`vkCmdEncodeVideoKHR`、
    /// `vkGetEncodedVideoSessionParametersKHR`）
    video_encode_queue_table: ash::khr::video_encode_queue::Device,
    video_queue_family: u32,
    video_queue: vk::Queue,
    compute_queue_family: u32,
    compute_queue: vk::Queue,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    storage_formats: StorageFormatSupport,
    /// 驱动是否**真正**支持 intra-refresh（扩展已启用且 feature 位为真）
    intra_refresh: bool,
    /// `vkCmdPushDescriptorSet` 单次可推送的描述符数量上限；0 表示不可用
    ///
    /// 两条通路都会给出这个值：1.4 核心的 `VkPhysicalDeviceVulkan14Properties`，
    /// 或 `VK_KHR_push_descriptor` 的 `VkPhysicalDevicePushDescriptorPropertiesKHR`。
    max_push_descriptors: u32,
    /// 走哪条通路下发 push descriptor
    push_descriptor: PushDescriptor,
    enabled_extensions: Vec<&'static str>,
    /// 视频编码队列族的时间戳有效位数；0 表示该族不能写时间戳
    ///
    /// 只在测试构建里采集：GPU 时间戳探针是性能剖析设施，生产路径不发查询。
    #[cfg(test)]
    timestamp_valid_bits: u32,
    /// 时间戳每 tick 的纳秒数（`VkPhysicalDeviceLimits::timestampPeriod`）
    #[cfg(test)]
    timestamp_period_ns: f32,
}

impl Drop for DeviceInner {
    fn drop(&mut self) {
        // SAFETY: device 由 `VideoDevice::new` 创建且只在此处销毁一次；此时所有
        // 依赖该设备的子对象（会话、图像、队列）都已随各自的 Arc 引用先行释放
        unsafe {
            self.device.destroy_device(None);
        }
    }
}

/// 已创建的 Vulkan 设备与本次会话要用的队列
#[derive(Clone)]
pub struct VideoDevice {
    inner: Arc<DeviceInner>,
}

impl VideoDevice {
    /// 按目标 codec 建设备：选编码/计算队列族，fail-closed 校验必需扩展与
    /// `synchronization2` feature 位，可选能力（外部内存/信号量、intra-refresh、
    /// push descriptor）只在驱动声明时启用
    ///
    /// codec 无编码队列族、缺必需扩展或 synchronization2 缺位即报错；AV1 feature
    /// 位缺失不阻断（告警后由上层决定是否降级 NVENC）。Clone 共享同一设备句柄，
    /// 销毁随最后一个 Arc 引用发生
    pub fn new(instance: &VulkanInstance, codec: VideoCodec) -> Result<Self, String> {
        let pd = instance.physical_device();

        // 1. 队列族按 codec 查。同一个族也可能同时带 COMPUTE，那就只建一条队列
        let video_family = instance
            .video_encode_family(codec_encode_operation(codec))
            .ok_or_else(|| format!("{codec:?} 没有可用的编码队列族"))?;
        let compute_family = instance
            .compute_family()
            .ok_or_else(|| "没有可用的计算队列族".to_string())?;

        // 2. 扩展：两个视频基础扩展 + 该 codec 的编码扩展，缺一即 fail-closed
        let mut extensions = vec![
            "VK_KHR_video_queue",
            "VK_KHR_video_encode_queue",
            codec_encode_extension(codec),
        ];
        for &name in &extensions {
            if !instance.supports_device_extension(name) {
                return Err(format!("缺少必需设备扩展 {name}"));
            }
        }

        // 可选扩展：外部内存与外部信号量。缺了只是没有显存直通（S3），
        // 纯 CPU 上传路径照常工作，因此逐个判断、能开就开
        for &name in EXTERNAL_MEMORY_EXTENSIONS {
            if instance.supports_device_extension(name) {
                extensions.push(name);
            }
        }
        // Intra-Refresh 同样可选：扩展名只代表「声明了」，能否用要等设备建完查 feature 位
        if instance.supports_device_extension(INTRA_REFRESH_EXTENSION) {
            extensions.push(INTRA_REFRESH_EXTENSION);
        }

        // push descriptor：两条通路，建设备之前就得定。走 KHR 通路时扩展名必须**显式启用**——
        // 不启用就去 `vkGetDeviceProcAddr("vkCmdPushDescriptorSetKHR")`，AMD Adrenalin 25.5.1
        // 会返回非法指针（GPUOpen-Drivers/AMD-Gfx-Drivers#55）。
        let push_plan = plan_push_descriptor(instance);
        if matches!(push_plan, PushDescriptorPlan::Khr(_)) {
            extensions.push(PUSH_DESCRIPTOR_EXTENSION);
        }

        let ext_cstrings: Vec<CString> = extensions
            .iter()
            .map(|n| CString::new(*n).map_err(|e| format!("扩展名含 NUL: {e}")))
            .collect::<Result<_, _>>()?;
        let ext_ptrs: Vec<*const c_char> = ext_cstrings.iter().map(|c| c.as_ptr()).collect();

        // 1.2 / 1.3 的核心 feature。两条都是**核心 feature 位**，不是扩展名：
        // - `timeline_semaphore`（1.2）：显存槽位的跨进程同步；不支持只是没有显存直通
        // - `synchronization2`（1.3）：所有图像屏障都走 `vkCmdPipelineBarrier2`，
        //   不启用就是非法用法（`VUID-vkCmdPipelineBarrier2-synchronization2-03848`）
        //
        // intra-refresh 的 feature 位也在这里一起查：`vkGetPhysicalDeviceFeatures2` 问的是
        // **物理设备**，建设备之前就可用；查到的结果稍后必须链进 `VkDeviceCreateInfo`。
        //
        // 一次查询带上全部结构体，省驱动往返。
        let mut vk12_features = vk::PhysicalDeviceVulkan12Features::default();
        let mut vk13_features = vk::PhysicalDeviceVulkan13Features::default();
        let mut intra_features = vk::PhysicalDeviceVideoEncodeIntraRefreshFeaturesKHR::default();
        // codec 专属 feature（目前只有 AV1 需要显式开启），查询与启用都封装在 codec 模块
        let mut codec_features = DeviceFeatures::new(codec);
        // 扩展没声明就绝不去查它的 feature 结构体：未支持的扩展在 pNext 里属于未知 sType
        let probe_intra = extensions.contains(&INTRA_REFRESH_EXTENSION);
        {
            // 内层作用域：`push` 可变借走这几个结构体，查询结束后借用必须先释放才能读回字段
            let mut features2 = vk::PhysicalDeviceFeatures2::default()
                .push(&mut vk13_features)
                .push(&mut vk12_features);
            if probe_intra {
                features2 = features2.push(&mut intra_features);
            }
            features2 = codec_features.push_query(features2);
            // SAFETY: 纯查询，features2 及其 pNext 链在调用期间存活
            unsafe {
                instance
                    .instance()
                    .get_physical_device_features2(pd, &mut features2);
            }
        }
        if vk13_features.synchronization2 != vk::TRUE {
            return Err("设备未提供 synchronization2 特性".to_string());
        }
        let timeline_semaphore = vk12_features.timeline_semaphore == vk::TRUE;
        // 扩展名与 feature 位都成立才算真正支持（fail-closed）：不支持就绝不下发该参数，
        // 否则驱动可能产出没人能解的码流
        let intra_refresh_supported =
            probe_intra && intra_features.video_encode_intra_refresh == vk::TRUE;
        // 查询会把整个结构体填成驱动的实际支持，清零后只开需要的那一位，
        // 避免顺手启用一堆我们没验证过的特性
        vk13_features = vk::PhysicalDeviceVulkan13Features::default();
        vk13_features.synchronization2 = vk::TRUE;
        vk12_features = vk::PhysicalDeviceVulkan12Features::default();
        vk12_features.timeline_semaphore = vk::TRUE;
        intra_features = vk::PhysicalDeviceVideoEncodeIntraRefreshFeaturesKHR::default();
        if intra_refresh_supported {
            intra_features.video_encode_intra_refresh = vk::TRUE;
        }

        // 驱动声明支持才启用；缺失时只告警，由上层决定是否降级
        codec_features.finish();

        // 3. 每个用到的队列族各取一条队列
        let mut families = vec![video_family];
        if compute_family != video_family {
            families.push(compute_family);
        }
        let priorities = [1.0f32];
        let queue_infos: Vec<vk::DeviceQueueCreateInfo> = families
            .iter()
            .map(|&family| {
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(family)
                    .queue_priorities(&priorities)
            })
            .collect();

        // 构建器入参是 bool，写进字段时为 Bool32::TRUE
        let mut push_features = vk::PhysicalDeviceVulkan14Features::default().push_descriptor(true);
        let mut create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&ext_ptrs);
        // 只有核心通路才链 1.4 的 feature 结构体；KHR 通路靠扩展名，链上去就是超版本使用
        if matches!(push_plan, PushDescriptorPlan::Core(_)) {
            // SAFETY: push_features 声明在 create_info 之前，生命周期覆盖 create_device；
            // 字段已置为 TRUE，其余 feature 保持关闭（默认值）
            create_info = create_info.push(&mut push_features);
        }
        // SAFETY: vk13_features / vk12_features 同样声明在 create_info 之前，
        // 生命周期覆盖 create_device；各自只有一位为 TRUE，插入时 p_next 均为空
        create_info = create_info.push(&mut vk13_features);
        if timeline_semaphore {
            create_info = create_info.push(&mut vk12_features);
        }
        // intra-refresh 的 feature 位必须在这里链进去：物理设备「支持」不等于逻辑设备「开启」。
        // SAFETY: intra_features 声明在 create_info 之前，生命周期覆盖 create_device；
        // 只有一位为 TRUE，插入时 p_next 为空
        if intra_refresh_supported {
            create_info = create_info.push(&mut intra_features);
        }
        // codec 专属 feature 位：不链进去 = 没启用，会话 / 图像 / 缓冲 / 查询池全会被判非法
        // （AV1 为 VUID 10248 / 10249 / 10250 / 10269）。
        // SAFETY: codec_features 声明在 create_info 之前，生命周期覆盖 create_device
        create_info = codec_features.push_enable(create_info);

        // SAFETY: queue_infos / ext_ptrs 及其引用的 CString 在调用期间均存活；
        // 出参 device 交由 DeviceInner 持有并在其 Drop 中销毁一次
        let device = unsafe { instance.instance().create_device(pd, &create_info, None) }
            .map_err(|e| format!("创建 Vulkan 设备失败: {e:?}"))?;

        // SAFETY: 队列族索引来自同一设备的队列族快照，索引 0 在 queueCount 之内
        let video_queue = unsafe { device.get_device_queue(video_family, 0) };
        // SAFETY: 同上
        let compute_queue = unsafe { device.get_device_queue(compute_family, 0) };
        // SAFETY: 纯查询，返回驱动常量的拷贝
        let memory_properties = unsafe {
            instance
                .instance()
                .get_physical_device_memory_properties(pd)
        };

        // Intra-Refresh 的准入结论在建设备之前就已确定，feature 位也已链进 create_info；
        // 这里只做转存，不再查询——查询与启用必须是同一份数据，否则会「查到支持但没开启」
        let intra_refresh = intra_refresh_supported;

        // GPU 时间戳探针的两项准入：队列族的 validBits 决定能不能写时间戳，
        // `timestampPeriod` 决定 tick 怎么折算成纳秒。两者在建设备前就能读到，
        // 存下来避免逐帧再查
        #[cfg(test)]
        let timestamp_valid_bits = instance.queue_family_timestamp_valid_bits(video_family);
        #[cfg(test)]
        let timestamp_period_ns = instance.properties().limits.timestamp_period;

        // CSC 输出平面的 storage image 格式能力。逐格式查，不假设「有 R8 就有 R8G8」
        let storage_formats = StorageFormatSupport {
            // SAFETY: 纯查询，pd 来自同一 instance 且此刻有效
            r8: unsafe {
                instance
                    .instance()
                    .get_physical_device_format_properties(pd, vk::Format::R8_UNORM)
            }
            .optimal_tiling_features
            .contains(vk::FormatFeatureFlags::STORAGE_IMAGE),
            // SAFETY: 同上
            r8g8: unsafe {
                instance
                    .instance()
                    .get_physical_device_format_properties(pd, vk::Format::R8G8_UNORM)
            }
            .optimal_tiling_features
            .contains(vk::FormatFeatureFlags::STORAGE_IMAGE),
        };

        // 两张命令表在建设备时一次填满，之后全后端共用，不再逐条解析。
        //
        // `load` 把 `vkGetDeviceProcAddr` 的返回值直接转成函数指针、**不检查空指针**，
        // 所以表可用性的前提是上面已确认两个视频扩展都已启用（缺任何一个都已在此前
        // 返回 Err）。构造本身不失败，调用未导出的命令才会崩。
        let video_queue_table = ash::khr::video_queue::Device::load(instance.instance(), &device);
        let video_encode_queue_table =
            ash::khr::video_encode_queue::Device::load(instance.instance(), &device);

        // push descriptor 的运行时形态。KHR 表必须在设备建好、扩展已启用之后构造
        let (push_descriptor, max_push_descriptors) = match push_plan {
            PushDescriptorPlan::Core(n) => (PushDescriptor::Core, n),
            PushDescriptorPlan::Khr(n) => (
                PushDescriptor::Khr(ash::khr::push_descriptor::Device::load(
                    instance.instance(),
                    &device,
                )),
                n,
            ),
            PushDescriptorPlan::Unavailable => (PushDescriptor::Unavailable, 0),
        };

        Ok(Self {
            inner: Arc::new(DeviceInner {
                device,
                instance: instance.clone(),
                video_queue_table,
                video_encode_queue_table,
                video_queue_family: video_family,
                video_queue,
                compute_queue_family: compute_family,
                compute_queue,
                memory_properties,
                storage_formats,
                intra_refresh,
                max_push_descriptors,
                push_descriptor,
                enabled_extensions: extensions,
                #[cfg(test)]
                timestamp_valid_bits,
                #[cfg(test)]
                timestamp_period_ns,
            }),
        })
    }

    /// 按内存需求分配一块显存
    ///
    /// 不做子分配：本后端的分配全部发生在初始化阶段、数量固定（约 10 块）、
    /// 生命周期到退出，子分配没有收益，反而多一层要维护的抽象。
    fn allocate_raw(
        &self,
        requirements: vk::MemoryRequirements,
        required: vk::MemoryPropertyFlags,
        what: &str,
    ) -> Result<vk::DeviceMemory, String> {
        let memory_type = self
            .find_memory_type(requirements.memory_type_bits, required)
            .ok_or_else(|| format!("{what}找不到满足 {required:?} 的内存类型"))?;
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: alloc_info 已完整初始化；失败时不产生所有权
        unsafe { self.device().allocate_memory(&alloc_info, None) }
            .map_err(|e| format!("{what}分配失败: {e:?}"))
    }

    /// 建图 + 分配显存 + 绑定，**失败时把已创建的资源一并回收**
    ///
    /// 「图建好了、显存没分到 / 没绑上」是构造路径上必然存在的中间态，每个调用点各写
    /// 一遍 destroy 只会漏。这里把三步合成一个原子操作：要么都成功，要么什么都不留。
    pub fn create_image_allocated(
        &self,
        info: &vk::ImageCreateInfo<'_>,
        required: vk::MemoryPropertyFlags,
        what: &str,
    ) -> Result<(vk::Image, vk::DeviceMemory), String> {
        // SAFETY: info 由调用方保证完整初始化，其 pNext 链在调用期间存活
        let image = unsafe { self.device().create_image(info, None) }
            .map_err(|e| format!("{what}创建失败: {e:?}"))?;

        // 分配 + 绑定；任一失败都要先把本步已产生的所有权归还，再销毁图
        let memory = (|| -> Result<vk::DeviceMemory, String> {
            // SAFETY: 纯查询，image 为刚创建的有效句柄
            let requirements = unsafe { self.device().get_image_memory_requirements(image) };
            let memory = self.allocate_raw(requirements, required, what)?;
            // SAFETY: memory 按该 image 的 requirements 分配，且只绑定这一次
            if let Err(e) = unsafe { self.device().bind_image_memory(image, memory, 0) } {
                // SAFETY: memory 刚分配且未绑定成功，先释放一次
                unsafe { self.device().free_memory(memory, None) };
                return Err(format!("{what}绑定显存失败: {e:?}"));
            }
            Ok(memory)
        })();
        match memory {
            Ok(memory) => Ok((image, memory)),
            Err(e) => {
                // SAFETY: image 刚创建且未绑定成功显存，此处销毁一次
                unsafe { self.device().destroy_image(image, None) };
                Err(e)
            }
        }
    }

    /// 为缓冲分配显存并绑定，返回的内存由调用方在缓冲销毁后 `free_memory`
    ///
    /// `preferred` 是选型加分项：先找「required | preferred」全满足的类型，找不到再
    /// 退回只满足 `required` 的——读回类缓冲（码流）传 `HOST_CACHED`，写上传类暂存
    /// 传空（write-combined 对纯写反而更合适）
    pub fn allocate_buffer(
        &self,
        buffer: vk::Buffer,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
        what: &str,
    ) -> Result<(vk::DeviceMemory, vk::MemoryPropertyFlags), String> {
        // SAFETY: 纯查询，buffer 为调用方刚创建的有效句柄
        let requirements = unsafe { self.device().get_buffer_memory_requirements(buffer) };
        let memory_type = self
            .find_memory_type(requirements.memory_type_bits, required | preferred)
            .or_else(|| self.find_memory_type(requirements.memory_type_bits, required))
            .ok_or_else(|| format!("{what}找不到满足 {required:?} 的内存类型"))?;
        let flags = self.inner.memory_properties.memory_types[memory_type as usize].property_flags;
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: alloc_info 已完整初始化；失败时不产生所有权
        let memory = unsafe { self.device().allocate_memory(&alloc_info, None) }
            .map_err(|e| format!("{what}分配失败: {e:?}"))?;
        // SAFETY: memory 按该 buffer 的 requirements 分配，且只绑定这一次
        unsafe { self.device().bind_buffer_memory(buffer, memory, 0) }.map_err(|e| {
            // SAFETY: memory 刚分配且尚未绑定成功，此处释放一次
            unsafe { self.device().free_memory(memory, None) };
            format!("{what}绑定显存失败: {e:?}")
        })?;
        Ok((memory, flags))
    }

    pub fn device(&self) -> &ash::Device {
        &self.inner.device
    }

    /// 建一块主机可见、一致的缓冲并**立刻映射一次**，映射持续到析构
    ///
    /// 见 [`HostBuffer`]。
    pub fn host_buffer(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
    ) -> Result<HostBuffer, String> {
        HostBuffer::new(self, size, usage, what)
    }

    /// 缓存堆变体：优先落 `HOST_CACHED` 类型
    ///
    /// 「CPU 整帧写、GPU 读一次」的池块专用（堆选依据见 `allocate_host_frames`）；
    /// GPU 侧 DMA 读不受堆类型影响。GPU 写一次、CPU 读的码流缓冲仍走
    /// [`Self::host_buffer_video`]（CPU 读无 `HOST_CACHED` 的堆极慢）。
    pub fn host_buffer_cached(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
    ) -> Result<HostBuffer, String> {
        HostBuffer::new_cached(self, size, usage, what)
    }

    /// 视频编码的码流缓冲：创建时必须声明所属 profile
    ///
    /// 否则驱动不把它当视频资源（`VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08201`）。
    /// `profile` 只存指针链，必须活过本次 create 调用。
    pub fn host_buffer_video(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
        profile: &vk::VideoProfileInfoKHR<'_>,
    ) -> Result<HostBuffer, String> {
        HostBuffer::new_video(self, size, usage, what, profile)
    }

    /// `VK_KHR_video_queue` 的命令表
    ///
    /// 表在建设备时一次构造，调用方不需要（也不应该）再自己解析函数指针。
    pub(crate) fn video_queue_table(&self) -> &ash::khr::video_queue::Device {
        &self.inner.video_queue_table
    }

    /// `VK_KHR_video_encode_queue` 的命令表
    pub(crate) fn video_encode_queue_table(&self) -> &ash::khr::video_encode_queue::Device {
        &self.inner.video_encode_queue_table
    }

    /// `ash::Instance`：按需构造**可选**扩展的命令表时要用
    ///
    /// 可选扩展（外部内存/信号量）不能放进 `DeviceInner` 无条件构造 —— 扩展没启用时
    /// 表里的函数指针是空指针，构造不报错、调用才崩。调用方必须先查 `has_extension`。
    pub(crate) fn instance(&self) -> &ash::Instance {
        self.inner.instance.instance()
    }

    pub fn video_queue(&self) -> vk::Queue {
        self.inner.video_queue
    }

    pub fn video_queue_family(&self) -> u32 {
        self.inner.video_queue_family
    }

    pub fn compute_queue(&self) -> vk::Queue {
        self.inner.compute_queue
    }

    pub fn compute_queue_family(&self) -> u32 {
        self.inner.compute_queue_family
    }

    /// 视频编码队列族的时间戳有效位数；0 表示该族不允许写时间戳
    #[cfg(test)]
    pub fn timestamp_valid_bits(&self) -> u32 {
        self.inner.timestamp_valid_bits
    }

    /// 时间戳每 tick 的纳秒数
    #[cfg(test)]
    pub fn timestamp_period_ns(&self) -> f32 {
        self.inner.timestamp_period_ns
    }

    #[cfg(test)]
    pub fn enabled_extensions(&self) -> &[&'static str] {
        &self.inner.enabled_extensions
    }

    /// 某个设备扩展是否已启用
    ///
    /// 可选能力（显存直通、外部信号量）在调用前必须先问这个，不能假设它在。
    pub fn has_extension(&self, name: &str) -> bool {
        self.inner.enabled_extensions.contains(&name)
    }

    /// CSC 输出平面的 storage image 格式能力
    pub fn format_support(&self) -> StorageFormatSupport {
        self.inner.storage_formats
    }

    /// 单次 `vkCmdPushDescriptorSet` 可推送的描述符数量上限；0 表示不可用
    ///
    /// 调用方拿实际需要的绑定数来比，不假设某个下限。
    pub fn max_push_descriptors(&self) -> u32 {
        self.inner.max_push_descriptors
    }

    /// 把一组描述符直接推进命令缓冲
    ///
    /// 两条通路的参数与行为完全一致，差别只在入口名与能力来源：1.4 走核心命令，
    /// 1.3 走 `vkCmdPushDescriptorSetKHR`（扩展已在建设备时显式启用）。调用方不需要
    /// 知道走哪条，也不该自己判断——判断只写在 [`plan_push_descriptor`] 一处。
    ///
    /// # Safety
    ///
    /// 调用方必须保证：`cmd` 处于录制状态；`layout` 以 `PUSH_DESCRIPTOR` 标志创建
    /// 且本函数只在录制命令期间调用；`writes` 及其引用的 `DescriptorImageInfo` 等
    /// 数据在调用期间存活。能力可用性已由构造期准入兜底（Unavailable 即 unreachable）
    // SAFETY: `cmd` 必须处于录制状态；`layout` 必须以 `PUSH_DESCRIPTOR` 标志创建；
    // `writes` 及其引用的 `DescriptorImageInfo` 在调用期间必须存活
    pub(crate) unsafe fn push_descriptor_set(
        &self,
        cmd: vk::CommandBuffer,
        bind_point: vk::PipelineBindPoint,
        layout: vk::PipelineLayout,
        set: u32,
        writes: &[vk::WriteDescriptorSet<'_>],
    ) {
        match &self.inner.push_descriptor {
            // SAFETY: 由调用方按函数级 Safety 保证
            PushDescriptor::Core => unsafe {
                self.inner
                    .device
                    .cmd_push_descriptor_set(cmd, bind_point, layout, set, writes)
            },
            // SAFETY: 同上
            PushDescriptor::Khr(table) => unsafe {
                table.cmd_push_descriptor_set(cmd, bind_point, layout, set, writes)
            },
            // 不可达：`ColorConverter::new` 在 `max_push_descriptors` 为 0 时已返回 Err。
            // 真到这里说明准入被绕过——静默跳过会留下未绑定的描述符，比 panic 更难查
            PushDescriptor::Unavailable => unreachable!("push descriptor 不可用却仍在下发"),
        }
    }

    /// 驱动是否真正支持 intra-refresh
    ///
    /// 为假时**绝不能**往编码命令里挂 `VkVideoEncodeIntraRefreshInfoKHR`。
    pub fn supports_intra_refresh(&self) -> bool {
        self.inner.intra_refresh
    }

    /// 按 requirements 的 type bits 与属性要求选内存类型
    pub fn find_memory_type(
        &self,
        type_bits: u32,
        required: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        (0..self.inner.memory_properties.memory_type_count).find(|&i| {
            type_bits & (1 << i) != 0
                && self.inner.memory_properties.memory_types[i as usize]
                    .property_flags
                    .contains(required)
        })
    }
}

/// 主机可见、一致、**构造时映射一次**的缓冲
///
/// 码流回读、CSC 的 RGBA 上传、NV12 暂存上传都是逐帧操作，每帧一对
/// `vkMapMemory` / `vkUnmapMemory` 会多出两次驱动往返，而映射本来就可以贯穿整个
/// 结构生命周期。本类型把「创建缓冲 → 分配显存 → 映射」合成一步，析构时逆序归还，
/// 调用方只看到 [`HostBuffer::write`] / [`HostBuffer::read`] 两个主机侧动作。
///
/// 映射范围取**缓冲大小**而不是 `VK_WHOLE_SIZE`：分配大小可能大于缓冲，按缓冲大小映射
/// 能让越界访问落在本类型的长度校验上，而不是悄悄写坏同一块分配里的别的东西。
pub struct HostBuffer {
    device: VideoDevice,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// 持久映射的基址：解映射之前恒有效，`map_memory` 成功即非空
    ptr: NonNull<u8>,
    size: u64,
    /// 落点是否为“可缓存且非设备本地”的系统内存——决定整帧写入用普通还是非暂存存储
    cached_ram: bool,
}

// SAFETY: `HostBuffer` 不实现 `Clone`，可变访问一律要 `&mut self`，所以跨线程移动
// 只是所有权转移、不会产生并发访问；映射地址与显存句柄都是进程内资源，与线程无关。
unsafe impl Send for HostBuffer {}

impl HostBuffer {
    fn new(
        device: &VideoDevice,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
    ) -> Result<Self, String> {
        Self::new_inner(device, size, usage, what, None, false)
    }

    /// 缓存堆变体：见 [`VideoDevice::host_buffer_cached`]
    fn new_cached(
        device: &VideoDevice,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
    ) -> Result<Self, String> {
        Self::new_inner(device, size, usage, what, None, true)
    }

    /// 视频编码的码流缓冲：创建时必须声明所属 profile
    ///
    /// 否则驱动不把它当视频资源（`VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08201`）。
    /// `profile` 只存指针链，必须活过本次 create 调用。
    fn new_video(
        device: &VideoDevice,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
        profile: &vk::VideoProfileInfoKHR<'_>,
    ) -> Result<Self, String> {
        Self::new_inner(device, size, usage, what, Some(profile), true)
    }

    fn new_inner(
        device: &VideoDevice,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
        video_profile: Option<&vk::VideoProfileInfoKHR<'_>>,
        prefer_cached: bool,
    ) -> Result<Self, String> {
        if size == 0 {
            return Err(format!("{what}大小不能为零"));
        }
        let mut info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // list 必须活到 create 调用之后：`VkVideoProfileListInfoKHR` 里存的是指向
        // 调用方 profile 的指针，而 `info` 又存着指向 list 的指针。写在 `if let` 块里
        // 会在块尾就 drop，借检查直接报 E0597。
        let mut profile_list = video_profile
            .map(|p| vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(p)));
        if let Some(list) = profile_list.as_mut() {
            info = info.push(list);
        }
        // SAFETY: info 已完整初始化；失败时不产生所有权
        let buffer = unsafe { device.device().create_buffer(&info, None) }
            .map_err(|e| format!("{what}创建失败: {e:?}"))?;

        // 码流缓冲是「GPU 写一次、CPU 逐帧读回」：优先落 HOST_CACHED 类型（本机实测
        // 存在 HOST_VISIBLE|HOST_COHERENT|HOST_CACHED 与无 CACHED 两种堆，见设备
        // 构造时的内存类型转储）；纯写暂存不偏向，write-combined 对写更合适
        let preferred = if prefer_cached {
            vk::MemoryPropertyFlags::HOST_CACHED
        } else {
            vk::MemoryPropertyFlags::empty()
        };
        let memory = match device.allocate_buffer(
            buffer,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            preferred,
            what,
        ) {
            Ok((memory, flags)) => (memory, flags),
            Err(e) => {
                // SAFETY: buffer 刚创建且尚未销毁，此处销毁一次；显存未分配成功
                unsafe { device.device().destroy_buffer(buffer, None) };
                return Err(e);
            }
        };
        let (memory, memory_flags) = memory;

        // SAFETY: memory 为刚分配并绑定的显存，属性已确认含 HOST_VISIBLE；
        // 映射起点 0 + 长度 size 不超过分配大小；此后一直保持映射，直到 Drop 解映射
        let ptr = unsafe {
            device
                .device()
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        };
        let ptr = match ptr {
            Ok(ptr) => match NonNull::new(ptr as *mut u8) {
                Some(ptr) => ptr,
                None => {
                    // SAFETY: buffer 与 memory 均已创建且尚未销毁，此处各归还一次
                    unsafe {
                        device.device().destroy_buffer(buffer, None);
                        device.device().free_memory(memory, None);
                    }
                    return Err(format!("{what}映射返回空指针"));
                }
            },
            Err(e) => {
                // SAFETY: buffer 与 memory 均已创建且尚未销毁，此处各归还一次
                unsafe {
                    device.device().destroy_buffer(buffer, None);
                    device.device().free_memory(memory, None);
                }
                return Err(format!("{what}映射失败: {e:?}"));
            }
        };

        Ok(Self {
            device: device.clone(),
            buffer,
            memory,
            ptr,
            size,
            // 设备本地 + 可缓存的堆（若存在）依旧靠非暂存直写；只有纯系统内存才换普通存储
            cached_ram: memory_flags.contains(vk::MemoryPropertyFlags::HOST_CACHED)
                && !memory_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL),
        })
    }

    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    /// 这块缓冲是否创建在 `device` 上
    ///
    /// 池里的块可能跨越一次后端重建：旧域的块属于旧设备，绝不能喂给新设备使用（池按域拦下，这里再判一次）
    /// （`vkCmdCopyBufferToImage` 要求命令缓冲与源缓冲同属一个 `VkDevice`）。
    pub(crate) fn belongs_to(&self, device: &VideoDevice) -> bool {
        Arc::ptr_eq(&self.device.inner, &device.inner)
    }

    /// 所属 `VkDevice` 的身份
    ///
    /// 设备由本结构的 `VideoDevice` 持有，块存活期间该地址不可能被复用，故可直接当域身份用
    pub(crate) fn domain_identity(&self) -> usize {
        Arc::as_ptr(&self.device.inner) as usize
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// 持久映射基址：供生产者线程直写（主机帧池的 Vulkan 块）
    ///
    /// 写入范围与生命周期由持有者（`PinnedBlock::Vulkan`）用容量检查与 RAII 归还保证
    #[inline]
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// 把 `src` 拷到缓冲起始处（主机侧，不产生任何 GPU 命令）
    ///
    /// 堆是“可缓存系统内存”时走普通存储：非暂存直写在那里反而绕过 Cache 白白多一遭
    /// （实测 1440p 整帧 14.75 MB：cached 堆普通写 ≈ 16 GB/s，非暂存写 ≈ 11 GB/s）。
    /// Write-Combined / 设备本地堆仍走 AVX2 非暂存直写，避开写分配开销。
    pub fn write(&mut self, src: &[u8]) -> Result<(), String> {
        self.write_at(0, src)
    }

    /// 从 `offset` 起写入
    ///
    /// 逐行上传（补齐行、行距与源不同）要写进缓冲中间，`write` 只能从头覆盖。
    /// 存储策略与 [`HostBuffer::write`] 完全一致。
    pub fn write_at(&mut self, offset: usize, src: &[u8]) -> Result<(), String> {
        let end = offset as u64 + src.len() as u64;
        if end > self.size {
            return Err(format!(
                "写入区间 {offset}..{end} 超出缓冲大小 {}",
                self.size
            ));
        }
        if src.is_empty() {
            return Ok(());
        }
        // SAFETY: ptr 指向持久映射的基址、覆盖 [0, size)；区间已按 size 校验；
        // 源是 Rust 侧切片，与驱动映射显存不重叠；两条分支都不要求 32B 对齐
        // （copy_streaming 自带对齐与 AVX2 特性检测）
        unsafe {
            let dst = self.ptr.as_ptr().add(offset);
            if self.cached_ram {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
            } else {
                copy_streaming(src.as_ptr(), dst, src.len());
            }
        }
        Ok(())
    }

    /// 读出前 `len` 字节
    ///
    /// 由 GPU 写入的内容必须等提交完成后才读：本后端的编码路径在等过栅栏之后才调用它。
    pub fn read(&self, len: usize) -> Result<Vec<u8>, String> {
        if len as u64 > self.size {
            return Err(format!("读取长度 {len} 超出缓冲大小 {}", self.size));
        }
        // SAFETY: ptr 覆盖 [0, size)，len 已按 size 校验；调用方保证这 len 字节
        // 此刻没有并发写者（GPU 侧提交已完成）
        let src = unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), len) };
        // `to_vec` 直接分配 + 拷贝，不像 `vec![0; len]` 那样先清零再整体覆盖一遍
        Ok(src.to_vec())
    }

    /// 读出前 `len` 字节，并在前面拼一段前缀——单次分配
    ///
    /// 给 IDR 帧用：参数集（SPS/PPS 等）拼在码流最前面，避免「read 一份 + 再拼一份」
    /// 两次分配与拷贝。前置条件同 [`HostBuffer::read`]。
    pub fn read_with_prefix(&self, prefix: &[u8], len: usize) -> Result<Vec<u8>, String> {
        if len as u64 > self.size {
            return Err(format!("读取长度 {len} 超出缓冲大小 {}", self.size));
        }
        // SAFETY: 同 `read`，ptr 覆盖 [0, size)、len 已校验、无并发写者
        let src = unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), len) };
        let mut out = Vec::with_capacity(prefix.len() + len);
        out.extend_from_slice(prefix);
        out.extend_from_slice(src);
        Ok(out)
    }

    /// 用固定字节填前 `len` 字节（测试用来污染暂存缓冲）
    #[cfg(test)]
    pub fn fill(&mut self, byte: u8, len: usize) -> Result<(), String> {
        if len as u64 > self.size {
            return Err(format!("填充长度 {len} 超出缓冲大小 {}", self.size));
        }
        // SAFETY: 同上，写入范围落在映射内
        unsafe { std::ptr::write_bytes(self.ptr.as_ptr(), byte, len) };
        Ok(())
    }
}

impl Drop for HostBuffer {
    fn drop(&mut self) {
        // 析构可能落在任意线程：先等设备空闲，确保没有在途命令还在读写这块缓冲
        let device = self.device.device();
        // SAFETY: 各句柄均由本结构创建且尚未销毁，此处各归还一次；解映射必须在
        // 释放显存之前——`vkFreeMemory` 虽会隐式解映射，但那样指针的失效时机就只
        // 写在规范里，不写在代码里
        unsafe {
            let _ = device.device_wait_idle();
            device.unmap_memory(self.memory);
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// 大块连续内存流式写入（优先 AVX2 非暂存直写，不支持或非对齐时安全回退标准 memcpy）
///
/// # Safety
///
/// 调用方必须保证：`src` 在 `[0, len)` 范围内有效可读；`dst` 在 `[0, len)` 范围内
/// 有效可写；`src` 与 `dst` 的内存区间不得重叠。函数体内逐语句的 SAFETY 论证
/// 依赖上述前置条件。
// SAFETY: `src` 必须在 `[0, len)` 范围内有效可读；
// `dst` 必须在 `[0, len)` 范围内有效可写；
// `src` 与 `dst` 的内存区间不得重叠。
#[inline]
pub(crate) unsafe fn copy_streaming(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        // 目标指针必须 32 字节对齐，且大块写入（>= 128 字节）时非暂存直写才能发挥性能优势
        if is_x86_feature_detected!("avx2") && (dst as usize).is_multiple_of(32) && len >= 128 {
            // SAFETY: 已通过运行时 CPU 特性检测与指针 32B 对齐校验；
            // 长度限制保证至少能完整写入一个 128B chunk
            unsafe { copy_streaming_avx2(src, dst, len) };
            return;
        }
    }
    // SAFETY: 不满足 AVX2 条件时标准拷贝；调用方已保证指针有效与不重叠
    unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
}

/// 128B 流水化非暂存直写拷贝（AVX2 `stream_si256`）
///
/// # Safety
///
/// 调用方必须保证：`src` 与 `dst` 各自 `[0, len)` 范围落在有效缓冲区内且互不重叠；
/// AVX2 支持已由调用方用 `is_x86_feature_detected!` 校验（`target_feature` 只声明
/// 编译期可能性）。函数体内逐语句的 SAFETY 论证依赖这两条前置条件。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn copy_streaming_avx2(src: *const u8, dst: *mut u8, len: usize) {
    use std::arch::x86_64::*;

    unsafe {
        // SAFETY: 本块为 unsafe fn 的实现体，全部语句的安全性依赖上方 # Safety
        // 前置条件（范围有效、互不重叠、AVX2 已检测），逐语句论证见行内注释
        let chunks_128 = len / 128;
        let mut s = src;
        let mut d = dst;

        // 每次迭代写入 128 字节（4 个 256 位寄存器），流水线化非暂存直写
        for _ in 0..chunks_128 {
            // SAFETY: s..s+128 在源范围内，loadu 支持任意对齐的源地址
            let v0 = _mm256_loadu_si256(s as *const __m256i);
            let v1 = _mm256_loadu_si256(s.add(32) as *const __m256i);
            let v2 = _mm256_loadu_si256(s.add(64) as *const __m256i);
            let v3 = _mm256_loadu_si256(s.add(96) as *const __m256i);

            // SAFETY: d 是 32 字节对齐且在 [0, len) 目标有效显存区内；
            // stream_si256 直接写入总线 Write-Combining 缓冲，绕过 CPU 各级 Cache
            _mm256_stream_si256(d as *mut __m256i, v0);
            _mm256_stream_si256(d.add(32) as *mut __m256i, v1);
            _mm256_stream_si256(d.add(64) as *mut __m256i, v2);
            _mm256_stream_si256(d.add(96) as *mut __m256i, v3);

            s = s.add(128);
            d = d.add(128);
        }

        let remainder = len % 128;
        let chunks_32 = remainder / 32;
        for _ in 0..chunks_32 {
            let v = _mm256_loadu_si256(s as *const __m256i);
            _mm256_stream_si256(d as *mut __m256i, v);
            s = s.add(32);
            d = d.add(32);
        }

        let trailing = remainder % 32;
        if trailing > 0 {
            // SAFETY: 末尾不足 32 字节零头标准拷贝
            std::ptr::copy_nonoverlapping(s, d, trailing);
        }

        // SAFETY: sfence 确保弱序的流式直写指令在后续 GPU 访问前全部刷新到总线
        _mm_sfence();
    }
}

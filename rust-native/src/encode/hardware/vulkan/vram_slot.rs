//! 显存直通槽位 (VRAM Slot Interop)
//!
//! 把渲染侧产出的图像以外部内存句柄（Linux `OPAQUE_FD` / Windows `OPAQUE_WIN32`）
//! 导入到本设备——像素全程留在显存里，不经过主机内存。当前只接受单平面
//! `R8G8B8A8_UNORM`；NV12 由导入后的 CSC 生成。格式写死在导入路径、描述符不携带，
//! 生产侧（Java `VramExportSet`）必须导出同一格式，否则会静默错色。
//!
//! 这里只需要 [`VideoDevice`]：设备级命令一律走 `VideoDevice::resolve`
//! （`vkGetDeviceProcAddr`），不需要 instance，也不赌 ash 扩展包装层的形态。
//!
//! **显存必须手写分配**：导入要带 `ImportMemoryFdInfoKHR` /
//! `ImportMemoryWin32HandleInfoKHR` 且使用专用分配，不能走通用分配路径。
//!
//! **失败即全败**：导入过程中任一阶段出错，`SlotImportGuard` 逆序销毁已创建的 Vulkan
//! 资源并关闭尚未被驱动接管的系统句柄，不会留下半就绪的槽位。

use ash::vk::{self, TaggedStructure};

use super::device::VideoDevice;
use crate::encode::hardware::slots::{VramSlotDescriptor, close_raw_handle};

/// 导入后的显存槽位
pub struct VramSlot {
    pub slot_idx: u32,
    pub width: u32,
    pub height: u32,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    /// 生产侧的时间线信号量；生产侧不支持时为空
    semaphore: vk::Semaphore,
    /// 本设备视角下的当前布局：首次使用前 `UNDEFINED`
    layout: vk::ImageLayout,
    device: VideoDevice,
}

impl VramSlot {
    pub fn image(&self) -> vk::Image {
        self.image
    }

    pub fn view(&self) -> vk::ImageView {
        self.view
    }

    pub fn layout(&self) -> vk::ImageLayout {
        self.layout
    }

    /// 记录本设备视角下的布局；由色彩转换路径在提交后回写
    pub(crate) fn set_layout(&mut self, layout: vk::ImageLayout) {
        self.layout = layout;
    }

    /// 本帧要等的时间线信号量与值；`None` 表示不需要等
    ///
    /// 等待交给 GPU：返回值直接进 `vkQueueSubmit2` 的 `waitSemaphoreInfos`，主机不调
    /// `vkWaitSemaphores`。这样渲染侧还在画的时候，CSC 与编码指令已经录好并提交，
    /// GPU 侧没有「渲染完 → 唤醒 CPU → CPU 再提交」的空档。
    ///
    /// 生产侧没给信号量时返回 `None`——此时调用方拿到的是「可能还没写完」的画面，
    /// 属于生产侧的降级，不是这里的错误。
    ///
    /// 代价：GPU 侧的等待没有超时。生产侧卡死会让编码队列永久阻塞——同一调用链末尾的
    /// 栅栏等待本来就是无限的，这里保持一致，不额外造一条有界的失败路径。
    pub fn frame_wait(&self, value: u64) -> Option<(vk::Semaphore, u64)> {
        if value == 0 || self.semaphore == vk::Semaphore::null() {
            return None;
        }
        Some((self.semaphore, value))
    }
}

impl Drop for VramSlot {
    fn drop(&mut self) {
        let device = self.device.device();
        unsafe {
            // SAFETY: 句柄均由本结构在 import 中创建且尚未销毁，此处各销毁一次
            let _ = device.device_wait_idle();
            if self.semaphore != vk::Semaphore::null() {
                device.destroy_semaphore(self.semaphore, None);
            }
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// 导入过程的 RAII 守卫：任一阶段出错即逆序清理
///
/// 句柄一律用 `Option` 表达「是否已创建/尚未交出」：`finish` 里 `take()` 之后守卫的
/// `Drop` 看不到它们，自然不会再销毁一次。比 `null()` 哨兵少一层「记得清空」的约定。
struct SlotImportGuard {
    device: VideoDevice,
    image: Option<vk::Image>,
    memory: Option<vk::DeviceMemory>,
    view: Option<vk::ImageView>,
    semaphore: Option<vk::Semaphore>,
    /// 尚未被驱动接管的系统句柄；`None` 表示所有权已移交
    mem_handle: Option<i64>,
    sem_handle: Option<i64>,
}

impl SlotImportGuard {
    fn new(device: &VideoDevice, mem_handle: i64, sem_handle: i64) -> Self {
        Self {
            device: device.clone(),
            image: None,
            memory: None,
            view: None,
            semaphore: None,
            mem_handle: (mem_handle > 0).then_some(mem_handle),
            sem_handle: (sem_handle > 0).then_some(sem_handle),
        }
    }

    fn finish(mut self, slot_idx: u32, width: u32, height: u32) -> VramSlot {
        let image = self.image.take().expect("外部图像已创建");
        let memory = self.memory.take().expect("外部显存已导入");
        let view = self.view.take().expect("视图已创建");
        // 信号量是可选的：生产侧可能没给，或导入失败后降级为无同步直通
        let semaphore = self.semaphore.take().unwrap_or(vk::Semaphore::null());
        // 走到 finish 说明导入已成功；仍留在守卫里的句柄表示驱动未接管
        // （信号量降级），必须在移交前关闭。
        //
        // SAFETY: 这些句柄从未交给驱动，由本结构唯一持有。
        unsafe {
            if let Some(h) = self.mem_handle.take() {
                close_raw_handle(h);
            }
            if let Some(h) = self.sem_handle.take() {
                close_raw_handle(h);
            }
        }

        VramSlot {
            slot_idx,
            width,
            height,
            image,
            memory,
            view,
            semaphore,
            layout: vk::ImageLayout::UNDEFINED,
            device: self.device.clone(),
        }
    }
}

impl Drop for SlotImportGuard {
    fn drop(&mut self) {
        let device = self.device.device();
        unsafe {
            // SAFETY: 以下句柄均由导入流程创建且尚未销毁，此处各处理一次；
            // 系统句柄仅在驱动尚未接管时关闭，避免 double-close
            if let Some(h) = self.sem_handle.take() {
                close_raw_handle(h);
            }
            if let Some(h) = self.mem_handle.take() {
                close_raw_handle(h);
            }
            if let Some(semaphore) = self.semaphore.take() {
                device.destroy_semaphore(semaphore, None);
            }
            if let Some(view) = self.view.take() {
                device.destroy_image_view(view, None);
            }
            if let Some(image) = self.image.take() {
                device.destroy_image(image, None);
            }
            if let Some(memory) = self.memory.take() {
                device.free_memory(memory, None);
            }
        }
    }
}

/// 导入一个外部显存槽位
///
/// `desc` 的系统句柄所有权会被原子接管：无论成功还是失败，调用方都不应再关闭它们。
pub fn import(device: &VideoDevice, desc: &mut VramSlotDescriptor) -> Result<VramSlot, String> {
    let slot_idx = desc.slot_idx;
    let width = desc.width;
    let height = desc.height;

    // 1. 先校验再接管：校验失败时描述符仍是句柄所有者，由其 Drop 统一关闭
    if desc.mem_handle <= 0 {
        return Err(format!(
            "槽位 {} 的内存句柄非法: {}",
            desc.slot_idx, desc.mem_handle
        ));
    }
    // 2. 原子接管句柄所有权，杜绝与调用方之间的 double-close
    let (mem_handle, sem_handle) = desc.take_handles();

    #[cfg(target_os = "linux")]
    if !device.has_extension("VK_KHR_external_memory_fd") {
        return Err("设备未启用 VK_KHR_external_memory_fd，无法导入显存".to_string());
    }
    #[cfg(target_os = "windows")]
    if !device.has_extension("VK_KHR_external_memory_win32") {
        return Err("设备未启用 VK_KHR_external_memory_win32，无法导入显存".to_string());
    }

    let mut guard = SlotImportGuard::new(device, mem_handle, sem_handle);
    let raw = device.device();

    // 3. 外部图像
    #[cfg(target_os = "linux")]
    let handle_type = vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD;
    #[cfg(target_os = "windows")]
    let handle_type = vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32;

    let mut ext_img_info = vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_type);
    let img_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        // 只要 `SAMPLED`：CSC 着色器用 texelFetch 读取。声明 `STORAGE` 会让导入在
        // 外部图本身没有 STORAGE 能力的实现上失败（BGRA8 常见如此）
        .usage(vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push(&mut ext_img_info);

    // SAFETY: img_info 及其 pNext 在调用期间存活；失败时不产生所有权
    let image = unsafe { raw.create_image(&img_info, None) }
        .map_err(|e| format!("槽位 {slot_idx} 创建外部图像失败: {e:?}"))?;
    guard.image = Some(image);

    // 4. 导入外部显存
    #[cfg(target_os = "linux")]
    {
        // SAFETY: image 为上方刚创建成功的本设备图像句柄，已被守卫接管、尚未销毁
        let requirements = unsafe { raw.get_image_memory_requirements(image) };
        let memory_type = device
            .find_memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .ok_or_else(|| {
                format!(
                    "槽位 {slot_idx} 找不到匹配的 DEVICE_LOCAL 显存类型 (bits: {:#x})",
                    requirements.memory_type_bits
                )
            })?;

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD)
            .fd(mem_handle as i32);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_size = (desc.size_bytes as u64).max(requirements.size);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(alloc_size)
            .memory_type_index(memory_type)
            .push(&mut import_info)
            .push(&mut dedicated);

        // SAFETY: 导入成功后驱动全权接管该 fd 的生命周期，因此立刻放弃本地所有权
        guard.memory = Some(
            unsafe { raw.allocate_memory(&alloc_info, None) }
                .map_err(|e| format!("槽位 {slot_idx} 导入外部显存 (FD) 失败: {e:?}"))?,
        );
        guard.mem_handle = None;
    }

    #[cfg(target_os = "windows")]
    {
        // 扩展已在上面的 `has_extension` 守卫里确认启用，表里的指针必然有效
        let table =
            ash::khr::external_memory_win32::Device::load(device.instance(), device.device());
        let mut win32_props = vk::MemoryWin32HandlePropertiesKHR::default();
        // SAFETY: mem_handle 已校验为正；属性结构在栈上
        unsafe {
            table.get_memory_win32_handle_properties(
                vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32,
                mem_handle as vk::HANDLE,
                &mut win32_props,
            )
        }
        .map_err(|e| format!("槽位 {slot_idx} 获取外部内存 Win32 句柄属性失败: {e:?}"))?;

        let memory_type = device
            .find_memory_type(
                win32_props.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .ok_or_else(|| {
                format!(
                    "槽位 {slot_idx} 找不到匹配的 DEVICE_LOCAL 显存类型 (bits: {:#x})",
                    win32_props.memory_type_bits
                )
            })?;

        // SAFETY: image 为本函数上方刚创建成功的图像句柄，已被守卫接管、尚未销毁
        let requirements = unsafe { raw.get_image_memory_requirements(image) };
        let mut import_info = vk::ImportMemoryWin32HandleInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32)
            .handle(mem_handle as vk::HANDLE);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_size = (desc.size_bytes as u64).max(requirements.size);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(alloc_size)
            .memory_type_index(memory_type)
            .push(&mut import_info)
            .push(&mut dedicated);

        // SAFETY: 导入成功后驱动接管该 HANDLE，因此立刻放弃本地所有权
        guard.memory = Some(
            unsafe { raw.allocate_memory(&alloc_info, None) }
                .map_err(|e| format!("槽位 {slot_idx} 导入外部显存 (Win32) 失败: {e:?}"))?,
        );
        guard.mem_handle = None;
    }

    // 5. 绑定
    let memory = guard.memory.expect("外部显存已导入");
    // SAFETY: image 与 memory 均已创建/导入成功，offset 由生产侧给出（专用分配应为 0）
    unsafe { raw.bind_image_memory(image, memory, desc.offset) }
        .map_err(|e| format!("槽位 {slot_idx} 绑定外部显存失败: {e:?}"))?;

    // 6. 视图
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });
    // SAFETY: view_info 已完整初始化
    let view = unsafe { raw.create_image_view(&view_info, None) }
        .map_err(|e| format!("槽位 {slot_idx} 创建视图失败: {e:?}"))?;
    guard.view = Some(view);

    // 7. 时间线信号量（可选）：成功时登记进守卫，失败时降级为无同步直通
    import_semaphore(device, &mut guard, slot_idx, sem_handle);

    Ok(guard.finish(slot_idx, width, height))
}

/// 导入外部时间线信号量；不支持或失败时降级为「无同步直通」
///
/// 降级是**有意**的：生产侧可能来自不支持信号量导出的 API（例如部分 GL 路径），
/// 此时只能靠帧间顺序保证，画面可能撕裂但不会崩。调用方通过 `semaphore()` 判断。
///
/// 成功时把句柄登记进 `guard`；失败时不登记 Vulkan 信号量，创建成功的信号量就地
/// 销毁，原始系统句柄留给 `finish` 或守卫 `Drop` 关闭。
fn import_semaphore(
    device: &VideoDevice,
    guard: &mut SlotImportGuard,
    slot_idx: u32,
    sem_handle: i64,
) {
    if sem_handle <= 0 {
        log::warn!("槽位 {slot_idx}: 未提供时间线信号量句柄，降级为无同步直通");
        return;
    }
    #[cfg(target_os = "linux")]
    {
        if !device.has_extension("VK_KHR_external_semaphore_fd") {
            log::warn!("槽位 {slot_idx}: 未启用 VK_KHR_external_semaphore_fd，跳过信号量导入");
            if let Some(h) = guard.sem_handle.take() {
                // SAFETY: 驱动未接管该 fd，由本进程关闭一次
                unsafe { close_raw_handle(h) };
            }
            return;
        }
        let raw = device.device();
        let mut type_info = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let create_info = vk::SemaphoreCreateInfo::default().push(&mut type_info);
        // SAFETY: create_info 及其 pNext 在调用期间存活
        let sem = match unsafe { raw.create_semaphore(&create_info, None) } {
            Ok(sem) => sem,
            Err(e) => {
                log::warn!("槽位 {slot_idx}: 创建时间线信号量失败: {e:?}");
                return;
            }
        };
        // 先登记：后续任何提前返回都由守卫负责销毁，不会漏
        guard.semaphore = Some(sem);

        let import_info = vk::ImportSemaphoreFdInfoKHR::default()
            .semaphore(sem)
            .flags(vk::SemaphoreImportFlags::empty())
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
            .fd(sem_handle as i32);
        // 扩展已在上面的 `has_extension` 守卫里确认启用，表里的指针必然有效
        let table =
            ash::khr::external_semaphore_fd::Device::load(device.instance(), device.device());
        // SAFETY: import_info 已完整初始化，sem 由本函数创建且尚未交出
        if let Err(e) = unsafe { table.import_semaphore_fd(&import_info) } {
            log::warn!("槽位 {slot_idx}: 导入外部时间线信号量失败: {e:?}，降级为直通");
            // 驱动没接管这个信号量：就地销毁并从守卫摘除，避免它被当成有效句柄交出去
            // SAFETY: import 失败即驱动未接管，sem 仍归本函数创建、由守卫登记销毁，
            // 此后不会二次销毁
            unsafe { raw.destroy_semaphore(sem, None) };
            guard.semaphore = None;
            return;
        }
        guard.sem_handle = None;
    }
    #[cfg(target_os = "windows")]
    {
        if !device.has_extension("VK_KHR_external_semaphore_win32") {
            log::warn!("槽位 {slot_idx}: 未启用 VK_KHR_external_semaphore_win32，跳过信号量导入");
            if let Some(h) = guard.sem_handle.take() {
                // SAFETY: 驱动未接管该句柄，由本进程关闭一次
                unsafe { close_raw_handle(h) };
            }
            return;
        }
        let raw = device.device();
        let mut type_info = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let create_info = vk::SemaphoreCreateInfo::default().push(&mut type_info);
        // SAFETY: create_info 及其 pNext 在调用期间存活
        let sem = match unsafe { raw.create_semaphore(&create_info, None) } {
            Ok(sem) => sem,
            Err(e) => {
                log::warn!("槽位 {slot_idx}: 创建时间线信号量失败: {e:?}");
                return;
            }
        };
        // 先登记：后续任何提前返回都由守卫负责销毁，不会漏
        guard.semaphore = Some(sem);

        let import_info = vk::ImportSemaphoreWin32HandleInfoKHR::default()
            .semaphore(sem)
            .flags(vk::SemaphoreImportFlags::empty())
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_WIN32)
            .handle(sem_handle as vk::HANDLE);
        // 扩展已在上面的 `has_extension` 守卫里确认启用，表里的指针必然有效
        let table =
            ash::khr::external_semaphore_win32::Device::load(device.instance(), device.device());
        // SAFETY: import_info 已完整初始化，sem 由本函数创建且尚未交出
        if let Err(e) = unsafe { table.import_semaphore_win32_handle(&import_info) } {
            log::warn!("槽位 {slot_idx}: 导入外部时间线信号量失败: {e:?}，降级为直通");
            // 驱动没接管这个信号量：就地销毁并从守卫摘除，避免它被当成有效句柄交出去
            // SAFETY: import 失败即驱动未接管，sem 仍归本函数创建、由守卫登记销毁，
            // 此后不会二次销毁
            unsafe { raw.destroy_semaphore(sem, None) };
            guard.semaphore = None;
            return;
        }
        guard.sem_handle = None;
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (device, slot_idx);
        if let Some(h) = guard.sem_handle.take() {
            // SAFETY: 本平台不支持外部信号量，句柄由本进程关闭一次
            unsafe { close_raw_handle(h) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::VideoCodec;
    use crate::encode::hardware::vulkan::codec::caps;
    use crate::encode::hardware::vulkan::context::VulkanInstance;

    fn descriptor(mem_handle: i64, sem_handle: i64) -> VramSlotDescriptor {
        VramSlotDescriptor {
            slot_idx: 0,
            mem_handle,
            sem_handle,
            width: 256,
            height: 256,
            size_bytes: 256 * 256 * 4,
            offset: 0,
            handles_taken: false,
        }
    }

    /// 非法句柄必须被立刻拦下，且句柄所有权已被接管（不会 double-close）
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_slot_import_invalid_handles() {
        let Ok(ctx) = VulkanInstance::new("Vistrel-S3") else {
            println!("Vulkan 不可用，跳过");
            return;
        };
        let device = match VideoDevice::new(&ctx, VideoCodec::H264) {
            Ok(d) => d,
            Err(e) => {
                println!("设备创建失败，跳过: {e}");
                return;
            }
        };
        let mut desc = descriptor(-1, -1);
        let res = import(&device, &mut desc);
        assert!(res.is_err(), "非法句柄必须被优雅拦截");
        assert!(desc.handles_taken, "句柄所有权必须已被接管");
    }

    /// 句柄「合法但无效」：外部图像创建成功、导入失败，守卫必须干净回收
    ///
    /// 这条能证明两件事：`ExternalMemoryImageCreateInfo` 在这台设备上真的能建出图，
    /// 以及失败路径不会留下半就绪资源（随后设备能正常销毁，不崩不挂）。
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_slot_import_bad_fd() {
        let Ok(ctx) = VulkanInstance::new("Vistrel-S3") else {
            println!("Vulkan 不可用，跳过");
            return;
        };
        let Ok(_caps) = caps::probe_codec(&ctx, VideoCodec::H264) else {
            println!("H.264 能力探测失败，跳过");
            return;
        };
        let device = match VideoDevice::new(&ctx, VideoCodec::H264) {
            Ok(d) => d,
            Err(e) => {
                println!("设备创建失败，跳过: {e}");
                return;
            }
        };
        println!("已启用扩展: {:?}", device.enabled_extensions());

        #[cfg(target_os = "linux")]
        if !device.has_extension("VK_KHR_external_memory_fd") {
            println!("设备未启用 VK_KHR_external_memory_fd，跳过");
            return;
        }

        // 999999 是一个几乎必然无效的 fd：图像能建出来，导入内存必然失败
        let mut desc = descriptor(999_999, -1);
        match import(&device, &mut desc) {
            Ok(_) => println!("意外成功：fd 999999 竟然被接受"),
            Err(e) => println!("导入按预期失败: {e}"),
        }
        assert!(desc.handles_taken, "句柄所有权必须已被接管");
    }

    /// 导出→导入真机回路：替身生产者建 3 张可导出槽位，消费设备逐槽导入
    ///
    /// 显存直通「设备侧半边」的闭环验收：外部内存导出、dedicated 导入、时间线信号量
    /// 登记全部真实发生。无外部内存能力的环境自动跳过。
    #[test]
    #[ignore = "requires GPU"]
    fn test_vram_slot_export_import_roundtrip() {
        let blank = vec![0u8; 64 * 64 * 4];
        let contents = [blank.clone(), blank.clone(), blank];
        let Ok(producer) =
            crate::encode::hardware::vulkan::vram_export::VramSlotProducer::new(&contents, 64, 64)
        else {
            println!("外部内存导出不可用，跳过");
            return;
        };
        let Ok(ctx) = VulkanInstance::new("Vistrel-S3") else {
            println!("Vulkan 不可用，跳过");
            return;
        };
        let Ok(device) = VideoDevice::new(&ctx, VideoCodec::H264) else {
            println!("设备创建失败，跳过");
            return;
        };

        let mut descriptors = producer.descriptors().expect("导出描述符");
        assert_eq!(
            descriptors.len(),
            crate::encode::hardware::vulkan::vram_export::VramSlotProducer::SLOT_COUNT
        );
        for (idx, desc) in descriptors.iter_mut().enumerate() {
            assert_eq!(desc.slot_idx, idx as u32);
            assert!(!desc.handles_taken, "句柄尚未被消费方接管");

            let mut slot = import(&device, desc).expect("合法导出句柄必须导入成功");
            assert!(desc.handles_taken, "导入即接管，失败时才归还");
            assert_ne!(slot.image(), vk::Image::null());
            assert_ne!(slot.view(), vk::ImageView::null());

            assert_eq!(slot.frame_wait(0), None, "信号值 0 不等待");
            let (semaphore, value) = slot.frame_wait(7).expect("已登记信号量必须返回等待对");
            assert_eq!(value, 7);
            assert_ne!(semaphore, vk::Semaphore::null());

            slot.set_layout(vk::ImageLayout::GENERAL);
            assert_eq!(slot.layout(), vk::ImageLayout::GENERAL);
            // drop 走逆序回收：device_wait_idle + 各句柄销毁一次
        }
    }
}

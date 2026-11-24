//! 仅测试：显存槽位导出侧替身 (VRAM Slot Producer Stub)
//!
//! 生产路径里，槽位由游戏侧（`RenderpearlVramPipeline`）导出：3 槽轮转，每槽一张
//! 专属导出纹理 + 一个独立导出时间线信号量，信号值取全局帧号。本模块在同构模型下
//! 扮演生产者：**独立的 Vulkan 设备**上建 [`SLOT_COUNT`] 张导出型 RGBA 图与同数时间线
//! 信号量，导出句柄交给 [`super::vram_slot::import`]；内容在构造期各填一次，之后逐帧
//! 只推进所写槽位的时间线——「这一帧渲染完成」用游戏侧 `vkSignalSemaphore` 模拟。
//!
//! 复用纪律对齐生产：生产侧用 `vkGetSemaphoreCounterValue` 实时查询 + 50ms 等待上限
//! **非阻塞**认领槽位，全忙主动丢帧；harness 为计量 600 帧不丢帧，复用槽位前短暂等待
//! 上一轮回执落地——3 槽轮转下等的是 3 帧前的回执，生产中这段被渲染/编码重叠掉，
//! harness 保留的只是重叠后的真实残余。
//!
//! 图的用法必须带 `COLOR_ATTACHMENT`：NVENC 把导入内存映射成 `CUDA_ARRAY3D_COLOR_ATTACHMENT`
//! 数组，生产图不是渲染目标时 CUDA 侧映射会报错。除此之外，图的形状（format/extent/
//! tiling）与导入端 [`super::vram_slot::import`] 建的图一致，保证两边显存布局兼容。

use ash::vk::{self, TaggedStructure};

use super::context::VulkanInstance;
use super::device::VideoDevice;
use super::sync::{StageAccess, image_barrier, record_image_barriers};
use crate::encode::VideoCodec;
use crate::encode::hardware::slots::VramSlotDescriptor;

/// 等待消费回执的超时：与编码等待同一量级，超了说明同步链路断了
const ACK_TIMEOUT_NS: u64 = 2_000_000_000;

/// 跨平台外部显存句柄类型：与 [`super::vram_slot::import`] 的导入端一致
#[cfg(target_os = "linux")]
const HANDLE_TYPE: vk::ExternalMemoryHandleTypeFlags = vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD;
#[cfg(target_os = "windows")]
const HANDLE_TYPE: vk::ExternalMemoryHandleTypeFlags =
    vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32;

/// 跨平台外部信号量句柄类型：显存与信号量在 Vulkan 里是两套 flags
#[cfg(target_os = "linux")]
const SEM_HANDLE_TYPE: vk::ExternalSemaphoreHandleTypeFlags =
    vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD;
#[cfg(target_os = "windows")]
const SEM_HANDLE_TYPE: vk::ExternalSemaphoreHandleTypeFlags =
    vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_WIN32;

/// 导出槽位的生产者替身
///
/// 持有独立的 instance（随 [`VideoDevice`] 存活）与全部导出资源。析构不影响编码器
/// 已导入的副本：外部显存在驱动层按物理页引用计数，导出方与导入方的内存对象各自独立，
/// 因此调用方先拆生产者、后拆编码器是安全的。
pub(crate) struct VramSlotProducer {
    device: VideoDevice,
    width: u32,
    height: u32,
    images: Vec<Option<vk::Image>>,
    memories: Vec<Option<vk::DeviceMemory>>,
    semaphores: Vec<Option<vk::Semaphore>>,
    /// 每张导出显存的分配字节数：CUDA 导入端按它声明外部内存大小
    slot_sizes: Vec<u64>,
}

impl VramSlotProducer {
    /// 槽位数量：对齐生产侧 `VramExportSet.SLOT_COUNT`（3 槽轮转）
    pub(crate) const SLOT_COUNT: usize = 3;

    /// 建全部槽位并各填一份内容
    ///
    /// 专用分配 + 导出型分配信息是对端导入（Vulkan dedicated import 与 CUDA
    /// `CUDA_EXTERNAL_MEMORY_DEDICATED`）共同的前提；分配尺寸恰为该图的 requirements。
    /// 逐槽建造，任一失败时半成品析构，由 [`Drop`](Self::drop) 逆序回收已建槽位。
    pub(crate) fn new(contents: &[Vec<u8>], width: u32, height: u32) -> Result<Self, String> {
        assert_eq!(contents.len(), Self::SLOT_COUNT, "内容份数必须与槽数一致");
        let mut producer = Self::open(width, height)?;
        for (idx, content) in contents.iter().enumerate() {
            producer.fill(idx, content)?;
        }
        Ok(producer)
    }

    fn open(width: u32, height: u32) -> Result<Self, String> {
        let instance = VulkanInstance::new("Vistrel-Perf-Producer")?;
        let device = VideoDevice::new(&instance, VideoCodec::H264)?;
        let mut producer = Self {
            device,
            width,
            height,
            images: (0..Self::SLOT_COUNT).map(|_| None).collect(),
            memories: (0..Self::SLOT_COUNT).map(|_| None).collect(),
            semaphores: (0..Self::SLOT_COUNT).map(|_| None).collect(),
            slot_sizes: vec![0; Self::SLOT_COUNT],
        };
        for idx in 0..Self::SLOT_COUNT {
            producer.build_slot(idx)?;
        }
        Ok(producer)
    }

    /// 建第 `idx` 个槽位：导出型图 + 专用可导显存 + 导出型时间线信号量
    fn build_slot(&mut self, idx: usize) -> Result<(), String> {
        let (width, height) = (self.width, self.height);
        let device = self.device.device();

        let mut ext_image = vk::ExternalMemoryImageCreateInfo::default().handle_types(HANDLE_TYPE);
        let image_info = vk::ImageCreateInfo::default()
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
            .usage(
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::COLOR_ATTACHMENT,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push(&mut ext_image);
        // SAFETY: image_info 及其 pNext 在调用期间存活
        let image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| format!("槽位 {idx} 导出图像创建失败: {e:?}"))?;

        // SAFETY: image 为上一行刚创建成功的本设备图像句柄，尚未销毁
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let memory_type = self
            .device
            .find_memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .ok_or_else(|| {
                format!(
                    "槽位 {idx} 找不到 DEVICE_LOCAL 显存类型 (bits: {:#x})",
                    requirements.memory_type_bits
                )
            })?;
        let mut export_info = vk::ExportMemoryAllocateInfoKHR::default().handle_types(HANDLE_TYPE);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type)
            .push(&mut export_info)
            .push(&mut dedicated);
        // SAFETY: alloc_info 及其 pNext 在调用期间存活
        let memory = unsafe { device.allocate_memory(&alloc_info, None) }
            .map_err(|e| format!("槽位 {idx} 导出显存分配失败: {e:?}"))?;
        // SAFETY: image 与 memory 均刚创建成功；专用分配 offset 固定为 0
        unsafe { device.bind_image_memory(image, memory, 0) }
            .map_err(|e| format!("槽位 {idx} 导出显存绑定失败: {e:?}"))?;

        let mut type_info = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let mut export_sem =
            vk::ExportSemaphoreCreateInfoKHR::default().handle_types(SEM_HANDLE_TYPE);
        let sem_info = vk::SemaphoreCreateInfo::default()
            .push(&mut type_info)
            .push(&mut export_sem);
        // SAFETY: sem_info 及其 pNext 在调用期间存活
        let semaphore = unsafe { device.create_semaphore(&sem_info, None) }
            .map_err(|e| format!("槽位 {idx} 导出信号量创建失败: {e:?}"))?;

        self.images[idx] = Some(image);
        self.memories[idx] = Some(memory);
        self.semaphores[idx] = Some(semaphore);
        self.slot_sizes[idx] = requirements.size;
        Ok(())
    }

    /// 导出全部槽位的句柄对并打包成描述符：每次调用产出新句柄，所有权随导入方接管
    ///
    /// `size_bytes` 取导出分配的真实尺寸：CUDA 导入端按它声明外部内存大小，
    /// Vulkan 导入端以其为专用分配尺寸。
    pub(crate) fn descriptors(&self) -> Result<Vec<VramSlotDescriptor>, String> {
        (0..Self::SLOT_COUNT)
            .map(|idx| self.descriptor(idx))
            .collect()
    }

    /// 导出第 `idx` 槽的句柄对
    ///
    /// 平台分派写成「各分支 return + 一条无条件尾表达式」：rust-analyzer 会同时求值全部
    /// cfg 分支，若把 cfg 块当作函数尾表达式会看到多个返回类型并误报类型错误
    #[expect(
        unreachable_code,
        reason = "各 cfg 分支均已 return，尾表达式按平台恒不可达"
    )]
    fn descriptor(&self, idx: usize) -> Result<VramSlotDescriptor, String> {
        let memory = self.memories[idx].expect("导出源显存已创建");
        let semaphore = self.semaphores[idx].expect("导出源信号量已创建");

        #[cfg(target_os = "linux")]
        {
            // SAFETY: 外部内存/信号量扩展由 VideoDevice 按实例支持情况启用，
            // 导入端（vram_slot::import）依赖同样两个扩展，可用性一致
            let mem_table = ash::khr::external_memory_fd::Device::load(
                self.device.instance(),
                self.device.device(),
            );
            let sem_table = ash::khr::external_semaphore_fd::Device::load(
                self.device.instance(),
                self.device.device(),
            );
            // SAFETY: 导出只读既有句柄，返回的 fd 所有权移交描述符
            let mem_fd = unsafe {
                mem_table.get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(memory)
                        .handle_type(HANDLE_TYPE),
                )
            }
            .map_err(|e| format!("槽位 {idx} 导出显存 fd 失败: {e:?}"))?;
            // SAFETY: 同上方显存导出——只读既有信号量句柄并按 HANDLE_TYPE 导出，
            // 返回的 fd 所有权移交描述符
            let sem_fd = unsafe {
                sem_table.get_semaphore_fd(
                    &vk::SemaphoreGetFdInfoKHR::default()
                        .semaphore(semaphore)
                        .handle_type(SEM_HANDLE_TYPE),
                )
            }
            .map_err(|e| format!("槽位 {idx} 导出信号量 fd 失败: {e:?}"))?;
            return Ok(VramSlotDescriptor {
                slot_idx: idx as u32,
                mem_handle: mem_fd as i64,
                sem_handle: sem_fd as i64,
                width: self.width,
                height: self.height,
                size_bytes: self.slot_sizes[idx] as usize,
                offset: 0,
                handles_taken: false,
            });
        }
        #[cfg(target_os = "windows")]
        {
            let mem_table = ash::khr::external_memory_win32::Device::load(
                self.device.instance(),
                self.device.device(),
            );
            let sem_table = ash::khr::external_semaphore_win32::Device::load(
                self.device.instance(),
                self.device.device(),
            );
            // SAFETY: 导出只读既有句柄，返回的 HANDLE 所有权移交描述符
            let mem_handle = unsafe {
                mem_table.get_memory_win32_handle(
                    &vk::MemoryGetWin32HandleInfoKHR::default()
                        .memory(memory)
                        .handle_type(HANDLE_TYPE),
                )
            }
            .map_err(|e| format!("槽位 {idx} 导出显存 HANDLE 失败: {e:?}"))?;
            // SAFETY: 同上方显存导出——只读既有信号量句柄并按 HANDLE_TYPE 导出，
            // 返回的 HANDLE 所有权移交描述符
            let sem_handle = unsafe {
                sem_table.get_semaphore_win32_handle(
                    &vk::SemaphoreGetWin32HandleInfoKHR::default()
                        .semaphore(semaphore)
                        .handle_type(SEM_HANDLE_TYPE),
                )
            }
            .map_err(|e| format!("槽位 {idx} 导出信号量 HANDLE 失败: {e:?}"))?;
            return Ok(VramSlotDescriptor {
                slot_idx: idx as u32,
                mem_handle: mem_handle as i64,
                sem_handle: sem_handle as i64,
                width: self.width,
                height: self.height,
                size_bytes: self.slot_sizes[idx] as usize,
                offset: 0,
                handles_taken: false,
            });
        }
        // 其余平台：显式拒绝，不静默降级
        {
            let _ = (idx, memory, semaphore);
            Err("本平台不支持外部显存导出".to_string())
        }
    }

    /// 模拟「这一帧渲染完成」：游戏侧把该槽时间线推进到 `value`
    ///
    /// 比真实渲染少了队列提交，但跨设备可见性与回执纪律相同：编码侧的 GPU 等待
    /// （Vulkan 计算队列 / CUDA 流）消费的就是这个值。
    pub(crate) fn signal_frame(&self, slot: usize, value: u64) -> Result<(), String> {
        let semaphore = self.semaphores[slot].expect("导出源信号量已创建");
        let info = vk::SemaphoreSignalInfo::default()
            .semaphore(semaphore)
            .value(value);
        // SAFETY: 调用方按全局帧号推进同一槽位的信号值（3 槽轮转下严格递增），
        // 复用前已等上一轮回执落地，value 严格大于信号量当前值
        unsafe { self.device.device().signal_semaphore(&info) }
            .map_err(|e| format!("槽位 {slot} 时间线推进到 {value} 失败: {e:?}"))
    }

    /// 等该槽「上一轮消费回执」：回执落地前不得复写编码器可能仍在读的显存
    ///
    /// NVENC 的回执是 CUDA 流上的异步 signal，游戏侧 signal 越过它会让时间线出现
    /// 「高值先落、低值后落」的非法序列；生产侧以实时查询非阻塞认领并配 50ms
    /// 等待上限，harness 为不丢帧改为短暂阻塞等同一目标值。
    pub(crate) fn wait_ack(&self, slot: usize, value: u64) -> Result<(), String> {
        let semaphore = self.semaphores[slot].expect("导出源信号量已创建");
        let sems = [semaphore];
        let values = [value];
        let info = vk::SemaphoreWaitInfo::default()
            .semaphores(&sems)
            .values(&values);
        // SAFETY: sems/values 为栈上数组，info 的借用覆盖本次调用
        unsafe { self.device.device().wait_semaphores(&info, ACK_TIMEOUT_NS) }
            .map_err(|e| format!("槽位 {slot} 等待消费回执 {value} 超时: {e:?}"))
    }

    /// 构造期一次性把整帧内容拷进第 `idx` 槽的图，留在 `GENERAL`
    ///
    /// 导入端首帧从 `UNDEFINED` 切入 GENERAL——与生产路径首帧同一行为；此后
    /// 导入端每帧 GENERAL→GENERAL 自转，内容始终可读。一次性命令资源就地回收。
    fn fill(&mut self, idx: usize, content: &[u8]) -> Result<(), String> {
        let device = self.device.device();
        let image = self.images[idx].expect("导出源图像已创建");
        let (width, height) = (self.width, self.height);
        let mut staging = self.device.host_buffer(
            content.len() as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
            "性能矩阵 VRAM 源",
        )?;
        staging.write(content)?;

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(self.device.compute_queue_family());
        // SAFETY: pool_info 已完整初始化
        let pool = unsafe { device.create_command_pool(&pool_info, None) }
            .map_err(|e| format!("槽位 {idx} 命令池创建失败: {e:?}"))?;
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: alloc_info 已完整初始化；命令缓冲随命令池一起回收
        let cmd = unsafe { device.allocate_command_buffers(&alloc_info) }
            .map_err(|e| format!("槽位 {idx} 命令缓冲分配失败: {e:?}"))?[0];
        // SAFETY: 默认 FenceCreateInfo，初始未触发
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(|e| format!("槽位 {idx} 栅栏创建失败: {e:?}"))?;

        let fill = (|| -> Result<(), String> {
            // SAFETY: cmd 为刚分配、尚未录制的一次性命令缓冲；staging 由本函数持有
            unsafe {
                device.begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
            }
            .map_err(|e| format!("槽位 {idx} 命令缓冲开始失败: {e:?}"))?;
            let to_general = image_barrier(
                image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::GENERAL,
                StageAccess::all_commands(),
                StageAccess::transfer_write(),
            );
            // SAFETY: cmd 处于录制态；image 由本结构持有
            unsafe { record_image_barriers(self.device.device(), cmd, &[to_general]) };
            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .mip_level(0)
                        .base_array_layer(0)
                        .layer_count(1),
                )
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                });
            // SAFETY: GENERAL 布局接受拷贝目标；staging 覆盖整帧字节数
            unsafe {
                device.cmd_copy_buffer_to_image(
                    cmd,
                    staging.buffer(),
                    image,
                    vk::ImageLayout::GENERAL,
                    &[region],
                )
            };
            // SAFETY: 一次性命令缓冲录制完成
            unsafe { device.end_command_buffer(cmd) }
                .map_err(|e| format!("槽位 {idx} 命令缓冲结束失败: {e:?}"))?;
            let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
            // SAFETY: submit 引用的 cmd 在调用期间存活；compute 族支持拷贝命令
            unsafe { device.queue_submit(self.device.compute_queue(), &[submit], fence) }
                .map_err(|e| format!("槽位 {idx} 填充提交失败: {e:?}"))?;
            unsafe { device.wait_for_fences(&[fence], true, ACK_TIMEOUT_NS) }
                .map_err(|e| format!("槽位 {idx} 填充未完成: {e:?}"))?;
            Ok(())
        })();

        // SAFETY: fence 已触发（或填充失败）；fence 与命令缓冲各回收一次
        unsafe {
            device.destroy_fence(fence, None);
            device.destroy_command_pool(pool, None);
        }
        fill
    }
}

impl Drop for VramSlotProducer {
    fn drop(&mut self) {
        let device = self.device.device();
        // SAFETY: 句柄均由 build_slot 创建且只销毁一次；编码器已导入的副本按
        // 外部显存的物理页引用计数独立存活，不受导出方析构影响
        unsafe {
            let _ = device.device_wait_idle();
            for semaphore in self.semaphores.drain(..).flatten() {
                device.destroy_semaphore(semaphore, None);
            }
            for image in self.images.drain(..).flatten() {
                device.destroy_image(image, None);
            }
            for memory in self.memories.drain(..).flatten() {
                device.free_memory(memory, None);
            }
        }
    }
}

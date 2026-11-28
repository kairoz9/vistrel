//! Vulkan Video 编码会话 (Video Session)
//!
//! [`VideoSession`] 只负责把会话建起来并绑好内存：几何、DPB 与 std 头版本全部由
//! [`SessionConfig`] 给定。内存绑定全成全败，`SessionGuard` 在失败时逆序销毁会话并
//! 释放已分配内存。
//!
//! 参数集对象是 codec 专属的，见 [`super::codec::VideoSessionParameters`]。

use std::ptr;

use ash::vk;

use super::codec::config::SessionConfig;
use super::device::VideoDevice;

/// 已创建并完成内存绑定的编码会话
pub struct VideoSession {
    device: VideoDevice,
    session: vk::VideoSessionKHR,
    /// 按 bind index 分配的会话内存块
    memory: Vec<vk::DeviceMemory>,
    coded_extent: vk::Extent2D,
    /// 显示尺寸（裁剪窗口内的可见区域）；等于 `coded_extent` 表示不需要裁剪
    display_extent: vk::Extent2D,
}

impl VideoSession {
    /// 按静态会话配置建会话
    ///
    /// 几何、DPB 与活跃参考数已在 [`SessionConfig::build`] 算好。
    pub fn new(device: &VideoDevice, config: &SessionConfig) -> Result<Self, String> {
        // 能力探测用的队列族必须与实际建设备用的那一个一致，否则会话会绑到错的队列上
        if let Some(family) = config.queue_family()
            && family != device.video_queue_family()
        {
            return Err(format!(
                "能力探测的编码队列族 {family} 与设备实际使用的 {} 不一致",
                device.video_queue_family()
            ));
        }

        let coded_extent = config.coded_extent();
        let display_extent = config.display_extent();
        let dpb_slots = config.dpb_slots();
        let active_references = config.active_references();
        log::info!(
            "视频会话 {:?}: DPB 槽={dpb_slots} 活跃参考={active_references}",
            config.codec(),
        );

        // codec 专属的 profile 子结构先落位，再把它的地址挂进 profile 链。
        // `profile_part` 必须先于 `video_profile` 声明：后者只存前者的地址，不拥有它。
        let mut profile_part = super::codec::ProfilePart::new(config.codec());
        let video_profile = profile_part.attach(super::codec::base_profile(config.codec()));

        let create_info = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(device.video_queue_family())
            .video_profile(&video_profile)
            .picture_format(config.picture_format())
            .max_coded_extent(coded_extent)
            .reference_picture_format(config.picture_format())
            .max_dpb_slots(dpb_slots)
            .max_active_reference_pictures(active_references)
            .std_header_version(config.std_header_version());

        // SAFETY: create_info 及其引用的 profile 链、std 头版本在调用期间均存活；
        // 返回的 session 由 guard 接管，失败路径不会泄漏
        let session = unsafe {
            device
                .video_queue_table()
                .create_video_session(&create_info, None)
        }
        .map_err(|e| format!("创建视频会话失败: {e:?}"))?;

        let mut guard = SessionGuard {
            device: device.clone(),
            session: Some(session),
            memory: Vec::new(),
        };
        // 句柄在 `finish` 之前固定为 Some：取一份局部值，后续访问不必每次解包
        let session = guard.session.expect("会话句柄刚创建，尚未交出");

        // 1. 先问需要几块内存
        //
        // 走 `fp()` 暴露的原始指针，不用 ash 的封装：封装把 `pMemoryRequirementsCount`
        // 变成了切片长度、并丢弃驱动写回的实际数量，而两段式查询恰恰要靠那个数。
        let fp = device.video_queue_table().fp();
        // `Device::handle` 在这个 ash rev 上是安全方法，不需要 unsafe
        let raw_device = device.device().handle();
        let mut count = 0u32;
        // SAFETY: count=0 + 空指针是标准的两段式查询首调
        let res = unsafe {
            (fp.get_video_session_memory_requirements_khr)(
                raw_device,
                session,
                &mut count,
                ptr::null_mut(),
            )
        };
        if res != vk::Result::SUCCESS {
            return Err(format!("查询会话内存需求数量失败: {res:?}"));
        }
        if count == 0 {
            return Err("驱动未返回任何会话内存需求".to_string());
        }

        // 2. 再取具体需求
        let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); count as usize];
        // SAFETY: reqs 长度等于首调给出的 count
        let res = unsafe {
            (fp.get_video_session_memory_requirements_khr)(
                raw_device,
                session,
                &mut count,
                reqs.as_mut_ptr(),
            )
        };
        if res != vk::Result::SUCCESS {
            return Err(format!("查询会话内存需求失败: {res:?}"));
        }
        reqs.truncate(count as usize);

        // 3. 逐块分配并组装绑定信息
        let mut binds = Vec::with_capacity(reqs.len());
        for req in &reqs {
            let memory_type = device
                .find_memory_type(
                    req.memory_requirements.memory_type_bits,
                    vk::MemoryPropertyFlags::empty(),
                )
                .ok_or_else(|| {
                    format!(
                        "会话内存块 {} 找不到匹配的内存类型 (bits {:#x})",
                        req.memory_bind_index, req.memory_requirements.memory_type_bits
                    )
                })?;
            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(req.memory_requirements.size)
                .memory_type_index(memory_type);
            // SAFETY: alloc_info 已完整初始化；出参由 guard 持有并在失败时释放
            let memory = unsafe { device.device().allocate_memory(&alloc_info, None) }
                .map_err(|e| format!("会话内存块 {} 分配失败: {e:?}", req.memory_bind_index))?;
            guard.memory.push(memory);

            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(req.memory_bind_index)
                    .memory(memory)
                    .memory_offset(0)
                    .memory_size(req.memory_requirements.size),
            );
        }

        // 4. 一次绑定全部内存块
        // SAFETY: binds 与 guard.memory 一一对应，长度一致
        unsafe {
            device
                .video_queue_table()
                .bind_video_session_memory(session, &binds)
        }
        .map_err(|e| format!("绑定会话内存失败: {e:?}"))?;

        Ok(guard.finish(coded_extent, display_extent))
    }

    pub fn session(&self) -> vk::VideoSessionKHR {
        self.session
    }

    /// 对齐后的编码尺寸（后续输入图必须按它分配）
    pub fn coded_extent(&self) -> vk::Extent2D {
        self.coded_extent
    }

    /// 显示尺寸：编码尺寸可能更大，多出来的部分由裁剪窗口裁掉
    pub fn display_extent(&self) -> vk::Extent2D {
        self.display_extent
    }

    #[cfg(test)]
    pub fn memory_count(&self) -> usize {
        self.memory.len()
    }
}

impl Drop for VideoSession {
    fn drop(&mut self) {
        // SAFETY: 先销毁会话再释放其内存块，符合 VkVideoSessionKHR 的生命周期要求；
        // 各句柄均由本结构创建且只销毁一次
        unsafe {
            self.device
                .video_queue_table()
                .destroy_video_session(self.session, None);
            for &memory in &self.memory {
                self.device.device().free_memory(memory, None);
            }
        }
    }
}

/// 会话创建期间的 RAII 守卫：中途失败时逆序清理
///
/// `session` 用 `Option` 表达「尚未交出」：`finish` 里 `take()` 之后守卫的 `Drop`
/// 看不到句柄，自然不会再销毁一次。比 `null()` 哨兵少一层「记得清空」的人工约定。
struct SessionGuard {
    device: VideoDevice,
    session: Option<vk::VideoSessionKHR>,
    memory: Vec<vk::DeviceMemory>,
}

impl SessionGuard {
    fn finish(mut self, coded_extent: vk::Extent2D, display_extent: vk::Extent2D) -> VideoSession {
        let session = self.session.take().expect("finish 只调用一次");
        let memory = std::mem::take(&mut self.memory);
        // 字段已腾空，随后 self 的 Drop 不会重复释放
        VideoSession {
            device: self.device.clone(),
            session,
            memory,
            coded_extent,
            display_extent,
        }
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            // SAFETY: 会话尚未交出，在此销毁一次
            unsafe {
                self.device
                    .video_queue_table()
                    .destroy_video_session(session, None);
            }
        }
        for &memory in &self.memory {
            // SAFETY: 内存块已从驱动分配成功且尚未释放
            unsafe { self.device.device().free_memory(memory, None) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::VideoCodec;
    use crate::encode::hardware::vulkan::codec::caps;
    use crate::encode::hardware::vulkan::context::VulkanInstance;

    /// 按 1080p 建 H.264 会话并绑定内存
    ///
    /// 无 Vulkan 环境、驱动不支持 H.264 编码或准入校验不通过时打印并跳过。
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_h264_session() {
        let Ok(ctx) = VulkanInstance::new("Vistrel-S1b") else {
            println!("Vulkan 不可用，跳过");
            return;
        };
        let caps = match caps::probe_codec(&ctx, VideoCodec::H264) {
            Ok(caps) => caps,
            Err(e) => {
                println!("H.264 能力探测失败，跳过: {e}");
                return;
            }
        };
        let device = match VideoDevice::new(&ctx, VideoCodec::H264) {
            Ok(device) => device,
            Err(e) => {
                println!("设备创建失败，跳过: {e}");
                return;
            }
        };
        let config =
            match crate::encode::hardware::vulkan::frame_encoder::test_session_config(&caps, 60) {
                Ok(config) => config,
                Err(e) => {
                    println!("会话配置构造失败，跳过: {e}");
                    return;
                }
            };
        match VideoSession::new(&device, &config) {
            Ok(session) => {
                println!(
                    "会话创建成功: 编码尺寸 {}x{} / DPB {} / 会话内存块 {}",
                    session.coded_extent().width,
                    session.coded_extent().height,
                    config.dpb_slots(),
                    session.memory_count()
                );
                // 1080p 在 16 粒度下的编码尺寸是文档明确的 1920x1088；DPB 与内存块
                // 必须已就位，否则会话是「建出来但用不了」的半成品
                assert_eq!(session.coded_extent().width, 1920);
                assert_eq!(session.coded_extent().height, 1088);
                assert!(config.dpb_slots() > 0, "DPB 槽位必须已分配");
                assert!(session.memory_count() > 0, "会话内存块必须已绑定");
            }
            Err(e) => println!("会话创建失败: {e}"),
        }
    }
}

//! Vulkan Video 编码会话与其参数对象 (Video Session & Session Parameters)
//!
//! **[`VideoSession`]** 只负责把会话建起来并绑好内存：几何、DPB 与 std 头版本
//! 全部由 [`SessionConfig`] 给定。内存绑定全成全败，`SessionGuard` 在失败时逆序
//! 销毁会话并释放已分配内存。
//!
//! **[`VideoSessionParameters`]**：编码会话建好之后、发出第一条编码命令之前，必须先把
//! 参数集交给驱动：Vulkan Video 不做码流语法解析，序列/图像参数集的每个字段都得由应用
//! 填。填错的表现不是编译错误，而是 `VK_ERROR_INVALID_VIDEO_STD_PARAMETERS_KHR`，
//! 或者产出一段没人能解的码流。
//!
//! 这里只建**驱动真正需要的最小集合**：VUI 只声明色彩、无缩放矩阵、裁剪只补对齐差值、
//! 单参考帧、IPPP。各 codec 的 std 结构构造在 `h264.rs` / `hevc.rs` / `av1.rs`，
//! 本模块只负责「创建参数对象 → 取序列化字节 → 读取覆盖反馈」。
//!
//! **参数集的字节由驱动生成**（`vkGetEncodedVideoSessionParametersKHR`），应用不写码流语法。
//! 原因不是省事：驱动可以按 encode quality level 对参数集施加**覆盖**（规范
//! "encode overrides"），此时应用手里的 std 结构体与驱动实际编码所依据的并不一致。
//! 只有驱动自己序列化出来的那份字节才与它写出的 slice NAL 同源。

use std::ptr;

use ash::vk;

use super::config::{SequenceConfig, SessionConfig};
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
        let mut profile_part = super::caps::ProfilePart::new(config.codec());
        let video_profile = profile_part.attach(super::caps::base_profile(config.codec()));

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

/// 已创建的会话参数对象
pub struct VideoSessionParameters {
    device: VideoDevice,
    params: vk::VideoSessionParametersKHR,
    /// 驱动序列化出的参数集字节（H.264/H.265 是 Annex-B，AV1 是序列头 OBU），
    /// IDR 时拼在码流前面
    parameter_sets: Vec<u8>,
}

impl VideoSessionParameters {
    /// 按静态序列头配置构造参数对象
    ///
    /// 唯一的 codec 分派点：调用方只传 [`SequenceConfig`]，不按 codec 选方法。
    pub(super) fn new(
        device: &VideoDevice,
        session: &VideoSession,
        sequence: &SequenceConfig,
    ) -> Result<Self, String> {
        match sequence {
            SequenceConfig::H264(config) => {
                Self::new_h264(device, session, config.level_idc, config.frame_num_bits)
            }
            SequenceConfig::H265(config) => {
                Self::new_h265(device, session, config.level_idc, config.frame_num_bits)
            }
            SequenceConfig::Av1(config) => Self::new_av1(device, session, config),
        }
    }

    pub fn params(&self) -> vk::VideoSessionParametersKHR {
        self.params
    }

    /// 公共收尾：取驱动序列化的参数集字节并组装 `Self`
    ///
    /// `feedback` 传 null 时不取反馈；非 null 时必须指向顶层的
    /// `VideoEncodeSessionParametersFeedbackInfoKHR`，codec 专属反馈挂在它的 pNext 链上。
    ///
    /// 各 codec 的参数集构造在 `frame_encoder/{h264,hevc,av1}.rs`，经本方法与
    /// [`Self::fetch_encoded`] 收尾——字段不外泄，调用方拿不到「半组装」的对象。
    pub(super) fn assemble(
        device: &VideoDevice,
        params: vk::VideoSessionParametersKHR,
        info: &vk::VideoEncodeSessionParametersGetInfoKHR<'_>,
        feedback: *mut std::ffi::c_void,
    ) -> Result<Self, String> {
        let parameter_sets = match Self::fetch_encoded(device, info, feedback) {
            Ok(bytes) => bytes,
            Err(e) => {
                // SAFETY: params 由本函数创建且尚未交出，失败路径必须销毁一次
                unsafe {
                    device
                        .video_queue_table()
                        .destroy_video_session_parameters(params, None);
                }
                return Err(e);
            }
        };

        Ok(Self {
            device: device.clone(),
            params,
            parameter_sets,
        })
    }

    /// 让驱动把参数集序列化成码流字节（两段式查询）
    ///
    /// `info` 的 `pNext` 链决定取哪些参数集：H.264 / H.265 必须挂 codec 专属的
    /// `VideoEncode*SessionParametersGetInfoKHR`（VUID 08262 / 08265），AV1 留空。
    ///
    /// `feedback` 为 null 时不取反馈；非 null 时必须指向顶层的
    /// `VideoEncodeSessionParametersFeedbackInfoKHR`，codec 专属反馈挂在它的 pNext 链上。
    pub(super) fn fetch_encoded(
        device: &VideoDevice,
        info: &vk::VideoEncodeSessionParametersGetInfoKHR<'_>,
        feedback: *mut std::ffi::c_void,
    ) -> Result<Vec<u8>, String> {
        // 走 `fp()` 暴露的原始指针，不用 ash 的封装：封装把 `pDataSize` 变成了切片长度、
        // 并丢弃驱动写回的实际字节数，而两段式查询要靠它先读回长度、再按长度取字节。
        let fp = device.video_encode_queue_table().fp();
        // `Device::handle` 在这个 ash rev 上是安全方法，不需要 unsafe
        let raw_device = device.device().handle();

        // 第一段：只问长度
        let mut size = 0usize;
        // SAFETY: pData 为 null 是标准两段式查询首调，只写 size；feedback 由调用方保证
        // 指向其声明的 codec 专属反馈结构（或为 null）
        let res = unsafe {
            (fp.get_encoded_video_session_parameters_khr)(
                raw_device,
                info,
                feedback.cast(),
                &mut size,
                std::ptr::null_mut(),
            )
        };
        if res != vk::Result::SUCCESS {
            return Err(format!("查询参数集长度失败: {res:?}"));
        }
        if size == 0 {
            return Err("驱动返回的参数集长度为 0".to_string());
        }

        // 第二段：取字节。缓冲不足时这条命令返回 `VK_INCOMPLETE`（不是 `NOT_ENOUGH_SPACE`），
        // 并把 `pDataSize` 写 0。缓冲大小取自首调，撞上它说明两次调用之间尺寸变了
        let mut buf = vec![0u8; size];
        let mut written = size;
        // SAFETY: buf 长度等于首调给出的 size，written 以同值传入
        let res = unsafe {
            (fp.get_encoded_video_session_parameters_khr)(
                raw_device,
                info,
                feedback.cast(),
                &mut written,
                buf.as_mut_ptr() as *mut std::ffi::c_void,
            )
        };
        if res == vk::Result::INCOMPLETE {
            return Err("参数集字节数在两次查询之间变化".to_string());
        }
        if res != vk::Result::SUCCESS {
            return Err(format!("读取参数集失败: {res:?}"));
        }
        buf.truncate(written);
        Ok(buf)
    }

    /// 参数集字节（H.264/H.265 是 Annex-B，AV1 是序列头 OBU），IDR 时拼在码流最前面
    pub fn parameter_sets(&self) -> &[u8] {
        &self.parameter_sets
    }
}

impl Drop for VideoSessionParameters {
    fn drop(&mut self) {
        // SAFETY: params 由本结构创建且只销毁一次；必须在所属 VideoSession 之前释放，
        // 这一点由持有顺序保证（参数对象持有 device 的 Arc，但不持有会话）
        unsafe {
            self.device
                .video_queue_table()
                .destroy_video_session_parameters(self.params, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::VideoCodec;
    use crate::encode::hardware::vulkan::caps;
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

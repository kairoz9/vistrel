//! `VulkanEncoder` 的构造与析构事务：会话、参数集、输入图、DPB、码流缓冲与命令资源
//!
//! 构造是全成全败的：`EncoderBuildGuard` 持有构造期创建的裸句柄，任一步失败时
//! 按逆序归还。成功构造后，`Drop for VulkanEncoder` 按同一顺序释放这些句柄；
//! 会话、图像、码流缓冲本身是 RAII 类型，由各自的 `Drop` 收尾。

use ash::vk::{self, TaggedStructure};

#[cfg(test)]
use super::FrameProfileTimings;
use super::VulkanEncoder;
#[cfg(test)]
use super::submit::TS_COUNT;
use crate::encode::hardware::vulkan::codec::CodecState;
use crate::encode::hardware::vulkan::codec::ProfilePart;
use crate::encode::hardware::vulkan::codec::VideoSessionParameters;
use crate::encode::hardware::vulkan::codec::base_profile;
use crate::encode::hardware::vulkan::codec::config::SessionConfig;
use crate::encode::hardware::vulkan::codec::rate_control::CodingControl;
use crate::encode::hardware::vulkan::csc::ColorConverter;
use crate::encode::hardware::vulkan::device::VideoDevice;
use crate::encode::hardware::vulkan::image::{DpbImages, Nv12Image};
use crate::encode::hardware::vulkan::session::VideoSession;

/// `VulkanEncoder` 构造期的资源守卫
///
/// `query_pool` / `ts_pool` / `pool` / `cmd` / `timeline_sem` 是裸句柄，`new` 里
/// 任一步失败都要逆序归还。用 `Option` 表达「已创建」：`finish` 里 `take()` 之后
/// 守卫不再持有任何东西，失败路径则靠 `Drop` 自动清理。会话、图像、暂存缓冲本身
/// 已是 RAII 类型，不在守卫里。
struct EncoderBuildGuard {
    device: VideoDevice,
    query_pool: Option<vk::QueryPool>,
    #[cfg(test)]
    ts_pool: Option<vk::QueryPool>,
    pool: Option<vk::CommandPool>,
    cmd: Option<vk::CommandBuffer>,
    timeline_sem: Option<vk::Semaphore>,
}

impl EncoderBuildGuard {
    fn new(device: &VideoDevice) -> Self {
        Self {
            device: device.clone(),
            query_pool: None,
            #[cfg(test)]
            ts_pool: None,
            pool: None,
            cmd: None,
            timeline_sem: None,
        }
    }

    fn finish(mut self) -> EncoderParts {
        EncoderParts {
            query_pool: self.query_pool.take().expect("feedback 查询池已创建"),
            // 时间戳池是可选能力：队列族不支持时它从未被创建
            #[cfg(test)]
            ts_pool: self.ts_pool.take(),
            pool: self.pool.take().expect("命令池已创建"),
            cmd: self.cmd.take().expect("命令缓冲已分配"),
            timeline_sem: self.timeline_sem.take().expect("时间线信号量已创建"),
        }
    }
}

impl Drop for EncoderBuildGuard {
    fn drop(&mut self) {
        // 构造期没有提交过任何命令，不需要 `device_wait_idle`。
        // SAFETY: 句柄均由本守卫在构造期创建且尚未交出，此处各销毁一次；
        // 命令缓冲随命令池一并回收，不需要单独销毁
        unsafe {
            let device = self.device.device();
            if let Some(sem) = self.timeline_sem.take() {
                device.destroy_semaphore(sem, None);
            }
            if let Some(pool) = self.pool.take() {
                device.destroy_command_pool(pool, None);
            }
            if let Some(query_pool) = self.query_pool.take() {
                device.destroy_query_pool(query_pool, None);
            }
            #[cfg(test)]
            if let Some(ts_pool) = self.ts_pool.take() {
                device.destroy_query_pool(ts_pool, None);
            }
        }
    }
}

/// 构造完成、可以交给 `VulkanEncoder` 的裸句柄集合
struct EncoderParts {
    query_pool: vk::QueryPool,
    #[cfg(test)]
    ts_pool: Option<vk::QueryPool>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    timeline_sem: vk::Semaphore,
}

impl VulkanEncoder {
    /// 按静态会话配置建会话、参数对象、输入图、DPB 与码流缓冲，并接管运行期码控。
    /// 能力钳制与 level 门禁已在 `SessionConfig::build` 完成，这里只做资源分配。
    ///
    /// 全成全败。CSC 是可选能力：构造失败保留为 `Err`，只影响主机帧与显存槽位路径。
    pub fn new(
        device: &VideoDevice,
        config: SessionConfig,
        coding: CodingControl,
    ) -> Result<Self, String> {
        let codec = config.codec();
        let session = VideoSession::new(device, &config)?;
        let params = VideoSessionParameters::new(device, &session, config.sequence())?;
        // 驱动只写 slice NAL，参数集要拼到 IDR 前面（字节由驱动序列化，见 session）
        let parameter_sets = params.parameter_sets().to_vec();
        let extent = session.coded_extent();
        let image_extent = config.image_extent();

        // 码流缓冲、编码输入图、DPB 图**都必须声明所属视频 profile**，否则驱动不把它们
        // 当成编码资源（VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08201 / -08206 /
        // VUID-VkVideoBeginCodingInfoKHR-pPictureResource-07240）。DPB 图尤其致命：
        // 重建帧写不进去，参考帧就是垃圾。
        //
        // profile 只存指针链（顶层 → codec 专属子结构），必须活过这三处 create 调用，
        // 所以建在这里；下面的 feedback 查询池复用同一份。
        // `profile_part` 先于 `video_profile` 声明：后者只存前者的地址。
        let mut profile_part = ProfilePart::new(codec);
        let mut video_profile = profile_part.attach(base_profile(codec));

        // 设备级 `maxImageDimension2D` 不保证等于该格式的 `maxExtent`：padding 尺寸
        // 建图失败时退回 coded extent，避免 advisory 字段把整个后端拒掉。
        let image = match Nv12Image::new_video(device, extent, image_extent, &video_profile) {
            Ok(image) => image,
            Err(first) if image_extent != extent => {
                log::warn!(
                    "{:?} padding 尺寸 {}x{} 创建输入图失败（{first}），退回 coded extent {}x{}",
                    codec,
                    image_extent.width,
                    image_extent.height,
                    extent.width,
                    extent.height
                );
                Nv12Image::new_video(device, extent, extent, &video_profile).map_err(|second| {
                    format!("输入图创建失败（padding 尺寸: {first}；coded 尺寸仍失败: {second}）")
                })?
            }
            Err(e) => return Err(e),
        };
        // CSC 是可选能力：不支持 storage image 的机器仍然能走 CPU 上传路径，
        // 因此这里不把构造失败当成整体失败，Err 原样保留
        let csc = ColorConverter::new(device, extent);
        // DPB 槽位必须由同一张图的**不同数组层**提供，除非驱动声明了
        // VK_VIDEO_CAPABILITY_SEPARATE_REFERENCE_IMAGES_BIT_KHR。本实现统一走数组层：
        // 少一次图像/显存/视图创建，也不依赖那个可选能力位。
        let dpb_slots = config.dpb_slots();
        if dpb_slots < 2 {
            return Err(format!(
                "驱动只提供 {dpb_slots} 个 DPB 槽，本实现需要 2 个（当前帧 + 1 参考帧）"
            ));
        }
        let dpb = DpbImages::new(device, extent, dpb_slots, &video_profile)?;

        // 码流缓冲：容量与对齐已由 `SessionConfig::build` 算好。
        // 主机可见 + 一致，GPU 写、CPU 读回；映射在构造时做一次，逐帧不再往返
        let bitstream = device.host_buffer_video(
            config.bitstream_size(),
            vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR,
            "码流缓冲",
            &video_profile,
        )?;

        // feedback 查询池：pNext 必须同时带 profile 与 feedback 创建信息。
        // profile 必须与本次编码的 codec 一致；`HAS_OVERRIDES` 支持时一并打开。
        let feedback_has_overrides = config.feedback_has_overrides();
        let mut feedback_flags = vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN;
        if feedback_has_overrides {
            feedback_flags |= vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_HAS_OVERRIDES;
        }
        let mut feedback_create = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default()
            .encode_feedback_flags(feedback_flags);
        // feedback_create 是叶子，直接 push；video_profile 已经挂了 h264_profile 子链，
        // push 会因 next.p_next 非空而 panic，必须用 extend 让 ash 找到子链尾节点再拼接。
        //
        // SAFETY: video_profile → codec profile 这条链上的结构体都在本函数栈上存活，
        // 布局符合 BaseOutStructure 的要求，尾节点的 p_next 为 NULL 且可写
        let pool_info = unsafe {
            vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
                .query_count(1)
                .push(&mut feedback_create)
                .extend(&mut video_profile)
        };
        // 3. 命令资源：查询池、命令池、命令缓冲、栅栏。这四个是裸句柄，任一后续步骤
        //    失败都要逆序归还，因此先交给构造期守卫，全部成功后再 `finish` 取走
        let mut res = EncoderBuildGuard::new(device);
        // SAFETY: pool_info 及其 pNext 链引用的结构体在调用期间均存活
        res.query_pool = Some(
            unsafe { device.device().create_query_pool(&pool_info, None) }
                .map_err(|e| format!("feedback 查询池创建失败: {e:?}"))?,
        );

        // GPU 时间戳探针：只在队列族声明了非零 `timestampValidBits` 时建池。
        // 位数为 0 时 `vkCmdWriteTimestamp` 属于非法用法
        // （VUID-vkCmdWriteTimestamp-timestampValidBits-00829），因此不建池、不测量，
        // 编码路径照常跑；矩阵里该列于是显示为「—」
        #[cfg(test)]
        if device.timestamp_valid_bits() > 0 {
            let ts_info = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP)
                .query_count(TS_COUNT);
            // SAFETY: ts_info 已完整初始化且无 pNext；池句柄交给构造期守卫，
            // 后续任一步失败都会在 Drop 里销毁
            res.ts_pool = Some(
                unsafe { device.device().create_query_pool(&ts_info, None) }
                    .map_err(|e| format!("时间戳查询池创建失败: {e:?}"))?,
            );
        }

        // 命令池挂在视频编码队列族上
        let cmd_pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(device.video_queue_family());
        // SAFETY: cmd_pool_info 已完整初始化
        res.pool = Some(
            unsafe { device.device().create_command_pool(&cmd_pool_info, None) }
                .map_err(|e| format!("命令池创建失败: {e:?}"))?,
        );
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(res.pool.expect("命令池刚创建"))
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: alloc_info 指向刚创建的命令池
        res.cmd = Some(
            unsafe { device.device().allocate_command_buffers(&alloc_info) }
                .map_err(|e| format!("命令缓冲分配失败: {e:?}"))?[0],
        );
        // 4. 时间线信号量（替代二值 VkFence）
        let mut type_info = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let sem_info = vk::SemaphoreCreateInfo::default().push(&mut type_info);
        // SAFETY: sem_info 及其 push 的 type_info 在调用期间存活
        res.timeline_sem = Some(
            unsafe { device.device().create_semaphore(&sem_info, None) }
                .map_err(|e| format!("时间线信号量创建失败: {e:?}"))?,
        );
        let res = res.finish();

        Ok(Self {
            device: device.clone(),
            codec,
            params,
            session,
            image,
            pending_upload: None,
            csc,
            bitstream,
            parameter_sets,
            dpb,
            next_slot: 0,
            frame_num: 0,
            codec_state: CodecState::new(codec),
            coding,
            gop_length: config.gop_length(),
            gop_remaining_frames: config.gop_remaining_frames(),
            feedback_has_overrides,
            overrides_seen: false,
            intra_refresh_cycle: config.intra_refresh_cycle(),
            intra_refresh_index: 0,
            query_pool: res.query_pool,
            #[cfg(test)]
            ts_pool: res.ts_pool,
            pool: res.pool,
            cmd: res.cmd,
            timeline_sem: res.timeline_sem,
            timeline_value: 0,
            poisoned: false,
            #[cfg(test)]
            last_timings: FrameProfileTimings::default(),
        })
    }
}

impl Drop for VulkanEncoder {
    fn drop(&mut self) {
        // SAFETY: 先等设备空闲再逆序销毁；各句柄均由本结构创建且只销毁一次
        let device = self.device.device();
        unsafe {
            let _ = device.device_wait_idle();
            device.destroy_semaphore(self.timeline_sem, None);
            device.destroy_command_pool(self.pool, None);
            device.destroy_query_pool(self.query_pool, None);
            #[cfg(test)]
            if let Some(ts_pool) = self.ts_pool.take() {
                device.destroy_query_pool(ts_pool, None);
            }
        }
        // 码流缓冲（含持久映射与显存）由 `HostBuffer` 自己的 Drop 归还；
        // session / params / image 同样各自 Drop，顺序由字段声明顺序保证
    }
}

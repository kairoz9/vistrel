//! 单帧视频编码 (Encode One Frame)
//!
//! 把「一条编码命令」需要的全部东西装在一起：码流缓冲、feedback 查询池、命令缓冲，
//! 以及 begin/encode/end 三条视频命令。
//!
//! 三个 codec 的「std 结构 + 会话参数装配 + 编码命令装配」各自收在 `codec/` 下的
//! `h264` / `h265` / `av1`；本模块持有 codec 共享的机器（结构体、构造、公开入口），
//! 一帧的执行管线在 `submit` 子模块，测试夹具在 `test_support` 子模块。
//!
//! 码流缓冲是主机可见的：构造时映射一次，逐帧只做 memcpy 读取，省掉一次拷贝。
//!
//! 三个 codec 的差别只在「怎么构造编码命令」（见 `codec/` 下的 `h264` / `h265` / `av1`），
//! 提交与读回对它们完全一致。

// `push` 是 `TaggedStructure` trait 的方法，不导入就无法链 pNext
use ash::vk::{self, TaggedStructure};

use super::codec::CodecGopRemaining;
use super::codec::VideoSessionParameters;
use super::codec::config::SessionConfig;
use super::codec::rate_control::CodingControl;
use super::codec::{ProfilePart, base_profile};
use super::csc::ColorConverter;
use super::device::{HostBuffer, VideoDevice};
use super::image::{DpbImages, Nv12Image};
use super::session::VideoSession;
use super::vram_slot::VramSlot;
use crate::encode::VideoCodec;

mod submit;
#[cfg(test)]
mod test_support;

#[cfg(test)]
pub(in crate::encode::hardware::vulkan) use self::test_support::{
    make_noisy_frame, test_frame_encoder, test_profile, test_session_config,
    test_session_config_with,
};

#[cfg(test)]
use self::submit::TS_COUNT;

/// 构造指向整张图的 picture resource
///
/// `base_array_layer` 是 DPB 槽位在数组图里的层号；输入图是单层图，传 0。
/// 只填句柄与尺寸，不借用任何外部数据，因此生命周期可以取 `'static`
pub(in crate::encode::hardware::vulkan) fn picture_resource(
    view: vk::ImageView,
    extent: vk::Extent2D,
    base_array_layer: u32,
) -> vk::VideoPictureResourceInfoKHR<'static> {
    vk::VideoPictureResourceInfoKHR::default()
        .coded_offset(vk::Offset2D { x: 0, y: 0 })
        .coded_extent(extent)
        .base_array_layer(base_array_layer)
        .image_view_binding(view)
}

/// 本帧的 intra-refresh 参数；`None` 表示未启用
///
/// KEY / IDR 帧全帧内，索引固定为 0；其余帧按周期推进（见 `finish_frame`）。
/// 周期必须与建会话时选定的一致，否则驱动按它切分条带、我们按另一个周期推进，
/// 会有区域永远轮不到刷新。
pub(in crate::encode::hardware::vulkan) fn intra_refresh_info<'a>(
    cycle: Option<u32>,
    index: u32,
) -> Option<vk::VideoEncodeIntraRefreshInfoKHR<'a>> {
    cycle.map(|cycle| {
        vk::VideoEncodeIntraRefreshInfoKHR::default()
            .intra_refresh_cycle_duration(cycle)
            .intra_refresh_index(index)
    })
}

/// 组装 begin 作用域的槽位列表：历史参考槽按真实槽号在前，setup 槽以 -1 追加
///
/// 排布只允许这一种（照抄 FFmpeg `vulkan_encode.c`）：
/// - 参考槽都已激活，必须按真实槽号声明，否则 VUID-vkCmdBeginVideoCodingKHR-slotIndex-07239；
/// - 当前帧的输出槽尚未激活，`slotIndex = -1` 不受「必须是活动槽位」约束，而
///   `pPictureResource` 仍要给出，`vkCmdEncodeVideoKHR` 的 `pSetupReferenceSlot` 才能与它
///   匹配（VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08215）。
///
/// 调用方按 IDR 与否决定是否携带参考槽；本实现每帧至多 1 个参考槽 + 1 个 setup 槽，
/// 栈上定长两格即够，由各 codec 直接把刚构造好的槽位传进来——不必再从 encode_info 的
/// pNext 链回走一遍
/// begin 作用域要声明的槽位
///
/// ash 的 `Extends` 只接受连续切片，「数组 + 有效长度」这个表示无法消灭；关进 `slots()`
/// 之后，调用点不必再各自写一遍 `&scope.0[..scope.1]`。
pub(in crate::encode::hardware::vulkan) struct Scope<'a> {
    storage: [vk::VideoReferenceSlotInfoKHR<'a>; 2],
    len: usize,
}

impl<'a> Scope<'a> {
    fn slots(&self) -> &[vk::VideoReferenceSlotInfoKHR<'a>] {
        &self.storage[..self.len]
    }
}

pub(in crate::encode::hardware::vulkan) fn begin_scope<'a>(
    reference: Option<&vk::VideoReferenceSlotInfoKHR<'a>>,
    setup_slot: &vk::VideoReferenceSlotInfoKHR<'a>,
) -> Scope<'a> {
    let mut storage = [vk::VideoReferenceSlotInfoKHR::default(); 2];
    let mut len = 0;
    if let Some(reference) = reference {
        storage[len] = *reference;
        len += 1;
    }
    let mut setup = *setup_slot;
    setup.slot_index = -1;
    storage[len] = setup;
    len += 1;
    Scope { storage, len }
}

/// 本帧待录制的输入图上传
///
/// 上传不再自己提交：它被录进编码命令缓冲，与编码命令同处一次提交，中间没有 CPU 往返。
/// 因此「本帧的像素从哪来」必须由公开入口先登记，再由 `submit_encode` 在录制时消费。
///
/// 每个公开入口都会先登记再进 `encode_prepared`，所以即使 `encode_prepared` 中途失败，
/// 残留值也一定会被下一次调用覆盖。
enum PendingUpload {
    /// 主机 NV12 已写进暂存缓冲，只等录制拷贝
    Staging,
    /// CSC 已把两张平面图写好，等待拷进 NV12 的两个平面
    Planes { y: vk::Image, uv: vk::Image },
}

/// 一帧编码所需的全部常驻资源
pub struct FrameEncoder {
    device: VideoDevice,
    /// 本编码器服务的 codec：决定每帧构造哪套 std 结构与码控
    codec: VideoCodec,
    /// 必须先于 `session` 销毁：参数对象依赖会话存活
    params: VideoSessionParameters,
    session: VideoSession,
    pub(in crate::encode::hardware::vulkan) image: Nv12Image,
    /// 本帧待录制的上传；`None` 表示直接复用图像现有内容
    pending_upload: Option<PendingUpload>,
    /// RGBA → NV12 的 GPU 转换器；设备不支持所需 storage image 格式时为 `Err`，
    /// 原因随之保留，供 NV12 主机帧与显存槽位两条路径报错
    csc: Result<ColorConverter, String>,
    /// 码流缓冲：主机可见 + 一致，构造时映射一次，逐帧直接 memcpy 读取
    pub(in crate::encode::hardware::vulkan) bitstream: HostBuffer,
    /// SPS + PPS 的 Annex-B 字节，IDR 时拼在码流最前面
    parameter_sets: Vec<u8>,
    /// DPB：一张多层数组图，槽位用数组层区分（当前帧写一层，参考另一层）
    pub(in crate::encode::hardware::vulkan) dpb: DpbImages,
    /// 下一帧写入哪个 DPB 槽
    pub(in crate::encode::hardware::vulkan) next_slot: usize,
    /// 已编码帧数（IDR 时归零）
    pub(in crate::encode::hardware::vulkan) frame_num: u32,
    /// codec 专属的逐帧状态
    pub(in crate::encode::hardware::vulkan) codec_state: super::codec::CodecState,
    /// 运行期码控状态：当前码率/帧率、上次下发值、是否需要重发
    coding: CodingControl,
    /// GOP 长度（帧）：`frame_num` 到达它就强制重出 IDR，也是码控里的 `idrPeriod`
    ///
    /// intra-refresh 打开时取大值，靠渐进刷新代替周期性 IDR。
    pub(in crate::encode::hardware::vulkan) gop_length: u32,
    /// 驱动要求/偏好 begin 时给出 GOP 剩余帧数（`requires || prefers`）
    pub(in crate::encode::hardware::vulkan) gop_remaining_frames: bool,
    /// 反馈查询是否包含 `HAS_OVERRIDES`（驱动支持时）
    pub(in crate::encode::hardware::vulkan) feedback_has_overrides: bool,
    /// 是否已经报告过逐帧覆写，避免每帧刷日志
    overrides_seen: bool,
    /// 已启用的 intra-refresh 周期；`None` 表示关闭
    pub(in crate::encode::hardware::vulkan) intra_refresh_cycle: Option<u32>,
    /// 下一帧的 intra-refresh 索引，按周期循环
    pub(in crate::encode::hardware::vulkan) intra_refresh_index: u32,
    query_pool: vk::QueryPool,
    /// GPU 时间戳查询池；队列族不支持时间戳或非测试构建时为 `None`
    #[cfg(test)]
    ts_pool: Option<vk::QueryPool>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    timeline_sem: vk::Semaphore,
    timeline_value: u64,
    /// 等待超时的永久毒化标记（毒化后的行为约束与拆构风险见本文件「执行管线」一节）
    poisoned: bool,
    #[cfg(test)]
    last_timings: FrameProfileTimings,
}

/// 帧编码内部关键阶段耗时记录（仅在测试和性能剖析中使用）
#[cfg(test)]
#[derive(Default, Debug, Clone, Copy)]
pub struct FrameProfileTimings {
    pub stage_ns: u64,
    pub submit_ns: u64,
    pub wait_fence_ns: u64,
    /// GPU 侧整个视频编码作用域的耗时（不含上传与屏障）
    pub gpu_scope_ns: u64,
    /// GPU 侧 `vkCmdEncodeVideoKHR` 本身的耗时
    pub gpu_encode_ns: u64,
    /// 整帧 GPU 工作：命令缓冲开头到编码作用域结束，含输入图上传与 CSC
    pub gpu_frame_ns: u64,
}

/// `FrameEncoder` 构造期的资源守卫
///
/// `query_pool` / `pool` / `cmd` / `timeline_sem` 是裸句柄，`new` 里任一步失败都要逆序归还。
/// 用 `Option` 表达「已创建」：`finish` 里 `take()` 之后守卫不再持有任何东西，
/// 失败路径则靠 `Drop` 自动清理。会话、图像、暂存缓冲本身已是 RAII 类型，不在守卫里。
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

/// 构造完成、可以交给 `FrameEncoder` 的裸句柄集合
struct EncoderParts {
    query_pool: vk::QueryPool,
    #[cfg(test)]
    ts_pool: Option<vk::QueryPool>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    timeline_sem: vk::Semaphore,
}

impl FrameEncoder {
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
            codec_state: super::codec::CodecState::new(codec),
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

    pub fn extent(&self) -> vk::Extent2D {
        self.image.extent()
    }

    /// 本帧要挂进 begin 的 GOP 剩余帧数结构；驱动既不强求也不偏好时返回 `None`
    pub(in crate::encode::hardware::vulkan) fn gop_remaining<'a>(
        &self,
        frame_num: u32,
        is_key: bool,
    ) -> Option<CodecGopRemaining<'a>> {
        super::codec::gop_remaining(self, frame_num, is_key)
    }

    /// GPU 色彩转换是否可用
    ///
    /// 为假时持有 `VkBuffer` 的主机帧也走不了零拷贝平面摊开，调用方应改走
    /// `encode_frame` 的暂存上传（帧内容已是 NV12，无需任何换算）。
    pub fn has_gpu_csc(&self) -> bool {
        self.csc.is_ok()
    }

    /// 是否因提交等待超时毒化；毒化后的行为约束与拆构风险见本文件「执行管线」一节
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// 目标帧率（帧/秒）
    pub(in crate::encode::hardware::vulkan) fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// 运行期码控状态：码率/帧率、已下发快照与量化范围
    pub(in crate::encode::hardware::vulkan) fn coding(&self) -> &CodingControl {
        &self.coding
    }

    /// 显示尺寸（= 协商档位）；只有 AV1 序列头与显存槽位几何校验需要
    pub(in crate::encode::hardware::vulkan) fn display_extent(&self) -> vk::Extent2D {
        self.session.display_extent()
    }

    pub(in crate::encode::hardware::vulkan) fn fps(&self) -> u32 {
        self.coding.fps()
    }

    /// 目标码率（bps）
    pub(in crate::encode::hardware::vulkan) fn bitrate_bps(&self) -> u64 {
        self.coding.bitrate_bps()
    }

    /// 运行期改码率：下一帧随码控命令重发，不需要重建会话
    ///
    /// 钳制与脏标记由 [`CodingControl`] 负责。
    pub fn set_bitrate(&mut self, bps: u64) {
        self.coding.set_bitrate(bps);
    }

    #[cfg(test)]
    pub fn bitrate(&self) -> u64 {
        self.bitrate_bps()
    }

    /// 运行期改帧率：码控每帧都带 `frame_rate_numerator`，改字段 + 置脏即可生效
    ///
    /// **不重建会话**：几何没变时重建会清空显存槽位表，而采集侧在尺寸不变时不会
    /// 重建管线、也就不会重新注册槽位，槽位就此永久失效（表现为
    /// 「VRAM slot N not found」+ 画面冻结）。
    ///
    /// GOP 保持建会话时的值不变：`frame_num` 的位宽写在 SPS 里，能改的只有
    /// 「多久强制一次 IDR」这个软件判断；改小会让 IDR 更频繁，改大可能超出位宽。
    /// 帧率变化时 IDR 的**时间**间隔随之改变（60fps 下 1 秒 → 120fps 下 0.5 秒），
    /// 这是可接受的：码率尖峰更密，但不会出错。
    pub fn set_fps(&mut self, fps: u32) {
        self.coding.set_fps(fps);
    }

    /// 上传一帧 NV12 并编码出一个 IDR
    #[cfg(test)]
    pub fn encode_idr(&mut self, nv12: &[u8]) -> Result<(Vec<u8>, bool), String> {
        self.encode_frame(nv12, true)
    }

    /// 编码一帧 CPU 侧 NV12；`force_idr` 为真、或帧号即将溢出时出 IDR
    ///
    /// 像素先进暂存缓冲，拷贝与编码录进同一次提交——本帧只有末尾一个同步点。
    pub fn encode_frame(
        &mut self,
        nv12: &[u8],
        force_idr: bool,
    ) -> Result<(Vec<u8>, bool), String> {
        #[cfg(test)]
        let t_stage = std::time::Instant::now();
        self.image.stage(nv12)?;
        #[cfg(test)]
        {
            self.last_timings.stage_ns = t_stage.elapsed().as_nanos() as u64;
        }
        self.pending_upload = Some(PendingUpload::Staging);
        self.encode_prepared(force_idr)
    }

    #[cfg(test)]
    pub fn last_timings(&self) -> FrameProfileTimings {
        self.last_timings
    }

    /// 从生产者已写好的主机缓冲编码一帧：零 CPU 拷贝（主机帧池的 Vulkan 块）
    ///
    /// 生产者把 NV12 直接写进了持久映射缓冲，主机侧没有整帧 memcpy：GPU 只把 Y 与交织 UV
    /// 摊进两张平面图（`copy_nv12_from_buffer`），不需要色彩转换。缓冲内容须保持到本帧编码
    /// 完成，主机帧池的归还时机恰好满足（槽在编码返回后才可能再次被取用）
    pub fn encode_nv12_buffer_frame(
        &mut self,
        buffer: vk::Buffer,
        force_idr: bool,
    ) -> Result<(Vec<u8>, bool), String> {
        let csc = match self.csc.as_mut() {
            Ok(csc) => csc,
            Err(why) => {
                return Err(format!("本设备无法在 GPU 上摊开 NV12 平面: {why}"));
            }
        };
        csc.copy_nv12_from_buffer(buffer)?;
        let (y, uv) = csc.plane_images();
        self.pending_upload = Some(PendingUpload::Planes { y, uv });
        self.encode_prepared(force_idr)
    }

    /// 从显存槽位编码一帧：像素全程留在显存，不经过主机内存
    ///
    /// `sync_value` 是生产侧时间线信号量的值；槽位没有信号量时传 0（直接放行）。
    /// 等待本身在 GPU 上完成——它作为 `waitSemaphoreInfos` 挂到 CSC 的计算队列提交上，
    /// 主机不在渲染完成前阻塞。
    /// 返回后槽位的布局状态被更新，下一帧自动沿用。
    pub fn encode_vram_frame(
        &mut self,
        slot: &mut VramSlot,
        sync_value: u64,
        force_idr: bool,
    ) -> Result<(Vec<u8>, bool), String> {
        // 槽位几何必须与编码尺寸一致：分派范围按编码尺寸算，尺寸不符会越界读显存
        let extent = self.image.extent();
        // 槽位可以是采集尺寸（CSC 按源尺寸夹取，把补齐边复制出来），也可以是编码尺寸；
        // 其余尺寸一律拒绝——着色器只保证「源不大于目标」时的补齐语义
        let slot_extent = vk::Extent2D {
            width: slot.width,
            height: slot.height,
        };
        let display = self.session.display_extent();
        if slot_extent != extent && slot_extent != display {
            return Err(format!(
                "槽位 {} 尺寸 {}x{} 既不是编码尺寸 {}x{} 也不是采集尺寸 {}x{}：\
                 显存直通只接受这两者",
                slot.slot_idx,
                slot.width,
                slot.height,
                extent.width,
                extent.height,
                display.width,
                display.height
            ));
        }
        let csc = match self.csc.as_mut() {
            Ok(csc) => csc,
            Err(why) => {
                return Err(format!("本设备无法在 GPU 上做 RGBA→NV12 转换: {why}"));
            }
        };
        // 生产侧写完这一帧之前，CSC 不能读源图。等待交给计算队列，主机不在这里阻塞
        let wait = slot.frame_wait(sync_value);
        let new_layout =
            csc.convert_from_view(slot.image(), slot.view(), slot.layout(), slot_extent, wait)?;
        let (y, uv) = csc.plane_images();
        slot.set_layout(new_layout);
        self.pending_upload = Some(PendingUpload::Planes { y, uv });
        self.encode_prepared(force_idr)
    }

    /// 编码一帧：输入图已就绪，按 codec 分派
    ///
    /// 返回（NALU 数据, 是否关键帧）。码流长度由 feedback 查询给出，不靠猜。
    fn encode_prepared(&mut self, force_idr: bool) -> Result<(Vec<u8>, bool), String> {
        super::codec::encode_prepared(self, force_idr)
    }
}

impl Drop for FrameEncoder {
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

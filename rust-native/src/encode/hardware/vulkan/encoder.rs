//! Vulkan Video 编码引擎 (Vulkan Encode Engine)
//!
//! 跨帧持有一路编码会话的全部常驻资源：`VideoSession`、参数集、输入图、DPB、码流缓冲、
//! 命令缓冲与时间线信号量；每次调用把一帧输入编码成码流。
//!
//! 三个 codec 的「std 结构 + 会话参数装配 + 编码命令装配」各自收在 `codec/` 下的
//! `h264` / `h265` / `av1`；本模块持有 codec 共享的机器（结构体、公开入口）。
//! 构造与资源归还在 `builder` 子模块，一帧的执行管线在 `submit` 子模块，测试夹具在
//! `test_support` 子模块。
//!
//! 码流缓冲是主机可见的：构造时映射一次，逐帧只做 memcpy 读取，省掉一次拷贝。
//!
//! 三个 codec 的差别只在「怎么构造编码命令」（见 `codec/` 下的 `h264` / `h265` / `av1`），
//! 提交与读回对它们完全一致。

use ash::vk;

use super::codec::CodecGopRemaining;
use super::codec::VideoSessionParameters;
use super::codec::rate_control::CodingControl;
use super::csc::ColorConverter;
use super::device::{HostBuffer, VideoDevice};
use super::image::{DpbImages, Nv12Image};
use super::session::VideoSession;
use super::vram_slot::VramSlot;
use crate::encode::VideoCodec;

mod builder;
mod submit;
#[cfg(test)]
mod test_support;

#[cfg(test)]
pub(in crate::encode::hardware::vulkan) use self::test_support::{
    make_noisy_frame, test_profile, test_session_config, test_session_config_with,
    test_vulkan_encoder,
};

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

/// 一路 Vulkan Video 编码：跨帧持有会话资源，每次调用产出一帧码流
pub struct VulkanEncoder {
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
    /// 等待超时的永久毒化标记（毒化后的行为约束与拆构风险见 `submit` 子模块文档）
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

impl VulkanEncoder {
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

    /// 是否因提交等待超时毒化；毒化后的行为约束与拆构风险见 `submit` 子模块文档
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

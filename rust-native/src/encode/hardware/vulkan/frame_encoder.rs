//! 单帧视频编码 (Encode One Frame)
//!
//! 把「一条编码命令」需要的全部东西装在一起：码流缓冲、feedback 查询池、命令缓冲，
//! 以及 begin/encode/end 三条视频命令。
//!
//! 三个 codec 的「std 结构 + 会话参数装配 + 编码命令装配」各自收在子模块
//! `h264` / `hevc` / `av1`；本模块持有全部 codec 共享的机器（结构体、构造、公开入口、
//! 逐帧状态与码控、分派），一帧的执行管线在 [`execute`]。
//!
//! 码流缓冲是主机可见的：构造时映射一次，逐帧只做 memcpy 读取，省掉一次拷贝。

// `push` 是 `TaggedStructure` trait 的方法，不导入就无法链 pNext
use ash::vk::{self, TaggedStructure};

use super::caps::{CodecCaps, CodecSpecific, ProfilePart, base_profile};
use super::csc::ColorConverter;
use super::device::{HostBuffer, VideoDevice};
use super::image::{DpbImages, Nv12Image};
use super::session::{VideoSession, VideoSessionParameters};
use super::vram_slot::VramSlot;
use crate::encode::{StreamingProfile, VideoCodec, nv12_len};

mod av1;
mod execute;
mod h264;
mod hevc;

#[cfg(test)]
use self::execute::TS_COUNT;

/// 构造指向整张图的 picture resource
///
/// `base_array_layer` 是 DPB 槽位在数组图里的层号；输入图是单层图，传 0。
/// 只填句柄与尺寸，不借用任何外部数据，因此生命周期可以取 `'static`
fn picture_resource(
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

/// 让一个 GOP 内的 `frame_num` 不重复所需的最小位宽（`log2(MaxFrameNum)`）
fn frame_num_bits(gop_length: u32) -> u32 {
    let needed = gop_length.saturating_add(1).max(16);
    // ceil_log2：needed 不是 2 的幂时也要装得下
    let bits = 32 - needed.saturating_sub(1).leading_zeros();
    bits.clamp(4, 12)
}

/// 本帧的 intra-refresh 参数；`None` 表示本轮未启用
///
/// KEY / IDR 帧本身全帧内，索引无意义，用 0 表示从头开始；其余帧按周期推进（索引在
/// `finish_frame` 里推进）。`intra_refresh_cycle_duration` 必须与 `set_intra_refresh`
/// 设的值一致，否则驱动按它切分条带、我们按另一个周期推进，会有区域永远轮不到刷新
/// ——表现是误差只增不减。
fn intra_refresh_info<'a>(
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
struct Scope<'a> {
    storage: [vk::VideoReferenceSlotInfoKHR<'a>; 2],
    len: usize,
}

impl<'a> Scope<'a> {
    fn slots(&self) -> &[vk::VideoReferenceSlotInfoKHR<'a>] {
        &self.storage[..self.len]
    }
}

fn begin_scope<'a>(
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

/// 码控的量化范围：H.26x 用 QP，AV1 用量化索引，两者数值域不同
#[derive(Debug, Clone, Copy)]
enum QuantCaps {
    Qp { min: i32, max: i32 },
    QIndex { min: u32, max: u32 },
}

impl QuantCaps {
    /// 从能力快照取出当前 codec 的量化范围
    fn from_caps(caps: &CodecCaps) -> Self {
        match &caps.specific {
            CodecSpecific::H264(specific) | CodecSpecific::H265(specific) => Self::Qp {
                min: specific.min_qp,
                max: specific.max_qp,
            },
            CodecSpecific::Av1(specific) => Self::QIndex {
                min: specific.min_q_index,
                max: specific.max_q_index,
            },
        }
    }
}

/// 一帧编码所需的全部常驻资源
pub struct FrameEncoder {
    device: VideoDevice,
    /// 本编码器服务的 codec：决定每帧构造哪套 std 结构与码控
    codec: VideoCodec,
    /// 必须先于 `session` 销毁：参数对象依赖会话存活
    params: VideoSessionParameters,
    session: VideoSession,
    image: Nv12Image,
    /// 本帧待录制的上传；`None` 表示直接复用图像现有内容
    pending_upload: Option<PendingUpload>,
    /// RGBA → NV12 的 GPU 转换器；设备不支持所需 storage image 格式时为 `Err`，
    /// 原因随之保留，供 NV12 主机帧与显存槽位两条路径报错
    csc: Result<ColorConverter, String>,
    /// 码流缓冲：主机可见 + 一致，构造时映射一次，逐帧直接 memcpy 读取
    bitstream: HostBuffer,
    /// SPS + PPS 的 Annex-B 字节，IDR 时拼在码流最前面
    parameter_sets: Vec<u8>,
    /// DPB：一张多层数组图，槽位用数组层区分（当前帧写一层，参考另一层）
    dpb: DpbImages,
    /// 下一帧写入哪个 DPB 槽
    next_slot: usize,
    /// 已编码帧数（IDR 时归零）
    frame_num: u32,
    /// 下一个 IDR 要用的 `idr_pic_id`
    ///
    /// H.264 7.4.3 要求**相邻两个 IDR 的 `idr_pic_id` 必须不同**。固定为 0 会让解码器
    /// 把第二个及之后的 IDR 当成「同一张图」丢掉。只有两个连续值需要互异，交替 0/1 就够。
    idr_pic_id: u16,
    /// 上一帧是否为 IDR
    ///
    /// 参考槽里装的是**上一帧**，它的 `primary_pic_type` 必须反映那一帧的实际类型。
    /// 用当前帧的 `is_idr` 去填是错的：IDR 之后的第一帧参考的是 IDR，填成 P 会让驱动
    /// 按 P 帧去管理这个参考槽（H.264 里 IDR 会清空 DPB 并重置 POC，与 P 帧行为不同）。
    last_was_idr: bool,
    /// 协商档位：帧率与码率的唯一事实源，运行期 `set_bitrate` / `set_fps` 直改它
    profile: StreamingProfile,
    /// 驱动声明的码率上限（`caps.max_bitrate`）；0 表示未设上限。请求超出属于非法用法
    /// （码控层的 averageBitrate / maxBitrate 不得超过它），运行期改码率时按它钳制
    max_bitrate: u64,
    /// GOP 长度（帧）：`frame_num` 到达它就强制重出 IDR，也是码控里的 `idrPeriod`
    ///
    /// intra-refresh 打开时取大值，靠渐进刷新代替周期性 IDR。
    gop_length: u32,
    /// AV1 码控的量化索引范围 / H.26x 的 QP 范围（来自能力快照）
    quant: QuantCaps,
    /// 驱动要求/偏好 begin 时给出 GOP 剩余帧数（`requires || prefers`）
    gop_remaining_frames: bool,
    /// 反馈查询是否包含 `HAS_OVERRIDES`（驱动支持时）
    feedback_has_overrides: bool,
    /// 是否已经报告过逐帧覆写，避免每帧刷日志
    overrides_seen: bool,
    /// 驱动声明的最大 intra-refresh 周期；0 表示不支持
    max_intra_refresh_cycle: u32,
    /// 已启用的 intra-refresh 周期；`None` 表示关闭
    intra_refresh_cycle: Option<u32>,
    /// 下一帧的 intra-refresh 索引，按周期循环
    intra_refresh_index: u32,
    /// 是否已下发过首次 RESET
    rate_control_initialized: bool,
    /// 码控是否需要重新下发（首帧或码率/帧率变更后）
    rate_control_dirty: bool,
    /// 上次**真正配置进会话**的码率与帧率
    ///
    /// begin 的 `VkVideoEncodeRateControlInfoKHR` 必须与已配置状态逐字段一致（08254），
    /// 而 `averageBitrate` / `maxBitrate` 也在比较范围内、码率又每帧跟着带宽估计变。
    /// 所以 begin 声明这一组「上次配置值」，当前值在作用域内再配进去（差一帧，永远一致）。
    configured_bitrate_bps: u64,
    configured_fps: u32,
    query_pool: vk::QueryPool,
    /// GPU 时间戳查询池；队列族不支持时间戳或非测试构建时为 `None`
    #[cfg(test)]
    ts_pool: Option<vk::QueryPool>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    timeline_sem: vk::Semaphore,
    timeline_value: u64,
    /// 等待超时的永久毒化标记（毒化后的行为约束与拆构风险见 `execute` 模块文档）
    poisoned: bool,
    /// 显示尺寸（=协商档位）：只用于 AV1 的 `render_size` 声明显示区域；
    /// 主机帧本身按编码尺寸交付
    display: vk::Extent2D,
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

/// begin / coding control 的 pNext 上要挂的 codec 专属码控结构
///
/// **它是 `VkVideoEncodeRateControlInfoKHR` 的兄弟，不是子节点。** ash 的 `Extends`
/// 实现由 vk.xml 的 pNext 合法列表生成，只为它生成了 `VideoBeginCodingInfoKHR` /
/// `VideoCodingControlInfoKHR` 两个父类型。
///
/// 存**按值**的副本而不是 `&mut`：begin 与 coding control 两处都要挂它，而 ash 的 `push`
/// 要 `&mut`；按值存一份就能各拷一份，避开同一结构被两处同时可变借用。三个变体类型不同，
/// 只能用枚举统一。
#[derive(Clone, Copy)]
enum CodecRateControl<'a> {
    H264(vk::VideoEncodeH264RateControlInfoKHR<'a>),
    H265(vk::VideoEncodeH265RateControlInfoKHR<'a>),
    Av1(vk::VideoEncodeAV1RateControlInfoKHR<'a>),
}

/// begin 时告诉码控「当前 GOP 还剩多少 I/P/B 帧」
///
/// `requiresGopRemainingFrames` 时必须在 begin 的 pNext 里带上（VUID 08256 / 10282），
/// `prefersGopRemainingFrames` 时能让 CBR 分配更准。三个结构都是
/// `VideoBeginCodingInfoKHR` 的直接扩展，与码控结构是兄弟。
///
/// 按值保存：三处 begin 各拷一份，避开同一结构被同时可变借用。
#[derive(Clone, Copy)]
enum CodecGopRemaining<'a> {
    H264(vk::VideoEncodeH264GopRemainingFrameInfoKHR<'a>),
    H265(vk::VideoEncodeH265GopRemainingFrameInfoKHR<'a>),
    Av1(vk::VideoEncodeAV1GopRemainingFrameInfoKHR<'a>),
}

/// 本帧在 GOP 内的剩余帧数（含当前帧，IDR 为 0）
///
/// `gopRemainingI/P/B` 表示「执行本次编码前码控应假设 GOP 还剩多少帧」。本实现
/// 是 IPPP：IDR 占 I，其余全是 P，B 恒 0；`frame_num` 在 IDR 上为 0，其余落在
/// 1..=gop_length-1。
fn gop_remaining_counts(gop_length: u32, frame_num: u32, is_key: bool) -> (u32, u32) {
    let remaining_i = u32::from(is_key);
    let remaining_p = gop_length
        .saturating_sub(frame_num)
        .saturating_sub(u32::from(is_key));
    (remaining_i, remaining_p)
}

/// 把推导出的 level 换算成与 `CodecCaps::max_level` 相同的枚举域
///
/// H.264/H.265 的 std 字段是枚举，AV1 的 `maxLevel` 与 `seq_level_idx` 同域；
/// 三个映射在这里收口，`FrameEncoder::new` 不必按 codec 分支。
fn level_enum(codec: VideoCodec, level: u32) -> Result<u32, String> {
    match codec {
        VideoCodec::H264 => h264::level_to_vk(level),
        VideoCodec::H265 => hevc::level_enum(level),
        VideoCodec::Av1 => Ok(level),
    }
}

impl FrameEncoder {
    /// 帧编码器构造：请求码率钳进驱动声明上限（钳制写回档位，0 视为无上限）、建视频
    /// 会话与参数对象（level 与 frame_num 位宽按档位与 GOP 推导）、NV12 输入图、
    /// DPB 图与码流缓冲。codec 由能力快照决定。
    ///
    /// 全成全败，任一环节失败即 `Err`；GOP 长度必须 ≥ 2。CSC 是可选能力，其构造
    /// 失败原样保留为 `Err`，主机帧与显存槽位两条路径据此报错，CPU 上传路径照常工作
    pub fn new(
        device: &VideoDevice,
        caps: &CodecCaps,
        profile: StreamingProfile,
        gop_length: u32,
    ) -> Result<Self, String> {
        let codec = caps.codec();
        if gop_length < 2 {
            return Err(format!("GOP 长度必须 ≥ 2，收到 {gop_length}"));
        }
        // 请求码率钳到驱动声明上限之内：超出属于非法用法，驱动可能直接判参数错。
        // 钳制而不是报错——低能力驱动上「降码率继续播」好过「整个后端起不来」。
        // maxBitrate 为 0 视为驱动未设上限。钳完写回档位，后续读的是同一份值
        let mut profile = profile;
        let bitrate_bps = profile.bitrate() as u64;
        let bitrate_bps = if caps.max_bitrate > 0 && bitrate_bps > caps.max_bitrate {
            log::warn!(
                "请求码率 {bitrate_bps} bps 超过驱动上限 {} bps，已钳制",
                caps.max_bitrate
            );
            caps.max_bitrate
        } else {
            bitrate_bps
        };
        profile.set_bitrate(bitrate_bps as usize);
        let session = VideoSession::new(
            device,
            caps,
            profile.width() as u32,
            profile.height() as u32,
        )?;
        // 码流的 level 一律按**当前档位**推导 —— 与 NVENC 同一个函数、同一组输入。
        let level = crate::encode::profile_level(
            codec,
            profile.width(),
            profile.height(),
            profile.fps(),
            (bitrate_bps / 1000) as u32,
        );
        // 推导 level 不能超过驱动上限：低端编码器可能只到 4.1，而 1080p60 + 8 Mbps
        // 就是 4.2。超限时声明该 level 不合法，明确报错胜过静默降级，由上层换后端
        // 或降档；SDP 的 level 仍按内容推导，不受这里影响。
        let level_vk = level_enum(codec, level)?;
        if level_vk > caps.max_level() {
            return Err(format!(
                "{:?} 档位 {}x{}@{} 需要 level 枚举 {}（码流值 {}），超过驱动声明的上限 {}",
                codec,
                profile.width(),
                profile.height(),
                profile.fps(),
                level_vk,
                level,
                caps.max_level()
            ));
        }
        // SPS 的 frame_num 位宽必须装得下整个 GOP，否则参考链会错乱
        let params = match codec {
            VideoCodec::H264 => VideoSessionParameters::new_h264(
                device,
                &session,
                level,
                frame_num_bits(gop_length),
            )?,
            VideoCodec::H265 => VideoSessionParameters::new_h265(
                device,
                &session,
                level,
                frame_num_bits(gop_length),
            )?,
            // AV1 没有 frame_num 位宽这回事：序列头由驱动生成，参数集里没有可调的位宽字段。
            // codec 由 `caps.codec()` 推出，此分支的变体必然是 `Av1`
            VideoCodec::Av1 => match &caps.specific {
                CodecSpecific::Av1(specific) => {
                    VideoSessionParameters::new_av1(device, &session, specific, level)?
                }
                _ => unreachable!("codec 与能力快照变体不一致"),
            },
        };
        // 驱动只写 slice NAL，参数集要拼到 IDR 前面（字节由驱动序列化，见 session）
        let parameter_sets = params.parameter_sets().to_vec();
        let extent = session.coded_extent();
        let display = session.display_extent();

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

        // 输入图 padding 由 `CodecCaps::input_pad` 决定；编码尺寸仍是 `extent`。
        let pad = caps.input_pad(extent);
        let image_extent = pad.extent;
        if pad.dropped {
            log::warn!(
                "{:?} 输入 padding 超出图像上限 maxImageDimension2D={}，\
                 超限轴退回 coded extent；实际分配 {}x{}",
                codec,
                caps.max_image_dimension,
                pad.extent.width,
                pad.extent.height
            );
        } else if image_extent != extent {
            log::debug!(
                "{:?} 输入图按 encodeInputPictureGranularity 补齐: coded {}x{} → 分配 {}x{}",
                codec,
                extent.width,
                extent.height,
                image_extent.width,
                image_extent.height
            );
        }
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
        let dpb_slots = session.dpb_slots();
        if dpb_slots < 2 {
            return Err(format!(
                "驱动只提供 {dpb_slots} 个 DPB 槽，本实现需要 2 个（当前帧 + 1 参考帧）"
            ));
        }
        let dpb = DpbImages::new(device, extent, dpb_slots, &video_profile)?;

        // 1. 码流缓冲：按驱动要求的大小对齐向上取整，宁可多给。
        //    主机可见 + 一致，GPU 写、CPU 读回；映射在构造时做一次，逐帧不再往返
        //
        // 容量取「一帧 NV12 大小」与「2 秒目标码率」的较大者：
        // - AV1 的关键帧可以接近一帧 NV12 的量级（1080p 约 3.1 MB），这是下限；
        // - 高码率档位（1440p @ 40 Mbps）下 2 秒就是 10 MB，只按帧大小分配会把
        //   关键帧截断在缓冲末尾 —— 而 `bytes_written == size` 不触发校验，
        //   截断的码流语法上仍可能合法，表现为画面错乱而不是报错。
        let frame_bytes = nv12_len(extent.width as usize, extent.height as usize) as u64;
        let two_seconds = bitrate_bps / 8 * 2;
        let raw_size = frame_bytes.max(two_seconds);
        let bitstream_size = raw_size.next_multiple_of(caps.min_bitstream_size_alignment.max(1));
        let bitstream = device.host_buffer_video(
            bitstream_size,
            vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR,
            "码流缓冲",
            &video_profile,
        )?;

        // 2. feedback 查询池：pNext 必须同时带 profile 与 feedback 创建信息
        //
        // profile 必须与本次编码的 codec 一致：查询池是按视频 profile 创建的，声明成别的
        // codec 属于参数不符。这里复用上面那份 —— 它已经是本 codec 的 profile。
        //
        // `HAS_OVERRIDES` 是可选能力：支持时一并打开，逐帧判断驱动是否改写了帧头参数。
        let feedback_has_overrides = caps
            .feedback_flags
            .contains(vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_HAS_OVERRIDES);
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
            // 首帧 IDR 会先 `^= 1` 变成 1，与「初始 0」也不同，两个方向都安全
            idr_pic_id: 0,
            // 首帧一定是 IDR，但它不挂参考槽，这个初值只会影响「首帧之后」的判断
            last_was_idr: false,
            profile,
            max_bitrate: caps.max_bitrate,
            gop_length,
            quant: QuantCaps::from_caps(caps),
            gop_remaining_frames: caps.wants_gop_remaining_frames(),
            feedback_has_overrides,
            overrides_seen: false,
            // 准入条件必须两条同时成立：能力查询说支持（模式非空、周期 >= 2）**且**
            // 逻辑设备真的开启了该 feature 位。只看前者会在 feature 未链进 create_info
            // 的机器上下发非法参数
            max_intra_refresh_cycle: if caps.supports_intra_refresh()
                && device.supports_intra_refresh()
            {
                caps.intra_refresh.max_cycle_duration
            } else {
                0
            },
            intra_refresh_cycle: None,
            intra_refresh_index: 0,
            // 首帧必须下发一次码控，否则会话跑的是驱动默认状态
            rate_control_initialized: false,
            rate_control_dirty: true,
            // 首帧还没配置过任何码控状态
            configured_bitrate_bps: 0,
            configured_fps: 0,
            query_pool: res.query_pool,
            #[cfg(test)]
            ts_pool: res.ts_pool,
            pool: res.pool,
            cmd: res.cmd,
            timeline_sem: res.timeline_sem,
            timeline_value: 0,
            poisoned: false,
            display,
            #[cfg(test)]
            last_timings: FrameProfileTimings::default(),
        })
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.image.extent()
    }

    /// 把配置的 QP 地板钳进驱动声明的 `min_qp..=max_qp`，上限再夹到 H.26x 的 51
    ///
    /// 只由 H.264/H.265 路径调用：码控分派按 `self.codec` 走，AV1 不进这里。
    /// 返回的 `(min, max)` 保证 `min <= max`；写死范围在窄范围驱动上会违反
    /// `useMinQp` / `useMaxQp` 的 VUID。
    fn qp_range(&self, floor: i32) -> (i32, i32) {
        let QuantCaps::Qp { min, max } = self.quant else {
            unreachable!("AV1 的量化索引范围不适用 QP 门禁");
        };
        let lo = min.min(max);
        let hi = min.max(max);
        let min = floor.clamp(lo, hi);
        let max = 51.min(hi).max(min);
        (min, max)
    }

    /// AV1 码控的量化索引范围；只由 AV1 路径调用
    fn q_index_range(&self) -> (u32, u32) {
        let QuantCaps::QIndex { min, max } = self.quant else {
            unreachable!("H.26x 的 QP 范围不适用 AV1 量化索引");
        };
        (min, max)
    }

    /// 本帧要挂进 begin 的 GOP 剩余帧数结构；驱动既不强求也不偏好时返回 `None`
    ///
    /// 三个 codec 结构语义相同、只有类型不同，构造集中在这里。
    fn gop_remaining<'a>(
        &self,
        codec: VideoCodec,
        frame_num: u32,
        is_key: bool,
    ) -> Option<CodecGopRemaining<'a>> {
        if !self.gop_remaining_frames {
            return None;
        }
        let (remaining_i, remaining_p) = gop_remaining_counts(self.gop_length, frame_num, is_key);
        Some(match codec {
            VideoCodec::H264 => CodecGopRemaining::H264(
                vk::VideoEncodeH264GopRemainingFrameInfoKHR::default()
                    .use_gop_remaining_frames(true)
                    .gop_remaining_i(remaining_i)
                    .gop_remaining_p(remaining_p)
                    .gop_remaining_b(0),
            ),
            VideoCodec::H265 => CodecGopRemaining::H265(
                vk::VideoEncodeH265GopRemainingFrameInfoKHR::default()
                    .use_gop_remaining_frames(true)
                    .gop_remaining_i(remaining_i)
                    .gop_remaining_p(remaining_p)
                    .gop_remaining_b(0),
            ),
            VideoCodec::Av1 => CodecGopRemaining::Av1(
                vk::VideoEncodeAV1GopRemainingFrameInfoKHR::default()
                    .use_gop_remaining_frames(true)
                    .gop_remaining_intra(remaining_i)
                    .gop_remaining_predictive(remaining_p)
                    .gop_remaining_bipredictive(0),
            ),
        })
    }

    /// GPU 色彩转换是否可用
    ///
    /// 为假时持有 `VkBuffer` 的主机帧也走不了零拷贝平面摊开，调用方应改走
    /// `encode_frame` 的暂存上传（帧内容已是 NV12，无需任何换算）。
    pub fn has_gpu_csc(&self) -> bool {
        self.csc.is_ok()
    }

    /// 是否因提交等待超时毒化；毒化后的行为约束与拆构风险见 `execute` 模块文档
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// 目标帧率（帧/秒）；唯一来源是协商档位
    fn fps(&self) -> u32 {
        self.profile.fps() as u32
    }

    /// 目标码率（bps）；唯一来源是协商档位
    fn bitrate_bps(&self) -> u64 {
        self.profile.bitrate() as u64
    }

    /// 运行期改码率：下一帧随码控命令重发，不需要重建会话
    ///
    /// 这是自研路径的关键收益——码控参数本来就每帧随 `VkVideoCodingControlInfoKHR` 下发，
    /// 只差一个对外入口。
    pub fn set_bitrate(&mut self, bps: u64) {
        if bps == 0 || bps == self.bitrate_bps() {
            return;
        }
        // 钳到驱动上限（理由见 `new` 里的说明）。带宽估计逐帧变化，这里用 debug 不刷屏
        let bps = if self.max_bitrate > 0 && bps > self.max_bitrate {
            self.max_bitrate
        } else {
            bps
        };
        self.profile.set_bitrate(bps as usize);
        self.rate_control_dirty = true;
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
        if fps == 0 || fps == self.fps() {
            return;
        }
        self.profile.set_fps(fps as usize);
        self.rate_control_dirty = true;
    }

    /// 启用 intra-refresh：用渐进刷新替代周期性 IDR
    ///
    /// `cycle_duration` 是刷完整帧所需的帧数，必须落在 `2..=max_intra_refresh_cycle`。
    /// 超出上限直接报错而不是截断——静默截断会让调用方以为按自己的周期在刷。
    ///
    /// **行为**：渐进刷新不产出 IDR，因此这些帧一律按「非关键帧」上报（见 `encode_prepared`
    /// 的返回值），新观众仍要靠 `force_keyframe` 产真 IDR。这一点不能含糊：把渐进刷新
    /// 谎报成关键帧，会让接收端的 `waiting_for_keyframe` 门提前打开，然后花屏。
    pub fn set_intra_refresh(&mut self, cycle_duration: u32) -> Result<(), String> {
        if self.max_intra_refresh_cycle == 0 {
            return Err(
                "驱动未提供 intra-refresh（能力查询或设备 feature 位不成立），不能启用".to_string(),
            );
        }
        if cycle_duration < 2 || cycle_duration > self.max_intra_refresh_cycle {
            return Err(format!(
                "intra-refresh 周期必须在 2..={} 之间，收到 {}",
                self.max_intra_refresh_cycle, cycle_duration
            ));
        }
        self.intra_refresh_cycle = Some(cycle_duration);
        self.intra_refresh_index = 0;
        Ok(())
    }

    /// 当前 intra-refresh 周期；`None` 表示关闭
    #[cfg(test)]
    pub fn intra_refresh_cycle(&self) -> Option<u32> {
        self.intra_refresh_cycle
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
        let display = vk::Extent2D {
            width: self.display.width,
            height: self.display.height,
        };
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
        match self.codec {
            VideoCodec::H264 => self.encode_prepared_h264(force_idr),
            VideoCodec::H265 => self.encode_prepared_h265(force_idr),
            VideoCodec::Av1 => self.encode_prepared_av1(force_idr),
        }
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

/// 测试档位：1080p60，显式 8 Mbps（绕开动态码率公式，让断言的输入确定）。
/// codec 不在档位里，由各用例按能力快照单独传给 `FrameEncoder::new`
#[cfg(test)]
pub(super) fn test_profile() -> StreamingProfile {
    StreamingProfile::new(1920, 1080, 60, 8_000_000)
}

/// 造一帧 NV12：亮度低频渐变、色度四色块
///
/// **色度不能再填 128。** 色度恒为中性灰时，色度平面无论怎么编码错，画面都只是
/// 灰的 —— 亮度对、色度崩，画面上正是「彩色噪点 / 整片偏色」，而测试全绿。
/// 真实画面的色度有内容，测试必须覆盖它。
///
/// **亮度也不能用满幅噪声。** 满幅随机噪声的熵接近理论上限，8 Mbps 会被亮度
/// 全部吃光，色度只能分到极少比特 —— 实测表现为「四块边界清晰但颜色发灰、
/// 块内渐变」，那是码率不足，不是色度 bug。低频对角渐变的码率需求极小，
/// 色度能拿到足够比特：**只有这样才能把「色度编码错误」和「色度被饿死」分开**。
///
/// 色度用四个大色块而不是噪声：噪声色度与「色度崩坏」在画面上难以区分，
/// 而色块边界清晰，解出来偏色 / 串块 / 边界错位都能一眼判定。
/// NV12 的 UV 是交错的 `U0 V0 U1 V1 ...`，一个 UV 对覆盖 2×2 像素。
#[cfg(test)]
pub(super) fn make_noisy_frame(extent: vk::Extent2D, seed: u32) -> Vec<u8> {
    let w = extent.width as usize;
    let h = extent.height as usize;
    let pixels = w * h;
    let mut out = vec![0u8; pixels + pixels / 2];
    // 亮度：低频对角渐变 + 每 64 像素一格的细微台阶（保证有可预测的高频结构，
    // 但不至于把码率吃光）。种子只影响台阶相位，不同种子仍有真实残差。
    let phase = (seed % 64) as usize;
    for y in 0..h {
        let base = (y * 255) / h.max(1);
        for x in 0..w {
            let ramp = (x * 255) / w.max(1);
            let step = if ((x + phase) / 64 + y / 64).is_multiple_of(2) {
                0
            } else {
                8
            };
            out[y * w + x] = ((base + ramp) / 2).saturating_add(step).min(255) as u8;
        }
    }
    // 色度：8 条竖色带。
    //
    // 纯色块内部平坦，色度残差固定为 0，测不出色度残差编码的问题；
    // 噪声色度又与「色度崩坏」在画面上无法区分。竖色带的垂直边界是色度高频，
    // 能真正压出色度残差，而边界位置固定，解出来错位 / 串色 / 边界发虚都能一眼判定。
    const BARS: [(u8, u8); 8] = [
        (128, 128), // 中性
        (64, 128),  // 偏蓝
        (192, 128), // 偏黄
        (128, 64),  // 偏红
        (128, 192), // 偏青
        (64, 64),
        (192, 192),
        (96, 160),
    ];
    let (cw, ch) = (w / 2, h / 2);
    for y in 0..ch {
        for x in 0..cw {
            let (u, v) = BARS[(x * BARS.len() / cw).min(BARS.len() - 1)];
            let idx = pixels + (y * cw + x) * 2;
            out[idx] = u;
            out[idx + 1] = v;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 推导 level 必须换算到与 `CodecCaps::max_level` 相同的枚举域
    #[test]
    fn test_level_enum_domains() {
        // H.264 语法值 42（4.2）在 std 里是枚举 12
        assert_eq!(level_enum(VideoCodec::H264, 42), Ok(12));
        // H.265 语法值 123（4.1 × 30）是枚举 6
        assert_eq!(level_enum(VideoCodec::H265, 123), Ok(6));
        // AV1 的 seq_level_idx 与 StdVideoAV1Level 同域
        assert_eq!(level_enum(VideoCodec::Av1, 9), Ok(9));
        assert!(
            level_enum(VideoCodec::H264, 43).is_err(),
            "非法语法值必须拒绝"
        );
    }

    /// GOP 剩余帧数：含当前帧，IDR 占 I，其余全是 P，B 恒 0
    #[test]
    fn test_gop_remaining_counts() {
        // 30 帧 GOP：IDR + 29 个 P
        assert_eq!(gop_remaining_counts(30, 0, true), (1, 29));
        assert_eq!(gop_remaining_counts(30, 1, false), (0, 29));
        assert_eq!(gop_remaining_counts(30, 2, false), (0, 28));
        assert_eq!(gop_remaining_counts(30, 29, false), (0, 1));
        // 最短 GOP：IDR + 1 个 P
        assert_eq!(gop_remaining_counts(2, 0, true), (1, 1));
        assert_eq!(gop_remaining_counts(2, 1, false), (0, 1));
    }

    /// `frame_num` 位宽换算：装得下 `gop_length + 1` 个不重复值
    ///
    /// 这条不需要 GPU，纯算术，放在这里是因为它决定 SPS 的合法性——
    /// 位宽算小了，参考链会在 GOP 中途断掉，而且不报错。
    #[test]
    fn test_frame_num_bits() {
        assert_eq!(frame_num_bits(2), 4, "MaxFrameNum 至少 16");
        assert_eq!(frame_num_bits(15), 4, "16 个值正好 4 位");
        assert_eq!(frame_num_bits(16), 5, "17 个值需要 32");
        assert_eq!(frame_num_bits(300), 9, "301 个值需要 512");
        assert_eq!(frame_num_bits(4095), 12, "4096 个值正好 12 位");
        // 12 位是上限：更长的 GOP 会截断到 12，`MaxFrameNum` 仍是 4096，
        // 由 `is_idr` 的 `frame_num >= gop_length` 分支保证到点出 IDR
        assert_eq!(frame_num_bits(65_535), 12, "超过 12 位一律截断");
        assert_eq!(frame_num_bits(65_536), 12, "超出一位也截断到 12");
        assert_eq!(frame_num_bits(u32::MAX), 12, "上限截断，不溢出");
    }
}

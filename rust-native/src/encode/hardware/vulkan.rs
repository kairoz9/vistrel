//! Vulkan Video 硬件编码与 Compute CSC 管线模块
//!
//! 整条路径全部基于 ash 手写：能力探测、会话、参数集构造、NV12 输入、DPB 轮转、
//! 码控、Compute CSC、显存槽位导入。不依赖任何第三方 Vulkan 封装。
//!
//! 参数集的**码流字节**由驱动序列化（`vkGetEncodedVideoSessionParametersKHR`），
//! 应用只填 `StdVideo*` 结构体 —— 驱动可以覆盖参数，只有它自己序列化出来的那份
//! 才与它写出的 slice NAL 同源。

pub(crate) mod caps;
pub(crate) mod context;
mod csc;
mod device;
mod frame_encoder;
mod image;
mod session;
mod sync;
/// 显存槽位导出侧替身：只有 `encode::perf` 的性能矩阵消费它，生产构建里不存在
#[cfg(test)]
pub(crate) mod vram_export;
mod vram_slot;

#[cfg(test)]
use crate::encode::encoder::BackendStageTimings;
use crate::encode::encoder::{EncodeBackendKind, VideoEncodeBackend};
use crate::encode::hardware::slots::{SlotTable, VramSlotDescriptor, ensure_batch_geometry};
use crate::encode::pool::{HOST_SLOT_BYTES, HostBlock, POOL_DEPTH};
use crate::encode::{FramePayload, StageCell, StreamingProfile, VideoCodec, nv12_len};

use std::sync::Arc;

use ash::vk;
use bytes::Bytes;

use caps::CodecCaps;
use context::VulkanInstance;
use frame_encoder::FrameEncoder;
use vram_slot::VramSlot;

/// 主机帧池的 Vulkan 块类型（`frame.rs` 经此引用，不开放整个 device 模块）
pub(crate) use device::{HostBuffer, VideoDevice};

/// 启用 intra-refresh 时的 GOP 长度
///
/// 取 12 位 `frame_num` 能表达的上界（`MaxFrameNum = 4096`），即「不靠周期性 IDR，
/// 靠渐进刷新兜底」。
///
/// **不能取 16 位的 65535**：intra-refresh 是否真的生效取决于驱动
/// （`maxIntraRefreshActiveReferencePictures` 与 `maxActiveReferencePictures` 必须
/// 都够），一旦驱动静默忽略 `VkVideoEncodeIntraRefreshInfoKHR`，这串帧就全是普通 P 帧，
/// 任何一次解码错误都会一直残留到下一个 IDR——65535 帧在 @240fps 是 273 秒，
/// 表现为「花屏几分钟不恢复」。4095 帧（@60fps 68 秒）是能接受的最坏恢复时间。
const INTRA_REFRESH_GOP: u32 = 4095;

/// 把 RGBA 转成**编码尺寸**的 NV12（BT.709 + Limited Range）
///
/// `rgba` 是 `visible` 大小的紧凑 RGBA（byte0 = R，4 字节/像素）；`nv12` 是 `coded`
/// 大小的目标缓冲（Y 平面 `w*h` 紧跟交错 UV `w*h/2`）。
///
/// `coded` 通常比 `visible` 大：驱动要求编码尺寸按 `pictureAccessGranularity` 对齐，
/// 1080 会变成 1088。多出来的右侧与底部区域用**最后一行 / 最后一列复制**填充 ——
/// 这些行会进编码器、也会进 DPB 参考帧，留垃圾会污染后续 P 帧。
///
/// 生产路径已由采集侧完成 CSC，本函数保留为测试参考实现（`csc` 模块的浮点对照测试）。
#[cfg(test)]
fn rgba_to_nv12(
    rgba: &[u8],
    visible: vk::Extent2D,
    coded: vk::Extent2D,
    nv12: &mut [u8],
) -> Result<(), String> {
    let (vw, vh) = (visible.width as usize, visible.height as usize);
    let (cw, ch) = (coded.width as usize, coded.height as usize);

    if vw == 0 || vh == 0 {
        return Err("RGBA 帧几何不能为零".to_string());
    }
    // 4:2:0 要求偶数几何；下面的填充也按「整行 / 整色度对」操作
    if vw % 2 != 0 || vh % 2 != 0 || cw % 2 != 0 || ch % 2 != 0 {
        return Err(format!(
            "4:2:0 要求偶数几何: 可见 {vw}x{vh}, 编码 {cw}x{ch}"
        ));
    }
    if vw > cw || vh > ch {
        return Err(format!("可见区域 {vw}x{vh} 超出编码尺寸 {cw}x{ch}"));
    }
    if rgba.len() < vw * vh * 4 {
        return Err(format!(
            "RGBA 缓冲区长度不足: 期望 {}, 实际 {}",
            vw * vh * 4,
            rgba.len()
        ));
    }
    let y_size = cw * ch;
    let uv_size = cw * (ch / 2);
    if nv12.len() < y_size + uv_size {
        return Err(format!(
            "NV12 目标缓冲区长度不足: 期望 {}, 实际 {}",
            y_size + uv_size,
            nv12.len()
        ));
    }

    // 1. 只转可见区域，但按**编码 stride** 落盘（y_stride = uv_stride = 编码宽度），
    //    填充区留给后面两步。传给 crate 的切片按「stride × 行数」精确裁剪：
    //    多给的部分它看不到，也就不会去写填充区
    {
        let (y_plane, uv_plane) = nv12.split_at_mut(y_size);
        let mut image = yuv::YuvBiPlanarImageMut {
            y_plane: yuv::BufferStoreMut::Borrowed(&mut y_plane[..cw * vh]),
            y_stride: coded.width,
            uv_plane: yuv::BufferStoreMut::Borrowed(&mut uv_plane[..cw * (vh / 2)]),
            uv_stride: coded.width,
            width: visible.width,
            height: visible.height,
        };
        yuv::rgba_to_yuv_nv12(
            &mut image,
            rgba,
            visible.width * 4,
            yuv::YuvRange::Limited,
            yuv::YuvStandardMatrix::Bt709,
            yuv::YuvConversionMode::Balanced,
        )
        .map_err(|e| format!("RGBA→NV12 转换失败: {e:?}"))?;
    }

    let (y_plane, uv_plane) = nv12.split_at_mut(y_size);

    // 2. 右侧填充：每行复制最后一个像素 / 最后一个色度对
    if vw < cw {
        for row in 0..vh {
            let base = row * cw;
            let last = y_plane[base + vw - 1];
            y_plane[base + vw..base + cw].fill(last);
        }
        // UV 每个色度样本 2 字节（U、V 交错），必须按对复制，否则 U/V 会串位
        for row in 0..vh / 2 {
            let base = row * cw;
            let (u, v) = (uv_plane[base + vw - 2], uv_plane[base + vw - 1]);
            for col in (vw..cw).step_by(2) {
                uv_plane[base + col] = u;
                uv_plane[base + col + 1] = v;
            }
        }
    }

    // 3. 底部填充：整行复制最后一行（此时右侧已经填好，复制过来即可）
    for row in vh..ch {
        let (head, tail) = y_plane.split_at_mut(row * cw);
        tail[..cw].copy_from_slice(&head[(vh - 1) * cw..vh * cw]);
    }
    for row in vh / 2..ch / 2 {
        let (head, tail) = uv_plane.split_at_mut(row * cw);
        tail[..cw].copy_from_slice(&head[(vh / 2 - 1) * cw..(vh / 2) * cw]);
    }

    Ok(())
}

/// Vulkan Video 硬件编码器后端
pub(crate) struct VulkanVideoBackend {
    device: VideoDevice,
    encoder: FrameEncoder,
    /// 当前档位。`reconfigure` 靠它判断「几何是否真的变了」——
    /// 只有 fps / 码率变化时绝不能重建会话（会清空显存槽位表）
    profile: StreamingProfile,
    /// 当前 codec
    codec: VideoCodec,
    slots: SlotTable<VramSlot>,
    /// 编码阻塞阶段出口：本后端只写阶段码，心跳由编码循环推进
    stage: Arc<StageCell>,
    /// 本设备为该 codec 声明的 `pictureAccessGranularity`
    ///
    /// 采集侧必须按它出帧（`StreamingProfile::coded_extent`）。存成字段而不是每次回查
    /// `CodecCaps`，是因为上层要用它校正全局探测快照——探测后端与实际建成的后端
    /// 可能不是同一个（见 `VideoEncoder::new_hardware`）。
    coded_align: (u32, u32),
}

// SAFETY: 资源与 Vulkan 句柄仅在专属编码工作线程操作，底层句柄的生命周期由
// `VideoDevice` 的 Arc 与各 RAII 结构自己管理，满足 Send 与 Sync 要求
unsafe impl Send for VulkanVideoBackend {}
unsafe impl Sync for VulkanVideoBackend {}

impl VulkanVideoBackend {
    /// 帧块必须分配在**本后端自己的设备**上（GPU 拷贝的源缓冲与命令缓冲必须同属一个
    /// `VkDevice`），所以只能在这里分配，随构造一并交回给帧池。分配失败即整体构造失败——
    /// 不允许出现「编码器起来了、块却没就绪」的半状态
    pub(crate) fn new(
        profile: StreamingProfile,
        codec: VideoCodec,
        stage: Arc<StageCell>,
    ) -> Result<(Self, Vec<HostBlock>), String> {
        let width = profile.width();
        let height = profile.height();
        let fps = profile.fps() as u32;

        // 1. instance → 能力快照 → 设备。顺序是硬性的：设备要按 codec 选队列族，
        //    会话要该 codec 的能力快照。
        let instance = VulkanInstance::new("Vistrel")?;
        let caps: CodecCaps = caps::probe_codec(&instance, codec)?;
        let device = VideoDevice::new(&instance, codec)?;

        // 2. intra-refresh：设备真的开启了 feature 位、且驱动声明了可用周期才启用。
        //    三个 codec 走同一套：`VkVideoEncodeIntraRefreshInfoKHR` 是 codec 无关的
        //    扩展，H.264 / H.265 / AV1 的编码命令都能挂。周期取 0.5s（fps/2），
        //    与 NVENC 的 `intra_refresh_timing` 同一口径。
        //
        //    准入条件必须同时满足三项（见 `CodecCaps::supports_intra_refresh`），
        //    其中 `max_intra_refresh_active_reference_pictures != 0` 最容易漏 ——
        //    漏了会对不支持的驱动照常下发，驱动静默忽略，每帧退化成普通 P 帧，
        //    而 GOP 是按「有渐进刷新」选的超长值，于是任何一次解码错误都永不恢复。
        let intra_cycle = if caps.supports_intra_refresh() && device.supports_intra_refresh() {
            Some((fps / 2).max(2).min(caps.max_intra_refresh_cycle_duration))
        } else {
            None
        };
        // GOP：启用 intra-refresh 后靠渐进刷新兜底，不再周期性出 IDR；否则按 1 秒出 IDR
        let gop = match intra_cycle {
            Some(_) => INTRA_REFRESH_GOP,
            None => fps.max(2),
        };
        let mut encoder = FrameEncoder::new(&device, &caps, profile, codec, gop)?;
        if let Some(cycle) = intra_cycle {
            encoder.set_intra_refresh(cycle)?;
        }

        log::info!(
            "Vulkan {:?} {width}x{height}@{fps}: intra-refresh 模式 [{}] present={} \
                     max_cycle={} 驱动 active_ref={} 设备 feature={} ⇒ 会话 cycle={:?} gop={gop}",
            caps.codec,
            caps.intra_refresh_modes,
            caps.intra_refresh_modes_present,
            caps.max_intra_refresh_cycle_duration,
            caps.max_intra_refresh_active_reference_pictures,
            device.supports_intra_refresh(),
            intra_cycle,
        );
        // 帧块必须建在本后端自己的设备上：分配失败即整体构造失败，块随构造交回给帧池
        let blocks = allocate_host_frames(&device)?;

        Ok((
            Self {
                device,
                encoder,
                profile,
                codec,
                slots: SlotTable::new(),
                stage,
                coded_align: (
                    caps.picture_access_granularity.width.max(1),
                    caps.picture_access_granularity.height.max(1),
                ),
            },
            blocks,
        ))
    }

    /// 显存槽位编码：CSC 直接读槽位图，像素全程留在显存
    fn encode_vram_slot(
        &mut self,
        slot_idx: u32,
        sync_value: u64,
        force_keyframe: bool,
        frame_idx: u64,
    ) -> Result<(Bytes, bool), String> {
        let slot = self
            .slots
            .get_mut(slot_idx)
            .ok_or_else(|| format!("Vulkan Video VRAM slot {slot_idx} not found"))?;
        let result = self
            .encoder
            .encode_vram_frame(slot, sync_value, force_keyframe);

        if let Ok((_, is_key)) = &result {
            log::debug!(
                "[VulkanVideo] VRAM 槽位显存直通帧编码完成 frame_idx={}, force_keyframe={}, packet_key={}",
                frame_idx,
                force_keyframe,
                is_key
            );
        }
        result.map(|(out, is_key)| (Bytes::from(out), is_key))
    }
}

/// [`VideoEncodeBackend`] 落点：主机帧经 Compute CSC 管线上传；
/// CUDA 上下文等 NVENC 私有物与本后端无关。
impl VideoEncodeBackend for VulkanVideoBackend {
    fn kind(&self) -> EncodeBackendKind {
        EncodeBackendKind::VulkanVideo
    }

    /// 本设备声明的编码尺寸对齐粒度
    ///
    /// 与 `hardware::coded_align()` 不同：那个读全局探测快照，这里读**实际建成的
    /// 这个设备**。两者在正常路径上相同；不同时以上层校正为准（见
    /// `VideoEncoder::new_hardware`）。
    fn coded_align(&self) -> (u32, u32) {
        self.coded_align
    }

    /// 批量导入外部显存槽位并导入为 Vulkan Image/ImageView 与时间线信号量
    fn import_vram_slots(&mut self, descriptors: &mut [VramSlotDescriptor]) -> Result<(), String> {
        // 槽位可以是编码尺寸，也可以是采集尺寸（CSC 按源尺寸夹取，把补齐边复制出来）。
        // 其余尺寸没有这套保证，注册期一次拦下，让采集侧直接降级到 CPU 回读，而不是逐帧
        // 失败 30 次后被断路器踢掉——后者会让第一秒的画面白丢
        let extent = self.encoder.extent();
        ensure_batch_geometry(
            descriptors,
            (extent.width, extent.height),
            Some((self.profile.width() as u32, self.profile.height() as u32)),
        )?;
        // 先克隆句柄再借 slots：`VideoDevice` 只是 Arc 计数 +1，两个字段的借用因此不重叠
        let device = self.device.clone();
        self.slots
            .import_all(descriptors, |desc| vram_slot::import(&device, desc))
    }

    /// 清空显存槽位表并释放底层显存资源
    fn clear_vram_slots(&mut self) {
        self.slots.clear();
    }

    /// 运行期改码率：下一帧随码控命令重发，不重建会话
    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), String> {
        self.encoder.set_bitrate(u64::from(bitrate_bps));
        // 档位副本必须跟着走：`reconfigure` 拿它判断几何是否变化，
        // 陈旧的码率会让「跟随码率」与「档位上限」两份值悄悄分叉
        self.profile.set_bitrate(bitrate_bps as usize);
        Ok(())
    }

    /// 排空：等待超时过的编码器仍带着在途命令缓冲（毒化机制见 `execute` 模块文档）
    ///
    /// 编码逐帧同步：`encode` 在时间线信号量上等到 2s 上限才返回码流，返回时没有在途
    /// 任务，所以正常路径下这里无须等任何东西。
    ///
    /// 不在这里 `device_wait_idle`：毒化时那会阻塞到那个卡住的提交有个结果（设备已
    /// lost 时可能就是不返回），代价比保留资源更大。
    fn drain(&mut self) -> Result<(), String> {
        if self.encoder.is_poisoned() {
            return Err("编码器因等待超时毒化：仍有在途命令缓冲".to_string());
        }
        Ok(())
    }

    fn reconfigure(
        &mut self,
        new_profile: StreamingProfile,
        new_codec: VideoCodec,
    ) -> Result<Vec<HostBlock>, String> {
        // 几何与 codec 都没变：coded extent、DPB、SPS 全部仍然有效，只有码控参数要更新。
        //
        // **绝不能重建会话**。重建会清空显存槽位表，而采集侧在尺寸不变时不会重建
        // 采集管线、也就不会重新注册槽位——之后每一帧都变成「VRAM slot N not found」，
        // 而两条采集路径里 RAM 回退也没被启用（游戏侧认为管线还健康），画面永久冻结。
        // 这是「只切帧率就卡死」的根因。
        let same_shape = self.profile.width() == new_profile.width()
            && self.profile.height() == new_profile.height()
            && self.codec == new_codec;
        if same_shape {
            // 码控每帧随 `VkVideoCodingControlInfoKHR` 下发，改字段 + 置脏即可生效
            self.encoder.set_fps(new_profile.fps() as u32);
            self.encoder.set_bitrate(new_profile.bitrate() as u64);
            self.profile = new_profile;
            return Ok(Vec::new());
        }

        // 先建新后端再替换：新建失败时旧后端仍然可用。新后端带在自己的新设备上分配的帧块，
        // 由调用方装进帧池——池里旧域的块会落进 `stale`，等下次 `attach` 时释放
        let (new_backend, blocks) = Self::new(new_profile, new_codec, Arc::clone(&self.stage))?;
        *self = new_backend;
        Ok(blocks)
    }

    fn encode(
        &mut self,
        payload: &FramePayload,
        force_keyframe: bool,
        frame_idx: u64,
    ) -> Result<(Bytes, bool), String> {
        match payload {
            FramePayload::Host { frame } => {
                // 主机帧一律 NV12，按**编码尺寸**交付：Y 平面 w*h 之后紧跟交织 UV 平面 w*h/2
                let extent = self.encoder.extent();
                let host_bytes = nv12_len(extent.width as usize, extent.height as usize);
                let nv12 = frame.as_slice();
                // 对不上说明采集管线与编码器档位不同步——直接报错，不去猜怎么补
                if nv12.len() < host_bytes {
                    return Err(format!(
                        "主机帧长度不足: 期望 {host_bytes}（编码尺寸 {}x{}, NV12）, 实际 {}",
                        extent.width,
                        extent.height,
                        nv12.len()
                    ));
                }

                // 输入已是 NV12，色彩转换由采集侧完成，这里只剩上传+编码。
                // 池块是 Vulkan 缓冲时走零拷贝：生产者已把 NV12 直写进持久映射缓冲，
                // GPU 只需把两平面摊进平面图；堆块没有 `VkBuffer`，只能走暂存上传
                let buffer = frame.vulkan_buffer_of(&self.device);
                let result = match buffer {
                    Some(buffer) if self.encoder.has_gpu_csc() => self
                        .encoder
                        .encode_nv12_buffer_frame(buffer, force_keyframe),
                    _ => self.encoder.encode_frame(nv12, force_keyframe),
                };

                if let Ok((_, is_key)) = &result {
                    log::debug!(
                        "[VulkanVideo] 主机帧编码完成 frame_idx={}, force_keyframe={}, packet_key={}",
                        frame_idx,
                        force_keyframe,
                        is_key
                    );
                }
                result.map(|(out, is_key)| (Bytes::from(out), is_key))
            }
            FramePayload::VramSlot { slot, sync_value } => {
                // 编码无论成败都由载荷 Drop 归还——但等待超时毒化例外：提交仍在途、
                // 纹理仍被 GPU 读取，此刻归还等于让渲染线程复写在读显存。arm 后
                // Drop 不再释放，粘滞 BUSY 由断路器 / 换代全清 / 心跳超时三层兜底
                let result =
                    self.encode_vram_slot(slot.slot(), *sync_value, force_keyframe, frame_idx);
                if self.encoder.is_poisoned() {
                    slot.poison();
                }
                result
            }
        }
    }

    #[cfg(test)]
    fn benchmark_stages(&self) -> BackendStageTimings {
        let timings = self.encoder.last_timings();
        BackendStageTimings {
            input_copy_ns: timings.stage_ns,
            submit_ns: timings.submit_ns,
            wait_ns: timings.wait_fence_ns,
            gpu_scope_ns: timings.gpu_scope_ns,
            gpu_encode_ns: timings.gpu_encode_ns,
            gpu_frame_ns: timings.gpu_frame_ns,
            ..BackendStageTimings::default()
        }
    }
}

/// 帧块：Vulkan 持久映射的 host-visible 缓冲（TRANSFER_SRC）
///
/// 必须分配在**编码器自己的设备**上：GPU 拷贝的源缓冲与命令缓冲必须同属一个 `VkDevice`。
/// 整批分配失败时已分配的块随 `?` 逆序释放（`HostBuffer` 的 RAII 负责）。
///
/// 堆选 `HOST_CACHED`（类型 3）：池写带宽 micro-benchmark（`pool_write_memory_bandwidth_probe`）
/// 实测 CACHED 堆 + 既有流式 NT 写是全组合最快（871µs/14.4MiB vs 默认堆 1447µs，-40%）；
/// 全管线 A/B 收益被运行期内存控制器争用压缩（1080p -6%、1440p 持平），但无回归且
/// 隔离优势明确，故采用。写路径不动：`copy_streaming` 的 128B 四寄存器流水快于单发 NT。
fn allocate_host_frames(device: &VideoDevice) -> Result<Vec<HostBlock>, String> {
    let mut blocks = Vec::with_capacity(POOL_DEPTH);
    for i in 0..POOL_DEPTH {
        let buffer = device.host_buffer_cached(
            HOST_SLOT_BYTES as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
            &format!("Vistrel 主机帧 {i}"),
        )?;
        blocks.push(HostBlock::Vulkan(buffer));
    }
    Ok(blocks)
}

/// 仅测试使用的码流结构扫描与一次性提交设施
///
/// H.264 / H.265 / AV1 的用例都要从裸流里切出 NAL / OBU 再断言类型。三份各自手写
/// 起始码扫描的差别只在「头字节怎么解」，切分逻辑完全一致，因此收在这里。
#[cfg(test)]
mod test_support {
    use ash::vk;

    use super::device::VideoDevice;

    /// 测试专用的一次性提交
    ///
    /// 生产路径的提交全在 `FrameEncoder` 里，`Nv12Image` 不持有命令池。测试要单独验证
    /// 上传与读回，就得自己搭一套最小设施——放在 `#[cfg(test)]` 下，生产代码为零。
    ///
    /// `wait` 是跨队列依赖要等的信号量（CSC 的完成信号），没有就传 `None`。
    pub(crate) fn run_oneshot(
        device: &VideoDevice,
        wait: Option<vk::Semaphore>,
        record: impl FnOnce(vk::CommandBuffer),
    ) -> Result<(), String> {
        let raw = device.device();
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::TRANSIENT)
            .queue_family_index(device.video_queue_family());
        // SAFETY: pool_info 已完整初始化；以下句柄均由本函数创建且只销毁一次
        unsafe {
            let pool = raw
                .create_command_pool(&pool_info, None)
                .map_err(|e| format!("测试命令池创建失败: {e:?}"))?;
            let alloc = vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let cmd = raw
                .allocate_command_buffers(&alloc)
                .map_err(|e| format!("测试命令缓冲分配失败: {e:?}"))?[0];
            let fence = raw
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(|e| format!("测试栅栏创建失败: {e:?}"))?;

            let out = (|| -> Result<(), String> {
                raw.begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| format!("测试命令缓冲开始失败: {e:?}"))?;
                record(cmd);
                raw.end_command_buffer(cmd)
                    .map_err(|e| format!("测试命令缓冲结束失败: {e:?}"))?;

                let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
                // 等待数量是运行时的 0 或 1。不能写 `unwrap_or_default()`：`[T; 1]` 的 Default
                // 会给出一个「全默认」的元素（空句柄）而不是空数组
                let wait_one = wait.map(|semaphore| {
                    [vk::SemaphoreSubmitInfo::default()
                        .semaphore(semaphore)
                        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)]
                });
                let wait_slice: &[vk::SemaphoreSubmitInfo<'_>] = match &wait_one {
                    Some(arr) => arr,
                    None => &[],
                };
                let submit = vk::SubmitInfo2::default()
                    .command_buffer_infos(&cmds)
                    .wait_semaphore_infos(wait_slice);
                // SAFETY: submit 及其引用的 cmds / wait_one 在调用期间均存活；
                // `synchronization2` feature 位在建设备时已显式启用
                raw.queue_submit2(device.video_queue(), &[submit], fence)
                    .map_err(|e| format!("测试提交失败: {e:?}"))?;
                raw.wait_for_fences(&[fence], true, u64::MAX)
                    .map_err(|e| format!("测试等待完成失败: {e:?}"))?;
                Ok(())
            })();

            raw.destroy_fence(fence, None);
            raw.destroy_command_pool(pool, None);
            out
        }
    }

    /// 扫描 Annex-B 起始码，返回每个 NAL 头字节在 `stream` 里的偏移
    ///
    /// 规范没写死驱动用 3 字节还是 4 字节起始码，两种都得认。先试 4 字节，命中就跳过
    /// 5 字节，避免把 `00 00 00 01` 里的 `00 00 01` 重复计一次。
    pub(crate) fn nal_header_offsets(stream: &[u8]) -> Vec<usize> {
        let mut offsets = Vec::new();
        let mut i = 0;
        while i + 3 <= stream.len() {
            if stream[i] == 0 && stream[i + 1] == 0 {
                if stream[i + 2] == 1 {
                    if i + 3 < stream.len() {
                        offsets.push(i + 3);
                    }
                    i += 4;
                    continue;
                }
                if i + 3 < stream.len() && stream[i + 2] == 0 && stream[i + 3] == 1 {
                    if i + 4 < stream.len() {
                        offsets.push(i + 4);
                    }
                    i += 5;
                    continue;
                }
            }
            i += 1;
        }
        offsets
    }

    /// H.264 各 NAL 的类型（头字节低 5 位）
    pub(crate) fn h264_nal_types(stream: &[u8]) -> Vec<u8> {
        nal_header_offsets(stream)
            .into_iter()
            .map(|i| stream[i] & 0x1F)
            .collect()
    }

    /// H.265 各 NAL 的类型（2 字节头，类型在首字节 bit1..6）
    pub(crate) fn h265_nal_types(stream: &[u8]) -> Vec<u8> {
        nal_header_offsets(stream)
            .into_iter()
            .map(|i| (stream[i] >> 1) & 0x3F)
            .collect()
    }

    /// 参数集的原始 NAL 头字节（用来断言 SPS → PPS 的顺序）
    pub(crate) fn nal_headers(stream: &[u8]) -> Vec<u8> {
        nal_header_offsets(stream)
            .into_iter()
            .map(|i| stream[i])
            .collect()
    }

    /// 按 OBU 头切分 AV1 码流，返回各 OBU 的类型
    ///
    /// OBU 头：forbidden(1) | type(4) | extension(1) | has_size(1) | reserved(1)。
    /// 没有 `has_size_field` 时无法继续切分，直接停在当前位置。
    pub(crate) fn av1_obu_types(stream: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0usize;
        while i < stream.len() {
            let header = stream[i];
            if header & 0x80 != 0 {
                break;
            }
            let obu_type = (header >> 3) & 0x0F;
            let has_extension = (header >> 2) & 1 == 1;
            let has_size = (header >> 1) & 1 == 1;
            types.push(obu_type);

            let mut p = i + 1 + usize::from(has_extension);
            if !has_size {
                break;
            }
            // leb128 长度
            let mut size = 0usize;
            let mut shift = 0u32;
            loop {
                let Some(&byte) = stream.get(p) else {
                    return types;
                };
                size |= ((byte & 0x7F) as usize) << shift;
                p += 1;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
                if shift > 56 {
                    return types;
                }
            }
            i = p + size;
        }
        types
    }
}

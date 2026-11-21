//! 跨平台硬件视频编码子域 (Hardware Video Encoding Subsystem)
//!
//! 统一封装 NVIDIA NVENC 与 Vulkan Video 硬件编码后端，提供硬件能力快照
//! `hardware_caps`；全后端约定（含软编）见 [`crate::encode::backend`]，
//! 策略层门面见 `encode::encoder`。

pub(crate) mod nvenc;
pub(crate) mod slots;
pub(crate) mod vulkan;

pub use slots::{FLAT_STRIDE, VramSlotDescriptor};

use arc_swap::ArcSwap;
use std::sync::{Arc, LazyLock};

use crate::encode::{VideoCodec, encoder::EncodeBackendKind};

/// 硬件探测结果：承载当前可用硬件后端、具体支持的 Codec 集合与编码设备名
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareProbeResult {
    pub backend_type: EncodeBackendKind,
    pub supported_codecs: Vec<VideoCodec>,
    /// 编码尺寸的对齐粒度，取自驱动声明的 `pictureAccessGranularity`
    ///
    /// 采集侧必须按这个粒度出帧（[`crate::encode::StreamingProfile::coded_extent`]），
    /// 否则建出来的纹理比编码器要的小，显存直通与内存回读会同时失效。
    /// NVENC 与软编不要求对齐，取 `(1, 1)`。
    pub coded_align: (u32, u32),
    /// 承担硬件编码的显卡设备名（NVENC=CUDA 设备名，Vulkan Video=Vulkan 物理设备名）；
    /// 空串表示未知，软编探测不产出本结构
    pub gpu_name: String,
}

/// 解析 NVENC 应使用的 CUDA 设备序号
///
/// 优先级：与 Vulkan 主设备名称对齐 > device 0。
/// 多显卡机器（核显输出 + 独显渲染）上若盲目使用 device 0，会与 Vulkan 实际使用的设备不一致，
/// 导致 dma-buf 导入失败；名称由同一份 NVIDIA 驱动提供，是可用的对齐依据。
pub(crate) fn nvenc_device_ordinal() -> i32 {
    static ORDINAL: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *ORDINAL.get_or_init(|| {
        if nvenc::probe_cuda().is_err() {
            return 0;
        }
        let devices = nvenc::cuda::enumerate_devices();
        let fallback = devices.first().map(|d| d.ordinal).unwrap_or(0);

        // 显式覆盖优先于一切自动对齐：多卡机器上设备名可能重名，或者名字根本读不到。
        // 序号必须在驱动报告的范围内，否则 `cuDeviceGet` 会在后面失败，报错点离这里很远
        if let Ok(raw) = std::env::var("VISTREL_CUDA_DEVICE") {
            match raw.trim().parse::<i32>() {
                Ok(ordinal) if devices.iter().any(|d| d.ordinal == ordinal) => {
                    log::info!("NVENC 使用 VISTREL_CUDA_DEVICE 指定的 CUDA 设备 {ordinal}");
                    return ordinal;
                }
                _ => log::warn!(
                    "VISTREL_CUDA_DEVICE={raw} 非法（合法范围 0..{}），忽略",
                    devices.len()
                ),
            }
        }

        if devices.len() <= 1 {
            return fallback;
        }

        if let Ok(ctx) = vulkan::context::VulkanInstance::new("Vistrel") {
            let vk_name = ctx.device_name();
            if let Some(dev) = devices.iter().find(|d| d.name == vk_name) {
                log::info!(
                    "NVENC 选择 CUDA 设备 {} ({})，与 Vulkan 设备对齐",
                    dev.ordinal,
                    dev.name
                );
                return dev.ordinal;
            }
            log::warn!(
                "无法按名称对齐 CUDA 设备 (Vulkan: {})，回退 device {}；可用 VISTREL_CUDA_DEVICE 显式指定",
                vk_name,
                fallback
            );
        }
        fallback
    })
}

fn probe_nvenc_internal() -> Option<HardwareProbeResult> {
    // 探路 + cuInit：无 NVIDIA 驱动时在这里返回 None，不会走到 cudarc 的 panic 路径
    nvenc::ensure_cuda_init().ok()?;
    let nvenc_driver = nvenc::NvencDriver::load().ok()?;
    let ordinal = nvenc_device_ordinal();
    let cuda_ctx = nvenc::new_cuda_context(ordinal).ok()?;
    let gpu_name = nvenc::cuda::enumerate_devices()
        .into_iter()
        .find(|d| d.ordinal == ordinal)
        .map(|d| d.name)
        .unwrap_or_default();
    // `new` 内部已显式绑定当前线程（primary context 不像 cuCtxCreate 那样自动绑定）；
    // 析构时先解绑、再 `cuDevicePrimaryCtxRelease` 减引用计数

    let encoder = nvenc::session_params::open_encoder(nvenc_driver, &cuda_ctx).ok()?;

    let mut guid_count = 0u32;
    let get_guid_count = nvenc_driver.api().nvEncGetEncodeGUIDCount?;
    // SAFETY: encoder 为 open_encoder 刚建立的会话句柄且此刻存活；
    // guid_count 为合法 u32 出参槽位
    let res = unsafe { get_guid_count(encoder.as_raw(), &mut guid_count) };
    if res != nvenc::ffi::NVENCSTATUS::NV_ENC_SUCCESS || guid_count == 0 {
        // 编码器随 encoder 句柄析构销毁
        return None;
    }

    let mut guids = vec![
        nvenc::ffi::GUID {
            Data1: 0,
            Data2: 0,
            Data3: 0,
            Data4: [0; 8],
        };
        guid_count as usize
    ];
    let mut fetched_count = 0u32;
    let get_guids = nvenc_driver.api().nvEncGetEncodeGUIDs?;
    // SAFETY: guids 已按驱动报告的 guid_count 预留且预填零值 GUID（bindgen 生成的
    // C POD，全零位合法），容量与之相符；fetched_count 为合法 u32 出参槽位；
    // encoder 为有效会话句柄且此刻存活
    let res = unsafe {
        get_guids(
            encoder.as_raw(),
            guids.as_mut_ptr(),
            guid_count,
            &mut fetched_count,
        )
    };
    if res != nvenc::ffi::NVENCSTATUS::NV_ENC_SUCCESS {
        return None;
    }
    guids.truncate(fetched_count as usize);

    let mut supported_codecs = Vec::new();
    if guids.contains(&nvenc::ffi::NV_ENC_CODEC_AV1_GUID) {
        supported_codecs.push(VideoCodec::Av1);
    }
    if guids.contains(&nvenc::ffi::NV_ENC_CODEC_HEVC_GUID) {
        supported_codecs.push(VideoCodec::H265);
    }
    if guids.contains(&nvenc::ffi::NV_ENC_CODEC_H264_GUID) {
        supported_codecs.push(VideoCodec::H264);
    }

    if supported_codecs.is_empty() {
        return None;
    }

    Some(HardwareProbeResult {
        backend_type: EncodeBackendKind::Nvenc,
        supported_codecs,
        // NVENC 的 `encodeWidth/Height` 直接吃 profile 尺寸，没有对齐要求
        coded_align: (1, 1),
        gpu_name,
    })
}

fn probe_vulkan_video_internal() -> Option<HardwareProbeResult> {
    let ctx = match vulkan::context::VulkanInstance::new("Vistrel") {
        Ok(c) => c,
        Err(e) => {
            log::debug!("Vulkan Video 探测失败或未满足条件: {}", e);
            return None;
        }
    };

    // 逐个 codec 查真实编码能力：能建会话的参数都从这份快照派生
    let mut codecs = Vec::new();
    // 对齐粒度按 codec 分别查询，取最大值作为统一粒度：不同 codec 的
    // `pictureAccessGranularity` 可以不同，而采集侧只有一个尺寸。取最大值保证
    // 任一 codec 的编码尺寸都不超过采集尺寸
    let mut align = (1u32, 1u32);
    for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
        match vulkan::caps::probe_codec(&ctx, codec) {
            Ok(caps) => {
                codecs.push(codec);
                align.0 = align.0.max(caps.picture_access_granularity.width);
                align.1 = align.1.max(caps.picture_access_granularity.height);
            }
            Err(e) => log::debug!("{codec:?} 编码能力探测失败: {}", e),
        }
    }

    if codecs.is_empty() {
        return None;
    }

    let props = ctx.properties();
    log::info!(
        "Probed available Vulkan Video hardware video encoder on device: {} (vendor 0x{:04x}), supported codecs: {:?}, pictureAccessGranularity: {}x{}",
        ctx.device_name(),
        props.vendor_id,
        codecs,
        align.0,
        align.1
    );

    Some(HardwareProbeResult {
        backend_type: EncodeBackendKind::VulkanVideo,
        supported_codecs: codecs,
        coded_align: align,
        gpu_name: ctx.device_name(),
    })
}

/// 硬件能力快照
///
/// 三个状态编码进同一个值，因此「快照是否已建立」和「快照内容是什么」是**一次**
/// 原子发布，不会出现「值已写入但标志还没置位」的窗口。
#[derive(Debug)]
enum HardwareCaps {
    /// 尚未探测——此时任何查询都代表调用顺序错误，而不是本机没有硬编
    Unknown,
    /// 已探测：本机没有可用的硬件编码器
    Unavailable,
    /// 已探测：全部探测成功的硬编后端，按厂商优先级排序，[0] 为默认胜者
    Available(Arc<[HardwareProbeResult]>),
}

/// 硬件能力快照：由 `bootstrap_hardware_caps` 在引擎启动路径上写入
///
/// 探测本身不是查询：它会创建并销毁一个 `CudaContext`、开关一次真实 NVENC session
/// （`bootstrap_hardware_caps` 的探测段），或构建完整的 Vulkan instance + device。代价落在
/// **第一个调用它的线程**上
///
/// 用 `LazyLock` 而非 `ArcSwapOption::const_empty()`：`ArcSwap::from_pointee` 不是
/// const fn，而这里需要构造一个带初值的 `Unknown`，让它只分配一次。
static HARDWARE_CAPS: LazyLock<ArcSwap<HardwareCaps>> =
    LazyLock::new(|| ArcSwap::from_pointee(HardwareCaps::Unknown));

/// 当前默认胜者的硬件能力快照
///
/// `None` 表示尚未 bootstrap，或本机确实没有硬件编码器——两者在能力上是同一件事
/// （都不能硬编）。需要区分二者时用 [`is_hardware_caps_bootstrapped`]；需要全部
/// 候选后端时用 [`hardware_backends`]。
pub fn hardware_caps() -> Option<Arc<HardwareProbeResult>> {
    // `load_full` 多克隆一次外层 Arc（引用计数 +1），换来的是 `&*caps` 上无歧义的解引用：
    // `Guard<Arc<T>>` 的 Deref 目标在不同版本里容易读错，这里不赌。
    let caps = HARDWARE_CAPS.load_full();
    match &*caps {
        HardwareCaps::Available(list) => list.first().cloned().map(Arc::new),
        HardwareCaps::Unknown | HardwareCaps::Unavailable => None,
    }
}

/// 全部已探测的硬编后端清单（厂商优先级序，首元素即 [`hardware_caps`] 的默认胜者）
pub fn hardware_backends() -> Option<Arc<[HardwareProbeResult]>> {
    let caps = HARDWARE_CAPS.load_full();
    match &*caps {
        HardwareCaps::Available(list) => Some(Arc::clone(list)),
        HardwareCaps::Unknown | HardwareCaps::Unavailable => None,
    }
}

/// 快照是否已建立
///
/// 区分「尚未探测」与「探测过、确实没有」——两者对 [`hardware_caps`] 都是 `None`，
/// 但诊断意义完全相反：前者是调用顺序错误，后者是本机能力缺失。
pub fn is_hardware_caps_bootstrapped() -> bool {
    let caps = HARDWARE_CAPS.load_full();
    !matches!(&*caps, HardwareCaps::Unknown)
}

/// 纯格式化：`后端|codec,codec[|显卡名]`，显卡段仅在名称可得时携带
fn format_caps_summary(
    backend: EncodeBackendKind,
    codecs: &[VideoCodec],
    gpu_name: &str,
) -> String {
    let joined = codecs
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let summary = format!("{}|{}", backend.as_str(), joined);
    if gpu_name.is_empty() {
        summary
    } else {
        format!("{}|{}", summary, gpu_name)
    }
}

/// 面向控制面的编码能力摘要（拉式查询）
///
/// 纯读常驻快照，进程内硬件能力不变：已探测返回 `nvenc|H264,H265,AV1|显卡名` 式摘要；
/// 已探测且无硬件返回 `software|H264`（软件编码仅支持 H.264）；尚未探测返回 `unknown`。
pub fn encoder_caps_summary() -> String {
    let caps = HARDWARE_CAPS.load_full();
    match &*caps {
        HardwareCaps::Available(list) => {
            // Available 清单恒非空（首元素即胜者），由探测写入点保证
            let winner = list.first().expect("Available 快照的清单不得为空");
            format_caps_summary(
                winner.backend_type,
                &winner.supported_codecs,
                &winner.gpu_name,
            )
        }
        HardwareCaps::Unavailable => "software|H264".to_string(),
        HardwareCaps::Unknown => "unknown".to_string(),
    }
}

/// 采集侧出帧时使用的编码尺寸对齐粒度
///
/// 唯一来源是驱动声明的 `pictureAccessGranularity`（见
/// [`HardwareProbeResult::coded_align`]）。快照未建立时回退 `(16, 16)`：
pub fn coded_align() -> (usize, usize) {
    hardware_caps()
        .map(|c| (c.coded_align.0 as usize, c.coded_align.1 as usize))
        .unwrap_or((16, 16))
}

/// 协商回执能力清单（纯函数）：探测清单原序映射，软件编码恒以 H264 收尾；
/// 空探测清单产出仅含软件编码的单元素清单
fn caps_options_from(entries: &[HardwareProbeResult]) -> Vec<(EncodeBackendKind, Vec<VideoCodec>)> {
    let mut options: Vec<_> = entries
        .iter()
        .map(|p| (p.backend_type, p.supported_codecs.clone()))
        .collect();
    options.push((EncodeBackendKind::Software, vec![VideoCodec::H264]));
    options
}

/// 协商回执的编码能力清单：硬编按优先级序在前、软件编码恒收尾
/// （远程玩家设置面板「编码后端 / Codec」的可选值来源）
pub fn encoder_caps_options() -> Vec<(EncodeBackendKind, Vec<VideoCodec>)> {
    match &*HARDWARE_CAPS.load_full() {
        HardwareCaps::Available(list) => caps_options_from(list),
        HardwareCaps::Unavailable | HardwareCaps::Unknown => caps_options_from(&[]),
    }
}

/// 用实际建成的编码器校正快照：驱动声明的对齐粒度与建成值可能不一致，建成值才是
/// 采集侧出帧的真相。按后端身份对号入座；清单外后端无从校正（协商层会拒绝指定
/// 清单外后端），保持快照不动。
pub(crate) fn publish_actual_backend(backend_type: EncodeBackendKind, coded_align: (u32, u32)) {
    // 软编不进硬件能力快照：快照只承载硬件后端，软编构造时无快照可校正
    if matches!(backend_type, EncodeBackendKind::Software) {
        return;
    }
    let caps = HARDWARE_CAPS.load_full();
    let HardwareCaps::Available(list) = &*caps else {
        return;
    };
    if list
        .iter()
        .all(|p| p.backend_type != backend_type || p.coded_align == coded_align)
    {
        return;
    }
    let mut corrected = list.as_ref().to_vec();
    let entry = corrected
        .iter_mut()
        .find(|p| p.backend_type == backend_type)
        .expect("存在性已由上面的 all 检查确认");
    entry.coded_align = coded_align;
    HARDWARE_CAPS.store(Arc::new(HardwareCaps::Available(corrected.into())));
}

/// 探测并发布硬件能力快照——**唯一写入点**，由引擎启动路径调用
///
/// **首探之后快照常驻**：同一进程内的后续调用直接返回既有快照，不再重探。
/// 反复「retain → 建 NVENC 会话 → 销毁 → release」全周期会触发驱动状态劣化
/// （十余轮后 `cuDevicePrimaryCtxRetain` 永久返回 `CUDA_ERROR_UNKNOWN`，
/// 引用计数配平且全部销毁调用成功仍复现，已在测试中实证）；而硬件能力在
/// 进程生命周期内不会变化，重探唯一换来的只是踩中该限制的风险。
///
/// 探测按 Vulkan 主设备厂商圈定候选：NVIDIA 上 NVENC 与 Vulkan Video 都试，
/// NVENC 优先（Tier 1）；AMD/Intel 探测 Vulkan Video；无 Vulkan 环境兜底探测
/// NVENC。全部成功的后端进入清单，优先序即清单序。
pub fn bootstrap_hardware_caps() -> Option<Arc<HardwareProbeResult>> {
    // 首探之后快照常驻：只有尚未探测（Unknown）才走真实探测流程
    {
        let existing = HARDWARE_CAPS.load_full();
        match &*existing {
            HardwareCaps::Available(list) => {
                return list.first().cloned().map(Arc::new);
            }
            HardwareCaps::Unavailable => return None,
            HardwareCaps::Unknown => {}
        }
    }

    let detected: Vec<HardwareProbeResult> = match vulkan::context::VulkanInstance::new("Vistrel") {
        // NVIDIA：NVENC (Tier 1) 与 Vulkan Video (Tier 2) 都进候选清单
        Ok(ctx) if ctx.properties().vendor_id == 0x10DE => {
            let mut found = Vec::new();
            if let Some(probe) = probe_nvenc_internal() {
                found.push(probe);
            }
            if let Some(probe) = probe_vulkan_video_internal() {
                found.push(probe);
            }
            found
        }
        // AMD (0x1002) / Intel (0x8086) / 其他：探测 Vulkan Video
        Ok(_) => probe_vulkan_video_internal().into_iter().collect(),
        // 无 Vulkan 环境兜底探测 NVENC
        Err(_) => probe_nvenc_internal().into_iter().collect(),
    };

    let snapshot = if detected.is_empty() {
        log::warn!("未探测到硬件编码器，本引擎生命周期内将使用软件编码");
        HardwareCaps::Unavailable
    } else {
        log::info!(
            "硬件编码能力快照: {}",
            detected
                .iter()
                .map(|p| format!("{:?} codecs={:?}", p.backend_type, p.supported_codecs))
                .collect::<Vec<_>>()
                .join("; ")
        );
        HardwareCaps::Available(detected.into())
    };

    // 一次 store 同时发布「已探测」与探测结果
    HARDWARE_CAPS.store(Arc::new(snapshot));

    hardware_caps()
}

#[cfg(test)]
mod tests {
    use crate::encode::encoder::VideoEncoder;
    use crate::encode::hardware::nvenc::new_cuda_context;
    use crate::encode::hardware::slots::SlotStateBlock;
    use crate::encode::pool::test_support::{host_payload, pool_with};
    use crate::encode::pool::{POOL_DEPTH, heap_blocks};
    use crate::encode::{FramePayload, StreamingProfile};

    use super::*;

    /// 硬编后端用例的槽位所有权块：随用例泄漏 64B，测试进程内可忽略
    fn test_slot_state() -> &'static SlotStateBlock {
        SlotStateBlock::new_leaked()
    }

    /// 控制面摘要线格式：后端名与 codec 名的拼接次序、分隔符不可漂移；
    /// 显卡段仅在名称可得时作为第三段携带
    #[test]
    fn test_caps_summary_wire_format() {
        assert_eq!(
            format_caps_summary(
                EncodeBackendKind::Nvenc,
                &[VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
                "NVIDIA GeForce RTX 5060"
            ),
            "nvenc|H264,H265,AV1|NVIDIA GeForce RTX 5060"
        );
        assert_eq!(
            format_caps_summary(
                EncodeBackendKind::VulkanVideo,
                &[VideoCodec::H264],
                "AMD Radeon"
            ),
            "vulkan_video|H264|AMD Radeon"
        );
        assert_eq!(
            format_caps_summary(EncodeBackendKind::Nvenc, &[VideoCodec::H264], ""),
            "nvenc|H264"
        );
        assert_eq!(
            format_caps_summary(EncodeBackendKind::Software, &[VideoCodec::H264], ""),
            "software|H264"
        );
    }

    /// 协商回执能力清单的构造规则：探测清单原序保留，软件编码恒以 H264 收尾；
    /// 空清单（未探测/无硬件）产出仅含软件编码的单元素清单
    #[test]
    fn test_caps_options_appends_software_tail() {
        let nvenc = HardwareProbeResult {
            backend_type: EncodeBackendKind::Nvenc,
            supported_codecs: vec![VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264],
            coded_align: (1, 1),
            gpu_name: "RTX".to_string(),
        };
        let vulkan = HardwareProbeResult {
            backend_type: EncodeBackendKind::VulkanVideo,
            supported_codecs: vec![VideoCodec::H264],
            coded_align: (64, 16),
            gpu_name: String::new(),
        };

        let options = caps_options_from(&[nvenc.clone(), vulkan.clone()]);
        assert_eq!(
            options,
            vec![
                (
                    EncodeBackendKind::Nvenc,
                    vec![VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264]
                ),
                (EncodeBackendKind::VulkanVideo, vec![VideoCodec::H264]),
                (EncodeBackendKind::Software, vec![VideoCodec::H264]),
            ]
        );
        assert_eq!(
            caps_options_from(&[]),
            vec![(EncodeBackendKind::Software, vec![VideoCodec::H264])]
        );
    }

    /// 探测已并入 `bootstrap_hardware_caps`，没有绕开快照的旁路。并行执行时两个用例
    /// 各写一次快照，但写的是同一个值，且断言用的是各自返回值，无逻辑冲突。
    #[test]
    #[ignore = "requires GPU"]
    fn test_hardware_encoder_probing() {
        let probed = bootstrap_hardware_caps();
        println!("Probed Hardware Video Encoder: {:?}", probed);
        // 快照常驻行为下本测试的断言：有 NVENC 驱动库的机器上快照必须是
        // Available。「首探产出 Nvenc」的断言由排序更先的
        // test_caps_snapshot_stable_after_session_lifecycle 承担——本测试执行时
        // publish_actual_backend 可能已按实际建成的后端合法校正过快照，
        // backend_type 不能再被钉死为单一值。
        #[cfg(target_os = "linux")]
        {
            if std::path::Path::new("/lib64/libnvidia-encode.so.1").exists() {
                assert!(probed.is_some(), "本机有 NVENC 驱动库，快照必须探测到硬件");
            }
        }
    }

    /// 验证 Vulkan Video 硬件编码器生命周期及多编解码器 (H.264 / H.265 / AV1)
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_hardware_encoder_lifecycle() {
        let probe = bootstrap_hardware_caps();
        let supported = probe
            .map(|p| p.supported_codecs.clone())
            .unwrap_or_default();
        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            if supported.contains(&codec) {
                let profile = StreamingProfile::new(1920, 1080, 60, 12_400_000);
                let (mut encoder, blocks) = match VideoEncoder::new_backend(
                    profile,
                    codec,
                    EncodeBackendKind::VulkanVideo,
                    Some(test_slot_state()),
                ) {
                    Ok(pair) => pair,
                    Err(e) => {
                        println!("Vulkan Video {codec:?} 初始化失败，跳过: {e}");
                        continue;
                    }
                };
                let pool = pool_with(blocks);
                assert_eq!(encoder.backend_kind(), EncodeBackendKind::VulkanVideo);
                assert_eq!(encoder.profile(), profile);

                // 测试重配分辨率 (1080p -> 720p)：几何变化换了设备，后端交回新批
                let new_profile = StreamingProfile::new(1280, 720, 60, 4_000_000);
                let fresh = encoder
                    .reconfigure(new_profile, codec)
                    .expect("重配必须成功");
                pool.attach(fresh);
                assert_eq!(encoder.profile(), new_profile);
            }
        }
    }

    /// 编码器构造时采纳的初始 Profile，必须与传入的目标一致
    #[test]
    #[ignore = "requires GPU"]
    fn test_new_backend_keeps_requested_profile() {
        let profile = StreamingProfile::new(1920, 1080, 60, 12_400_000);
        let (encoder, _blocks) = match VideoEncoder::new_backend(
            profile,
            VideoCodec::H264,
            EncodeBackendKind::Nvenc,
            Some(test_slot_state()),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                println!("NVENC 初始化失败 (驱动不支持或无硬件)，跳过: {e}");
                return;
            }
        };
        assert_eq!(encoder.profile(), profile);
    }

    /// NVENC 重建式重配后编码器继续可用：`cuda_ctx` 不随会话重建，旧块仍属于本域
    #[test]
    #[ignore = "requires GPU"]
    fn test_reconfigure_keeps_imported_vram_slots() {
        if new_cuda_context(0).is_err() {
            println!("无 CUDA 环境，跳过重配存活用例");
            return;
        }
        let (mut encoder, blocks) = match VideoEncoder::new_backend(
            StreamingProfile::default(),
            VideoCodec::H264,
            EncodeBackendKind::Nvenc,
            Some(test_slot_state()),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                println!("NVENC 初始化失败，跳过: {e}");
                return;
            }
        };
        let pool = pool_with(blocks);

        let fresh = encoder
            .reconfigure(
                StreamingProfile::new(1280, 720, 60, 4_000_000),
                VideoCodec::H264,
            )
            .expect("NVENC 重配失败");
        assert!(fresh.is_empty(), "NVENC 不换 CUDA 上下文，重配不该交回新批");
        pool.attach(fresh);
        assert_eq!(encoder.profile().width(), 1280);

        // 重配后编码路径仍然可用：产出有效码流
        let (w, h) = (1280usize, 720usize);
        let payload = host_payload(&pool, vec![128u8; w * h * 4]);
        let (nalu, _) = encoder
            .encode(&payload, true)
            .expect("重配后主机帧编码必须成功");
        assert!(!nalu.is_empty());
    }

    /// 后端把自己的块交回：硬编一批设备块，软编一批堆块，池不关心是哪族
    #[test]
    #[ignore = "requires GPU"]
    fn test_backend_supplies_blocks_for_the_pool() {
        crate::encode::software::init_api_for_tests();
        let profile = StreamingProfile::new(1280, 720, 60, 4_000_000);

        match VideoEncoder::new_backend(
            profile,
            VideoCodec::H264,
            EncodeBackendKind::VulkanVideo,
            Some(test_slot_state()),
        ) {
            Ok((_hw, blocks)) => {
                assert_eq!(blocks.len(), POOL_DEPTH, "硬编必须交回一整批块");
                let pool = pool_with(blocks);
                assert!(pool.acquire().is_some(), "装进池后必须能取到块");
            }
            Err(e) => println!("Vulkan Video 不可用，跳过: {e}"),
        }

        let (_soft, heap) =
            VideoEncoder::new_software(profile, VideoCodec::H264).expect("软编必须可建");
        assert_eq!(heap.len(), POOL_DEPTH, "软编交回的是一整批堆块");
        assert!(
            heap.iter()
                .all(|b| b.cuda_host().is_none() && b.vulkan_buffer().is_none()),
            "软编的块不得持有任何设备内存"
        );
    }

    /// 几何变化重建后端 = 新设备 = 新内存域：旧域的块绝不能被新设备再取用
    #[test]
    #[ignore = "requires GPU"]
    fn test_geometry_change_moves_old_blocks_to_stale() {
        use crate::encode::VideoCodec::H264;
        let (mut encoder, blocks) = match VideoEncoder::new_backend(
            StreamingProfile::new(1920, 1080, 60, 12_400_000),
            H264,
            EncodeBackendKind::VulkanVideo,
            Some(test_slot_state()),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                println!("Vulkan Video 不可用，跳过: {e}");
                return;
            }
        };
        let pool = pool_with(blocks);
        // 旧域在途帧：还回时域已变，只能进 stale
        let in_flight = pool.acquire().expect("池里必须有余量");

        let fresh = encoder
            .reconfigure(StreamingProfile::new(1280, 720, 60, 4_000_000), H264)
            .expect("几何变化必须能重配");
        assert!(!fresh.is_empty(), "几何变化换了设备，后端必须交回新批块");
        pool.attach(fresh);
        assert_eq!(pool.stale_len(), 0, "换批时池里旧域的块必须已释放");
        assert_eq!(pool.len(), POOL_DEPTH, "当前批是全新的一批");

        drop(in_flight);
        assert_eq!(pool.stale_len(), 1, "晚归的旧域块只能进 stale");
        assert_eq!(pool.len(), POOL_DEPTH, "旧域块不得回到当前批");
    }

    /// 对 trait 编写的共享生命周期用例：请求哪个后端，同一组动作就跑在哪个后端上。
    /// 硬件缺席的后端自动跳过；此用例是「新后端接入 = 一个 impl」的验收面。
    #[test]
    #[ignore = "requires GPU"]
    fn test_backend_trait_shared_lifecycle() {
        for encoder_type in [EncodeBackendKind::Nvenc, EncodeBackendKind::VulkanVideo] {
            let (w, h) = (1280usize, 720usize);
            let profile = StreamingProfile::new(w, h, 60, 4_000_000);
            let (mut encoder, blocks) = match VideoEncoder::new_backend(
                profile,
                VideoCodec::H264,
                encoder_type,
                Some(test_slot_state()),
            ) {
                Ok(pair) => pair,
                Err(e) => {
                    println!("{encoder_type:?} 初始化失败，跳过: {e}");
                    continue;
                }
            };
            let pool = pool_with(blocks);
            assert_eq!(encoder.backend_kind(), encoder_type, "请求即所得");

            let payload = host_payload(&pool, vec![128u8; w * h * 4]);
            let (nalu, is_key) = encoder
                .encode(&payload, true)
                .expect("首帧关键帧编码必须成功");
            assert!(is_key, "首帧必须是关键帧");
            assert!(!nalu.is_empty(), "码流不得为空");

            let fresh = encoder
                .reconfigure(
                    StreamingProfile::new(1920, 1080, 60, 12_400_000),
                    VideoCodec::H264,
                )
                .expect("重配必须成功");
            pool.attach(fresh);
            encoder.drain().expect("排空必须成功");
        }
    }

    /// NVENC 直接消费 CUDA 锁页块并输出有效码流
    #[test]
    #[ignore = "requires GPU"]
    fn test_nvenc_encodes_device_pool_frame() {
        if new_cuda_context(0).is_err() {
            println!("无 CUDA 环境，跳过锁页硬编测试");
            return;
        }
        let (w, h) = (1280, 720);
        let profile = StreamingProfile::new(w, h, 60, 4_000_000);
        let (mut encoder, blocks) = match VideoEncoder::new_backend(
            profile,
            VideoCodec::H264,
            EncodeBackendKind::Nvenc,
            Some(test_slot_state()),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                println!("NVENC 初始化失败 (驱动不支持或无硬件)，跳过: {e}");
                return;
            }
        };
        let pool = pool_with(blocks);

        let payload = host_payload(&pool, vec![128u8; w * h * 4]);

        let (nalu, is_key) = encoder
            .encode(&payload, true)
            .expect("NVENC 硬件编码必须成功");
        assert!(is_key, "请求关键帧必须返回 true");
        assert!(!nalu.is_empty(), "NVENC 硬件编码必须产出非空码流");
        assert!(
            nalu.starts_with(&[0, 0, 0, 1]) || nalu.starts_with(&[0, 0, 1]),
            "NALU 必须以标准起始码开头"
        );
    }

    /// 堆块也是合法输入：Vulkan 后端没有设备块时走 CPU 暂存路径，仍须产出正确码流
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_encodes_heap_frame() {
        let (w, h) = (1280, 720);
        let profile = StreamingProfile::new(w, h, 60, 4_000_000);
        let (mut encoder, blocks) = match VideoEncoder::new_backend(
            profile,
            VideoCodec::H264,
            EncodeBackendKind::VulkanVideo,
            Some(test_slot_state()),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                println!("Vulkan Video 初始化失败 (驱动不支持或无硬件)，跳过: {e}");
                return;
            }
        };
        let _device_pool = pool_with(blocks);

        let heap_pool = pool_with(heap_blocks());
        let payload = host_payload(&heap_pool, vec![128u8; w * h * 4]);
        let (nalu, is_key) = encoder
            .encode(&payload, true)
            .expect("Vulkan Video 必须能编码堆块（CPU 暂存路径）");
        assert!(is_key, "请求关键帧必须返回 true");
        assert!(!nalu.is_empty(), "编码必须产出非空码流");
    }

    /// Vulkan 设备块的端到端：后端交回块 → 装池 → 生产者取块直写 → 编码
    /// 产出有效码流；帧归还后池恢复深度。GPU CSC 可用的机器上走零拷贝快路径
    /// (`encode_buffer_frame`)，不可用的机器走主机字节路径，两者产出都必须正确
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_encodes_pool_frame() {
        let (w, h) = (1280, 720);
        let profile = StreamingProfile::new(w, h, 60, 4_000_000);
        let (mut encoder, blocks) = match VideoEncoder::new_backend(
            profile,
            VideoCodec::H264,
            EncodeBackendKind::VulkanVideo,
            Some(test_slot_state()),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                println!("Vulkan Video 初始化失败 (驱动不支持或无硬件)，跳过: {e}");
                return;
            }
        };
        let pool = pool_with(blocks);
        assert_eq!(pool.len(), POOL_DEPTH, "装批后池深度必须等于批大小");

        let payload = host_payload(&pool, vec![128u8; w * h * 4]);
        assert_eq!(pool.len(), POOL_DEPTH - 1, "取走一块后池里必须少一块");

        let (nalu, is_key) = encoder
            .encode(&payload, true)
            .expect("Vulkan 设备块编码必须成功");
        assert!(is_key, "请求关键帧必须返回 true");
        assert!(!nalu.is_empty(), "编码必须产出非空码流");
        assert!(
            nalu.starts_with(&[0, 0, 0, 1]) || nalu.starts_with(&[0, 0, 1]),
            "NALU 必须以标准起始码开头"
        );
        drop(payload);

        // 编码同步完成后帧才析构归还：池必须恢复满深度，下一帧可再次取块
        assert_eq!(pool.len(), POOL_DEPTH, "帧归还后池必须恢复深度");
    }

    /// 验证 Vulkan Video 编码未注册的 VRAM 槽位时能正确报错
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_vram_slot_not_found_returns_err() {
        let profile = StreamingProfile::new(1280, 720, 60, 4_000_000);
        let (mut encoder, _blocks) = match VideoEncoder::new_backend(
            profile,
            VideoCodec::H264,
            EncodeBackendKind::VulkanVideo,
            Some(test_slot_state()),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                println!("Vulkan Video 初始化失败 (驱动不支持或无硬件)，跳过: {e}");
                return;
            }
        };

        let payload = FramePayload::VramSlot {
            slot_idx: 99,
            sync_value: 0,
        };
        let res = encoder.encode(&payload, false);
        assert!(res.is_err(), "未导入槽位编码必须返回 Err");
        assert!(res.unwrap_err().contains("VRAM slot 99 not found"));
    }
}

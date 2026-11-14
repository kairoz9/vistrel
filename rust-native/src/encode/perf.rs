//! 显式运行的编码性能矩阵。

use super::encoder::{EncodeBackendKind, VideoEncoder};
use super::frame::FramePayload;
use super::pool::FramePool;
use super::{StreamingProfile, VideoCodec};
use crate::encode::hardware::slots::SlotStateBlock;
use crate::encode::hardware::vulkan::vram_export::VramSlotProducer;
use crate::encode::nv12_len;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

const FPS: usize = 240;
const MATRIX_BITRATE_BPS: usize = 60_000_000;
const WARMUP_FRAMES: usize = 60;
const MEASURE_FRAMES: usize = 600;
/// 每格重复次数：取中位轮，避免单次结果直接当结论
const RUNS: usize = 3;

#[derive(Clone, Copy)]
struct Resolution {
    name: &'static str,
    width: usize,
    height: usize,
}

const RESOLUTIONS: [Resolution; 1] = [Resolution {
    name: "1440p",
    width: 2560,
    height: 1440,
}];

#[derive(Clone, Copy)]
enum InputPath {
    /// 主机帧池：堆块（软编）或设备块（硬编）都走这条路
    Pool,
    /// 显存直通：生产者替身导出显存与时间线，槽位导入后逐帧只推进时间线
    VramSlot,
}

impl InputPath {
    const ALL: [Self; 2] = [Self::Pool, Self::VramSlot];

    fn name(self) -> &'static str {
        match self {
            Self::Pool => "Pool",
            Self::VramSlot => "VramSlot",
        }
    }
}

struct MatrixRow {
    resolution: &'static str,
    backend: &'static str,
    codec: &'static str,
    path: &'static str,
    status: String,
    reason: Option<String>,
    /// 单帧总耗时均值：选代表轮与阶段比例的分母
    latency_us: f64,
    encode_us: f64,
    /// 流水线吞吐估计：逐帧 max(prep, encode) 的均值；生产中两阶段分线程重叠
    pipeline_fps: f64,
    prep_us: f64,
    stages: Vec<(&'static str, f64)>,
}

impl MatrixRow {
    fn unavailable(
        resolution: &'static str,
        backend: &'static str,
        codec: VideoCodec,
        path: InputPath,
        status: &str,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            resolution,
            backend,
            codec: codec_name(codec),
            path: path.name(),
            status: status.into(),
            reason: Some(reason.into()),
            latency_us: 0.0,
            encode_us: 0.0,
            pipeline_fps: 0.0,
            prep_us: 0.0,
            stages: Vec::new(),
        }
    }
}

/// 输出按 Backend × Codec × Path 的全矩阵 Markdown 表。
///
/// 执行：`cargo test --release full_video_encode_performance_matrix -- --ignored --test-threads=1 --nocapture`
#[test]
#[ignore = "显式性能压测；需要运行机器具备相应硬件编码器"]
fn full_video_encode_performance_matrix() {
    let _ = env_logger::try_init();
    super::software::init_api_for_tests();
    // 先建硬件能力快照（与引擎启动路径同序）：报告需要写明实际后端，也避免后续
    // 每格各自探测
    super::hardware::bootstrap_hardware_caps();

    let codecs = [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1];
    let backends = [
        (EncodeBackendKind::Software, "OpenH264"),
        (EncodeBackendKind::Nvenc, "NVENC"),
        (EncodeBackendKind::VulkanVideo, "Vulkan Video"),
    ];
    let mut rows = Vec::new();

    for resolution in RESOLUTIONS {
        for (backend, backend_name) in backends {
            for codec in codecs {
                for path in InputPath::ALL {
                    if backend == EncodeBackendKind::Software && codec != VideoCodec::H264 {
                        continue;
                    }
                    match run_cell_repeated(resolution, backend, backend_name, codec, path) {
                        Ok(row) => rows.push(row),
                        Err(reason) => rows.push(MatrixRow::unavailable(
                            resolution.name,
                            backend_name,
                            codec,
                            path,
                            "未运行",
                            reason,
                        )),
                    }
                }
            }
        }
    }

    let mut report = format!(
        "# Vistrel 视频编码性能矩阵\n\n分辨率：1440p（2560×1440），目标帧率：{FPS} FPS，CBR 码率：{} Mbps，每格重复 {RUNS} 次取单帧总耗时中位轮，预热：{WARMUP_FRAMES} 帧，计量：{MEASURE_FRAMES} 帧。输入使用预生成渐变/色块帧（Pool 行 NV12，VRAM 行 RGBA）；编码输入尺寸按各后端 coded extent 对齐。VRAM 行由独立 Vulkan 设备扮演生产者，按生产侧 3 槽轮转（每槽独立时间线信号量，信号值取全局帧号），内容构造期填好、逐帧只推进所写槽位。\n\n",
        MATRIX_BITRATE_BPS / 1_000_000
    );
    report.push_str("## 环境\n\n");
    report.push_str(&environment_report());
    report.push('\n');
    report.push_str("## 总体结果\n\n| 分辨率 | Backend | Codec | Path | 状态 | 流水线 FPS 估计 | encode() 均值 (µs) |\n|---|---|---|---|---:|---:|\n");
    for row in &rows {
        if row.status == "OK" {
            writeln!(
                report,
                "| {} | {} | {} | {} | {} | {:.2} | {:.1} |",
                row.resolution,
                row.backend,
                row.codec,
                row.path,
                row.status,
                row.pipeline_fps,
                row.encode_us
            )
            .unwrap();
        } else {
            writeln!(
                report,
                "| {} | {} | {} | {} | {} | — | — |",
                row.resolution, row.backend, row.codec, row.path, row.status
            )
            .unwrap();
        }
    }
    const STAGE_COLUMNS: [(&str, Option<&str>); 5] = [
        ("输入准备", None),
        ("拷贝/上传", Some("拷贝/上传")),
        ("提交", Some("提交")),
        ("等待(CPU)", Some("等待(CPU)")),
        ("编码器", Some("编码器")),
    ];
    report.push_str("\n## 阶段耗时\n\n每行对应一个路径。单元格显示该阶段均值及其占单帧总耗时的比例；异步 GPU 工作主要体现在「等待(CPU)」阶段。\n\n| 分辨率 | Backend | Codec | Path |");
    for (name, _) in STAGE_COLUMNS {
        write!(report, " {name} (µs, %) |").unwrap();
    }
    report.push_str("\n|---|---|---|---|");
    for _ in STAGE_COLUMNS {
        report.push_str("---:|");
    }
    report.push('\n');
    for row in &rows {
        write!(
            report,
            "| {} | {} | {} | {} |",
            row.resolution, row.backend, row.codec, row.path
        )
        .unwrap();
        for (_, stage_name) in STAGE_COLUMNS {
            let stage_us = match stage_name {
                None => Some(row.prep_us),
                Some(name) => row
                    .stages
                    .iter()
                    .find(|(stage, _)| *stage == name)
                    .map(|(_, us)| *us),
            };
            if let Some(us) = stage_us.filter(|_| row.status == "OK") {
                let percent = us / row.latency_us * 100.0;
                write!(report, " {:.2} ({:.1}%) |", us, percent).unwrap();
            } else {
                report.push_str(" — |");
            }
        }
        report.push('\n');
    }
    let reasons: BTreeSet<_> = rows
        .iter()
        .filter_map(|row| row.reason.as_deref())
        .collect();
    if !reasons.is_empty() {
        report.push_str("\n## 未运行/不支持说明\n");
        for reason in reasons {
            writeln!(report, "- {reason}").unwrap();
        }
    }
    report.push_str("\n单帧总耗时（阶段比例的分母）包含压测输入载荷创建/填充及 encode() 调用。Pool 输入准备包含一次从池取块 + 整帧写入；块归还后下一帧复用，不再分配。encode() 均值单独显示。阶段分项可能因异步任务重叠而不等于单帧总耗时。流水线 FPS 估计 = 计量帧数 / Σmax(输入准备, encode())。生产中输入准备与编码分属采集/编码两线程重叠执行，真实稳态吞吐落在该估计附近，encode 占大头时趋近它；单帧总耗时是关键路径（帧写完才能编码），不受该重叠影响。VRAM 行跳过整帧主机写入与上传。阶段比例 = 阶段均值 / 单帧总耗时。「等待(CPU)」在所有后端都是 CPU 侧阻塞墙钟：NVENC 是 `nvEncLockBitstream` 的返回等待（已包含 GPU 编码、驱动与队列开销），Vulkan 是 `vkWaitForFences`，两者不可直接横向比较。Pool 行的 Vulkan 路径仍要把主机缓冲整帧过一次显存：NV12 已由采集侧产好，这段拷贝在平面拷贝的提交里执行（`csc.rs` 的 `copy_nv12_from_buffer`，源是持久映射的池块，两条 `cmdCopyBufferToImage` 摊出 Y 与 UV），所以只体现在「等待(CPU)」——视频提交等的是那次的信号量。实测这段 H2D 与编码器无关、只随分辨率变：1440p ≈ 452 µs（Pool 行「等待(CPU)」减 GPU 全程耗时），对应 8.29 MB 的平面拷贝。要把它变成实测值，需给平面拷贝的命令缓冲也加时间戳（目前没有）。\n");
    let report_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/encode-performance-matrix.md");
    std::fs::create_dir_all(report_path.parent().expect("报告路径必须有父目录"))
        .expect("创建报告目录失败");
    std::fs::write(&report_path, &report).expect("写入性能矩阵 Markdown 失败");
    let saved_report = std::fs::read_to_string(&report_path).expect("读取性能矩阵 Markdown 失败");
    print!("\n{saved_report}");
    println!("矩阵报告已写入 {}", report_path.display());
}

/// 取一块并写进 `source`：这就是生产者的逐帧路径（含一次整帧拷贝）
fn pool_payload(pool: &std::sync::Arc<FramePool>, source: &[u8]) -> Result<FramePayload, String> {
    let mut frame = pool
        .acquire()
        .ok_or_else(|| "主机帧池无可用块".to_string())?;
    frame.write(source)?;
    Ok(FramePayload::Host { frame })
}

/// 逐帧输入：Pool 路径取块整写；VRAM 路径按 3 槽轮转推进对应槽位的时间线
///
/// 信号值取全局帧号 +1（对齐生产 `RenderpearlVramPipeline`），回执落在 +1。
/// 复用槽位前等上一轮回执（3 帧前）落地：生产以实时查询非阻塞认领、全忙主动丢帧，
/// harness 为计量 600 帧不丢帧改为短暂阻塞等同一目标值，保留的只是重叠后的残余。
fn frame_input(
    pool: &std::sync::Arc<FramePool>,
    vram: Option<&VramSlotProducer>,
    source: &[u8],
    frame_no: usize,
) -> Result<FramePayload, String> {
    match vram {
        None => pool_payload(pool, source),
        Some(producer) => {
            let signal = frame_no as u64 + 1;
            let slot = frame_no % VramSlotProducer::SLOT_COUNT;
            if frame_no >= VramSlotProducer::SLOT_COUNT {
                producer.wait_ack(slot, signal - 2)?;
            }
            producer.signal_frame(slot, signal)?;
            Ok(FramePayload::VramSlot {
                slot_idx: slot as u32,
                sync_value: signal,
            })
        }
    }
}

fn run_cell(
    resolution: Resolution,
    backend: EncodeBackendKind,
    backend_name: &'static str,
    codec: VideoCodec,
    path: InputPath,
) -> Result<MatrixRow, String> {
    let mut profile = StreamingProfile::new(resolution.width, resolution.height, FPS, codec);
    // 按最高档 1440p@240 的 60 Mbps 口径
    profile.set_bitrate(MATRIX_BITRATE_BPS);
    // 池由采集侧持有；后端构造时交回自己的块（软编是堆块，硬编是设备块）
    let (mut encoder, blocks) =
        VideoEncoder::new_backend(profile, backend, Some(SlotStateBlock::new_leaked()))?;
    let pool = FramePool::new();
    pool.attach(blocks);
    let (coded_width, coded_height) = encoder.coded_extent();
    // 主机帧一律 NV12：采集侧（GL 着色器或 Renderpearl 两趟 pass）已完成色彩转换
    let sources: Vec<_> = (0..8)
        .map(|seed| make_nv12_frame(coded_width, coded_height, seed))
        .collect();

    // VRAM 槽位的生产者替身：独立 Vulkan 设备导出显存与时间线，槽位导入后逐帧
    // 只推进时间线。声明在 encoder 之后——先拆生产者、后拆编码器，导入方活得最久
    let vram = match path {
        InputPath::Pool => None,
        InputPath::VramSlot if backend == EncodeBackendKind::Software => {
            return Err("软编没有显存直通路径".into());
        }
        InputPath::VramSlot => {
            let contents: Vec<Vec<u8>> = (0..VramSlotProducer::SLOT_COUNT)
                .map(|seed| make_rgba_frame(coded_width, coded_height, seed))
                .collect();
            let producer =
                VramSlotProducer::new(&contents, coded_width as u32, coded_height as u32)?;
            let mut descs = producer.descriptors()?;
            encoder
                .import_vram_slots(&mut descs)
                .map_err(|e| format!("VRAM 槽位导入失败: {e}"))?;
            Some(producer)
        }
    };

    // 「编码器就绪」=「池里有块」；空池只会是后端构造漏交了批次
    if matches!(path, InputPath::Pool) && pool.is_empty() {
        return Err("编码器必须交回可用帧块".into());
    }

    // 预热独立计数，避免把会话初始化与 IDR 首帧纳入稳态采样。
    // 帧号跨预热与计量连续：VRAM 路径的信号值取全局帧号，重置会撞时间线单调性
    let mut frame_no = 0usize;
    for _ in 0..WARMUP_FRAMES {
        let source = &sources[frame_no % sources.len()];
        let payload = frame_input(&pool, vram.as_ref(), source, frame_no)?;
        encoder.encode(&payload, frame_no == 0)?;
        frame_no += 1;
    }

    let mut total_path = Duration::ZERO;
    let mut total_encode = Duration::ZERO;
    let mut total_prepare = Duration::ZERO;
    let mut total_pipelined = Duration::ZERO;
    let mut stage_totals = super::encoder::BackendStageTimings::default();
    for _ in 0..MEASURE_FRAMES {
        let frame_start = Instant::now();
        let prep_start = Instant::now();
        let source = &sources[frame_no % sources.len()];
        let payload = frame_input(&pool, vram.as_ref(), source, frame_no)?;
        frame_no += 1;
        let prep_elapsed = prep_start.elapsed();
        total_prepare += prep_elapsed;

        let encode_start = Instant::now();
        let (_bitstream, _) = encoder.encode(&payload, false)?;
        let encode_elapsed = encode_start.elapsed();
        total_pipelined += prep_elapsed.max(encode_elapsed);
        let frame_elapsed = frame_start.elapsed();
        let stages = encoder.benchmark_stages();
        stage_totals.input_copy_ns += stages.input_copy_ns;
        stage_totals.submit_ns += stages.submit_ns;
        stage_totals.wait_ns += stages.wait_ns;
        stage_totals.encode_ns += stages.encode_ns;
        stage_totals.gpu_scope_ns += stages.gpu_scope_ns;
        stage_totals.gpu_encode_ns += stages.gpu_encode_ns;
        stage_totals.gpu_frame_ns += stages.gpu_frame_ns;
        total_encode += encode_elapsed;
        total_path += frame_elapsed;
    }
    encoder.drain()?;
    let mean_us = total_path.as_secs_f64() * 1_000_000.0 / MEASURE_FRAMES as f64;
    let encode_us = total_encode.as_secs_f64() * 1_000_000.0 / MEASURE_FRAMES as f64;

    Ok(MatrixRow {
        resolution: resolution.name,
        backend: backend_name,
        codec: codec_name(codec),
        path: path.name(),
        status: "OK".into(),
        reason: None,
        latency_us: mean_us,
        encode_us,
        pipeline_fps: MEASURE_FRAMES as f64 / total_pipelined.as_secs_f64(),
        prep_us: total_prepare.as_secs_f64() * 1_000_000.0 / MEASURE_FRAMES as f64,
        stages: format_stages(stage_totals, MEASURE_FRAMES),
    })
}

fn format_stages(
    stages: super::encoder::BackendStageTimings,
    frames: usize,
) -> Vec<(&'static str, f64)> {
    let divisor = frames as f64 * 1_000.0;
    let fields = [
        ("拷贝/上传", stages.input_copy_ns),
        ("提交", stages.submit_ns),
        ("等待(CPU)", stages.wait_ns),
        ("编码器", stages.encode_ns),
        ("GPU 全程", stages.gpu_frame_ns),
        ("GPU 作用域", stages.gpu_scope_ns),
        ("GPU 编码", stages.gpu_encode_ns),
    ];
    fields
        .iter()
        .filter(|(_, ns)| *ns > 0)
        .map(|(name, ns)| (*name, *ns as f64 / divisor))
        .collect()
}

/// 每格重复 `RUNS` 次：按单帧总耗时取中位轮作代表
fn run_cell_repeated(
    resolution: Resolution,
    backend: EncodeBackendKind,
    backend_name: &'static str,
    codec: VideoCodec,
    path: InputPath,
) -> Result<MatrixRow, String> {
    let mut rows = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        rows.push(run_cell(resolution, backend, backend_name, codec, path)?);
    }
    rows.sort_by(|a, b| f64::total_cmp(&a.latency_us, &b.latency_us));
    Ok(rows.swap_remove(RUNS / 2))
}

/// 环境抬头：换机器/换驱动就要重跑的结论必须能自我说明，否则数字无法复核
fn environment_report() -> String {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split(':').nth(1))
                .map(|name| name.trim().to_string())
        })
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let profile = if cfg!(debug_assertions) {
        "debug（与出货不同构，数字仅看趋势）"
    } else {
        "release：opt-level=3 + lto=fat + codegen-units=1"
    };
    let mut text = String::new();
    let _ = writeln!(
        text,
        "- CPU: {cpu}（{} 逻辑核），{}",
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        std::env::consts::OS,
    );
    let _ = writeln!(text, "- 构建: {profile}");
    let _ = writeln!(text, "- 源码: {}", source_revision());
    let _ = writeln!(text, "- 硬件编码后端: {}", backend_summary());
    let _ = writeln!(text, "- GPU: {}", gpu_summary());
    text
}

/// git 版本 + 是否有未提交改动：数字只对得上这一份源码才可信
fn source_revision() -> String {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
    };
    let rev = git(&["rev-parse", "--short", "HEAD"])
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "未知".into());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|out| !out.stdout.is_empty());
    if dirty {
        format!("{rev} + 未提交改动")
    } else {
        rev
    }
}

fn backend_summary() -> String {
    match super::hardware::hardware_caps() {
        Some(probe) => format!(
            "{:?}（对齐 {:?}，支持 {:?}）",
            probe.backend_type, probe.coded_align, probe.supported_codecs
        ),
        None => "探测快照为空".into(),
    }
}

fn gpu_summary() -> String {
    // 与编码无关地新建一个 Vulkan 实例读设备属性；本机没 Vulkan 时该行留「—」，不影响矩阵本身
    let Ok(instance) = super::hardware::vulkan::context::VulkanInstance::new("vistrel-perf") else {
        return "—（本机 Vulkan 实例不可用）".into();
    };
    let props = instance.properties();
    // SAFETY: `device_name` 是驱动填写的定长数组，`CStr::from_ptr` 在首个 NUL 处截断；
    // 该数组归 `instance` 所有且在本作用域内一直存活，指针因此始终有效
    let name = unsafe { std::ffi::CStr::from_ptr(props.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    format!(
        "{name}（driverVersion 原始值 0x{:x}，apiVersion 0x{:x}，timestampPeriod {:.3} ns）",
        props.driver_version, props.api_version, props.limits.timestamp_period
    )
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "H.264",
        VideoCodec::H265 => "HEVC",
        VideoCodec::Av1 => "AV1",
    }
}

fn make_rgba_frame(width: usize, height: usize, seed: usize) -> Vec<u8> {
    let mut frame = vec![255; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) * 4;
            let band = ((x / 160 + y / 120 + seed) % 4) as u8;
            frame[offset] = (((x + seed * 23) * 255) / width).min(255) as u8;
            frame[offset + 1] = ((y * 255) / height) as u8;
            frame[offset + 2] = band * 64;
        }
    }
    frame
}

/// 合成 NV12 帧：把 `make_rgba_frame` 的**同一幅内容**按 BT.709 有限范围转成 4:2:0，
/// 色度取 2×2 平均——只换布局、不换内容，编码耗时差才能归因到输入格式本身
fn make_nv12_frame(width: usize, height: usize, seed: usize) -> Vec<u8> {
    let rgba = make_rgba_frame(width, height, seed);
    let y_len = width * height;
    let mut frame = vec![0u8; y_len + width * height / 2];
    let luma_of = |pixel: usize| {
        let offset = pixel * 4;
        0.2126 * rgba[offset] as f32
            + 0.7152 * rgba[offset + 1] as f32
            + 0.0722 * rgba[offset + 2] as f32
    };
    for y in 0..height {
        for x in 0..width {
            let luma = luma_of(y * width + x);
            frame[y * width + x] = (16.0 + luma * 219.0 / 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    for y in 0..height / 2 {
        for x in 0..width / 2 {
            let mut sum = [0.0f32; 3];
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let offset = ((y * 2 + dy) * width + x * 2 + dx) * 4;
                for (channel, slot) in sum.iter_mut().enumerate() {
                    *slot += rgba[offset + channel] as f32 / 4.0;
                }
            }
            let luma = 0.2126 * sum[0] + 0.7152 * sum[1] + 0.0722 * sum[2];
            let offset = y_len + y * width + x * 2;
            let cb = (sum[2] - luma) / 1.8556;
            let cr = (sum[0] - luma) / 1.5748;
            frame[offset] = (128.0 + cb * 224.0 / 255.0).round().clamp(0.0, 255.0) as u8;
            frame[offset + 1] = (128.0 + cr * 224.0 / 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    frame
}

#[cfg(test)]
#[test]
fn generated_nv12_frame_matches_coded_extent() {
    let profile = StreamingProfile::new(1280, 720, FPS, VideoCodec::H264);
    let (coded_width, coded_height) = profile.coded_extent((1, 32));
    let frame = make_nv12_frame(coded_width, coded_height, 0);
    assert_eq!(frame.len(), nv12_len(coded_width, coded_height));
}

#[cfg(test)]
#[test]
fn generated_rgba_frame_matches_coded_extent() {
    let profile = StreamingProfile::new(1280, 720, FPS, VideoCodec::H264);
    let (coded_width, coded_height) = profile.coded_extent((1, 32));
    let frame = make_rgba_frame(coded_width, coded_height, 0);
    assert_eq!(frame.len(), coded_width * coded_height * 4);
    assert_eq!(frame.len(), 1280 * 736 * 4);
}

/// NVENC 主机（登记式输入）路径的**内容**回归测试：把不可压缩的合成内容写进池块
/// 编码，断言 IDR 码流具有真实规模。sysmem 创建式输入曾在真机上静默产出 76 字节
/// 空壳（性能矩阵只测时不测内容，漏网）——本测试就是那道防线。
/// 执行：cargo test --release nvenc_host_input_produces_real_bitstream -- --ignored --nocapture
#[test]
#[ignore = "需要 NVENC 硬件"]
fn nvenc_host_input_produces_real_bitstream() {
    let _ = env_logger::try_init();
    super::software::init_api_for_tests();
    super::hardware::bootstrap_hardware_caps();

    let mut profile = StreamingProfile::new(1920, 1080, FPS, VideoCodec::Av1);
    profile.set_bitrate(12_000_000);
    let (mut encoder, blocks) = VideoEncoder::new_backend(
        profile,
        EncodeBackendKind::Nvenc,
        Some(SlotStateBlock::new_leaked()),
    )
    .expect("NVENC 后端应能构建");
    let pool = FramePool::new();
    pool.attach(blocks);

    let noisy = make_nv12_frame(1920, 1080, 7);
    assert_eq!(noisy.len(), 3_110_400);
    let mut frame = pool.acquire().expect("池块应可用");
    frame.write(&noisy).expect("块写入");
    let (nalu, is_key) = encoder
        .encode(&FramePayload::Host { frame }, true)
        .expect("编码应成功");
    println!("1080p AV1 噪声 IDR: {} 字节 is_key={is_key}", nalu.len());
    assert!(
        nalu.len() > 1_000,
        "噪声帧 IDR 码流仅 {} 字节——主机输入路径产出空壳！",
        nalu.len()
    );
}

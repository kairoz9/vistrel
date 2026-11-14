use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;

use bytes::Bytes;
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Level, RateControlMode,
};
use openh264::formats::YUVSlices;
use openh264_sys2::{
    ENCODER_OPTION, ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SBitrateInfo,
    SPATIAL_LAYER_0, SPATIAL_LAYER_ALL,
};

#[cfg(test)]
use crate::encode::encoder::BackendStageTimings;
use crate::encode::encoder::{EncodeBackendKind, VideoEncodeBackend};
use crate::encode::frame::FramePayload;
use crate::encode::hardware::slots::VramSlotDescriptor;
use crate::encode::pool::HostBlock;
use crate::encode::{MAX_BPP, StreamingProfile, calculate_dynamic_bitrate, h264_level_idc};

/// Cisco OpenH264 库的文件系统路径，由 `startEngine` 导出入口在引擎启动前注入。
/// 游戏侧进程内不变，OnceLock 保留首个注入值；值为解压后的最终库路径。
static OPENH264_LIB_PATH: OnceLock<PathBuf> = OnceLock::new();

/// 注入库路径；重复注入幂等保留首个。接受两种形态：
/// - 缓存命中的最终库路径：直接注入；
/// - Cisco 官方 `.bz2` 归档路径（首启下载产物）：解压到同目录最终名 → 白名单预校验 →
///   注入最终路径并清理归档。任一步失败删除产物且不注入，本 session 软编不可用（fail-soft）。
pub fn init_library(path: &str) {
    if path.is_empty() {
        return;
    }
    match install(path) {
        Ok(final_path) => {
            let _ = OPENH264_LIB_PATH.set(final_path);
        }
        Err(e) => log::error!("openh264 库安装失败，软件编码不可用: {e}"),
    }
}

/// `.bz2` 归档安装：解压到同目录最终名（`libopenh264.so.bz2` → `libopenh264.so`），
/// 随后 `from_blob_path` 白名单预校验——失败即删除产物，避免坏文件被缓存后
/// 每次构造都失败。非 `.bz2` 输入视为已是最终库路径，原样返回。
fn install(path: &str) -> Result<PathBuf, String> {
    let source = PathBuf::from(path);
    if source.extension().and_then(|e| e.to_str()) != Some("bz2") {
        return Ok(source);
    }
    let final_path = source.with_extension("");
    let archive =
        std::fs::File::open(&source).map_err(|e| format!("打开归档失败 ({source:?}): {e}"))?;
    let mut decoder = bzip2::read::BzDecoder::new(archive);
    let mut out = std::fs::File::create(&final_path)
        .map_err(|e| format!("创建库文件失败 ({final_path:?}): {e}"))?;
    std::io::copy(&mut decoder, &mut out)
        .map_err(|e| format!("解压失败 ({source:?} → {final_path:?}): {e}"))?;
    if let Err(e) = OpenH264API::from_blob_path(&final_path) {
        let _ = std::fs::remove_file(&final_path);
        return Err(format!("白名单校验失败 ({final_path:?}): {e:?}"));
    }
    let _ = std::fs::remove_file(&source);
    Ok(final_path)
}

/// 每次构造软编时现场 dlopen 并做 SHA-256 白名单校验：`from_blob_path` 只接受
/// openh264-sys2 收录的 Cisco 官方 2.6.0 产物，损坏/被替换的库在此 fail-closed。
/// dlopen + 全文件哈希约 10ms，编码器重建是低频事件，不缓存句柄还免掉
/// unsafe 与跨编码器共享 `libloading::Library` 的生命周期问题。
fn load_api() -> Result<OpenH264API, String> {
    let path = OPENH264_LIB_PATH
        .get()
        .ok_or_else(|| "openh264 库路径未注入：startEngine 未携带 openh264LibPath".to_string())?;
    OpenH264API::from_blob_path(path).map_err(|e| format!("加载 openh264 库失败 ({path:?}): {e:?}"))
}

/// OpenH264 软编码后端：CPU 上的 H.264 实时编码（系统默认底座）
///
/// 每次构造现场 dlopen 并做 SHA-256 白名单校验，损坏/被替换的库 fail-closed；
/// NV12 输入在内部拆 I420 三平面后编码，只拆分不换算色彩。实现体被编码线程
/// 独占，终生不跨线程
pub struct SoftwareBackend {
    encoder: Encoder,
    width: usize,
    height: usize,
    /// 编码器当前的目标码率 (bps)。`WelsEncoderApplyBitRate` 只改目标不改上限，需要靠它判断
    /// 本次是升还是降，从而决定两个 SetOption 的先后顺序。
    applied_bitrate: u32,
    /// 编码器尚未初始化时收到的码率，等首帧编码完成后补发
    pending_bitrate: Option<u32>,
    /// 是否已成功编码过至少一帧，等价于底层编码器已执行 `Initialize`
    initialized: bool,
    y_plane: Vec<u8>,
    u_plane: Vec<u8>,
    v_plane: Vec<u8>,
    #[cfg(test)]
    benchmark_stages: BackendStageTimings,
}

impl SoftwareBackend {
    /// 构造软编码器：直接接收 StreamingProfile，由类型不变式担保尺寸与帧率合法性，无需冗余校验
    pub fn new(profile: StreamingProfile) -> Result<Self, String> {
        let width = profile.width();
        let height = profile.height();
        let n = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        let ceiling_bps =
            calculate_dynamic_bitrate(width, height, profile.fps(), MAX_BPP, profile.codec())
                .max(profile.bitrate());

        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(profile.bitrate() as u32))
            .max_frame_rate(FrameRate::from_hz(profile.fps() as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .intra_frame_period(IntraFramePeriod::from_num_frames(profile.fps() as u32))
            .level(openh264_level(width, height, profile.fps(), ceiling_bps))
            .num_threads(n as u16);

        let api = load_api()?;
        let encoder = Encoder::with_api_config(api, config)
            .map_err(|e| format!("Failed to create openh264 encoder: {:?}", e))?;

        let y_len = width * height;
        let uv_len = (width / 2) * (height / 2);

        Ok(Self {
            encoder,
            width,
            height,
            applied_bitrate: profile.bitrate() as u32,
            pending_bitrate: None,
            initialized: false,
            y_plane: vec![0u8; y_len],
            u_plane: vec![0u8; uv_len],
            v_plane: vec![0u8; uv_len],
            #[cfg(test)]
            benchmark_stages: BackendStageTimings::default(),
        })
    }

    #[cfg(test)]
    pub(crate) fn width(&self) -> usize {
        self.width
    }

    #[cfg(test)]
    pub(crate) fn height(&self) -> usize {
        self.height
    }

    /// 布下一次性 IDR 意图：只影响紧接着的一次编码，产出关键帧后自动失效
    ///
    /// 意图的失败回滚由门面策略层保管（编码失败即意图未兑现，还原待重试），
    /// 本方法不自管
    pub fn force_intra_frame(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// 把码率真正提交进 openh264
    ///
    /// `ENCODER_OPTION_BITRATE` 只更新 `iSpatialBitrate`，**不更新 `iMaxSpatialBitrate`**；
    /// 而随后的 `WelsBitRateVerification` 在 `iMaxSpatialBitrate < iSpatialBitrate` 时直接
    /// 返回错误。建编码器时两者都等于初始档位码率，所以：
    ///
    /// - 升码率：必须先抬上限，否则目标一超过旧上限就被校验拒绝；
    /// - 降码率：先降目标，否则先降上限会让「旧目标 > 新上限」的中间态被拒。
    fn apply_bitrate_change(&mut self, bitrate_bps: u32) -> Result<(), String> {
        let raising = bitrate_bps > self.applied_bitrate;
        if raising {
            self.apply_bitrate_option(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, bitrate_bps)?;
        }
        self.apply_bitrate_option(ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL, bitrate_bps)?;
        if !raising {
            self.apply_bitrate_option(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, bitrate_bps)?;
        }
        Ok(())
    }

    /// 补发初始化之前挂起的码率。失败只告警不重试
    fn flush_pending_bitrate(&mut self) {
        let Some(bitrate_bps) = self.pending_bitrate.take() else {
            return;
        };
        match self.apply_bitrate_change(bitrate_bps) {
            Ok(()) => self.applied_bitrate = bitrate_bps,
            Err(e) => log::warn!("软编码率补发失败 ({} kbps): {}", bitrate_bps / 1000, e),
        }
    }

    /// 单个 `SetOption` 调用。`layer` 取 `SPATIAL_LAYER_ALL`（整体目标）或
    /// `SPATIAL_LAYER_0`（该空域层的码率上限）。
    fn apply_bitrate_option(
        &mut self,
        option: ENCODER_OPTION,
        layer: i32,
        bitrate_bps: u32,
    ) -> Result<(), String> {
        let bitrate = i32::try_from(bitrate_bps)
            .map_err(|_| format!("码率 {bitrate_bps} bps 超出 openh264 的 i32 参数范围"))?;
        let mut info = SBitrateInfo {
            iLayer: layer,
            iBitrate: bitrate,
        };

        // SAFETY: `info` 是 `SBitrateInfo`（C 布局），与 BITRATE / MAX_BITRATE 期望的参数
        // 类型一致；`set_option` 只读该指针，返回时 `info` 仍在作用域内。
        // `raw_api` 的文档警告「不要设置编码器依赖的参数」，而码率正是我们刻意要改的运行期
        // 参数——这正是 openh264 C API 为动态码率提供的官方入口。
        let ret = unsafe {
            self.encoder
                .raw_api()
                .set_option(option, std::ptr::from_mut(&mut info).cast())
        };
        if ret != 0 {
            return Err(format!("openh264 SetOption({option}) 返回错误码 {ret}"));
        }
        Ok(())
    }

    /// 将 NV12（Y 平面 + 交织 UV）拆成 I420 三平面后编码
    ///
    /// 采集侧已按 BT.709 有限范围转好，
    /// 这里只做平面拆分、不做色彩换算，所以先掉的是整段 CSC 开销。
    /// openh264 只收 I420（其高级封装写死 `videoFormatI420`），故无法直接喂 NV12。
    fn encode_nv12(&mut self, nv12: &[u8]) -> Result<(Vec<u8>, bool), String> {
        let width = self.width;
        let height = self.height;
        let y_len = width * height;
        let uv_len = y_len / 2;
        if nv12.len() < y_len + uv_len {
            return Err(format!(
                "NV12 帧长度不足: 收到 {} 字节，期望 {}",
                nv12.len(),
                y_len + uv_len
            ));
        }

        self.y_plane[..y_len].copy_from_slice(&nv12[..y_len]);
        let uv = &nv12[y_len..y_len + uv_len];
        let (pairs, _) = uv.as_chunks::<2>();
        for (pair, (u, v)) in pairs
            .iter()
            .zip(self.u_plane.iter_mut().zip(self.v_plane.iter_mut()))
        {
            *u = pair[0];
            *v = pair[1];
        }

        self.encode_planes()
    }

    /// 三平面已就位的编码尾部：两条输入路径共用
    fn encode_planes(&mut self) -> Result<(Vec<u8>, bool), String> {
        let width = self.width;
        let height = self.height;

        // 独立作用域：`yuv_slices` 借 `self.y_plane`、`bitstream` 借 `self.encoder`，
        // 都要在下面的 `&mut self` 调用之前结束
        let (nalu, is_keyframe) = {
            let yuv_slices = YUVSlices::new(
                (&self.y_plane, &self.u_plane, &self.v_plane),
                (width, height),
                (width, width / 2, width / 2),
            );

            #[cfg(test)]
            let encode_start = std::time::Instant::now();
            let bitstream = self
                .encoder
                .encode(&yuv_slices)
                .map_err(|e| format!("Encode error: {:?}", e))?;
            #[cfg(test)]
            {
                self.benchmark_stages.encode_ns = encode_start.elapsed().as_nanos() as u64;
            }
            let is_keyframe = matches!(
                bitstream.frame_type(),
                openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
            );
            let output = bitstream.to_vec();
            (output, is_keyframe)
        };

        // 首帧编码成功后底层才真正 Initialize，也才能接受 SetOption
        self.initialized = true;
        self.flush_pending_bitrate();

        Ok((nalu, is_keyframe))
    }
}

/// [`VideoEncodeBackend`] 落点：软编作为第三后端接入统一门面。
/// 能力面以 Err / no-op 诚实作答——软编无显存槽位、不碰设备内存、无在途 GPU 任务
impl VideoEncodeBackend for SoftwareBackend {
    fn kind(&self) -> EncodeBackendKind {
        EncodeBackendKind::Software
    }

    fn coded_align(&self) -> (u32, u32) {
        // 软编无设备粒度对齐，与 NVENC 同取 (1, 1)
        (1, 1)
    }

    fn encode(
        &mut self,
        payload: &FramePayload,
        force_idr: bool,
        _frame_idx: u64,
    ) -> Result<(Bytes, bool), String> {
        #[cfg(test)]
        {
            self.benchmark_stages = BackendStageTimings::default();
        }
        if force_idr {
            self.force_intra_frame();
        }
        match payload.host_slice() {
            Some(bytes) => self
                .encode_nv12(bytes)
                .map(|(out, is_key)| (Bytes::from(out), is_key)),
            None => Err("收到非 RAM 显存载荷，但当前是软件编码器".to_string()),
        }
    }

    /// 软编吃堆块，与设备块走同一个池；它不碰设备，重配也不换内存域，故恒返回空批
    fn reconfigure(&mut self, new_profile: StreamingProfile) -> Result<Vec<HostBlock>, String> {
        if self.width == new_profile.width() && self.height == new_profile.height() {
            return Ok(Vec::new());
        }
        *self = Self::new(new_profile)?;
        Ok(Vec::new())
    }

    /// 运行期改目标码率 (bps)，不重建编码器、不强制关键帧
    ///
    /// 编码器尚未初始化时只记录不落盘：openh264-rs 是惰性初始化（首次 `encode` 才调
    /// `Initialize`），而 openh264 的 `SetOption` 在未初始化时会直接返回 `cmInitExpected`。
    /// `ProfileChange` 重建软编后会立刻下发码率，正好落在这一刻，不延后就会变成一次假失败。
    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), String> {
        if bitrate_bps == self.applied_bitrate {
            self.pending_bitrate = None;
            return Ok(());
        }
        if !self.initialized {
            self.pending_bitrate = Some(bitrate_bps);
            return Ok(());
        }
        self.apply_bitrate_change(bitrate_bps)?;
        self.applied_bitrate = bitrate_bps;
        self.pending_bitrate = None;
        Ok(())
    }

    fn import_vram_slots(&mut self, _descriptors: &mut [VramSlotDescriptor]) -> Result<(), String> {
        Err("软件编码器不支持显存槽位注册".to_string())
    }

    fn clear_vram_slots(&mut self) {}

    fn drain(&mut self) -> Result<(), String> {
        Ok(())
    }

    #[cfg(test)]
    fn benchmark_stages(&self) -> BackendStageTimings {
        self.benchmark_stages
    }
}

/// 把 `h264_level_idc` 的结果映射到 openh264 的 `Level` 枚举
fn openh264_level(width: usize, height: usize, fps: usize, bitrate_bps: usize) -> Level {
    match h264_level_idc(width, height, fps, (bitrate_bps / 1000) as u32) {
        0..=30 => Level::Level_3_0,
        31 => Level::Level_3_1,
        32 => Level::Level_3_2,
        40 => Level::Level_4_0,
        41 => Level::Level_4_1,
        42 => Level::Level_4_2,
        50 => Level::Level_5_0,
        51 => Level::Level_5_1,
        _ => Level::Level_5_2,
    }
}

/// 测试专用：定位或下载官方预编译库并注入（幂等）。仓库不再含任何 OpenH264 二进制：
/// 库缓存于 `target/test-assets/`，未命中时经 curl 拉取 Cisco 官方归档并
/// 走生产同款 `.bz2` 安装分支。任一步失败返回 false，调用方应优雅跳过软编用例。
#[cfg(test)]
pub(crate) fn init_api_for_tests() -> bool {
    static ATTEMPTED: OnceLock<bool> = OnceLock::new();
    *ATTEMPTED.get_or_init(|| {
        let name = if cfg!(windows) {
            "openh264.dll"
        } else {
            "libopenh264.so"
        };
        let archive = if cfg!(windows) {
            "openh264-2.6.0-win64.dll.bz2"
        } else {
            "libopenh264-2.6.0-linux64.8.so.bz2"
        };
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-assets");
        let lib = dir.join(name);
        if lib.exists() {
            init_library(lib.to_str().expect("库路径非 UTF-8"));
            return OPENH264_LIB_PATH.get().is_some();
        }
        let _ = std::fs::create_dir_all(&dir);
        let bz2 = dir.join(format!("{name}.bz2"));
        let fetched = std::process::Command::new("curl")
            .args(["-fSL", "--max-time", "120", "-o"])
            .arg(&bz2)
            .arg(format!("https://ciscobinary.openh264.org/{archive}"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !fetched {
            eprintln!("跳过软编用例：openh264 测试库不可用（无法下载，检查网络）");
            return false;
        }
        init_library(bz2.to_str().expect("归档路径非 UTF-8"));
        OPENH264_LIB_PATH.get().is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_1080p_and_720p() {
        if !init_api_for_tests() {
            eprintln!("跳过：openh264 测试库不可用");
            return;
        }
        let p1080 = StreamingProfile::new(1920, 1080, 60, crate::encode::VideoCodec::H264);
        let mut enc1080 = SoftwareBackend::new(p1080).expect("Should create 1080p encoder");
        assert_eq!(enc1080.height(), 1080);
        let dummy1080 = vec![128u8; p1080.width() * p1080.height() * 3 / 2];
        let (nalu1080, is_keyframe1080) = enc1080
            .encode_nv12(&dummy1080)
            .expect("Should encode 1080p frame");
        assert!(!nalu1080.is_empty(), "Encoded NALU should not be empty");
        assert!(is_keyframe1080, "OpenH264 首帧必须是 IDR 关键帧");

        let p900 = StreamingProfile::new(1600, 900, 60, crate::encode::VideoCodec::H264);
        let mut enc900 = SoftwareBackend::new(p900).expect("Should create 900p encoder");
        assert_eq!(enc900.width(), 1600);
        assert_eq!(enc900.height(), 900);
        let dummy900 = vec![128u8; p900.width() * p900.height() * 3 / 2];
        let (nalu900, is_keyframe900) = enc900
            .encode_nv12(&dummy900)
            .expect("Should encode 900p frame");
        assert!(!nalu900.is_empty(), "Encoded NALU should not be empty");
        assert!(is_keyframe900, "OpenH264 首帧必须是 IDR 关键帧");

        let p720 = StreamingProfile::new(1280, 720, 60, crate::encode::VideoCodec::H264);
        let mut enc720 = SoftwareBackend::new(p720).expect("Should create 720p encoder");
        assert_eq!(enc720.width(), 1280);
        assert_eq!(enc720.height(), 720);
        let dummy720 = vec![128u8; 1280 * 720 * 3 / 2];
        let (nalu720, is_keyframe720) = enc720
            .encode_nv12(&dummy720)
            .expect("Should encode 720p frame");
        assert!(!nalu720.is_empty(), "Encoded NALU should not be empty");
        assert!(is_keyframe720, "OpenH264 首帧必须是 IDR 关键帧");
    }

    /// 升 / 降码率都必须被 openh264 接受，且初始化之前收到的码率要能延后补发。
    ///
    /// 真正钉住的是「两个 SetOption 的顺序」：`iMaxSpatialBitrate` 建编码器时就等于初始目标
    /// 码率，若升码率时先改目标，`WelsBitRateVerification` 会以 max < target 拒绝。
    #[test]
    fn test_set_bitrate_both_directions() {
        if !init_api_for_tests() {
            eprintln!("跳过：openh264 测试库不可用");
            return;
        }
        let profile = StreamingProfile::new(1280, 720, 60, crate::encode::VideoCodec::H264);
        let initial = profile.bitrate() as u32;
        let raised = initial * 3 / 2;
        let dummy = vec![128u8; 1280 * 720 * 3 / 2];
        let mut enc = SoftwareBackend::new(profile).expect("Should create encoder");

        // 首帧之前：底层还没 Initialize，SetOption 只会返回 cmInitExpected，必须挂起
        enc.set_bitrate(raised).expect("首帧前改码率不应报错");
        assert_eq!(enc.pending_bitrate, Some(raised));
        assert_eq!(
            enc.applied_bitrate, initial,
            "未初始化时不得改 applied_bitrate"
        );

        enc.encode_nv12(&dummy).expect("首帧编码应成功");
        assert_eq!(enc.pending_bitrate, None, "首帧后必须补发挂起的码率");
        assert_eq!(enc.applied_bitrate, raised, "补发必须真的落到编码器上");

        // 初始化之后：降码率走「先降目标、再降上限」
        enc.set_bitrate(initial / 2).expect("降码率必须成功");
        assert_eq!(enc.applied_bitrate, initial / 2);

        // 同值重复下发是幂等空操作
        enc.set_bitrate(initial / 2).expect("同值下发不应报错");
        assert_eq!(enc.applied_bitrate, initial / 2);

        let (nalu, _) = enc.encode_nv12(&dummy).expect("改码率后仍应能编码");
        assert!(!nalu.is_empty(), "改码率后编码产物不应为空");
    }
}

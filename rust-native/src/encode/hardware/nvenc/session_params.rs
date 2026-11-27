//! NVENC 会话构造与编码参数配置 (Session Construction & Config)
//!
//! 打开编码会话、查询硬件输入格式支持、构建超低延迟 CBR 参数（H.264 / HEVC / AV1、
//! Intra-Refresh、0 B 帧）并初始化 encoder——全部是构造期一次性流程；
//! 会话运行时类型见 `session`。

use std::ffi::c_void;

use cudarc::driver::CudaContext;

use super::EncoderHandle;
use super::InputSpec;
use super::dl::NvencDriver;
use super::ffi::*;
use crate::encode::hardware::nvenc::nvfn;
use crate::encode::{StreamingProfile, VideoCodec};

const FRAME_INTERVAL_P_NO_B_FRAMES: i32 = 1;
const DEFAULT_DAR_WIDTH: u32 = 16;
const DEFAULT_DAR_HEIGHT: u32 = 9;

/// Intra-Refresh 波次：约 0.5s 刷新一轮，波次在整轮内平铺
///
/// `cnt = period - 1` 让每帧只刷少量宏块行；集中在前 1/3 周期刷完会在周期开头堆出
/// 尖峰，CBR 下表现为每 0.5s 一次的 QP 脉动。
fn intra_refresh_timing(fps: u32) -> (u32, u32) {
    let period = (fps / 2).max(1);
    (period, (period - 1).max(1))
}

/// 把随档位重算的 Intra-Refresh 波次与显式 Level 写入对应 codec 的配置
///
/// 构造期与 reconfigure 共用：波次由帧率推导（约 0.5s 一轮），Level 由当前几何、
/// 帧率与码率推导。按 4K 包络推导会把 720p 流标成 Level 6.x，手机硬解会直接拒绝。
pub(super) fn apply_refresh_and_level(
    codec: VideoCodec,
    config: &mut NV_ENC_CONFIG,
    width: usize,
    height: usize,
    fps: u32,
    bitrate: u32,
) {
    let (period, cnt) = intra_refresh_timing(fps);
    let kbps = bitrate / 1000;
    // SAFETY: union 取成员需要 unsafe；codec 与构造会话时一致（重配不改 codec），
    // 访问的是构造期初始化过的活跃成员，写入标量字段不会破坏其有效性
    unsafe {
        match codec {
            VideoCodec::H264 => {
                let cfg = &mut config.encodeCodecConfig.h264Config;
                cfg.intraRefreshPeriod = period;
                cfg.intraRefreshCnt = cnt;
                cfg.level = u32::from(crate::encode::h264_level_idc(
                    width,
                    height,
                    fps as usize,
                    kbps,
                ));
            }
            VideoCodec::H265 => {
                let cfg = &mut config.encodeCodecConfig.hevcConfig;
                cfg.intraRefreshPeriod = period;
                cfg.intraRefreshCnt = cnt;
                cfg.level = u32::from(crate::encode::hevc_level_idc(
                    width,
                    height,
                    fps as usize,
                    kbps,
                ));
            }
            VideoCodec::Av1 => {
                let cfg = &mut config.encodeCodecConfig.av1Config;
                cfg.intraRefreshPeriod = period;
                cfg.intraRefreshCnt = cnt;
                cfg.level = u32::from(crate::encode::av1_level_idx(
                    width,
                    height,
                    fps as usize,
                    kbps,
                ));
            }
        }
    }
}

/// H.264 与 HEVC 两套 codec 配置的同构静态部分：SPS/PPS 内联与逐帧重复、AUD、
/// 无限 GOP、单 slice、Intra-Refresh 使能与 Recovery Point SEI，以及 BT.709 VUI。
/// `NV_ENC_CONFIG_H264` / `NV_ENC_CONFIG_HEVC` 字段同名同义但类型互异，泛型写不出，
/// 用宏在两个分支各自展开；Intra-Refresh 波次与 Level 依赖帧率/几何/码率，
/// 由 [`apply_refresh_and_level`] 统一写入。
macro_rules! itu_common {
    ($cfg:expr, $vui:ident) => {{
        $cfg.set_disableSPSPPS(u32::from(false));
        $cfg.set_repeatSPSPPS(u32::from(true));
        $cfg.set_outputAUD(u32::from(true));
        $cfg.idrPeriod = NVENC_INFINITE_GOPLENGTH;
        $cfg.sliceMode = 0;
        $cfg.set_enableIntraRefresh(u32::from(true));
        $cfg.set_outputRecoveryPointSEI(u32::from(true));

        let vui = &mut $cfg.$vui;
        vui.videoSignalTypePresentFlag = 1;
        vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        vui.videoFullRangeFlag = 0;
        vui.colourDescriptionPresentFlag = 1;
        vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
        vui.transferCharacteristics =
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    }};
}

/// 根据画面宽高自适应计算显示宽高比 (DAR, Display Aspect Ratio)
pub(super) fn calculate_dar(width: u32, height: u32) -> (u32, u32) {
    fn gcd(mut a: u32, mut b: u32) -> u32 {
        while b != 0 {
            let t = b;
            b = a % b;
            a = t;
        }
        a
    }
    if width == 0 || height == 0 {
        return (DEFAULT_DAR_WIDTH, DEFAULT_DAR_HEIGHT);
    }
    // 零值已在上方排除，gcd 结果必然 >= 1
    let g = gcd(width, height);
    (width / g, height / g)
}

/// 打开 NVENC 编码会话，严格匹配官方头文件版本与 API 版本定义
pub(crate) fn open_encoder(
    driver: NvencDriver,
    cuda_ctx: &CudaContext,
) -> Result<EncoderHandle, String> {
    // CUcontext 是 `*mut CUctx_st`，而 NVENC 的 device 字段声明成 `void*`
    let mut session_params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
        version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
        apiVersion: NVENCAPI_VERSION,
        deviceType: NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
        device: cuda_ctx.cu_ctx() as *mut std::ffi::c_void,
        ..Default::default()
    };

    let open_session = nvfn!(driver, nvEncOpenEncodeSessionEx);
    let mut raw_encoder: *mut c_void = std::ptr::null_mut();
    unsafe {
        // SAFETY: session_params 完全初始化且绑定有效 CUDA 上下文
        let res = open_session(&mut session_params, &mut raw_encoder);
        if res != NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(format!("nvEncOpenEncodeSessionEx failed: {:?}", res));
        }
    }
    // 出参校验收在 `from_raw` 里：`NvencSession` 由此不存在「空 encoder」状态，
    // 析构路径也就不需要进门检查
    EncoderHandle::from_raw(driver, raw_encoder)
        .ok_or_else(|| "nvEncOpenEncodeSessionEx 返回成功但句柄为空".to_string())
}

/// 编码配置的总装配：核验 `input_format` 在驱动标称支持之列（不支持即 fail-closed
/// 报错并附支持列表，禁静默妥协）→ 取 P4 超低延迟预设 → 定制 CBR（无 B 帧、无限
/// GOP、按 codec 的 minQP/AQ、VBV=单帧比特、BT.709 VUI、Intra-Refresh 周期）→
/// `nvEncInitializeEncoder` 初始化编码器
///
/// 输入约束：`profile` 须满足 `StreamingProfile` 不变量（fps/码率裸转 u32 不截断）；
/// 返回的 `Box` 钉死配置地址且 `init_params.encodeConfig` 指向它，两者必须成对
/// 移交、`Box` 活过会话。失败只发生在驱动查询与本地装配，不在驱动侧留下任何
/// 待回收状态，encoder 句柄由调用方统一收尾
pub(super) fn build_encoder_config(
    driver: NvencDriver,
    encoder: &EncoderHandle,
    profile: &StreamingProfile,
    codec: VideoCodec,
    input_format: NV_ENC_BUFFER_FORMAT,
) -> Result<(Box<NV_ENC_CONFIG>, NV_ENC_INITIALIZE_PARAMS), String> {
    // fps 由 StreamingProfile 构造器钳到离散档位（≤240），码率由公式/带宽控制器
    // 保证非零且远低于 u32 上限（见 `StreamingProfile::set_bitrate` 的不变量）——裸转换不会截断
    let (fps, bitrate) = (profile.fps() as u32, profile.bitrate() as u32);

    // 1. 查询输入格式支持，确保驱动接受所请求的输入布局（生产是 NV12，采集侧已完成 CSC）
    let (codec_guid, profile_guid) = match codec {
        VideoCodec::H264 => (NV_ENC_CODEC_H264_GUID, NV_ENC_H264_PROFILE_HIGH_GUID),
        VideoCodec::H265 => (NV_ENC_CODEC_HEVC_GUID, NV_ENC_HEVC_PROFILE_MAIN_GUID),
        VideoCodec::Av1 => (NV_ENC_CODEC_AV1_GUID, NV_ENC_AV1_PROFILE_MAIN_GUID),
    };

    let get_format_count = nvfn!(driver, nvEncGetInputFormatCount);
    let get_formats = nvfn!(driver, nvEncGetInputFormats);

    let mut format_count = 0u32;
    unsafe {
        // SAFETY: encoder 为构造期刚建立的会话句柄且此刻存活；codec_guid 是
        // bindgen 生成的静态 GUID；format_count 为合法 u32 出参槽位
        let res = get_format_count(encoder.as_raw(), codec_guid, &mut format_count);
        if res != NVENCSTATUS::NV_ENC_SUCCESS || format_count == 0 {
            return Err(format!(
                "nvEncGetInputFormatCount failed for {:?}: {:?}",
                codec, res
            ));
        }
    }

    // 防御性内存上限：取驱动标称支持格式与 64 的较小值
    let fetch_count = format_count.min(64);
    let mut formats: Vec<NV_ENC_BUFFER_FORMAT> =
        vec![NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_UNDEFINED; fetch_count as usize];
    let mut actual_count = 0u32;
    unsafe {
        // SAFETY: formats 已按 fetch_count 预填合法值（UNDEFINED）且容量与之相符；
        // actual_count 为合法 u32 出参槽位；encoder 为有效会话句柄且此刻存活
        let res = get_formats(
            encoder.as_raw(),
            codec_guid,
            formats.as_mut_ptr(),
            fetch_count,
            &mut actual_count,
        );
        if res != NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(format!(
                "nvEncGetInputFormats failed for {:?}: {:?}",
                codec, res
            ));
        }
    }
    formats.truncate(actual_count as usize);

    if !formats.contains(&input_format) {
        return Err(format!(
            "NVENC driver does not support {input_format:?} input format for {:?}, supported: {formats:?}",
            codec,
        ));
    }

    // 3. 查询预设配置并定制超低延迟 CBR 参数
    //
    // `NV_ENC_CONFIG.frameFieldMode` 的 C 枚举从 1 开始编号、**没有 0 值**，直接
    // `zeroed::<NV_ENC_PRESET_CONFIG>()` 会构造出非法判别值（debug 下 panic，
    // release 下 UB）。先零内存（`MaybeUninit` 不参与类型有效性检查），把该字段
    // 与两个 version 写好后，整块才被视为已初始化的 `NV_ENC_PRESET_CONFIG`。
    // 随后驱动会把 `presetCfg` 整体覆写。
    let mut preset_config = std::mem::MaybeUninit::<NV_ENC_PRESET_CONFIG>::zeroed();
    unsafe {
        // SAFETY: 除 frameFieldMode 外，两层结构的成员均为整型/指针或 0 值合法的
        // 枚举（RC_MODE 的 CONSTQP、MULTI_PASS 的 DISABLED、MV_PRECISION 的 DEFAULT
        // 等），MaybeUninit::zeroed 的全零位内存对其合法；frameFieldMode 是 1 起始的
        // 无 0 值枚举，在此显式写入合法变体后，连同两个 version 一起使整块完成初始化
        let raw = preset_config.as_mut_ptr();
        (*raw).version = NV_ENC_PRESET_CONFIG_VER;
        (*raw).presetCfg.version = NV_ENC_CONFIG_VER;
        (*raw).presetCfg.frameFieldMode =
            NV_ENC_PARAMS_FRAME_FIELD_MODE::NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
    }
    // SAFETY: 全部成员已初始化完毕（见上），此刻 assume_init 合法
    let mut preset_config = unsafe { preset_config.assume_init() };

    let get_preset = nvfn!(driver, nvEncGetEncodePresetConfigEx);
    unsafe {
        // SAFETY: preset_config 已 assume_init 且 version/frameFieldMode 均为合法值；
        // encoder 为有效会话句柄；codec GUID / P4 GUID / ULTRA_LOW_LATENCY 均为驱动
        // 标称组合，驱动应答时整体覆写 presetCfg
        let res = get_preset(
            encoder.as_raw(),
            codec_guid,
            NV_ENC_PRESET_P4_GUID,
            NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            &mut preset_config,
        );
        if res != NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(format!("nvEncGetEncodePresetConfigEx failed: {:?}", res));
        }
    }

    // 使用 Box 堆分配封装配置，确保其内存地址在 NvencSession 移动或跨线程 Send 时稳定不变，杜绝指针悬垂 (UAF)
    let mut config = Box::new(preset_config.presetCfg);
    config.profileGUID = profile_guid;
    config.gopLength = NVENC_INFINITE_GOPLENGTH;
    config.frameIntervalP = FRAME_INTERVAL_P_NO_B_FRAMES; // 0 B-frames

    let rc = &mut config.rcParams;
    rc.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
    rc.lowDelayKeyFrameScale = 1;
    rc.averageBitRate = bitrate;
    rc.maxBitRate = bitrate;
    rc.set_enableMinQP(u32::from(true));
    rc.set_enableMaxQP(u32::from(false));
    let min_qp = match codec {
        VideoCodec::H264 => 18,
        VideoCodec::H265 => 22,
        VideoCodec::Av1 => 22,
    };
    rc.minQP = NV_ENC_QP {
        qpInterP: min_qp,
        qpInterB: min_qp,
        qpIntra: min_qp,
    };
    // AQ 按 codec 区分：spatial AQ 令驱动在 nvEncEncodePicture 内做每帧阻塞预分析，
    // 性能矩阵对照实测（AQ on/off，Vulkan 行为对照组，波动 <0.5%）：
    // HEVC/AV1 提交 +1.1ms/帧（1440p +2.2ms）、端到端 +24~39%，延迟预算下得不偿失；
    // H264 提交仅 +20µs、硬件等待还略降，画质收益近乎免费，予以保留
    rc.set_enableAQ(u32::from(codec == VideoCodec::H264));
    rc.set_aqStrength(8);

    let frame_size_bits = bitrate / fps; // 平均单帧比特数
    rc.vbvBufferSize = frame_size_bits;
    rc.vbvInitialDelay = frame_size_bits;
    // 按当前帧率自适应 Intra-Refresh 周期：约 0.5s 刷新一轮
    match codec {
        VideoCodec::H264 => {
            // SAFETY: 取引用时该成员的内存刚被驱动在 get_preset 中按 codec_guid
            // 对应的活跃成员整体写入，枚举字段位模式均为 C 头文件声明的合法变体值；
            // h264Config，写入的 itu 字段也全部落在该成员上
            let cfg = unsafe { &mut config.encodeCodecConfig.h264Config };
            itu_common!(cfg, h264VUIParameters);
        }
        VideoCodec::H265 => {
            // SAFETY: 同 H264 分支——本成员由驱动按 codec_guid 写入，本分支只访问
            // hevcConfig
            let cfg = unsafe { &mut config.encodeCodecConfig.hevcConfig };
            itu_common!(cfg, hevcVUIParameters);
        }
        VideoCodec::Av1 => {
            // SAFETY: 同 H264 分支——本成员由驱动按 codec_guid 写入，本分支只访问
            // av1Config
            let av1_cfg = unsafe { &mut config.encodeCodecConfig.av1Config };
            // 协议约定：关闭 Annex-B，NVENC 直接输出 Raw OBU 序列以供 WebRTC RTP 封包直接消费
            av1_cfg.set_outputAnnexBFormat(u32::from(false));
            av1_cfg.set_disableSeqHdr(u32::from(false));
            av1_cfg.set_repeatSeqHdr(u32::from(true));
            av1_cfg.idrPeriod = NVENC_INFINITE_GOPLENGTH;
            av1_cfg.set_enableIntraRefresh(u32::from(true));
            av1_cfg.numTileColumns = 1;
            av1_cfg.numTileRows = 1;
            av1_cfg.colorPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            av1_cfg.matrixCoefficients = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
            av1_cfg.transferCharacteristics =
                NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
            av1_cfg.colorRange = 0;
        }
    }

    // Intra-Refresh 波次与 Level 依赖帧率/几何/码率，与 reconfigure 共用同一推导
    apply_refresh_and_level(
        codec,
        &mut config,
        profile.width(),
        profile.height(),
        fps,
        bitrate,
    );

    // 4. 初始化编码器
    let (dar_w, dar_h) = calculate_dar(profile.width() as u32, profile.height() as u32);
    // SAFETY: 该结构为 bindgen 生成的 C POD，成员均为整型/指针/GUID 或 0 值合法的
    // 枚举（tuningInfo 的 UNDEFINED），全零位模式合法；NVENC 约定零初始化后回填
    // version，保留字段保持 0
    let mut init_params: NV_ENC_INITIALIZE_PARAMS = unsafe { std::mem::zeroed() };
    init_params.version = NV_ENC_INITIALIZE_PARAMS_VER;
    init_params.encodeGUID = codec_guid;
    init_params.presetGUID = NV_ENC_PRESET_P4_GUID;
    init_params.encodeWidth = profile.width() as u32;
    init_params.encodeHeight = profile.height() as u32;
    // 包络与当前档位同源：AV1 要求 level 覆盖 maxEncodeWidth/Height，
    // 4K 包络会让 720p 的 level 被以 Invalid Level 拒绝。
    init_params.maxEncodeWidth = profile.width() as u32;
    init_params.maxEncodeHeight = profile.height() as u32;
    init_params.darWidth = dar_w;
    init_params.darHeight = dar_h;
    init_params.frameRateNum = fps;
    init_params.frameRateDen = 1;
    init_params.enablePTD = 1;
    init_params.tuningInfo = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    // 关键安全保证：encodeConfig 指向堆上固定的 Box<NV_ENC_CONFIG> 内存块
    init_params.encodeConfig = &mut *config;

    let initialize = nvfn!(driver, nvEncInitializeEncoder);
    unsafe {
        // SAFETY: init_params 已完整初始化，encodeConfig 指向堆上地址稳定的 config
        let res = initialize(encoder.as_raw(), &mut init_params);
        if res != NVENCSTATUS::NV_ENC_SUCCESS {
            // 状态码不带细节（如 INVALID_PARAM），last_error 会直接点名无效参数；
            // 它同时负责函数指针缺位与返回值 NULL 的兜底
            let detail = super::session::last_error(driver, encoder);
            return Err(format!(
                "nvEncInitializeEncoder failed: {:?} ({})",
                res, detail
            ));
        }
    }

    Ok((config, init_params))
}

/// 构造提交参数
///
/// `pictureStruct` 的 C 枚举从 1 开始编号、**没有 0 值**，直接
/// `zeroed::<NV_ENC_PIC_PARAMS>()` 会构造出非法判别值（debug 下 panic，
/// release 下 UB）。因此先零内存（`MaybeUninit` 不参与类型有效性检查），
/// 把这个字段写成合法值之后，整块才被视为已初始化的 `NV_ENC_PIC_PARAMS`。
pub(super) fn build_pic_params(
    input_buffer: *mut c_void,
    spec: InputSpec,
    force_idr: bool,
    frame_idx: u64,
) -> NV_ENC_PIC_PARAMS {
    let mut pic_params = std::mem::MaybeUninit::<NV_ENC_PIC_PARAMS>::zeroed();
    unsafe {
        // SAFETY: 除 pictureStruct 外该结构的成员均为整型/指针或 0 值合法的枚举，
        // MaybeUninit::zeroed 的全零位内存对其合法；pictureStruct 是 1 起始的无 0 值
        // 枚举，在此显式写入合法变体后整块完成初始化
        (*pic_params.as_mut_ptr()).pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
    }
    // SAFETY: 全部成员已初始化完毕（见上），此刻 assume_init 合法
    let mut pic_params = unsafe { pic_params.assume_init() };
    pic_params.version = NV_ENC_PIC_PARAMS_VER;
    pic_params.inputWidth = spec.width as u32;
    pic_params.inputHeight = spec.height as u32;
    pic_params.inputPitch = spec.pitch as u32;
    pic_params.inputBuffer = input_buffer;
    pic_params.bufferFmt = spec.buffer_format;
    pic_params.frameIdx = frame_idx as u32;
    pic_params.inputTimeStamp = frame_idx;
    if force_idr {
        pic_params.encodePicFlags = NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
            | NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32;
        pic_params.pictureType = NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
    }
    pic_params
}

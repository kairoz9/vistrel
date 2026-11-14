//! 跨平台原生动态库加载器 (Native Dynamic Loader)
//!
//! 经 `libloading` 统一加载 libnvidia-encode.so.1 / libcuda.so.1（Windows 侧即
//! nvEncodeAPI64.dll / nvcuda.dll，平台差异由 `libloading` 内部处理）。
//!
//! 分工：**NVENC** 的函数符号由本文件解析（`NvencDriver`）；**CUDA** 的符号交给
//! `cudarc::driver::sys`，本文件只保留「库是否可加载」的探测（[`probe_cuda`]）与
//! `cuInit` 的时机管理（[`ensure_cuda_init`]）——因为 cudarc 加载失败是 panic 而非
//! `Result`，要安全降级就得先有一道能失败的探测。

use libloading::Library;
use std::sync::OnceLock;

use super::ffi::*;

// ============================================================================
// 1. 跨平台动态库原始句柄包装器 (RAII Safe DlHandle)
// ============================================================================

/// 已加载动态库的 RAII 句柄（`libloading::Library` 自带 Drop 关库，且已实现 Send + Sync）
struct DlHandle {
    lib: Library,
}

impl DlHandle {
    fn open(names: &[&str]) -> Result<Self, String> {
        for &name in names {
            // SAFETY: `Library::new` 会执行库的初始化代码，库名取自本文件的常量
            // 候选表（驱动安装名），不含用户可控输入
            if let Ok(lib) = unsafe { Library::new(name) } {
                return Ok(Self { lib });
            }
        }
        Err(format!(
            "Failed to load dynamic library from candidate names: {:?}",
            names
        ))
    }

    fn get_symbol<T: Copy>(&self, symbol: &str) -> Result<T, String> {
        // SAFETY: 调用方保证目标泛型 T 与该库导出符号的调用约定一致（现有调用点
        // 全部是 NVENC 头文件声明的 extern "system" 函数指针）
        unsafe {
            self.lib
                .get::<T>(symbol)
                .map(|sym| *sym)
                .map_err(|e| format!("dlsym failed for {symbol}: {e}"))
        }
    }
}

// ============================================================================
// 2. CUDA Driver API 的可用性探测
// ============================================================================
//
// 具体的 `cuXxx` 函数符号不再由本文件维护：全部直接调 `cudarc::driver::sys`
// （经 `ffi` 平铺进来）。但 cudarc 的 dynamic-loading 在找不到库时走
// `panic_no_lib_found()`（返回 `-> !`，直接 panic），不是 `Result` ——
// 「无 NVIDIA 驱动就降级到 Vulkan / 软编」这件事必须由这里的探测兜住。

/// CUDA 驱动库的候选名
#[cfg(unix)]
const CUDA_LIB_NAMES: &[&str] = &["libcuda.so.1", "libcuda.so"];
#[cfg(windows)]
const CUDA_LIB_NAMES: &[&str] = &["nvcuda.dll"];

/// 探测用的库句柄，**常驻到进程结束**（不 drop、不 dlclose）
///
/// 提前 `dlclose` 再让 cudarc 重新 `dlopen`，部分驱动会重置内部状态；
/// 让引用计数一直挂着，cudarc 后续加载只是 +1。
static CUDA_PROBE: OnceLock<Result<DlHandle, String>> = OnceLock::new();

/// CUDA 驱动库是否可加载（幂等，结果缓存）
///
/// 任何 `cudarc` 的 CUDA 调用之前都必须先过这里，否则无 NVIDIA 驱动的机器上
/// 会 panic 而不是返回错误。
pub(crate) fn probe_cuda() -> Result<(), String> {
    CUDA_PROBE
        .get_or_init(|| DlHandle::open(CUDA_LIB_NAMES))
        .as_ref()
        .map(|_| ())
        .map_err(|e| e.clone())
}

/// 确保 `cuInit` 已调用（幂等）
///
/// `cuInit` 必须先于其它 Driver API 调用，而 `enumerate_devices` 在建上下文之前
/// 就要用到它。
pub(crate) fn ensure_cuda_init() -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        probe_cuda()?;
        // SAFETY: 库已由 `probe_cuda` 确认可加载，cudarc 的 sys 不会走
        // `panic_no_lib_found`；`cuInit` 本身无前置条件
        let res = unsafe { cuInit(0) };
        if res != CUresult::CUDA_SUCCESS {
            return Err(format!("cuInit failed: {res:?}"));
        }
        Ok(())
    })
    .clone()
}

// ============================================================================
// 3. NVENC API 驱动包装
// ============================================================================

/// 一次成功加载得到的驱动：持有库句柄与函数表。进程内唯一，且常驻到进程结束
struct LoadedDriver {
    _lib: DlHandle,
    api: NV_ENCODE_API_FUNCTION_LIST,
}

// SAFETY: `Library` 本身已实现 Send + Sync；不成立的上游原因是 bindgen 生成的
// 函数表里保留了 `*mut c_void` 字段，而这张表只由 `NvEncodeAPICreateInstance` 在
// 发布进 `OnceLock` 之前写入一次，之后全局只读（函数指针与 reserved 字段无任何写入点）
unsafe impl Send for LoadedDriver {}
unsafe impl Sync for LoadedDriver {}

/// 驱动句柄：指向那一次加载得到的函数表
///
/// 8 字节 `Copy`——所有权不在句柄里，而在 [`LoadedDriver`]。因此可按值传递，
/// 也不存在「谁负责卸载」的问题：驱动库与进程同寿。
#[derive(Clone, Copy)]
pub(crate) struct NvencDriver(&'static NV_ENCODE_API_FUNCTION_LIST);

impl NvencDriver {
    /// 取驱动句柄：进程级 OnceLock 缓存，全进程恰好加载一次（候选库名依次尝试 →
    /// `NvEncodeAPICreateInstance` 填函数表）
    ///
    /// 结果连同错误一并缓存：失败后的每次调用都返回同一条错误，无驱动机器上
    /// 重试没有观测差异。库句柄与函数表常驻到进程结束，任意线程可并发调用
    pub(crate) fn load() -> Result<Self, String> {
        static INSTANCE: OnceLock<Result<LoadedDriver, String>> = OnceLock::new();
        match INSTANCE.get_or_init(load_driver) {
            Ok(loaded) => Ok(Self(&loaded.api)),
            Err(e) => Err(e.clone()),
        }
    }

    /// 返回引用而非整表：函数表有几十个函数指针，按值拷贝没有意义
    #[inline]
    pub(crate) fn api(self) -> &'static NV_ENCODE_API_FUNCTION_LIST {
        self.0
    }
}

fn load_driver() -> Result<LoadedDriver, String> {
    #[cfg(unix)]
    let candidates = &["libnvidia-encode.so.1", "libnvidia-encode.so"];
    #[cfg(windows)]
    let candidates = &["nvEncodeAPI64.dll", "nvEncodeAPI.dll"];

    let lib = DlHandle::open(candidates)?;
    // 生成的 `Default` 是 derive 出来的全零值，version 必须显式赋值，
    // 否则 `NvEncodeAPICreateInstance` 会返回 NV_ENC_ERR_INVALID_VERSION
    let mut api = NV_ENCODE_API_FUNCTION_LIST {
        version: NV_ENCODE_API_FUNCTION_LIST_VER,
        ..Default::default()
    };

    unsafe {
        // SAFETY: 动态解析 NvEncodeAPICreateInstance 并由其填充 api 函数指针表。
        // 类型内联写：头文件里它是函数声明而非 typedef，bindgen 只能生成静态链接声明
        let create_instance: unsafe extern "system" fn(
            *mut NV_ENCODE_API_FUNCTION_LIST,
        ) -> NVENCSTATUS = lib.get_symbol("NvEncodeAPICreateInstance")?;
        let res = create_instance(&mut api);
        if res != NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(format!("NvEncodeAPICreateInstance failed: {:?}", res));
        }
    }

    Ok(LoadedDriver { _lib: lib, api })
}

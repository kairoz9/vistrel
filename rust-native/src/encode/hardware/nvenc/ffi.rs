//! NVENC 与 CUDA Driver API 的 FFI 声明
//!
//! NVENC 的类型声明由 bindgen 生成并提交进仓库（`ffi_nvenc.rs` ←
//! `nv-codec-headers/nvEncodeAPI.h`，生成命令见 `tools/gen-bindings/`）。
//!
//! CUDA Driver 的类型改用 `cudarc::driver::sys` 的预生成绑定（同一份 `cuda.h`，
//! 布局逐字节一致）。唯一的例外是 `CUDA_EXTERNAL_MEMORY_HANDLE_DESC`，见
//! [`ExternalMemoryHandleDesc`] 的说明。
//!
//! 本文件只保留**生成不了**的东西：
//!
//! 1. GUID 常量——头文件里是 `static const GUID X = { ... };`，bindgen 无法对
//!    结构体字面量求值，会退化成 `extern "C" { pub static X: GUID }`，链接期才炸；
//! 2. 结构体版本号——`NVENCAPI_STRUCT_VERSION(8)` 是函数式宏且含强制转换，求不出值；
//! 3. `CUDA_EXTERNAL_MEMORY_HANDLE_DESC` 的等价布局（cudarc 的版本不可用，见下）。

use std::ffi::c_void;

#[expect(warnings, reason = "bindgen 生成的 FFI 声明不符合本项目代码风格")]
mod generated {
    include!("ffi_nvenc.rs");
}
pub use generated::*;

/// CUDA Driver API 类型与常量（来自 cudarc 的预生成绑定）。
pub use cudarc::driver::sys::*;

// ============================================================================
// `CUDA_EXTERNAL_MEMORY_HANDLE_DESC` 的等价布局
// ============================================================================

/// `CUDA_EXTERNAL_MEMORY_HANDLE_DESC` 的手写等价布局
///
/// **为什么不用 cudarc 的同名结构体**：它的 `type_` 字段是 rustified enum
/// `CUexternalMemoryHandleType`，而 CUDA 头里
/// `CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD = 1` 与 `..._OPAQUE_WIN32 = 1` **同值**，
/// bindgen 的 rustify 模式会丢掉重复值变体。实测 cudarc 0.19.10 的这个枚举只剩
/// D3D11 / D3D12 / DMABUF_FD / NVSCIBUF 六个变体，**没有 `OPAQUE_FD`** ——
/// Linux 下导入 dma-buf 用的正是它。枚举字段也没法直接赋字面值，
/// `mem::zeroed()` 更会造出不在变体表里的值（UB）。
///
/// 于是这里按 `cuda.h` 的字段顺序手写同布局结构，`type_` 用 `u32` 表达，由调用方
/// 写字面值。字段顺序与宽度逐项对应：
///
/// ```c
/// typedef struct CUDA_EXTERNAL_MEMORY_HANDLE_DESC_st {
///     CUexternalMemoryHandleType type;
///     union { int fd;
///             struct { void *handle; const void *name; } win32;
///             const void *nvSciBufObject; } handle;
///     unsigned long long size;
///     unsigned int flags;
///     unsigned int reserved[16];
/// } CUDA_EXTERNAL_MEMORY_HANDLE_DESC_v1;
/// ```
///
/// 传给 `cuImportExternalMemory` 时按指针 cast：两边都是 `#[repr(C)]` 且逐字段同宽，
/// 布局一致。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExternalMemoryHandleDesc {
    /// `CUexternalMemoryHandleType`。用字面值：`1` = `OPAQUE_FD`（Linux dma-buf）
    /// 也等于 `OPAQUE_WIN32`（Windows 共享句柄）——两者在 CUDA 头里就是同一个值
    pub type_: u32,
    pub handle: ExternalMemoryHandleUnion,
    pub size: u64,
    /// `CUDA_EXTERNAL_MEMORY_DEDICATED` = 1
    pub flags: u32,
    pub reserved: [u32; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union ExternalMemoryHandleUnion {
    /// `CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD` 用：POSIX fd
    pub fd: i32,
    /// `CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32` 用
    pub win32: ExternalMemoryWin32Handle,
    pub nv_sci_buf_object: *const c_void,
}

/// 手写等价布局中 `handle` 联合的 Win32 成员（对应 `cuda.h` 的
/// `struct { void *handle; const void *name; } win32`）
///
/// 模块文档所称「唯一的例外」即本文件手写的 `CUDA_EXTERNAL_MEMORY_HANDLE_DESC`
/// 等价布局（见 [`ExternalMemoryHandleDesc`]），本类型是其联合成员之一：cudarc 的
/// rustified enum 丢弃了同值的 `OPAQUE_WIN32` 变体，Windows 导入共享句柄只能走
/// 这份手写结构。字段顺序与宽度逐项对应 C 定义，`name` 为 `LPCWSTR` 语义
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExternalMemoryWin32Handle {
    pub handle: *mut c_void,
    /// `LPCWSTR`，未命名对象传 NULL
    pub name: *const c_void,
}

// ============================================================================
// GUID 常量
// ============================================================================
//
// 只保留编码路径实际用到的 7 个。头文件里还有十几个 profile / preset GUID，
// 同样无法自动生成；用不到就不写。

pub const NV_ENC_CODEC_H264_GUID: GUID = GUID {
    Data1: 0x6bc8_2762,
    Data2: 0x4e63,
    Data3: 0x4ca4,
    Data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
};

pub const NV_ENC_CODEC_HEVC_GUID: GUID = GUID {
    Data1: 0x790c_dc88,
    Data2: 0x4522,
    Data3: 0x4d7b,
    Data4: [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x3],
};

pub const NV_ENC_CODEC_AV1_GUID: GUID = GUID {
    Data1: 0x0a35_2289,
    Data2: 0x0aa7,
    Data3: 0x4759,
    Data4: [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
};

pub const NV_ENC_PRESET_P4_GUID: GUID = GUID {
    Data1: 0x90a7_b826,
    Data2: 0xdf06,
    Data3: 0x4862,
    Data4: [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
};

pub const NV_ENC_H264_PROFILE_HIGH_GUID: GUID = GUID {
    Data1: 0xe7cb_c309,
    Data2: 0x4f7a,
    Data3: 0x4b89,
    Data4: [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
};

pub const NV_ENC_HEVC_PROFILE_MAIN_GUID: GUID = GUID {
    Data1: 0xb514_c39a,
    Data2: 0xb55b,
    Data3: 0x40fa,
    Data4: [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
};

pub const NV_ENC_AV1_PROFILE_MAIN_GUID: GUID = GUID {
    Data1: 0x5f2a_39f5,
    Data2: 0xf14e,
    Data3: 0x4f95,
    Data4: [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97],
};

// ============================================================================
// 结构体版本号（`NVENCAPI_STRUCT_VERSION` 系列宏）
// ============================================================================
//
// 头文件里写作：
//
//     #define NVENCAPI_STRUCT_VERSION(ver) \
//         ((uint32_t)NVENCAPI_VERSION | ((ver)<<16) | (0x7 << 28))
//     #define NV_ENC_CONFIG_VER (NVENCAPI_STRUCT_VERSION(8) | ( 1u<<31 ))
//
// 函数式宏 + 宏体内的 `(uint32_t)` 强制转换，bindgen 求不出值，因此整批常量手写。
// 括号里的版本号逐条对照 `nvEncodeAPI.h`，换 SDK 时必须重新核对，否则驱动会返回
// `NV_ENC_ERR_INVALID_VERSION` 或直接拒绝结构体。
//
// 只写当前用到的 11 个：新增结构体时按头文件补。

const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}

/// `NV_ENCODE_API_FUNCTION_LIST_VER`
pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);
/// `NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER`
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
/// `NV_ENC_CREATE_BITSTREAM_BUFFER_VER`
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
/// `NV_ENC_REGISTER_RESOURCE_VER`
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(4);
/// `NV_ENC_MAP_INPUT_RESOURCE_VER`
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
/// `NV_ENC_PRESET_CONFIG_VER`
pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
/// `NV_ENC_CONFIG_VER`
pub const NV_ENC_CONFIG_VER: u32 = struct_version(8) | (1 << 31);
/// `NV_ENC_INITIALIZE_PARAMS_VER`
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
/// `NV_ENC_RECONFIGURE_PARAMS_VER`
pub const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(1) | (1 << 31);
/// `NV_ENC_PIC_PARAMS_VER`
pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
/// `NV_ENC_LOCK_BITSTREAM_VER`
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(1) | (1 << 31);

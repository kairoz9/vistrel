//! 从第三方头文件生成 NVENC 的 Rust FFI 声明
//!
//! 产物提交进版本库，构建时不依赖 libclang：
//!
//! - `src/encode/hardware/nvenc/ffi_nvenc.rs` ← `third_party/nv-codec-headers/nvEncodeAPI.h`
//!
//! # 用法
//!
//! ```text
//! cargo run --manifest-path rust-native/tools/gen-bindings/Cargo.toml
//! cargo run --manifest-path rust-native/tools/gen-bindings/Cargo.toml -- nvenc
//! ```
//!
//! # 为什么是独立 package
//!
//! `cargo run --bin X` 会先编译整个 package（包括 lib）。主 crate 的 lib 依赖
//! 这份产物，产物一过期，生成器自己就编译不过——循环依赖。放到独立 package
//! 里，生成器只依赖 bindgen，永远跑得起来。

use std::path::Path;

use bindgen::Abi;

fn main() {
    // tools/gen-bindings → tools → rust-native
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tools/gen-bindings 应位于 rust-native/tools/ 下")
        .to_path_buf();

    let target = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    match target.as_str() {
        "nvenc" | "all" => gen_nvenc(&root),
        other => {
            eprintln!("未知目标 {other}，可选：nvenc / all（默认 all）");
            std::process::exit(1);
        }
    }
}

// ============================================================================
// NVENC
// ============================================================================

/// 生成范围：只列**顶层**用到的类型，其余由 `allowlist_recursively` 顺着字段自动带上
const NVENC_TYPES: &[&str] = &[
    "NV_ENCODE_API_FUNCTION_LIST",
    "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS",
    "NV_ENC_INITIALIZE_PARAMS",
    "NV_ENC_CONFIG",
    "NV_ENC_PRESET_CONFIG",
    "NV_ENC_PIC_PARAMS",
    // 位标志枚举：`encodePicFlags` 字段是裸 u32，递归带不到它，必须显式列出
    "NV_ENC_PIC_FLAGS",
    "NV_ENC_LOCK_BITSTREAM",
    "NV_ENC_CREATE_BITSTREAM_BUFFER",
    "NV_ENC_REGISTER_RESOURCE",
    "NV_ENC_MAP_INPUT_RESOURCE",
    "NV_ENC_RECONFIGURE_PARAMS",
    "NVENCSTATUS",
    "NV_ENC_BUFFER_FORMAT",
    "NV_ENC_PIC_TYPE",
    "NV_ENC_PIC_STRUCT",
    "NV_ENC_PARAMS_RC_MODE",
    "NV_ENC_TUNING_INFO",
    "NV_ENC_INPUT_RESOURCE_TYPE",
    "NV_ENC_DEVICE_TYPE",
    "NV_ENC_INPUT_PTR",
    "NV_ENC_OUTPUT_PTR",
    "NV_ENC_REGISTERED_PTR",
    "NV_ENC_CUSTREAM_PTR",
    "GUID",
];

/// 宏常量必须显式列出：它们不参与类型依赖递归
///
/// 刻意不匹配 `*_GUID`：头文件里它们是 `static const GUID X = { ... };`，bindgen
/// 无法对结构体字面量求值，会生成 `unsafe extern "C" { pub static X: GUID; }`——
/// 链接期才炸的悬空声明。GUID 常量保留在 ffi.rs 手写区。
///
/// 也刻意不匹配 `*_VER`：它们是 `NVENCAPI_STRUCT_VERSION(8)` 这类**函数式宏**，
/// 宏体内还有 `(uint32_t)` 强制转换，bindgen 求不出值，列了也只会生成零个符号。
/// 这批常量同样手写在 ffi.rs。
///
/// 同理不列 `NV_ENC_PIC_FLAG_.*` / `NV_ENC_RC_FLAG_.*` / `NV_ENC_VUI_.*`：它们不是
/// 宏，而是枚举变体（`NV_ENC_PIC_FLAGS`、`NV_ENC_VUI_COLOR_PRIMARIES` 等），
/// 靠上面 `NVENC_TYPES` 的 allowlist 生成。
const NVENC_VARS: &[&str] = &[
    "NVENCAPI_VERSION",
    "NVENCAPI_MAJOR_VERSION",
    "NVENCAPI_MINOR_VERSION",
    "NVENC_INFINITE_GOPLENGTH",
];

fn gen_nvenc(root: &Path) {
    let header = root.join("third_party/nv-codec-headers/nvEncodeAPI.h");
    let output = root.join("src/encode/hardware/nvenc/ffi_nvenc.rs");

    let mut builder = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .override_abi(Abi::System, "nvEnc.*")
        .override_abi(Abi::System, "NvEncode.*")
        .prepend_enum_name(false)
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: false,
        })
        .derive_default(true)
        // 错误路径大量使用 `format!("... {:?}", res)`，枚举必须有 Debug
        .derive_debug(true)
        .derive_partialeq(true)
        .generate_comments(false);
    // 不生成 `NvEncodeAPICreateInstance` 的函数声明：bindgen 会产出
    // `extern "C" { pub fn ... }` 静态链接声明，而本工程是 dlopen 运行时加载。

    for ty in NVENC_TYPES {
        builder = builder.allowlist_type(ty);
    }
    for var in NVENC_VARS {
        builder = builder.allowlist_var(var);
    }

    write(builder, &output);
}

fn write(builder: bindgen::Builder, output: &Path) {
    let bindings = match builder.generate() {
        Ok(bindings) => bindings,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    bindings
        .write_to_file(output)
        .unwrap_or_else(|e| panic!("写入 {} 失败: {}", output.display(), e));

    println!("已生成 {}", output.display());
}

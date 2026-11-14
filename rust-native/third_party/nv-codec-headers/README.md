# nv-codec-headers

`src/encode/hardware/nvenc/ffi_nvenc.rs` 由本目录的 `nvEncodeAPI.h` 生成。

## 获取

上游：<https://github.com/FFmpeg/nv-codec-headers>

由 FFmpeg 维护的版本

## 重新生成

在 `rust-native/` 下执行：

```bash
cargo run --manifest-path tools/gen-bindings/Cargo.toml -- nvenc
```

## 升级 SDK 的流程

1. 换头文件（改 tag，重新拷 `nvEncodeAPI.h`）
2. 重新生成
3. 看 `ffi_nvenc.rs` 的 diff：新增的结构体字段、变化的枚举值、`layout_tests`
   断言里变化的尺寸，都是需要人工确认的 ABI 变更
4. 同步 `ffi.rs` 手写区的 `NVENCAPI_VERSION`（如果头文件版本变了）
5. `cargo check --lib`

## 生成不了的部分

GUID 常量不在产物里。头文件中它们写作：

```c
static const GUID NV_ENC_CODEC_H264_GUID =
{
    0x6bc82762, 0x4e63, 0x4ca4, { 0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf }
};
```

是 `static const` 变量而非标量宏。bindgen 对结构体字面量初始化器无法求值，会
退化成：

```rust
unsafe extern "C" { pub static NV_ENC_CODEC_H264_GUID: GUID; }
```

所以 `ALLOWLIST_VARS` 刻意不匹配 `*_GUID`，这些常量保留在 `ffi.rs` 的手写区。

## 收窄范围的代价

`ALLOWLIST_TYPES` / `ALLOWLIST_VARS` 是显式清单，不是 `NV_ENC.*` 通配。好处是
产物只有三千多行而不是一万一千行；代价是**漏一个标识符就编译不过**。

这属于可接受的取舍：编译错误立刻可见，而通配带来的是一份没人会读的巨型文件。
新增 API 调用时若报 `cannot find value/type NV_ENC_XXX`，把名字加进清单重新生成
即可。

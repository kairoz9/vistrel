# Vistrel

**Vistrel** 是一个基于 WebRTC 与 Rust 高性能媒体引擎的 Minecraft 像素串流（Pixel Streaming）模组。安装本模组后，只需一个现代浏览器，打开网页、输入访问密码，即可低延迟畅玩。

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Minecraft](https://img.shields.io/badge/Minecraft-1.20%20~%2026.3-green.svg)](#支持矩阵)
[![Fabric](https://img.shields.io/badge/Fabric-%E2%9C%93-success.svg)](#支持矩阵)
[![NeoForge](https://img.shields.io/badge/NeoForge-%E2%9C%93-success.svg)](#支持矩阵)
[![Forge](https://img.shields.io/badge/Forge-%E2%9C%93-success.svg)](#支持矩阵)

---

## 核心特性

- **浏览器即点即玩**：远程玩家零安装，Chrome / Edge / Firefox / Safari 直接进入游戏，支持光标锁定。
- **双向交互**：游戏内点击链接、复制文本可直接同步到远程浏览器；浏览器侧可直接向游戏粘贴或输入文本。
- **双级渲染采集**：优先使用“显存直通”，极低延迟传输画面；不支持时无缝降级为内存异步回读，确保游戏主线程永不卡顿。
- **硬件编码**：调用底层原生引擎，优先使用 NVENC 或 Vulkan Video 硬件加速编码（支持 H.264 / H.265 / AV1）。无硬件支持时自动回退 OpenH264 软件编码（仅 H.264）。
- **低延迟输入**：绕过高层协议，直接注入游戏底层；双模式协议自适应——26.3+ 走 SDL3 注入，更早版本走 GLFW 注入。
- **录制与回放**：浏览器端可一键录制游玩过程并下载为通用的 mp4 格式；回放默认开启，常驻缓冲最近 60 秒画面，随时一键保存，无需提前开启录制。
- **跨版本多加载器**：覆盖三大加载器，多游戏版本（见下表）。

## 支持矩阵
操作系统: Windows 10/11 x64、Linux x86_64

| Minecraft        | Fabric | NeoForge | Forge | Java | 硬件编码             |
| ---------------- | ------ | -------- | ----- | ---- | -------------------- |
| 26.2 ~ 26.3      | ✅     | ✅       | —     | 25   | NVENC + Vulkan Video |
| 26.1             | ✅     | ✅       | —     | 25   | 仅 NVENC             |
| 1.21 ~ 1.21.11   | ✅     | ✅       | —     | 21   | 仅 NVENC             |
| 1.20 ~ 1.20.1    | ✅     | —        | ✅    | 17   | 仅 NVENC             |

- 硬件编码：26.2 及以上支持 NVIDIA（NVENC）与 AMD/Intel（Vulkan Video）；其余版本仅支持 NVIDIA（NVENC），其他显卡自动回退 OpenH264 软件编码。
- Fabric 端需安装 [Fabric API](https://modrinth.com/mod/fabric-api)。

## 快速开始

### 1. 安装

到 [Releases](../../releases) 下载与你 游戏版本 + 加载器 对应的 jar（如 `vistrel-fabric-26.3-0.1.0.jar`），放入游戏的 `mods/` 目录。

### 2. 启动服务并获取密码

游戏内执行 `/vistrel start`——聊天栏会显示访问地址、访问密码与编码状态，地址可点击跳转、密码点击复制。密码默认 `auto`：每次启动重新生成、不写入文件；想固定就改成字面值（6–64 位）；置空则禁用密码认证，任何浏览器可直接连接（仅建议可信网络使用）。

### 3. 游戏内命令

玩家在游戏内可随时控制服务：

| 命令                            | 作用               |
| ------------------------------- | ------------------ |
| `/vistrel start`               | 启动串流服务       |
| `/vistrel stop`                | 关停串流服务       |
| `/vistrel status`              | 查看服务状态       |
| `/vistrel password set <密码>` | 修改访问密码       |

## 配置

配置文件位于加载器配置目录（默认游戏实例的 `config/`）下的 `vistrel.properties`：

| 配置项            | 默认值    | 说明                                                   |
| ----------------- | --------- | ------------------------------------------------------ |
| `bindAddress`     | `0.0.0.0` | 监听地址，默认全部网卡, **推荐局域网/可信网络**：不要随意暴露到公网 |
| `port`            | `30808`   | HTTP / 信令监听端口（1024–65535）；媒体面 UDP 复用同号 |
| `password`        | `auto`    | auto 为每次启动换新密码；字面值（6–64 位）固定不变；空值禁用密码认证 |
| `vramDirect`      | `true`    | 显存直通开关，关闭则固定走内存回读                     |
| `softwareEncoder` | `true`    | 软件编码开关，关闭后不下载、不加载 OpenH264 |
| `audioBitrate`    | `128`     | 音频码率 (kbps)，允许范围 32–510                       |

原生库默认从 jar 内解压到加载器配置目录下的 `vistrel/natives/<平台>/`（默认 `config/`）；如需改用自定义库，可加 JVM 参数 `-Dvistrel.nativeLib=<库文件路径>`。

## 从源码构建

环境要求：**JDK 25**、**Rust 1.98+**。

```bash
# 1. 编译 Rust 原生引擎
cd rust-native && cargo build --release

# 2. 一键构建全版本全加载器
./gradlew buildAndCollect
# 非默认 target 编译时需同步注入：
# ./gradlew buildAndCollect -Pvistrel.windowsTriple=<target>
# ./gradlew buildAndCollect -Pvistrel.linuxTriple=<target>
# 产物位于 build/libs/<版本>/vistrel-<loader>-<版本>-<mod版本>.jar

# 3. 单独构建指定组合
./gradlew :fabric:26.3:assemble
./gradlew :neoforge:1.21:assemble
./gradlew :forge:1.20:assemble

# 4. 启动客户端
./gradlew :fabric:26.3:runClient
```

### Stonecutter 多版本开发

本项目采用 [Stonecutter](https://stonecutter.kikugie.dev/) 多版本架构，核心代码位于 `common`，各加载器模块仅作轻量引导。跨版本差异用 `//? if >=26.3` 条件注释表达。IDE 调试特定版本时切换源码视图：

```bash
./gradlew "Set active project to 1.21"     # 切换 IDE 视图
./gradlew "Reset active project"           # 重置回 26.3 基准（提交前务必执行）
```

### 模块结构

```
common/       核心逻辑：采集帧、输入转发、信令、Native 交互 (跨版本条件注释)
fabric/       Fabric 引导层
neoforge/     NeoForge 引导层
forge/        Forge 引导层
rust-native/  Rust 原生引擎：视频编码、音频编码、WebRTC
web/          浏览器前端 (Vite + ESLint)
```

## 许可证

本项目采用 [MIT License](LICENSE) 开源。

第三方组件：OpenH264 编解码库（Cisco 官方预编译产物）不随 jar 分发，游戏实例首次使用软编时运行期从 Cisco 官方源下载，经 SHA-256 白名单校验后加载（详见 [Binary License](https://www.openh264.org/BINARY_LICENSE.txt)）。

> NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.
>
> 本模组是与 Minecraft 无关的第三方作品，未经 Mojang / Microsoft 官方认可，也不与其有任何隶属关系。

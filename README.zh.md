# Vistrel

Vistrel 是一个客户端模组，把运行中的 Minecraft 客户端实时串流到桌面浏览器。浏览器接收游戏的画面、声音，并将操作送回游戏——玩到的就是主机上的世界、模组与设置。画面优先走硬件编码（NVENC / Vulkan Video）。连接是端到端加密的 WebRTC 直连。

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Minecraft](https://img.shields.io/badge/Minecraft-1.20%20~%2026.3-green.svg)](#支持矩阵)
[![Fabric](https://img.shields.io/badge/Fabric-%E2%9C%93-success.svg)](#支持矩阵)
[![NeoForge](https://img.shields.io/badge/NeoForge-%E2%9C%93-success.svg)](#支持矩阵)
[![Forge](https://img.shields.io/badge/Forge-%E2%9C%93-success.svg)](#支持矩阵)

**中文** · [English](README.md)

---

## 核心特性

- **零安装游玩**：任何现代桌面浏览器（Chrome、Edge、Firefox、Safari）都能用，跨平台，支持指针锁定。
- **画面自适应**：远程画面跟随游戏窗口和帧率设置自动调整，最高支持 4K 分辨率或 240 FPS；窗口变小或帧率降低时同步收缩，不浪费性能。
- **硬件加速编码**：优先使用 NVENC 或 Vulkan Video 硬件加速编码（支持 H.264 / H.265 / AV1）。无硬件支持时自动回退 OpenH264 软件编码（仅 H.264）。
- **双向交互**：游戏内触发的链接和复制操作会透传到远程浏览器；在浏览器中粘贴的文本（打开 GUI 时按 Ctrl/Cmd+V）会进入游戏的文本框。
- **录制与回放**：浏览器端可一键录制并下载为通用 MP4；回放默认开启，常驻最近 60 秒画面，随时保存，无需提前开启录制。
- **低延迟输入**：键鼠操作即时生效，和本地游玩一样跟手；26.3+ 使用 SDL3 注入，1.20–26.2 使用 GLFW，自动适配，无需设置。
- **跨版本多加载器**：Fabric / NeoForge / Forge，覆盖 1.20 ~ 26.3（见下表）。

## 支持矩阵

**操作系统：** Windows 10/11 x64、Linux x86_64

| Minecraft      | Fabric | NeoForge | Forge | Java（主机） | VRAM Direct | 硬件编码             |
| -------------- | :----: | :------: | :---: | ------------ | :---------: | -------------------- |
| 26.2 ~ 26.3    |   ✓    |    ✓     |   —   | 25           |      ✓      | NVENC + Vulkan Video |
| 26.1           |   ✓    |    ✓     |   —   | 25           |      —      | 仅 NVENC             |
| 1.21 ~ 1.21.11 |   ✓    |    ✓     |   —   | 21           |      —      | 仅 NVENC             |
| 1.20 ~ 1.20.1  |   ✓    |    —     |   ✓   | 17           |      —      | 仅 NVENC             |

**编码器——效果取决于显卡：**

- **NVIDIA**——每条受支持的 Minecraft 版本线（1.20–26.3）都支持硬件编码。**推荐**。
- **AMD / Intel**——只有在 Minecraft 26.2+ 且显卡支持 Vulkan Video 时才走硬件编码。
- **其他显卡**——OpenH264 软件编码。到处都能用，但更吃 CPU。

OpenH264 软件编码在每条 Minecraft 版本线和无论什么显卡上都可用；上面的硬件路径只是可选加速。

**VRAM Direct** 需要 NVENC 或 Vulkan Video；没有的话会自动降级到 RAM 回读。1.20.x、1.21.x 和 26.1 无论显卡如何都始终使用 RAM 回读。

**Fabric 端必须安装 [Fabric API](https://modrinth.com/mod/fabric-api)**，最低版本要求如下表。

| Minecraft      | Fabric API     | Fabric Loader |
| -------------- | -------------- | ------------- |
| 1.20 ~ 1.20.1  | `0.83.0+1.20`  | `0.15.0`      |
| 1.21 ~ 1.21.11 | `0.100.2+1.21` | `0.15.11`     |
| 26.1           | `0.144.1+26.1` | `0.18.4`      |
| 26.2           | `0.152.2+26.2` | `0.18.4`      |
| 26.3           | `0.160.6+26.3` | `0.19.3`      |


## 快速开始

### 1. 安装

到 [Releases](../../releases) 下载与游戏版本 + 加载器对应的 jar（如 `vistrel-fabric-26.3-0.1.0.jar`），放入游戏的 `mods/` 目录。

### 2. 启动服务并获取密码

游戏内执行 `/vistrel start`——聊天栏会显示访问地址、访问密码与编码状态，地址可点击跳转、密码点击复制。密码默认 `auto`：每次启动重新生成、不写入文件；想固定就改成字面值（6–64 位）；置空则禁用密码认证，任何浏览器可直接连接（仅建议可信网络使用）。

### 3. 游戏内命令

玩家在游戏内可随时控制服务：

| 命令                           | 作用         |
| ------------------------------ | ------------ |
| `/vistrel start`               | 启动串流服务 |
| `/vistrel stop`                | 关停串流服务 |
| `/vistrel status`              | 查看服务状态 |
| `/vistrel password set <密码>` | 修改访问密码 |

## 配置

配置文件位于加载器配置目录（默认游戏实例的 `config/`）下的 `vistrel.properties`：

| 配置项            | 默认值    | 说明                                                                 |
| ----------------- | --------- | -------------------------------------------------------------------- |
| `bindAddress`     | `0.0.0.0` | 监听地址，默认全部网卡, **推荐局域网/可信网络**：不要随意暴露到公网  |
| `port`            | `30808`   | HTTP / 信令监听端口（1024–65535）；媒体面 UDP 复用同号               |
| `password`        | `auto`    | auto 为每次启动换新密码；字面值（6–64 位）固定不变；空值禁用密码认证 |
| `vramDirect`      | `true`    | 显存直通开关，关闭则固定走内存回读                                   |
| `softwareEncoder` | `true`    | 软件编码开关，关闭后不下载、不加载 OpenH264                          |
| `audioBitrate`    | `128`     | 音频码率 (kbps)，允许范围 32–510                                     |

原生库默认从 jar 内解压到加载器配置目录下的 `vistrel/natives/<平台>/`（默认 `config/`）；如需改用自定义库，可加 JVM 参数 `-Dvistrel.nativeLib=<库文件路径>`。

## 工作原理

浏览器和主机在握手阶段协商 WebRTC 会话；随后画面、声音和输入直接在两者之间传输，浏览器把原始按键事件和物理鼠标位移（`movementX` / `movementY`）直接送入 Minecraft。

帧捕获有两种方式。**VRAM Direct**（26.2+）把渲染目标导出为外部 GPU 分配并直接导入编码器——Linux 上是 DRM PRIME `dma-buf` fd，Windows 上是 Win32 不透明句柄，并由 Vulkan 时间线信号量同步。更老的版本，以及所有无法导出显存的 GPU，使用 **RAM Readback**：异步双 PBO / GpuBuffer 传输，不阻塞渲染线程。

编码在 Rust 原生引擎（`libvistrel`）的专用 actor 线程上运行。输入注入方式按 Minecraft 版本选择——26.3+ 使用 SDL3 scancode，1.20 到 26.2 使用 GLFW 回调重放。

## 常见问题

**可以多人同时连接吗？**
不行——按设计同一时间只允许一个远程玩家。第二个连接通过认证后会**接管**，前一个会话会被干净地断开。

**能通过公网使用，而不只是局域网吗？**
Vistrel 不使用 STUN 或 TURN，也不做 NAT 穿透——浏览器必须直接连到主机，没有中继可以兜底。页面还是明文 `http://`，密码和 SDP 交换会被路径上的任何人读取和伪造。

局域网之外请在两台机器上安装 **Tailscale**（或 ZeroTier），并使用主机的 Tailscale IP。这些工具会把设备放进同一个虚拟局域网，即使跨互联网也有各自的私有地址，因此直连可以成功，流量端到端加密。不要把 `30808` 端口转发到公网。注意 `/vistrel status` 会优先显示 `192.168` / `10` / `172.16` 局域网地址而不是 Tailscale 的 `100.x` 地址——如果两者都有，请手动打开 `http://<tailscale-ip>:30808`，或把 `bindAddress` 设为 Tailscale IP。

**该选哪个 jar？**
每条游戏版本线 × 加载器各一个——5 个 Fabric 构建（1.20、1.21、26.1、26.2、26.3）、4 个 NeoForge 构建，以及 1 个 1.20 的 Forge 构建。它必须同时匹配 Minecraft 版本和加载器，不能混用。每个文件的加载器和游戏版本标签会指明该选哪个。见上方的 [支持矩阵](#支持矩阵) 表。

**远程玩家需要安装同样的模组吗？**
不需要。一切都运行在主机上——浏览器只接收编码后的视频并发送输入。主机本地安装了什么模组，串流出去的就是什么。

**怎样获得最低延迟？**
在可用时使用硬件编码，并使用基于 Chromium 的桌面浏览器。

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
# 产物位于 build/libs/vistrel-<loader>-<版本>-<mod版本>.jar

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

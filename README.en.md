# Vistrel

**Stream Minecraft from your PC to any desktop browser.** Vistrel is a host-side
Minecraft mod that captures the game and sends video, audio and input over
peer-to-peer WebRTC, encoded on your GPU. Remote players need no client, no Java
and no launcher — just a web page and the password.

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Minecraft](https://img.shields.io/badge/Minecraft-1.20%20~%2026.3-green.svg)](#support-matrix)
[![Fabric](https://img.shields.io/badge/Fabric-%E2%9C%93-success.svg)](#support-matrix)
[![NeoForge](https://img.shields.io/badge/NeoForge-%E2%9C%93-success.svg)](#support-matrix)
[![Forge](https://img.shields.io/badge/Forge-%E2%9C%93-success.svg)](#support-matrix)

[中文](README.md) · **English**

---

## Features

- **Zero install for players** — Chrome, Edge, Firefox and Safari connect
  directly, with full Pointer Lock support.
- **Two-way context passthrough** — links and clipboard writes triggered in-game
  open in the remote player's browser instead of the host; text pasted in the
  browser is typed into the focused in-game text box.
- **Two-tier frame capture** — VRAM Direct hands GPU textures straight to the
  encoder for the lowest latency. If the hand-off path is unavailable, capture
  cascades to RAM Readback, so the render thread never stalls either way.
- **Hardware encoding** — NVENC or Vulkan Video (H.264 / H.265 / AV1). Without
  suitable hardware, encoding falls back to OpenH264 software (H.264 only).
- **Low-latency input** — raw keyboard and mouse events are injected at the
  platform level, bypassing higher protocol layers. SDL3 injection on 26.3+,
  GLFW injection on earlier versions.
- **Recording and replay** — record a session in the browser and download a
  standard MP4. A replay buffer runs by default, keeping the last ~60 seconds so
  a clip can be saved at any moment without having started a recording.
- **Multi-loader, multi-version** — three loaders across four Minecraft eras
  (see below).

## Support Matrix

**Operating systems:** Windows 10/11 x64, Linux x86_64

| Minecraft      | Fabric | NeoForge | Forge | Java | Hardware encoding    |
| -------------- | ------ | -------- | ----- | ---- | -------------------- |
| 26.2 ~ 26.3    | ✅     | ✅       | —     | 25   | NVENC + Vulkan Video |
| 26.1           | ✅     | ✅       | —     | 25   | NVENC only           |
| 1.21 ~ 1.21.11 | ✅     | ✅       | —     | 21   | NVENC only           |
| 1.20 ~ 1.20.1  | ✅     | —        | ✅    | 17   | NVENC only           |

- **Hardware encoding:** 26.2 and later support NVIDIA (NVENC) and AMD/Intel
  (Vulkan Video). Earlier versions support NVIDIA only; other GPUs fall back to
  OpenH264 software encoding.
- **Fabric requires [Fabric API](https://modrinth.com/mod/fabric-api).** Minimum
  versions:

| Minecraft      | Fabric API      | Fabric Loader |
| -------------- | --------------- | ------------- |
| 1.20 ~ 1.20.1  | `0.83.0+1.20`   | `0.15.0`      |
| 1.21 ~ 1.21.11 | `0.100.2+1.21`  | `0.15.11`     |
| 26.1           | `0.144.1+26.1`  | `0.18.4`      |
| 26.2           | `0.152.2+26.2`  | `0.18.4`      |
| 26.3           | `0.160.6+26.3`  | `0.19.3`      |

## Getting Started

### 1. Install

Download the jar matching your Minecraft version and loader from
[Releases](../../releases) — for example `vistrel-fabric-26.3-0.1.0.jar` — and
drop it into your `mods/` directory.

### 2. Start the service

Run `/vistrel start` in game. The chat bar shows the access URL, the password and
the encoder status; the address is clickable and the password copies on click.
The password defaults to `auto`: regenerated on every start and never written to
disk. Set it to a literal value (6–64 characters) to keep it fixed, or leave it
empty to disable authentication entirely — only advisable on a trusted network.

### 3. In-game commands

| Command                        | Effect                 |
| ------------------------------ | ---------------------- |
| `/vistrel start`               | Start the streaming service |
| `/vistrel stop`                | Stop the streaming service  |
| `/vistrel status`              | Show service status         |
| `/vistrel password set <value>`| Change the access password  |

## Configuration

`vistrel.properties` lives in the loader's config directory (usually `config/`):

| Key               | Default   | Description                                            |
| ----------------- | --------- | ------------------------------------------------------ |
| `bindAddress`     | `0.0.0.0` | Listen address, all interfaces by default. **Preferred for LAN / trusted networks — do not expose to the public internet casually.** |
| `port`            | `30808`   | HTTP / signaling port (1024–65535). The media plane reuses the same UDP port. |
| `password`        | `auto`    | `auto` regenerates per start; a literal (6–64 chars) stays fixed; empty disables authentication. |
| `vramDirect`      | `true`    | VRAM Direct toggle. Disable to force RAM Readback.      |
| `softwareEncoder` | `true`    | Software encoder toggle. Disable to skip downloading and loading OpenH264. |
| `audioBitrate`    | `128`     | Audio bitrate in kbps (32–510).                          |

The native library is extracted from the jar into the loader's config directory
at `vistrel/natives/<platform>/`. To use a custom build, pass
`-Dvistrel.nativeLib=<path>` to the JVM.

## Building from Source

**Requirements:** JDK 25, Rust 1.98+

```bash
# 1. Build the Rust native engine
cd rust-native && cargo build --release

# 2. Build every version and loader
./gradlew buildAndCollect
# For non-default targets, inject the triple as well:
# ./gradlew buildAndCollect -Pvistrel.windowsTriple=<target>
# ./gradlew buildAndCollect -Pvistrel.linuxTriple=<target>
# Output: build/libs/<version>/vistrel-<loader>-<version>-<modversion>.jar

# 3. Build a single combination
./gradlew :fabric:26.3:assemble
./gradlew :neoforge:1.21:assemble
./gradlew :forge:1.20:assemble

# 4. Launch a client
./gradlew :fabric:26.3:runClient
```

### Multi-version development (Stonecutter)

The project uses [Stonecutter](https://stonecutter.kikugie.dev/). Core code lives
in `common`; each loader module is a thin bootstrap. Cross-version differences are
expressed with `//? if >=26.3` conditional comments. To debug a specific version
in the IDE, switch the source view:

```bash
./gradlew "Set active project to 1.21"     # switch the IDE view
./gradlew "Reset active project"           # reset to the 26.3 baseline (run before committing)
```

### Module layout

```
common/       Core logic: frame capture, input forwarding, signaling, native bridge
              (cross-version conditional comments)
fabric/       Fabric bootstrap
neoforge/     NeoForge bootstrap
forge/        Forge bootstrap
rust-native/  Rust native engine: video encoding, audio encoding, WebRTC
web/          Browser front end (Vite + ESLint)
```

## License

[MIT](LICENSE).

**Third-party components:** the OpenH264 codec (Cisco's official prebuilt binary)
is not distributed with the jar. On first use of software encoding, the game
instance downloads it at runtime from Cisco's official source, verifies it against
a SHA-256 allowlist, and only then loads it — see the
[Binary License](https://www.openh264.org/BINARY_LICENSE.txt).

> NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR
> MICROSOFT.

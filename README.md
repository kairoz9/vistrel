# Vistrel

Vistrel is a client-side Minecraft mod that streams a running client to a
desktop browser in real time. The browser receives the game's picture and sound
and sends input back — so the host's world, mods and settings are what get
played. Video prefers hardware encoding (NVENC or Vulkan Video). The connection
is an end-to-end encrypted WebRTC peer link.

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Minecraft](https://img.shields.io/badge/Minecraft-1.20%20~%2026.3-green.svg)](#support-matrix)
[![Fabric](https://img.shields.io/badge/Fabric-%E2%9C%93-success.svg)](#support-matrix)
[![NeoForge](https://img.shields.io/badge/NeoForge-%E2%9C%93-success.svg)](#support-matrix)
[![Forge](https://img.shields.io/badge/Forge-%E2%9C%93-success.svg)](#support-matrix)

[中文](README.zh.md) · **English**

---

## Features

- **Zero-install play** — any modern desktop browser (Chrome, Edge, Firefox,
  Safari) on any platform, with Pointer Lock capture.
- **Adaptive picture** — the remote picture follows the game window and
  frame-rate setting, up to 4K or 240 FPS; it scales down with the window or
  frame rate, so no performance is wasted.
- **Two-path frame capture** — VRAM Direct on 26.2+, asynchronous RAM Readback
  everywhere else; neither path blocks the render thread.
- **Hardware-accelerated encoding** — NVENC or Vulkan Video (H.264 / H.265 /
  AV1), with OpenH264 software fallback (H.264 only) when no hardware encoder is
  available.
- **Two-way interaction** — links and copy actions triggered in-game surface in
  the remote browser; text pasted in the browser (Ctrl/Cmd+V with a GUI open)
  lands in the game's text field.
- **Recording and replay** — one-click MP4 capture in the browser; replay is on
  by default and keeps the last 60 seconds buffered, ready to save at any time.
- **Low-latency input** — input feels immediate; SDL3 injection on 26.3+, GLFW
  on 1.20–26.2, adapting automatically with no setup.
- **Multi-loader, multi-version** — Fabric / NeoForge / Forge across Minecraft
  1.20–26.3 (see below).

## Support Matrix

**Operating systems:** Windows 10/11 x64, Linux x86_64

| Minecraft      | Fabric | NeoForge | Forge | Java (host) | VRAM Direct | Hardware encoding    |
| -------------- | :----: | :------: | :---: | ----------- | :---------: | -------------------- |
| 26.2 ~ 26.3    |   ✓    |    ✓     |   —   | 25          |      ✓      | NVENC + Vulkan Video |
| 26.1           |   ✓    |    ✓     |   —   | 25          |      —      | NVENC only           |
| 1.21 ~ 1.21.11 |   ✓    |    ✓     |   —   | 21          |      —      | NVENC only           |
| 1.20 ~ 1.20.1  |   ✓    |    —     |   ✓   | 17          |      —      | NVENC only           |

**Encoder — results depend on the GPU:**

- **NVIDIA** — hardware encoding on every supported Minecraft line (1.20–26.3). **Recommended**.
- **AMD / Intel** — hardware encoding only on Minecraft 26.2+ and only on GPUs that support Vulkan Video.
- **Anything else** — OpenH264 software encoding. Works everywhere, but costs more CPU.

OpenH264 software encoding is available on every Minecraft line and every GPU; the hardware paths above are optional acceleration.

**VRAM Direct** needs NVENC or Vulkan Video; without one it demotes to RAM readback automatically. 1.20.x, 1.21.x and 26.1 always use RAM readback regardless of GPU.

**Fabric requires [Fabric API](https://modrinth.com/mod/fabric-api).** Minimum versions:

| Minecraft      | Fabric API     | Fabric Loader |
| -------------- | -------------- | ------------- |
| 1.20 ~ 1.20.1  | `0.83.0+1.20`  | `0.15.0`      |
| 1.21 ~ 1.21.11 | `0.100.2+1.21` | `0.15.11`     |
| 26.1           | `0.144.1+26.1` | `0.18.4`      |
| 26.2           | `0.152.2+26.2` | `0.18.4`      |
| 26.3           | `0.160.6+26.3` | `0.19.3`      |

## Getting Started

### 1. Install

Download the jar matching the Minecraft version and loader from
[Releases](../../releases) — for example `vistrel-fabric-26.3-0.1.0.jar` — and
drop it into the `mods/` directory.

### 2. Start the service

Run `/vistrel start` in game. The chat bar shows the access URL, the password and
the encoder status; the address is clickable and the password copies on click.
The password defaults to `auto`: regenerated on every start and never written to
disk. Set it to a literal value (6–64 characters) to keep it fixed, or leave it
empty to disable authentication entirely — only advisable on a trusted network.

### 3. In-game commands

| Command                         | Effect                      |
| ------------------------------- | --------------------------- |
| `/vistrel start`                | Start the streaming service |
| `/vistrel stop`                 | Stop the streaming service  |
| `/vistrel status`               | Show service status         |
| `/vistrel password set <value>` | Change the access password  |

## Configuration

`vistrel.properties` lives in the loader's config directory (usually `config/`):

| Key               | Default   | Description                                                                                                                          |
| ----------------- | --------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `bindAddress`     | `0.0.0.0` | Listen address, all interfaces by default. **Preferred for LAN / trusted networks — do not expose to the public internet casually.** |
| `port`            | `30808`   | HTTP / signaling port (1024–65535). The media plane reuses the same UDP port.                                                        |
| `password`        | `auto`    | `auto` regenerates per start; a literal (6–64 chars) stays fixed; empty disables authentication.                                     |
| `vramDirect`      | `true`    | VRAM Direct toggle. Disable to force RAM Readback.                                                                                   |
| `softwareEncoder` | `true`    | Software encoder toggle. Disable to skip downloading and loading OpenH264.                                                           |
| `audioBitrate`    | `128`     | Audio bitrate in kbps (32–510).                                                                                                      |

The native library is extracted from the jar into the loader's config directory
at `vistrel/natives/<platform>/`. To use a custom build, pass
`-Dvistrel.nativeLib=<path>` to the JVM.

## How It Works

The browser and host negotiate a WebRTC session during the handshake; picture,
sound and input then flow directly between them. The browser sends raw key
events and physical mouse deltas (`movementX` / `movementY`) straight into
Minecraft.

Frames are captured in one of two ways. **VRAM Direct** (26.2+) exports the
render target as an external GPU allocation and imports it straight into the
encoder — a DRM PRIME `dma-buf` fd on Linux, an opaque Win32 handle on Windows,
synchronized with Vulkan timeline semaphores. Everything older, and every GPU
without an exportable path, uses **RAM Readback**: an asynchronous double-PBO /
GpuBuffer transfer that never stalls the render thread.

Encoding runs in the Rust native engine (`libvistrel`) on dedicated actor
threads. Input injection follows the Minecraft version — SDL3 scancodes on
26.3+, GLFW callback replay from 1.20 through 26.2.

## FAQ

**Can several people connect at once?**
No — one remote player at a time, by design. A second connection that
authenticates **takes over**, and the previous session is cleanly disconnected.

**Does it work over the internet, not just LAN?**
Vistrel uses no STUN or TURN and does no NAT traversal — the browser must reach
the host directly, and there is no relay to fall back on. The page is also plain
`http://`, so the password and SDP exchange would be readable and forgeable by
anyone on the path.

For play beyond the LAN, install **Tailscale** (or ZeroTier) on both machines
and use the host's Tailscale IP instead. These tools put the devices on the same
virtual LAN with their own private addresses, even across the internet — so the
direct connection succeeds and traffic is encrypted end to end. Do not
port-forward `30808` to the internet. Note that `/vistrel status` reports the
host's `192.168` / `10` / `172.16` LAN address in preference to a Tailscale
`100.x` address — if both exist, open `http://<tailscale-ip>:30808` manually or
set `bindAddress` to the Tailscale IP.

**Which jar do I need?**
One per game-version line and loader — five Fabric builds (1.20, 1.21, 26.1,
26.2, 26.3), four NeoForge builds and one Forge build for 1.20. It must match
both the Minecraft version and the loader; they are not interchangeable. The
loader and game-version tags on each file indicate exactly which one to pick.
See the [Support Matrix](#support-matrix) above.

**Does the remote player need the same mods installed?**
No. Everything runs on the host — the browser receives encoded video and sends
input, nothing more. Whatever mods the host runs locally are what gets streamed.

**How do I get the lowest latency?**
Hardware encoding where available, and a Chromium-based desktop browser.

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
# Output: build/libs/vistrel-<loader>-<version>-<modversion>.jar

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

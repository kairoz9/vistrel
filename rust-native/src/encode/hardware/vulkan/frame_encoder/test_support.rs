//! 帧编码器的测试夹具与 NV12 测试帧生成

use ash::vk;

use super::FrameEncoder;
use crate::encode::StreamingProfile;
use crate::encode::hardware::vulkan::codec::caps::CodecCaps;
use crate::encode::hardware::vulkan::codec::config::{
    EncodePolicy, GopPolicy, IntraRefreshPolicy, SessionConfig,
};
use crate::encode::hardware::vulkan::device::VideoDevice;

/// 测试档位：1080p60，显式 8 Mbps（绕开动态码率公式，让断言的输入确定）。
/// codec 不在档位里，由各用例按能力快照单独构造会话配置
pub(in crate::encode::hardware::vulkan) fn test_profile() -> StreamingProfile {
    StreamingProfile::new(1920, 1080, 60, 8_000_000)
}

/// 测试用静态会话配置：策略可指定，档位固定为 [`test_profile`]
pub(in crate::encode::hardware::vulkan) fn test_session_config_with(
    caps: &CodecCaps,
    policy: &EncodePolicy,
    device_supports_intra_refresh: bool,
) -> Result<SessionConfig, String> {
    SessionConfig::build(caps, &test_profile(), policy, device_supports_intra_refresh)
}

/// 测试用静态会话配置：固定 GOP，关闭 intra-refresh，保持断言按显式 GOP 进行
pub(in crate::encode::hardware::vulkan) fn test_session_config(
    caps: &CodecCaps,
    gop: u32,
) -> Result<SessionConfig, String> {
    test_session_config_with(
        caps,
        &EncodePolicy {
            gop: GopPolicy::Fixed(gop),
            intra_refresh: IntraRefreshPolicy::Disable,
            ..Default::default()
        },
        false,
    )
}

/// 测试用帧编码器：从静态配置派生默认码控
pub(in crate::encode::hardware::vulkan) fn test_frame_encoder(
    device: &VideoDevice,
    caps: &CodecCaps,
    gop: u32,
) -> Result<FrameEncoder, String> {
    let config = test_session_config(caps, gop)?;
    let coding = config.coding_control(60);
    FrameEncoder::new(device, config, coding)
}

/// 造一帧 NV12：亮度低频渐变、色度四色块
///
/// **色度不能再填 128。** 色度恒为中性灰时，色度平面无论怎么编码错，画面都只是
/// 灰的 —— 亮度对、色度崩，画面上正是「彩色噪点 / 整片偏色」，而测试全绿。
/// 真实画面的色度有内容，测试必须覆盖它。
///
/// **亮度也不能用满幅噪声。** 满幅随机噪声的熵接近理论上限，8 Mbps 会被亮度
/// 全部吃光，色度只能分到极少比特 —— 实测表现为「四块边界清晰但颜色发灰、
/// 块内渐变」，那是码率不足，不是色度 bug。低频对角渐变的码率需求极小，
/// 色度能拿到足够比特：**只有这样才能把「色度编码错误」和「色度被饿死」分开**。
///
/// 色度用四个大色块而不是噪声：噪声色度与「色度崩坏」在画面上难以区分，
/// 而色块边界清晰，解出来偏色 / 串块 / 边界错位都能一眼判定。
/// NV12 的 UV 是交错的 `U0 V0 U1 V1 ...`，一个 UV 对覆盖 2×2 像素。
pub(in crate::encode::hardware::vulkan) fn make_noisy_frame(
    extent: vk::Extent2D,
    seed: u32,
) -> Vec<u8> {
    let w = extent.width as usize;
    let h = extent.height as usize;
    let pixels = w * h;
    let mut out = vec![0u8; pixels + pixels / 2];
    // 亮度：低频对角渐变 + 每 64 像素一格的细微台阶（保证有可预测的高频结构，
    // 但不至于把码率吃光）。种子只影响台阶相位，不同种子仍有真实残差。
    let phase = (seed % 64) as usize;
    for y in 0..h {
        let base = (y * 255) / h.max(1);
        for x in 0..w {
            let ramp = (x * 255) / w.max(1);
            let step = if ((x + phase) / 64 + y / 64).is_multiple_of(2) {
                0
            } else {
                8
            };
            out[y * w + x] = ((base + ramp) / 2).saturating_add(step).min(255) as u8;
        }
    }
    // 色度：8 条竖色带。
    //
    // 纯色块内部平坦，色度残差固定为 0，测不出色度残差编码的问题；
    // 噪声色度又与「色度崩坏」在画面上无法区分。竖色带的垂直边界是色度高频，
    // 能真正压出色度残差，而边界位置固定，解出来错位 / 串色 / 边界发虚都能一眼判定。
    const BARS: [(u8, u8); 8] = [
        (128, 128), // 中性
        (64, 128),  // 偏蓝
        (192, 128), // 偏黄
        (128, 64),  // 偏红
        (128, 192), // 偏青
        (64, 64),
        (192, 192),
        (96, 160),
    ];
    let (cw, ch) = (w / 2, h / 2);
    for y in 0..ch {
        for x in 0..cw {
            let (u, v) = BARS[(x * BARS.len() / cw).min(BARS.len() - 1)];
            let idx = pixels + (y * cw + x) * 2;
            out[idx] = u;
            out[idx + 1] = v;
        }
    }
    out
}

//! OpenAL 48kHz 立体声 PCM 到 Opus 编码引擎
//!
//! 负责接收游戏侧音频采集投递的 48kHz Stereo 16-bit PCM (960 采样/通道，20ms)，
//! 采用 Opus Application::Audio 模式进行全频带高保真编码，并输出压缩后的音频 RTP Payload 数据。

use opus::{Application, Bitrate, Channels, Encoder, Error as OpusError};

/// Opus 处理的标准时钟频率 (48 kHz)
pub const AUDIO_SAMPLE_RATE: u32 = 48000;

/// 立体声双通道
pub const AUDIO_CHANNELS: usize = 2;

/// 单音频帧持续时间 (20 ms)
pub const AUDIO_FRAME_DURATION_MS: u64 = 20;

/// 20ms 每声道采样点数 (48000 * 0.02 = 960)
const AUDIO_SAMPLES_PER_CHANNEL: usize = 960;

/// 20ms 单帧总采样点数 (960 * 2 = 1920)
pub const AUDIO_TOTAL_SAMPLES: usize = AUDIO_SAMPLES_PER_CHANNEL * AUDIO_CHANNELS;

/// 默认音频推流码率 (128 kbps)
pub const DEFAULT_AUDIO_BITRATE_BPS: i32 = 128_000;

/// Opus 推荐最大单包输出缓冲大小
const MAX_OPUS_PACKET_SIZE: usize = 1275;

/// Opus 高保真音频编码引擎
pub struct AudioEncoder {
    encoder: Encoder,
    output_buffer: Vec<u8>,
}

/// 音频帧编码失败原因
#[derive(Debug)]
pub enum AudioEncodeError {
    /// PCM 帧长度与音频帧格式约定不符（直接报错，调用方必须丢帧）
    PcmLengthMismatch { expected: usize, actual: usize },
    /// libopus 编码失败
    Opus(OpusError),
}

impl AudioEncoder {
    /// 创建并初始化 Opus 编码引擎
    ///
    /// - 采用 48000Hz 立体声 (Stereo)
    /// - 采用 Application::LowDelay (对应 libopus OPUS_APPLICATION_RESTRICTED_LOWDELAY 模式)
    /// - 启用 In-Band FEC (带内前向纠错)
    pub fn new(bitrate_bps: i32) -> Result<Self, OpusError> {
        let mut encoder = Encoder::new(AUDIO_SAMPLE_RATE, Channels::Stereo, Application::LowDelay)?;
        encoder.set_bitrate(Bitrate::Bits(bitrate_bps))?;
        encoder.set_inband_fec(true)?;
        // 设置期望丢包率为 5%，激活内部冗余编码优化
        encoder.set_packet_loss_perc(5)?;

        log::info!(
            "Opus 音频编码引擎初始化成功: [模式: RestrictedLowdelay, 采样率: 48kHz, 声道: 立体声, 码率: {} kbps, In-Band FEC: 启用]",
            bitrate_bps / 1000
        );

        Ok(Self {
            encoder,
            output_buffer: vec![0u8; MAX_OPUS_PACKET_SIZE],
        })
    }

    /// 编码一帧 20ms 的 PCM 音频数据
    ///
    /// 长度不符音频帧格式约定时**直接拒绝**（返回 [`AudioEncodeError::PcmLengthMismatch`]），
    /// 严禁静默重定时——Opus 会把错误长度当成不同时长的帧编码，漂移将表现为每帧丢帧告警
    /// 而非无声的音画错位。
    pub fn encode_frame(&mut self, pcm: &[i16]) -> Result<&[u8], AudioEncodeError> {
        if pcm.len() != AUDIO_TOTAL_SAMPLES {
            return Err(AudioEncodeError::PcmLengthMismatch {
                expected: AUDIO_TOTAL_SAMPLES,
                actual: pcm.len(),
            });
        }
        let encoded_len = self
            .encoder
            .encode(pcm, &mut self.output_buffer)
            .map_err(AudioEncodeError::Opus)?;
        Ok(&self.output_buffer[..encoded_len])
    }

    /// 动态调节音频比特率
    pub fn set_bitrate(&mut self, bitrate_bps: i32) -> Result<(), OpusError> {
        self.encoder.set_bitrate(Bitrate::Bits(bitrate_bps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_frame_rejects_contract_violation() {
        let mut engine =
            AudioEncoder::new(DEFAULT_AUDIO_BITRATE_BPS).expect("Failed to create AudioEngine");
        // 与 AudioCaptureManager 的音频帧格式是同一条约定的两端：长度不符必须直接拒绝
        let short = vec![0i16; AUDIO_TOTAL_SAMPLES - 1];
        let long = vec![0i16; AUDIO_TOTAL_SAMPLES + 1];
        assert!(matches!(
            engine.encode_frame(&short),
            Err(AudioEncodeError::PcmLengthMismatch { .. })
        ));
        assert!(matches!(
            engine.encode_frame(&long),
            Err(AudioEncodeError::PcmLengthMismatch { .. })
        ));
    }

    #[test]
    fn test_opus_encode_silence() {
        let mut engine =
            AudioEncoder::new(DEFAULT_AUDIO_BITRATE_BPS).expect("Failed to create AudioEngine");
        let silence = vec![0i16; AUDIO_TOTAL_SAMPLES];
        let encoded = engine
            .encode_frame(&silence)
            .expect("Failed to encode silence frame");
        assert!(!encoded.is_empty());
        assert!(encoded.len() <= MAX_OPUS_PACKET_SIZE);
    }

    #[test]
    fn test_opus_encode_sine_wave() {
        let mut engine =
            AudioEncoder::new(DEFAULT_AUDIO_BITRATE_BPS).expect("Failed to create AudioEngine");
        // 生成 440Hz 简单正弦波立体声
        let mut pcm = Vec::with_capacity(AUDIO_TOTAL_SAMPLES);
        for i in 0..AUDIO_SAMPLES_PER_CHANNEL {
            let sample =
                ((i as f32 * 440.0 * 2.0 * std::f32::consts::PI / 48000.0).sin() * 16384.0) as i16;
            pcm.push(sample); // L
            pcm.push(sample); // R
        }
        let encoded = engine
            .encode_frame(&pcm)
            .expect("Failed to encode sine wave");
        assert!(!encoded.is_empty());
        assert!(encoded.len() <= MAX_OPUS_PACKET_SIZE);
    }
}

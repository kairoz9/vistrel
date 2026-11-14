//! 音频回环的原生半边 (Audio Loopback)
//!
//! 采集线程同步 Opus 编码后送进**当前会话**的有界队列，由会话的写者（`AudioWriter`）
//! 串行写轨道。队列满即丢帧——音频连续性让位于绝不阻塞采集线程。
//!
//! 会话的写循环随会话建立 spawn、随会话 cancel 退出、由会话有界 join。回环只保留编码器
//! 与「当前收件人」的发送端；没有会话时样本直接丢，不留写者。

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use parking_lot::Mutex;
use tokio::sync::mpsc::Sender;

use crate::encode::{
    AUDIO_FRAME_DURATION_MS, AudioEncodeError, AudioEncoder, AudioPacket, DEFAULT_AUDIO_BITRATE_BPS,
};

const AUDIO_QUEUE_DEPTH: usize = 8;

/// 音频回环：Opus 编码器与「当前收件人」发送端的二对一持有者
///
/// 编码器由互斥锁串行化（`push_sample` 同步调用），收件端随会话建立/关停原子换手；
/// 无收件人或编码器缺位时样本在编码前就丢，全程绝不阻塞采集线程
pub(crate) struct AudioLoopback {
    encoder: Mutex<Option<AudioEncoder>>,
    tx: ArcSwapOption<Sender<AudioPacket>>,
}

impl AudioLoopback {
    /// 构造回环：按默认码率建 Opus 编码器，失败经 `.ok()` 静默吞掉——这是语义选择
    /// 而非疏漏：音频是附属轨道，编码器缺位只表现为全程无声（`push_sample` 逐帧
    /// 短路），不构成阻断串流会话的理由。收件端初始为空，样本直接丢
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            encoder: Mutex::new(AudioEncoder::new(DEFAULT_AUDIO_BITRATE_BPS).ok()),
            tx: ArcSwapOption::empty(),
        })
    }

    /// 会话建立时装上一条新的有界队列，返回本会话的接收端
    ///
    /// 换手发生在「旧会话已 close」之后（引擎串行化建立/关闭），旧接收端的写循环
    /// 已随取消退出，不会与新会话抢样本。
    pub(crate) fn attach(&self) -> tokio::sync::mpsc::Receiver<AudioPacket> {
        self.attach_with_capacity(AUDIO_QUEUE_DEPTH)
    }

    /// [`Self::attach`] 的可注入变体：以指定深度装一条新的有界队列，返回本会话接收端
    ///
    /// 队列满即丢帧（try_send 非阻塞），语义与生产深度完全一致；测试用浅队列
    /// 制造背压、验证丢帧路径。换手时序约束同 `attach`
    pub(crate) fn attach_with_capacity(
        &self,
        depth: usize,
    ) -> tokio::sync::mpsc::Receiver<AudioPacket> {
        let (tx, rx) = tokio::sync::mpsc::channel(depth);
        self.tx.store(Some(Arc::new(tx)));
        rx
    }

    /// 摘除当前收件端：会话已关停，样本在编码前就丢
    ///
    /// 由引擎在关停收尾时调用；新会话建立时 attach 会装上新的。
    pub(crate) fn detach(&self) {
        self.tx.store(None);
    }

    /// 同步调用：编码 PCM 并送进本会话队列（非阻塞、保序、无 tokio::spawn）
    pub(crate) fn push_sample(&self, pcm: &[i16]) {
        // 没有会话就不编码：先取收件端，编码器锁不空转
        let Some(sender) = self.tx.load_full() else {
            return;
        };
        let mut encoder_guard = self.encoder.lock();
        let Some(encoder) = encoder_guard.as_mut() else {
            return;
        };
        match encoder.encode_frame(pcm) {
            Ok(opus_slice) => {
                let packet = AudioPacket {
                    data: bytes::Bytes::copy_from_slice(opus_slice),
                    duration: Duration::from_millis(AUDIO_FRAME_DURATION_MS),
                };
                match sender.try_send(packet) {
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        log::warn!("音频发送队列满，丢弃当前音频帧");
                    }
                    // 会话刚关：接收端已随写循环退出，样本直接丢
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) | Ok(()) => {}
                }
            }
            Err(AudioEncodeError::PcmLengthMismatch { expected, actual }) => {
                // 格式漂移 (游戏侧与原生侧常量组失配)：直接丢帧
                log::warn!(
                    "音频帧格式违规 (期望 {} 采样, 实际 {})，本帧已丢弃",
                    expected,
                    actual
                );
            }
            Err(e) => log::warn!("Opus 音频编码失败: {:?}", e),
        }
    }

    /// 调整 Opus 编码码率（kbps 输入，内部转 bps）；非正值回落到默认码率。
    /// 编码器缺位（构造期初始化失败）时静默无效，无声语义不变
    pub(crate) fn set_bitrate(&self, bitrate_kbps: i32) {
        let bitrate_bps = if bitrate_kbps > 0 {
            bitrate_kbps * 1000
        } else {
            DEFAULT_AUDIO_BITRATE_BPS
        };
        if let Some(encoder) = self.encoder.lock().as_mut() {
            let _ = encoder.set_bitrate(bitrate_bps);
        }
    }
}

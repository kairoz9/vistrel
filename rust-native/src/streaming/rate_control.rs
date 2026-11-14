//! 发送侧码率回路
//!
//! **不变量**：回路计算的跟随速率通过 `sink` 单向派发至下游编码器。
//! `set_bps` 无条件派发——`VideoEncoder::reconfigure` 重建 NVENC 会话时
//! 会把它设成档位 CBR，那是一次绕过本模块的写入；只有无条件补一次派发，"回路认为
//! 编码器跑多少"与"编码器实际跑多少"才不可能分叉。

use parking_lot::RwLock;

/// 死区：变化不足十分之一不重配 NVENC
const DEADBAND_DIVISOR: u32 = 10;

/// 探测起始速率比例：档位的 50%
#[cfg(test)]
const PROBE_RATIO: f64 = 0.50;

/// 控制回路下限比例：档位的 20%
const FLOOR_RATIO: f64 = 0.20;

fn min_bps(tier_bps: u32) -> u32 {
    let floor = (tier_bps as f64 * FLOOR_RATIO).round() as u32;
    floor.min(tier_bps).max(1_000_000)
}

#[cfg(test)]
fn initial_bps(max_bps: u32, min_bps: u32) -> u32 {
    let max_bps = f64::from(max_bps);
    let min_bps = f64::from(min_bps);
    (max_bps * PROBE_RATIO).round().clamp(min_bps, max_bps) as u32
}

struct Inner {
    max: u32,
    min: u32,
    /// 最后一次交给 sink 的值，同时是死区的参照
    followed: u32,
    /// 当前活跃会话的最新带宽估计 (bps)；0 表示尚未上报
    estimate: u32,
}

/// 发送侧码率回路：带宽估计 → 死区去抖 → 经 `sink` 单向派发至下游编码器
///
/// 「回路认为编码器跑多少」与「编码器实际跑多少」靠派发保证不分叉（例外与理由见
/// 模块文档）；档位是跟随值的上界兼初始值，估计缺位时诚实回落档位。并发经 `RwLock`
/// 串行化，`sink` 在写锁外调用、变化足量时恰好一次
pub(crate) struct RateControl {
    inner: RwLock<Inner>,
    sink: Box<dyn Fn(u32) + Send + Sync>,
}

impl RateControl {
    /// `tier_bps` 是档位自身的 CBR 目标，同时是跟随值的上界与初始值。
    /// sink 在跟随值变化时被调用一次；构造点唯一。
    pub(crate) fn new(tier_bps: u32, sink: impl Fn(u32) + Send + Sync + 'static) -> Self {
        Self {
            inner: RwLock::new(Inner {
                max: tier_bps,
                min: min_bps(tier_bps),
                followed: tier_bps,
                estimate: 0,
            }),
            sink: Box::new(sink),
        }
    }

    /// 记录当前对端的带宽估计。直接放行上报值。
    pub(crate) fn report(&self, bits_per_second: u32) {
        {
            let mut inner = self.inner.write();
            inner.estimate = bits_per_second;
        }
        self.recompute(false);
    }

    /// 档位变更：重算上下界并**无条件派发**，理由见模块文档。
    pub(crate) fn set_bps(&self, bps: u32) {
        {
            let mut inner = self.inner.write();
            inner.max = bps;
            inner.min = min_bps(bps);
        }
        self.recompute(true);
    }

    /// 探测起始速率，供拦截器链装配点一次性读取（档位的 50%，受 [floor, tier] 保护）
    #[cfg(test)]
    pub(crate) fn initial(&self) -> u32 {
        initial_bps(self.max(), self.min())
    }

    #[cfg(test)]
    pub(crate) fn min(&self) -> u32 {
        self.inner.read().min
    }

    pub(crate) fn max(&self) -> u32 {
        self.inner.read().max
    }

    /// 重算并派发。`force` 绕过去抖与死区，只用于档位变更。
    fn recompute(&self, force: bool) {
        let next = {
            let mut inner = self.inner.write();
            let raw = inner.estimate;
            // 无任何上报时回落到档位：这是"没有信息"的诚实默认，也保证向 sink 派发的
            // 永远是个可用的数——0 哨兵会把"尚无估计"这条实现细节泄给下游
            let next = if raw == 0 {
                inner.max
            } else {
                // 档位可能低于地板（极低档位），上界取二者较大者，避免 clamp 逆序 panic
                raw.clamp(inner.min, inner.max.max(inner.min))
            };
            if !force {
                if next == inner.followed {
                    return;
                }
                // 死区：不足十分之一的变化不重配 NVENC，避免在估计器的更新节拍上抖动
                if next.abs_diff(inner.followed) < inner.followed / DEADBAND_DIVISOR {
                    return;
                }
            }
            inner.followed = next;
            next
        };
        // 回路的失效方式是沉默，这行是唯一的读数
        log::debug!("编码器跟随码率 {} kbps", next / 1000);
        (self.sink)(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use parking_lot::Mutex;

    /// 记录每次派发值的替身 sink
    fn recorder() -> (Arc<Mutex<Vec<u32>>>, impl Fn(u32) + Send + Sync + 'static) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink_log = Arc::clone(&log);
        (log, move |bps| {
            sink_log.lock().push(bps);
        })
    }

    #[test]
    fn test_direct_rate_report() {
        let (log, sink) = recorder();
        let rate = RateControl::new(20_000_000, sink);
        rate.report(16_000_000);
        assert_eq!(*log.lock(), vec![16_000_000]);
        rate.report(13_000_000);
        assert_eq!(*log.lock(), vec![16_000_000, 13_000_000]);
    }

    #[test]
    fn test_clamp_bounds_the_followed_rate() {
        let (log, sink) = recorder();
        let rate = RateControl::new(20_000_000, sink);
        // 链路比档位宽也不越过档位：梯子选的是画质目标
        rate.report(100_000_000);
        assert!(log.lock().is_empty());
        // 比地板还窄就顶在地板上，编码器物理上产不出更低
        rate.report(1);
        // 地板是档位的固定比例（`FLOOR_RATIO`），不是绝对下限：20 Mbps 档 ⇒ 4 Mbps。
        // 调这个比例时这里要跟着改。
        assert_eq!(*log.lock(), vec![4_000_000]);
    }

    #[test]
    fn test_decrease_is_not_debounced() {
        let (log, sink) = recorder();
        let rate = RateControl::new(20_000_000, sink);
        rate.report(18_000_000);
        assert_eq!(*log.lock(), vec![18_000_000]);
        rate.report(14_000_000);
        assert_eq!(
            *log.lock(),
            vec![18_000_000, 14_000_000],
            "未开启去抖时，连续退让直接放行"
        );
    }

    #[test]
    fn test_increase_is_not_debounced() {
        let (log, sink) = recorder();
        let rate = RateControl::new(20_000_000, sink);
        rate.report(20_000_000);
        rate.report(10_000_000);
        rate.report(12_000_000);
        // 恢复不该被放慢
        assert_eq!(*log.lock(), vec![10_000_000, 12_000_000]);
    }

    #[test]
    fn test_deadband_suppresses_small_changes() {
        let (log, sink) = recorder();
        let rate = RateControl::new(20_000_000, sink);
        rate.report(19_000_000);
        assert!(log.lock().is_empty(), "5% 的变化不该触发 NVENC 重配");
    }

    #[test]
    fn test_set_bps_always_dispatches() {
        let (log, sink) = recorder();
        let rate = RateControl::new(20_000_000, sink);
        rate.report(15_000_000);
        log.lock().clear();
        // 值没变，但 NVENC 会话刚被重建成档位 CBR，必须重新断言
        rate.set_bps(20_000_000);
        assert_eq!(*log.lock(), vec![15_000_000]);
    }

    #[test]
    fn test_initial_rate_and_tier_are_readable() {
        let (_log, sink) = recorder();
        let rate = RateControl::new(20_000_000, sink);
        assert_eq!(rate.max(), 20_000_000);
        // 20M 的 50% = 10M
        assert!(f64::from(rate.initial() - 10_000_000).abs() < 1.0);
        rate.set_bps(8_000_000);
        // 8M 的 50% = 4M
        assert!(f64::from(rate.initial() - 4_000_000).abs() < 1.0);
    }
}

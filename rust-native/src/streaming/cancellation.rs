//! 轻量级取消令牌 (Cancellation Token)
//!
//! 基于 `tokio::sync::watch` 实现，替代庞大的 `tokio-util` 依赖。
//! 支持多端克隆、幂等触发 (`cancel`)、以及异步挂起等待 (`cancelled`)。

use std::sync::Arc;
use tokio::sync::watch;

/// 轻量级异步任务取消令牌
#[derive(Debug, Clone)]
pub struct CancellationToken {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    /// 创建一个新的取消令牌
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    /// 触发取消信号（幂等操作）
    pub fn cancel(&self) {
        let _ = self.tx.send_replace(true);
    }

    /// 异步等待取消信号触发
    ///
    /// 若已经处于取消状态，立即返回；否则异步挂起直至 `cancel()` 被调用。
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        if *rx.borrow_and_update() {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 未取消时挂起等待、cancel 唤醒；已取消立即返回；cancel 幂等
    #[tokio::test]
    async fn test_cancelled_wakes_on_cancel_and_is_immediate_after() {
        let token = CancellationToken::default();
        let waiter = {
            let token = token.clone();
            tokio::spawn(async move {
                token.cancelled().await;
            })
        };
        // 让等待者先跑到挂起点
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "未取消时不得提前返回");

        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("cancel 后等待者必须被唤醒")
            .unwrap();

        // 已取消状态：立即返回；重复 cancel 幂等
        token.cancel();
        tokio::time::timeout(Duration::from_millis(100), token.cancelled())
            .await
            .expect("已取消必须立即返回");
    }
}

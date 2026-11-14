//! Vistrel 原生多媒体与 WebRTC 引擎核心
//!
//! 提供基于硬件加速与 WebRTC 的超低延迟 Minecraft 像素串流服务：
//! - `encode`: 视频与音频编码管线（NVENC / Vulkan Video / OpenH264 / Opus）
//! - `streaming`: WebRTC 会话管理、拥塞控制、输入事件流与信令服务器
//! - `jni`: 游戏侧互操作层（导出符号、回调分发与全局引擎生命周期管理）

pub mod encode;
pub mod jni;
pub mod streaming;

/// 测试专用：硬件资源并发闸
///
/// 硬件用例会各自建 Vulkan 实例/设备/视频会话、分配 DPB 图像与锁页帧环，并打开
/// NVENC 编码会话。
///
/// 可重入语义：一个测试线程首次获取占一个全局闸位，同线程后续获取（同一用例里
/// 再建一个后端/实例）只发搭车位、不占新闸位。这样任何线程都不会「持着闸位等
/// 闸位」，持有-等待死锁在结构上不可能。闸位随持有者（Vulkan 实例 / NVENC 后端）
/// 的最后一个 `Arc` 引用归还；实例的 Clone 共享同一闸位对象，释放恰好多对一。
#[cfg(test)]
pub(crate) mod testing {
    use std::cell::Cell;
    use std::sync::{Arc, Condvar, Mutex};

    /// 同时占用闸位的测试线程上限：3 套并发管线在 8GB 卡上有充分余量，
    /// 既避开历史上全量并行撞墙的峰值，又足以遮住设备创建的串行延迟
    const MAX_CONCURRENT: usize = 3;

    static LIVE: Mutex<usize> = Mutex::new(0);
    static RELEASED: Condvar = Condvar::new();

    thread_local! {
        /// 本线程是否已持有全局闸位：持有期间的同线程获取一律搭车
        static HOLDS_SLOT: Cell<bool> = const { Cell::new(false) };
    }

    /// 一个闸位：`global: true` 者绑定全局闸位，搭车位 drop 时无操作。
    /// 经 `Arc` 共享，最后一个引用才触发释放，Clone 共享同一闸位对象。
    pub(crate) struct GpuSlot {
        global: bool,
    }

    impl GpuSlot {
        /// 领取闸位：本线程未持闸时阻塞等待全局空闲位（Condvar 睡眠等待，不忙等、
        /// 无超时），拿到后占位并把线程标记为持闸；已持闸线程的后续获取只发搭车位、
        /// 不占新闸位。闸位由返回的 `Arc` 持有，最后一个引用析构时归还并唤醒一个等待者
        pub(crate) fn acquire() -> Arc<Self> {
            let global = !HOLDS_SLOT.with(Cell::get);
            if global {
                let mut live = LIVE.lock().unwrap();
                while *live >= MAX_CONCURRENT {
                    live = RELEASED.wait(live).unwrap();
                }
                *live += 1;
                HOLDS_SLOT.with(|h| h.set(true));
            }
            Arc::new(Self { global })
        }
    }

    impl Drop for GpuSlot {
        fn drop(&mut self) {
            if !self.global {
                return;
            }
            HOLDS_SLOT.with(|h| h.set(false));
            let mut live = LIVE.lock().unwrap();
            *live -= 1;
            RELEASED.notify_one();
        }
    }
}

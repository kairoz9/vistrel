//! 编码路径的存活信号与阶段码。
//!
//! 两个独立信号，按引擎各持一份，互不依赖：
//! - [`Heartbeat`]：编码循环的存活信号，循环每轮 +1，JNI 快照只读；
//! - [`StageCell`]：阻塞点的诊断标签，编码循环与硬件后端写入，JNI 诊断只读。

use std::sync::atomic::{AtomicU64, Ordering};

/// 编码循环的存活信号：每轮循环 +1，冻结即判定编码线程停止迭代。
pub(crate) struct Heartbeat(AtomicU64);

impl Heartbeat {
    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// 编码循环每轮开始时 +1
    pub(crate) fn bump(&self) {
        self.0.fetch_add(1, Ordering::Release);
    }

    /// 当前计数：游戏侧按相等比较判定冻结
    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// 编码路径当前阻塞的阶段码，心跳冻结时由游戏侧读回定位卡点。
///
/// 由编码循环与硬件后端共同写入：前者写生命周期阶段，后者写编码阻塞点。
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EncodeStage {
    /// 主循环等待输入
    Idle = 0,
    /// 重配或重建编码器
    Reconfigure = 1,
    /// 排空编码器与 CUDA 上下文
    Drain = 2,
    /// 导入显存槽位
    ImportSlots = 3,
    /// 注销显存槽位
    ClearSlots = 4,
    /// 关停编码线程
    Shutdown = 5,
    /// 等待外部时间线信号量
    WaitTimeline = 6,
    /// 映射输入资源
    MapInput = 7,
    /// 提交编码
    EncodeSubmit = 8,
    /// 锁定码流
    LockBitstream = 9,
    /// 帧编码（软编与未细分阶段的后端）
    EncodeFrame = 10,
}

impl EncodeStage {
    /// 阶段码转诊断标签；表外数值原样展示
    pub(crate) fn describe(code: u64) -> String {
        let label = match code {
            0 => "空闲等待",
            1 => "重配编码器",
            2 => "排空编码器",
            3 => "导入显存槽位",
            4 => "注销显存槽位",
            5 => "编码器关停",
            6 => "等待时间线信号量",
            7 => "映射输入资源",
            8 => "提交编码",
            9 => "锁定码流",
            10 => "帧编码",
            _ => return format!("未知阶段({code})"),
        };
        label.to_string()
    }
}

/// 阶段码单元：按引擎持有，编码循环写生命周期阶段，硬件后端写编码阻塞点。
pub(crate) struct StageCell(AtomicU64);

impl StageCell {
    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(EncodeStage::Idle as u64))
    }

    /// 标记进入某阶段：只在可能长时间阻塞的调用前后写
    pub(crate) fn set(&self, stage: EncodeStage) {
        self.0.store(stage as u64, Ordering::Release);
    }

    /// 当前阶段码：仅供 JNI 诊断读取
    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// 阶段码只在 Native 内使用；JNI 诊断按它取标签
    #[test]
    fn test_stage_codes_and_labels() {
        let stage = StageCell::new();
        assert_eq!(stage.get(), EncodeStage::Idle as u64);

        stage.set(EncodeStage::LockBitstream);
        assert_eq!(stage.get(), EncodeStage::LockBitstream as u64);
        assert_eq!(EncodeStage::describe(0), "空闲等待");
        assert_eq!(EncodeStage::describe(9), "锁定码流");
        assert_eq!(EncodeStage::describe(99), "未知阶段(99)");
        assert_eq!(EncodeStage::Idle as u64, 0);
        assert_eq!(EncodeStage::Reconfigure as u64, 1);
        assert_eq!(EncodeStage::Drain as u64, 2);
        assert_eq!(EncodeStage::ImportSlots as u64, 3);
        assert_eq!(EncodeStage::ClearSlots as u64, 4);
        assert_eq!(EncodeStage::Shutdown as u64, 5);
        assert_eq!(EncodeStage::WaitTimeline as u64, 6);
        assert_eq!(EncodeStage::MapInput as u64, 7);
        assert_eq!(EncodeStage::EncodeSubmit as u64, 8);
        assert_eq!(EncodeStage::LockBitstream as u64, 9);
        assert_eq!(EncodeStage::EncodeFrame as u64, 10);
    }

    proptest! {
        // 心跳计数与 bump 次数严格一致
        #[test]
        fn heartbeat_counts_exactly_one_per_bump(bumps in 0u64..2048) {
            let heartbeat = Heartbeat::new();
            for _ in 0..bumps {
                heartbeat.bump();
            }
            prop_assert_eq!(heartbeat.get(), bumps);
        }
    }
}

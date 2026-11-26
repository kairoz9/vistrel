//! Java 数据面线协议：`vramSlotState` 的 packed 布局与 `pushVramSlot` 状态码。
//!
//! 与 Java 侧 `NativeWireContract` 一一对应。

use crate::encode::control::ControlStatus;

/// 在途掩码占用的低位数
const IN_FLIGHT_BITS: u32 = 8;
/// 心跳字段的位偏移
const HEARTBEAT_SHIFT: u32 = 8;
/// 状态字节的位偏移
const STATUS_SHIFT: u32 = 56;
/// 心跳字段掩码（32 位）
const HEARTBEAT_MASK: u64 = 0xFFFF_FFFF;
/// 在途掩码
const IN_FLIGHT_MASK: u64 = (1 << IN_FLIGHT_BITS) - 1;

/// 状态码 + 1：JNI 包装在 panic 时返回 0；若 `Ok` 编码为 0，一次异常会被 Java
/// 读成「成功且全忙」，槽位永在途。0 必须落在未知/故障一侧。
pub(crate) fn pack_status(status: ControlStatus) -> i32 {
    status.code() + 1
}

/// 把槽位状态、在途掩码与心跳打成一个 `u64`（`vramSlotState` 快照专用）。
pub(crate) fn pack_slot_state(status: ControlStatus, in_flight: u64, heartbeat: u64) -> u64 {
    ((pack_status(status) as u64) << STATUS_SHIFT)
        | ((heartbeat & HEARTBEAT_MASK) << HEARTBEAT_SHIFT)
        | (in_flight & IN_FLIGHT_MASK)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// push 回包只有状态码，不携带掩码与心跳
    #[test]
    fn test_push_status_is_code_plus_one() {
        assert_eq!(pack_status(ControlStatus::Ok), 1);
        assert_eq!(pack_status(ControlStatus::Poisoned), 7);
    }

    /// 固定金标准锁住位布局：状态高字节、心跳 32 位、在途掩码低 8 位
    #[test]
    fn test_packed_slot_state_golden_layout() {
        let packed = pack_slot_state(ControlStatus::Ok, 0b101, 0x1234_5678);
        assert_eq!(packed, 0x0100_0012_3456_7805);

        // 0 是 JNI 异常的默认值；正常状态必须落在高字节且非零
        assert_eq!(
            pack_slot_state(ControlStatus::ActorDead, 0, 0) >> STATUS_SHIFT,
            (ControlStatus::ActorDead.code() + 1) as u64
        );
    }

    /// 各字段按宽度截断，保留区保持为零
    #[test]
    fn test_packed_slot_state_masks_fields() {
        let packed = pack_slot_state(ControlStatus::VramFault, u64::MAX, u64::MAX);
        assert_eq!(packed & IN_FLIGHT_MASK, IN_FLIGHT_MASK);
        assert_eq!((packed >> HEARTBEAT_SHIFT) & HEARTBEAT_MASK, HEARTBEAT_MASK);
        assert_eq!((packed >> 40) & 0xFFFF, 0, "保留区必须为零");
    }
}

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const OPCODE_MOUSE_MOVE: u8 = 0x01;
const OPCODE_MOUSE_BUTTON: u8 = 0x02;
const OPCODE_KEY_EVENT: u8 = 0x03;
const OPCODE_MOUSE_SCROLL: u8 = 0x04;
const OPCODE_ABSOLUTE_MOUSE_MOVE: u8 = 0x05;

const MAX_WIRE_EVENT_SIZE: usize = 6;
const RING_CAPACITY: usize = 2048;
const RING_MASK: usize = RING_CAPACITY - 1;

/// 返回指定 opcode 的法定载荷长度（包含 opcode 自身）。
/// 若 opcode 未知则返回 None。
const fn wire_size(opcode: u8) -> Option<usize> {
    match opcode {
        OPCODE_MOUSE_MOVE => Some(5),          // [0x01, dx: i16, dy: i16]
        OPCODE_MOUSE_BUTTON => Some(3),        // [0x02, btn: u8, action: u8]
        OPCODE_KEY_EVENT => Some(6),           // [0x03, code: i32, action: u8]
        OPCODE_MOUSE_SCROLL => Some(2),        // [0x04, sy: i8]
        OPCODE_ABSOLUTE_MOUSE_MOVE => Some(5), // [0x05, x: u16, y: u16]
        _ => None,
    }
}

/// 将原生切片打包为 64-bit 硬件原子整数表示
/// 布局: byte 0 为有效长度 len, byte 1..=len 为实际载荷字节
#[inline(always)]
fn pack_slice(slice: &[u8]) -> u64 {
    let mut raw = [0u8; 8];
    raw[0] = slice.len() as u8;
    raw[1..1 + slice.len()].copy_from_slice(slice);
    u64::from_le_bytes(raw)
}

/// 将 64-bit 原子整数解包回字节切片，返回有效长度
#[inline(always)]
fn unpack_to_slice(packed: u64, out: &mut [u8]) -> usize {
    let raw = packed.to_le_bytes();
    let len = (raw[0] as usize).min(MAX_WIRE_EVENT_SIZE);
    out[..len].copy_from_slice(&raw[1..1 + len]);
    len
}

/// 尝试对已打包的 MouseMove 事件执行增量位移累加 (saturating_add)
#[inline(always)]
fn try_coalesce_mouse_move(packed: u64, dx_new: i16, dy_new: i16) -> Option<u64> {
    let mut raw = packed.to_le_bytes();
    // 验证必须是法定 MouseMove 事件 (len == 5 且 opcode == 0x01)
    if raw[0] != 5 || raw[1] != OPCODE_MOUSE_MOVE {
        return None;
    }
    let dx_old = i16::from_be_bytes([raw[2], raw[3]]);
    let dy_old = i16::from_be_bytes([raw[4], raw[5]]);

    let dx_sum = dx_old.saturating_add(dx_new);
    let dy_sum = dy_old.saturating_add(dy_new);

    let dx_bytes = dx_sum.to_be_bytes();
    let dy_bytes = dy_sum.to_be_bytes();

    raw[2] = dx_bytes[0];
    raw[3] = dx_bytes[1];
    raw[4] = dy_bytes[0];
    raw[5] = dy_bytes[1];

    Some(u64::from_le_bytes(raw))
}

/// 64 字节对齐填充单元
///
/// SPSC 环的 head（消费者写）与 tail（生产者写）若同处一条 cache line，每个事件都要
/// 把该行在线程间弹跳一次；各自独立成行后，跨线程通信只剩槽位数组本身
#[repr(align(64))]
struct CachePadded<T>(T);

impl<T> CachePadded<T> {
    const fn new(value: T) -> Self {
        Self(value)
    }
}

/// 无锁单 SPSC 原始二进制输入事件环形缓冲
pub(crate) struct InputRingBuffer {
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    slots: [AtomicU64; RING_CAPACITY],
}

impl Default for InputRingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl InputRingBuffer {
    pub const fn new() -> Self {
        Self {
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
            slots: [const { AtomicU64::new(0) }; RING_CAPACITY],
        }
    }
    /// 校验二进制输入帧并直接存入 SPSC 环形缓冲
    /// 对连续相对位移执行 O(1) 原地原子饱和折叠
    pub fn parse_and_push(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        let Some(expected_len) = wire_size(bytes[0]) else {
            log::warn!("Ignored Input Frame with unknown opcode: {:#x}", bytes[0]);
            return false;
        };
        if bytes.len() < expected_len {
            log::warn!(
                "Ignored truncated Input Frame for opcode {:#x}: expected {} bytes, got {}",
                bytes[0],
                expected_len,
                bytes.len()
            );
            return false;
        }

        let slice = &bytes[..expected_len];

        // 位移事件尝试折叠进前驱槽：前驱类型由槽内容自证（try_coalesce 校验
        // len 与 opcode），前驱已被读走（swap(0)）则 CAS 失败安全降级
        if slice[0] == OPCODE_MOUSE_MOVE {
            let tail = self.tail.0.load(Ordering::Relaxed);
            let head = self.head.0.load(Ordering::Acquire);

            // 仅当队列非空时，前驱槽位才可能尚未被渲染线程读走
            if tail.wrapping_sub(head) > 0 {
                let prev_idx = tail.wrapping_sub(1) & RING_MASK;
                let dx_new = i16::from_be_bytes([slice[1], slice[2]]);
                let dy_new = i16::from_be_bytes([slice[3], slice[4]]);

                let old_val = self.slots[prev_idx].load(Ordering::Acquire);
                if let Some(new_val) = try_coalesce_mouse_move(old_val, dx_new, dy_new)
                    && self.slots[prev_idx]
                        .compare_exchange(old_val, new_val, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    // 成功折叠：前驱槽原地累加，不消耗新槽位
                    return true;
                }
            }
        }

        // 普通入队 (Push New Slot)
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);

        // SPSC 不变量，写者绝不修改 head；满队列时仅丢弃新事件 (Drop-Newest)
        if tail.wrapping_sub(head) >= RING_CAPACITY {
            log::warn!(
                "Input RingBuffer capacity saturated ({}), dropping input event",
                RING_CAPACITY
            );
            return false;
        }

        let packed = pack_slice(slice);
        let idx = tail & RING_MASK;
        self.slots[idx].store(packed, Ordering::Release);

        // 推进 tail，正式向消费端发布该槽位
        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// 批量提取所有待处理原始输入事件切片写入共享的直接内存缓冲区
    /// 仅由单消费者（Minecraft 渲染主线程）调用
    pub fn drain_to_buffer(&self, out_buf: &mut [u8], max_events: usize) -> usize {
        let mut count = 0;
        let mut offset = 0;

        let tail = self.tail.0.load(Ordering::Acquire);
        let mut head = self.head.0.load(Ordering::Relaxed);

        while count < max_events && head != tail {
            let idx = head & RING_MASK;
            let peek_packed = self.slots[idx].load(Ordering::Acquire);
            let peek_len = (peek_packed.to_le_bytes()[0] as usize).min(MAX_WIRE_EVENT_SIZE);

            if peek_len == 0 || offset + peek_len > out_buf.len() {
                break;
            }

            // 确认缓冲区能够容纳后，原子取走并置空槽位，声明消费完成并阻断并发写者 CAS
            let packed = self.slots[idx].swap(0, Ordering::AcqRel);
            let mut temp = [0u8; MAX_WIRE_EVENT_SIZE];
            let event_len = unpack_to_slice(packed, &mut temp);

            if event_len == 0 {
                break;
            }

            out_buf[offset..offset + event_len].copy_from_slice(&temp[..event_len]);
            offset += event_len;
            count += 1;
            head = head.wrapping_add(1);
        }

        if count > 0 {
            self.head.0.store(head, Ordering::Release);
        }

        count
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Acquire);
        let tail = self.tail.0.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    /// 清空全部残留事件（仅测试用：生产路径上环随会话生灭，无清环概念）
    #[cfg(test)]
    pub fn clear(&self) {
        let tail = self.tail.0.load(Ordering::Relaxed);
        self.head.0.store(tail, Ordering::Release);
        for slot in &self.slots {
            slot.store(0, Ordering::Relaxed);
        }
    }
}

/// 仅供测试：构造合法的鼠标位移事件帧
#[cfg(test)]
pub(crate) fn test_motion_event(dx: i16, dy: i16) -> [u8; 5] {
    let mut frame = [0u8; 5];
    frame[0] = OPCODE_MOUSE_MOVE;
    frame[1..3].copy_from_slice(&dx.to_be_bytes());
    frame[3..5].copy_from_slice(&dy.to_be_bytes());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    /// head（消费者写）与 tail（生产者写）必须分处不同 cache line：
    /// 伪共享会让每个输入事件在线程间弹跳一次控制行，这条断言防止未来重构悄悄退回去
    #[test]
    fn test_ring_control_fields_are_cache_line_separated() {
        let head = std::mem::offset_of!(InputRingBuffer, head);
        let tail = std::mem::offset_of!(InputRingBuffer, tail);
        assert!(
            tail - head >= 64,
            "head @{head} / tail @{tail} 必须各自独占 64B 行"
        );
    }

    #[test]
    fn test_opcode_wire_sizes() {
        assert_eq!(wire_size(OPCODE_MOUSE_MOVE), Some(5));
        assert_eq!(wire_size(OPCODE_MOUSE_BUTTON), Some(3));
        assert_eq!(wire_size(OPCODE_KEY_EVENT), Some(6));
        assert_eq!(wire_size(OPCODE_MOUSE_SCROLL), Some(2));
        assert_eq!(wire_size(OPCODE_ABSOLUTE_MOUSE_MOVE), Some(5));
        assert_eq!(wire_size(0x00), None);
        assert_eq!(wire_size(0x06), None);
        assert_eq!(wire_size(0xFF), None);
    }

    #[test]
    fn test_parse_and_drain_all_5_types() {
        let queue = InputRingBuffer::new();

        let mouse_move = [0x01, 0x00, 0x0A, 0xFF, 0xF6]; // dx = 10, dy = -10
        let mouse_btn = [0x02, 0x01, 0x01]; // btn = 1, press
        let key_evt = [0x03, 0x00, 0x00, 0x00, 0x41, 0x02]; // key = 65, repeat
        let scroll = [0x04, 0x01]; // sy = 1
        let abs_move = [0x05, 0x80, 0x00, 0x40, 0x00]; // x = 32768, y = 16384

        assert!(queue.parse_and_push(&mouse_move));
        assert!(queue.parse_and_push(&mouse_btn));
        assert!(queue.parse_and_push(&key_evt));
        assert!(queue.parse_and_push(&scroll));
        assert!(queue.parse_and_push(&abs_move));

        let mut out = [0u8; 64];
        let drained = queue.drain_to_buffer(&mut out, 10);
        assert_eq!(drained, 5);

        let mut expected = Vec::new();
        expected.extend_from_slice(&mouse_move);
        expected.extend_from_slice(&mouse_btn);
        expected.extend_from_slice(&key_evt);
        expected.extend_from_slice(&scroll);
        expected.extend_from_slice(&abs_move);

        assert_eq!(&out[..expected.len()], &expected[..]);
    }

    #[test]
    fn test_truncated_input_is_ignored() {
        let queue = InputRingBuffer::new();

        // 0x01 requires 5 bytes, provide only 4
        assert!(!queue.parse_and_push(&[0x01, 0x00, 0x01, 0x00]));
        // 0x03 requires 6 bytes, provide only 5
        assert!(!queue.parse_and_push(&[0x03, 0x00, 0x00, 0x00, 0x41]));

        let mut out = [0u8; 32];
        assert_eq!(queue.drain_to_buffer(&mut out, 10), 0);
    }

    #[test]
    fn test_unknown_opcode_is_ignored() {
        let queue = InputRingBuffer::new();

        assert!(!queue.parse_and_push(&[0x99, 0x01, 0x02, 0x03]));
        let mut out = [0u8; 32];
        assert_eq!(queue.drain_to_buffer(&mut out, 10), 0);
    }

    #[test]
    fn test_drain_buffer_bounds_protection() {
        let queue = InputRingBuffer::new();

        let evt1 = [0x03, 0x00, 0x00, 0x00, 0x1E, 0x01]; // 6 bytes
        let evt2 = [0x03, 0x00, 0x00, 0x00, 0x1F, 0x01]; // 6 bytes
        assert!(queue.parse_and_push(&evt1));
        assert!(queue.parse_and_push(&evt2));

        // Buffer only fits 10 bytes: can fit evt1 (6 bytes), but cannot fit evt2 (6 bytes)
        let mut out = [0u8; 10];
        let drained = queue.drain_to_buffer(&mut out, 10);
        assert_eq!(drained, 1);
        assert_eq!(&out[..6], &evt1);

        // Second drain gets evt2
        let mut out2 = [0u8; 10];
        let drained2 = queue.drain_to_buffer(&mut out2, 10);
        assert_eq!(drained2, 1);
        assert_eq!(&out2[..6], &evt2);
    }

    #[test]
    fn test_queue_capacity_limit_drop_newest() {
        let queue = InputRingBuffer::new();

        let evt = [0x04, 0x01]; // 2 bytes (Scroll)
        for _ in 0..RING_CAPACITY {
            assert!(queue.parse_and_push(&evt));
        }

        assert_eq!(queue.len(), RING_CAPACITY);
        // 隐患 1 验证: 队列满时 Drop-Newest 绝不推进 head，写者返回 false
        assert!(!queue.parse_and_push(&evt));
        assert_eq!(queue.len(), RING_CAPACITY);
    }

    #[test]
    fn test_wire_format_contract_parity() {
        let cases: &[(&str, u8, &[u8])] = &[
            (
                "MouseMove",
                OPCODE_MOUSE_MOVE,
                &[0x01, 0x00, 0x0A, 0xFF, 0xF6],
            ),
            ("MouseButton", OPCODE_MOUSE_BUTTON, &[0x02, 0x01, 0x01]),
            (
                "KeyEvent",
                OPCODE_KEY_EVENT,
                &[0x03, 0x00, 0x00, 0x00, 0x1A, 0x01],
            ),
            ("MouseScroll", OPCODE_MOUSE_SCROLL, &[0x04, 0x01]),
            (
                "AbsoluteMouseMove",
                OPCODE_ABSOLUTE_MOUSE_MOVE,
                &[0x05, 0x80, 0x00, 0x40, 0x00],
            ),
        ];

        for (name, opcode, bytes) in cases {
            assert_eq!(
                wire_size(*opcode),
                Some(bytes.len()),
                "wire_size parity for {}",
                name
            );
            let queue = InputRingBuffer::new();
            assert!(queue.parse_and_push(bytes), "parse_and_push for {}", name);
            assert_eq!(queue.len(), 1);

            let mut out = [0u8; 16];
            let drained = queue.drain_to_buffer(&mut out, 1);
            assert_eq!(drained, 1);
            assert_eq!(&out[..bytes.len()], *bytes);
        }
    }

    #[test]
    fn test_motion_coalescing_accumulation_and_saturation() {
        let queue = InputRingBuffer::new();

        // 连续 3 次鼠标移动
        let move1 = [0x01, 0x00, 0x05, 0xFF, 0xFB]; // dx = 5, dy = -5
        let move2 = [0x01, 0x00, 0x07, 0x00, 0x0A]; // dx = 7, dy = 10
        let move3 = [0x01, 0xFF, 0xFE, 0x00, 0x02]; // dx = -2, dy = 2

        assert!(queue.parse_and_push(&move1));
        assert_eq!(queue.len(), 1);

        assert!(queue.parse_and_push(&move2));
        // 队列深度保持 1，完成原地折叠
        assert_eq!(queue.len(), 1);

        assert!(queue.parse_and_push(&move3));
        assert_eq!(queue.len(), 1);

        let mut out = [0u8; 16];
        let drained = queue.drain_to_buffer(&mut out, 10);
        assert_eq!(drained, 1);

        // 期望累加值: dx = 5 + 7 + (-2) = 10, dy = -5 + 10 + 2 = 7
        assert_eq!(out[0], OPCODE_MOUSE_MOVE);
        let dx = i16::from_be_bytes([out[1], out[2]]);
        let dy = i16::from_be_bytes([out[3], out[4]]);
        assert_eq!(dx, 10);
        assert_eq!(dy, 7);

        // 饱和累加验证 (防止溢出)
        queue.clear();
        let move_near_max = [0x01, 0x7F, 0xFF, 0x00, 0x00]; // dx = 32767 (i16::MAX)
        let move_add_more = [0x01, 0x00, 0x10, 0x00, 0x00]; // dx = 16
        assert!(queue.parse_and_push(&move_near_max));
        assert!(queue.parse_and_push(&move_add_more));

        let mut out2 = [0u8; 16];
        queue.drain_to_buffer(&mut out2, 1);
        let sat_dx = i16::from_be_bytes([out2[1], out2[2]]);
        assert_eq!(sat_dx, i16::MAX);
    }

    /// 引擎重启清环的契约（生产调用点 start_engine）：clear 后环必须为空，
    /// 且清空后照常接收并按序取出新事件
    #[test]
    fn test_clear_empties_ring_and_accepts_new_events() {
        let queue = InputRingBuffer::new();
        assert!(queue.parse_and_push(&[0x02, 0x01, 0x01]));
        assert!(queue.parse_and_push(&[0x01, 0x00, 0x0A, 0x00, 0x00]));
        assert_eq!(queue.len(), 2);

        queue.clear();
        assert_eq!(queue.len(), 0);
        let mut out = [0u8; 16];
        assert_eq!(queue.drain_to_buffer(&mut out, 10), 0);

        assert!(queue.parse_and_push(&[0x01, 0x00, 0x03, 0x00, 0x00]));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.drain_to_buffer(&mut out, 10), 1);
        assert_eq!(&out[0..5], &[0x01, 0x00, 0x03, 0x00, 0x00]);
    }

    #[test]
    fn test_temporal_isolation_across_non_motion_events() {
        // 隐患 2 验证: move1 -> click -> move2 绝不能跨 click 合并
        let queue = InputRingBuffer::new();

        let move1 = [0x01, 0x00, 0x0A, 0x00, 0x00]; // dx = 10
        let click = [0x02, 0x01, 0x01]; // MouseButton left press
        let move2 = [0x01, 0x00, 0x05, 0x00, 0x00]; // dx = 5

        assert!(queue.parse_and_push(&move1));
        assert!(queue.parse_and_push(&click));
        assert!(queue.parse_and_push(&move2));

        // 队列长度必须为 3，click 严格打断折叠链条
        assert_eq!(queue.len(), 3);

        let mut out = [0u8; 32];
        let drained = queue.drain_to_buffer(&mut out, 10);
        assert_eq!(drained, 3);

        // 依次比对顺序与载荷
        assert_eq!(&out[0..5], &move1);
        assert_eq!(&out[5..8], &click);
        assert_eq!(&out[8..13], &move2);
    }

    #[test]
    fn test_concurrent_coalesce_no_motion_lost() {
        use std::sync::Arc;
        use std::thread;

        let queue = Arc::new(InputRingBuffer::new());
        let q_producer = Arc::clone(&queue);
        let q_consumer = Arc::clone(&queue);

        // 折叠链长的上界即事件总数：必须低于 i16::MAX (32767)，否则消费者被调度饿死时
        // 单条折叠链会触发 saturating_add 饱和钳制（有意设计，见
        // test_motion_coalescing_accumulation_and_saturation），位移守恒断言不再无条件成立
        const TOTAL_EVENTS: i32 = 30_000;

        let producer_handle = thread::spawn(move || {
            let move_evt = [0x01, 0x00, 0x01, 0x00, 0x01]; // dx = 1, dy = 1
            for _ in 0..TOTAL_EVENTS {
                while !q_producer.parse_and_push(&move_evt) {
                    thread::yield_now();
                }
            }
        });

        let consumer_handle = thread::spawn(move || {
            let mut total_dx = 0i64;
            let mut out_buf = [0u8; 1024];
            let mut received_any = true;

            while received_any || !producer_handle.is_finished() {
                let drained = q_consumer.drain_to_buffer(&mut out_buf, 128);
                if drained == 0 {
                    received_any = false;
                    thread::yield_now();
                    continue;
                }
                received_any = true;
                let mut offset = 0;
                for _ in 0..drained {
                    if out_buf[offset] == OPCODE_MOUSE_MOVE {
                        let dx = i16::from_be_bytes([out_buf[offset + 1], out_buf[offset + 2]]);
                        total_dx += dx as i64;
                        offset += 5;
                    }
                }
            }
            producer_handle.join().unwrap();

            // 彻底清空剩余队列
            loop {
                let drained = q_consumer.drain_to_buffer(&mut out_buf, 128);
                if drained == 0 {
                    break;
                }
                let mut offset = 0;
                for _ in 0..drained {
                    if out_buf[offset] == OPCODE_MOUSE_MOVE {
                        let dx = i16::from_be_bytes([out_buf[offset + 1], out_buf[offset + 2]]);
                        total_dx += dx as i64;
                        offset += 5;
                    }
                }
            }
            total_dx
        });

        let total_dx = consumer_handle.join().unwrap();
        assert_eq!(
            total_dx, TOTAL_EVENTS as i64,
            "Motion coalescing lost events! Expected {}, got {}",
            TOTAL_EVENTS, total_dx
        );
    }
}

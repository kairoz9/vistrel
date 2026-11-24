//! 显存直通槽位的跨平台公共类型：
//! - 导入协议：7 元组描述符解析与句柄生命周期管理
//! - `SlotTable<S>`：硬件编码后端共用的槽位事务容器
//! - `SlotStateBlock`：游戏侧与编码 Actor 之间的槽位所有权共享块

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// SAFETY: CloseHandle 要求传入有效内核句柄且不得重复关闭，
// 本文件内的调用点均通过一次性 take 守卫保证句柄只关一次。
#[cfg(windows)]
unsafe extern "system" {
    fn CloseHandle(h_object: *mut std::ffi::c_void) -> i32;
}

/// 协议中每个槽位占用的 `long` (i64) 数。
pub const FLAT_STRIDE: usize = 7;

/// 待导入 VRAM 槽位的描述符。
#[derive(Debug)]
pub struct VramSlotDescriptor {
    /// 槽位索引（多槽位循环复用）
    pub slot_idx: u32,
    /// 外部显存通用句柄 (Linux: PRIME dma-buf POSIX fd, Windows: Win32 Opaque HANDLE)
    pub mem_handle: i64,
    /// 时间线信号量通用句柄 (Linux: Semaphore POSIX fd, Windows: Win32 Opaque HANDLE)
    pub sem_handle: i64,
    /// 纹理宽度（像素）
    pub width: u32,
    /// 纹理高度（像素）
    pub height: u32,
    /// 外部显存总大小（字节）
    pub size_bytes: usize,
    /// 纹理在外部显存中的字节偏移
    pub offset: u64,
    /// 句柄是否已被导入路径接管。导入路径无论成败都会释放句柄，进入导入
    /// 即置位；只有从未进入导入的槽位才落到 `Drop` 的关闭分支
    pub handles_taken: bool,
}

impl VramSlotDescriptor {
    /// 从协议的 7 元组还原描述符。字段顺序即协议顺序：
    /// `slot_idx, mem_handle, sem_handle, width, height, size_bytes, offset`
    pub fn from_flat(chunk: &[i64]) -> Self {
        debug_assert_eq!(chunk.len(), FLAT_STRIDE, "调用方须用 chunks_exact 切分");
        Self {
            slot_idx: chunk[0] as u32,
            mem_handle: chunk[1],
            sem_handle: chunk[2],
            width: chunk[3] as u32,
            height: chunk[4] as u32,
            size_bytes: chunk[5] as usize,
            offset: chunk[6] as u64,
            handles_taken: false,
        }
    }

    /// 解析协议的整批槽位。长度为零或不是 `FLAT_STRIDE` 的整数倍时返回
    /// `None`——协议一旦错位就整批拒绝，绝不按错位布局解析
    pub fn parse_batch(flat: &[i64]) -> Option<Vec<Self>> {
        if flat.is_empty() || !flat.len().is_multiple_of(FLAT_STRIDE) {
            return None;
        }
        Some(
            flat.as_chunks::<FLAT_STRIDE>()
                .0
                .iter()
                .map(|c| Self::from_flat(c))
                .collect(),
        )
    }

    /// 提取原始显存与信号量句柄，并将本描述符标记为已接管，避免在 Drop 时重复关闭
    pub fn take_handles(&mut self) -> (i64, i64) {
        self.handles_taken = true;
        (self.mem_handle, self.sem_handle)
    }
}

/// 跨平台安全关闭原始系统句柄 (POSIX fd 或 Win32 HANDLE)
///
/// # Safety
///
/// 调用方必须保证 `handle` 是所有权仍在调用方的未关闭原生句柄（fd 或 Win32
/// HANDLE），不得用它关闭已移交驱动接管的句柄；正值守卫只拦截 0/负值，
/// 不校验句柄是否仍然有效
#[inline]
pub(crate) unsafe fn close_raw_handle(handle: i64) {
    if handle <= 0 {
        return;
    }
    #[cfg(unix)]
    {
        // SAFETY: close 为 async-signal-safe 系统调用，作用于传入的有效正整数 fd
        unsafe {
            libc::close(handle as i32);
        }
    }
    #[cfg(windows)]
    {
        // SAFETY: CloseHandle 为 Win32 系统调用，作用于传入的非空 Win32 句柄
        unsafe {
            CloseHandle(handle as *mut std::ffi::c_void);
        }
    }
}

impl Drop for VramSlotDescriptor {
    fn drop(&mut self) {
        if self.handles_taken {
            return;
        }
        // SAFETY: handles_taken 是唯一的关闭依据——未被导入路径接管的句柄在此安全关闭，
        // close_raw_handle 保证仅正整数有效句柄被系统调用关闭一次，避免重复释放与句柄泄漏
        unsafe {
            close_raw_handle(self.mem_handle);
            close_raw_handle(self.sem_handle);
        }
    }
}

/// 显存槽位导入的公共不变量：整批纹理几何必须是编码尺寸或采集尺寸
///
/// 任何状态变更之前整批拒绝，让尺寸不符在注册期就以逐槽可诊断的错误拦下，而不是
/// 等到逐帧编码时才失败。
///
/// `display` 为 `None` 表示本后端无对齐（NVENC / 软编），只收编码尺寸。有对齐时也收
/// 采集尺寸：槽位由游戏侧按采集尺寸建，1080 → 1088 的补齐由 CSC 着色器夹取完成。
pub(crate) fn ensure_batch_geometry(
    descriptors: &[VramSlotDescriptor],
    coded: (u32, u32),
    display: Option<(u32, u32)>,
) -> Result<(), String> {
    if let Some(desc) = descriptors.iter().find(|d| {
        let geometry = (d.width, d.height);
        geometry != coded && Some(geometry) != display
    }) {
        let expected = match display {
            Some((display_w, display_h)) => format!(
                "不是编码尺寸 {}x{} 也不是采集尺寸 {display_w}x{display_h}：显存直通不接受其他几何",
                coded.0, coded.1
            ),
            None => format!(
                "与编码尺寸 {}x{} 不一致：该后端无对齐，只接受编码尺寸",
                coded.0, coded.1
            ),
        };
        return Err(format!(
            "槽位 {} 尺寸 {}x{}{expected}",
            desc.slot_idx, desc.width, desc.height
        ));
    }
    Ok(())
}

/// 专为显存槽位设计的轻量事务容器 (All-or-Nothing Transactional Registry)
///
/// 槽位索引是连续的小整数，直接按下标存进 `Vec<Option<S>>`：省掉 HashMap 的哈希开销，
/// 且 `iter` / `values_mut` 天然按索引升序（`pub(crate)` 容器上的顺序稳定性对上层无影响）
pub(crate) struct SlotTable<S> {
    slots: Vec<Option<S>>,
}

/// 槽位索引的协议上限：索引来自游戏侧的环形槽位号（`0 <= slotIdx < SLOT_COUNT`），
/// 越界即视为协议损坏并整批拒绝，避免按不可信的索引做无界分配
const MAX_SLOTS: usize = 256;

impl<S> Default for SlotTable<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> SlotTable<S> {
    pub(crate) fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// 批量原子导入：高阶函数注入硬件专有导入纯函数。
    /// - 任一失败：中止导入，已成功放进 staging 的 Slot 随作用域丢弃（RAII 释放硬件资源），
    ///   未进入导入的描述符由 Drop 关闭系统句柄，整体安全回滚。
    /// - 全部成功：无缝原子提交，原有旧槽位就地 Drop 释放。
    pub(crate) fn import_all<F>(
        &mut self,
        descriptors: &mut [VramSlotDescriptor],
        mut import_fn: F,
    ) -> Result<(), String>
    where
        F: FnMut(&mut VramSlotDescriptor) -> Result<S, String>,
    {
        if descriptors.is_empty() {
            return Err("待导入显存描述符列表为空".to_string());
        }

        let max_idx = descriptors
            .iter()
            .map(|d| d.slot_idx as usize)
            .max()
            .unwrap_or(0);
        if max_idx >= MAX_SLOTS {
            return Err(format!("槽位索引 {max_idx} 超出协议上限 {MAX_SLOTS}"));
        }
        let mut staging: Vec<Option<S>> = (0..=max_idx).map(|_| None).collect();
        for desc in descriptors.iter_mut() {
            let slot = import_fn(desc)?;
            // 标记已被驱动接管，避免在描述符 Drop 时重复关闭 fd/HANDLE
            desc.take_handles();
            staging[desc.slot_idx as usize] = Some(slot);
        }

        // 成功导入：替换原有槽位，旧槽位就地 Drop 释放驱动资源
        self.slots = staging;
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn get(&self, slot_idx: u32) -> Option<&S> {
        self.slots.get(slot_idx as usize).and_then(|s| s.as_ref())
    }

    /// 按索引取可变引用：槽位本体之外的旁路状态（如 NVENC 注册句柄）需要按索引回写
    #[inline(always)]
    pub(crate) fn get_mut(&mut self, slot_idx: u32) -> Option<&mut S> {
        self.slots
            .get_mut(slot_idx as usize)
            .and_then(|s| s.as_mut())
    }

    /// 按槽位索引遍历，键必须可见
    #[inline(always)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u32, &S)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| slot.as_ref().map(|slot| (idx as u32, slot)))
    }

    #[inline(always)]
    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut S> {
        self.slots.iter_mut().flatten()
    }

    pub(crate) fn clear(&mut self) {
        self.slots.clear();
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.iter().all(|slot| slot.is_none())
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }
}

/// 共享块容纳的槽位上限：与游戏侧掩码容量同一条约定，改动必须两端同步
pub(crate) const SLOT_STATE_SLOTS: usize = 8;

/// 编码 Actor 当前阻塞的阶段码，心跳冻结时由游戏侧读回定位卡点
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActorStage {
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

/// 共享块字节数：游戏侧按此尺寸包装直接内存缓冲区，两端必须一致
pub(crate) const SLOT_STATE_BLOCK_BYTES: usize = 64;

/// 游戏侧渲染线程与编码 Actor 之间共享的槽位所有权块：
/// 游戏侧认领槽位写 1，Actor 归还写 0（均为 CAS），心跳供游戏侧做停滞判定。
/// 64 字节单缓存行（按 64 字节对齐），经直接内存缓冲区暴露给游戏侧，访问全部走 acquire/release。
///
/// 布局是跨语言约定（游戏侧按原生端序按偏移访问）：
/// `[0..32) = busy[8]`、`[32..40) = heartbeat`、`[40..48) = stage`、
/// `[48..56) = binding_epoch`、`[56..64) = 保留`。
///
/// 块随引擎实例创建并故意泄漏：卡死弃用的旧 Actor 只能写到自己那一代的块上，
/// 碰不到新一代——跨代复用同一块的话，旧线程复活的心跳会让新一代的超时判定失效。
#[repr(C, align(64))]
pub(crate) struct SlotStateBlock {
    /// 1 = 渲染线程已认领且尚未归还；0 = Actor 手中无该槽的帧
    busy: [AtomicU32; SLOT_STATE_SLOTS],
    /// Actor 每轮循环 +1；游戏侧在 push 时快照，判定挂 capture tick 侧
    heartbeat: AtomicU64,
    /// Actor 当前阻塞的阶段码：心跳冻结时游戏侧读回写进停滞日志
    stage: AtomicU64,
    /// 显存绑定轮次：重建编码器或槽位失效的重配时 +1。游戏侧导入/推送/注销
    /// 都带上它，Native 据此拒绝旧绑定轮次的槽位操作
    binding_epoch: AtomicU64,
    /// 布局保留段：凑满单缓存行，游戏侧不得访问
    _reserved: [u8; 8],
}

const _: () = assert!(std::mem::size_of::<SlotStateBlock>() == SLOT_STATE_BLOCK_BYTES);
const _: () = assert!(std::mem::align_of::<SlotStateBlock>() == SLOT_STATE_BLOCK_BYTES);

impl SlotStateBlock {
    /// 分配并泄漏一块：泄漏是有界的（64 字节/引擎重建，30s 自愈冷却封顶），
    /// 换来的是被弃线程的写永远与活代隔离
    pub(crate) fn new_leaked() -> &'static Self {
        let block = Box::new(Self {
            busy: std::array::from_fn(|_| AtomicU32::new(0)),
            heartbeat: AtomicU64::new(0),
            stage: AtomicU64::new(ActorStage::Idle as u64),
            binding_epoch: AtomicU64::new(0),
            _reserved: [0; 8],
        });
        Box::leak(block)
    }

    /// 标记 Actor 进入某阶段：只在可能长时间阻塞的调用前后写
    pub(crate) fn set_stage(&self, stage: ActorStage) {
        self.stage.store(stage as u64, Ordering::Release);
    }

    /// 归还槽位（编码完成或帧被丢弃）；越界与重复归还均无害
    ///
    /// CAS 1→0：位是 1 才清零，过期或重复的归还自动无效。
    /// 位属于共享块而非编码器：没有编码器（软编/未就绪）时也必须归还
    pub(crate) fn mark_free(&self, slot_idx: u32) {
        if let Some(cell) = self.busy.get(slot_idx as usize) {
            let _ = cell.compare_exchange(1, 0, Ordering::Release, Ordering::Relaxed);
        }
    }

    /// 换表全清。只允许在编码器 drain 成功之后调用：位承载「GPU 正在读这张纹理」，
    /// 排空前的清零会让渲染线程复写仍被在途提交读取的显存
    pub(crate) fn mark_all_free(&self) {
        for cell in &self.busy {
            cell.store(0, Ordering::Release);
        }
    }

    /// Actor 每轮循环开始时 +1：心跳冻结即编码线程停止迭代
    pub(crate) fn heartbeat_bump(&self) {
        self.heartbeat.fetch_add(1, Ordering::Release);
    }

    /// 当前显存绑定轮次：导入、推送与注销的准入令牌
    pub(crate) fn binding_epoch(&self) -> u64 {
        self.binding_epoch.load(Ordering::Acquire)
    }

    /// 显存绑定轮次前移：旧轮次的槽位导入/推送/注销从此失效
    pub(crate) fn bump_binding_epoch(&self) -> u64 {
        self.binding_epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// 暴露给游戏侧的原始指针（由互操作层包装为直接内存缓冲区）
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self as *const Self as *const u8
    }

    #[cfg(test)]
    pub(crate) fn is_busy(&self, slot_idx: usize) -> bool {
        self.busy[slot_idx].load(Ordering::Acquire) != 0
    }

    /// 测试用：模拟渲染线程的在途认领（生产中位只由游戏侧 CAS 置 1）
    #[cfg(test)]
    pub(crate) fn set_busy(&self, slot_idx: usize) {
        self.busy[slot_idx].store(1, Ordering::Release);
    }

    #[cfg(test)]
    fn heartbeat(&self) -> u64 {
        self.heartbeat.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    struct MockSlot(#[expect(dead_code, reason = "占位构造，字段值不参与断言")] u32);

    #[test]
    fn test_parse_batch_validation() {
        assert!(VramSlotDescriptor::parse_batch(&[]).is_none());
        assert!(VramSlotDescriptor::parse_batch(&[1, 2, 3]).is_none());

        let flat = vec![0, 10, 20, 1920, 1080, 3110400, 0];
        let batch = VramSlotDescriptor::parse_batch(&flat).expect("必须成功解析");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].slot_idx, 0);
        assert_eq!(batch[0].mem_handle, 10);
        assert_eq!(batch[0].sem_handle, 20);
        assert_eq!(batch[0].width, 1920);
        assert_eq!(batch[0].height, 1080);
        assert_eq!(batch[0].size_bytes, 3110400);
        assert_eq!(batch[0].offset, 0);
    }

    fn geo_desc(slot_idx: u32, width: u32, height: u32) -> VramSlotDescriptor {
        VramSlotDescriptor {
            slot_idx,
            mem_handle: -1,
            sem_handle: -1,
            width,
            height,
            size_bytes: 0,
            offset: 0,
            handles_taken: true,
        }
    }

    /// 几何不变量：相符整批放行、任一不符整批拒绝且错误指认首个不符槽位、
    /// 空批放行（空批由调用方的空检查拒绝，不归本函数管）
    #[test]
    fn test_ensure_batch_geometry() {
        let coded = (1920u32, 1088u32);
        let display = (1920u32, 1080u32);
        assert!(ensure_batch_geometry(&[], coded, Some(display)).is_ok());

        // 两种都收：编码尺寸（生产侧知道对齐结果）与采集尺寸（由 CSC 夹取补齐）
        let coded_batch = vec![geo_desc(0, 1920, 1088), geo_desc(1, 1920, 1088)];
        assert!(ensure_batch_geometry(&coded_batch, coded, Some(display)).is_ok());
        let display_batch = vec![geo_desc(0, 1920, 1080), geo_desc(1, 1920, 1080)];
        assert!(ensure_batch_geometry(&display_batch, coded, Some(display)).is_ok());

        // 无对齐的后端（NVENC / 软编）：采集尺寸不再合法
        let err = ensure_batch_geometry(&display_batch, coded, None).unwrap_err();
        assert!(err.contains("1920x1080"), "错误必须报出不符槽位尺寸: {err}");
        assert!(err.contains("1920x1088"), "错误必须报出期望尺寸: {err}");
        assert!(!err.contains("采集尺寸"), "无对齐时不得提及采集尺寸");

        let bad = vec![geo_desc(0, 1920, 1080), geo_desc(2, 1280, 720)];
        let err = ensure_batch_geometry(&bad, coded, Some(display)).unwrap_err();
        assert!(err.contains("槽位 2"), "错误必须指认首个不符槽位: {err}");
        assert!(err.contains("1280x720") && err.contains("1920x1088"));
    }

    #[test]
    fn test_slot_table_all_or_nothing_rollback() {
        let mut table = SlotTable::<MockSlot>::new();
        let mut batch = vec![
            VramSlotDescriptor {
                slot_idx: 0,
                mem_handle: -1,
                sem_handle: -1,
                width: 1920,
                height: 1080,
                size_bytes: 8294400,
                offset: 0,
                handles_taken: true,
            },
            VramSlotDescriptor {
                slot_idx: 1,
                mem_handle: -1,
                sem_handle: -1,
                width: 1920,
                height: 1080,
                size_bytes: 8294400,
                offset: 0,
                handles_taken: true,
            },
        ];

        // 模拟第 2 个槽位失败
        let res = table.import_all(&mut batch, |desc| {
            if desc.slot_idx == 1 {
                Err("导入失败模拟".to_string())
            } else {
                Ok(MockSlot(desc.slot_idx))
            }
        });

        assert!(res.is_err());
        assert!(table.is_empty(), "任一失败必须整批回滚，保持原有表不变");

        // 模拟全部成功
        let res_ok = table.import_all(&mut batch, |desc| Ok(MockSlot(desc.slot_idx)));
        assert!(res_ok.is_ok());
        assert_eq!(table.len(), 2);
        assert!(table.get(0).is_some());
        assert!(table.get(1).is_some());
    }

    #[test]
    fn test_block_layout_is_cross_language_contract() {
        assert_eq!(
            std::mem::size_of::<SlotStateBlock>(),
            SLOT_STATE_BLOCK_BYTES
        );
        assert_eq!(std::mem::size_of::<SlotStateBlock>(), 64);
        assert_eq!(std::mem::offset_of!(SlotStateBlock, heartbeat), 32);
        assert_eq!(std::mem::offset_of!(SlotStateBlock, stage), 40);
    }

    /// 阶段码是跨语言约定：判别值被 Java 侧按下标取名，改动必须两端同步
    #[test]
    fn test_stage_codes_are_cross_language_contract() {
        let block = SlotStateBlock::new_leaked();
        block.set_stage(ActorStage::LockBitstream);
        assert_eq!(
            block.stage.load(Ordering::Acquire),
            ActorStage::LockBitstream as u64
        );
        assert_eq!(ActorStage::Idle as u64, 0);
        assert_eq!(ActorStage::Reconfigure as u64, 1);
        assert_eq!(ActorStage::Drain as u64, 2);
        assert_eq!(ActorStage::ImportSlots as u64, 3);
        assert_eq!(ActorStage::ClearSlots as u64, 4);
        assert_eq!(ActorStage::Shutdown as u64, 5);
        assert_eq!(ActorStage::WaitTimeline as u64, 6);
        assert_eq!(ActorStage::MapInput as u64, 7);
        assert_eq!(ActorStage::EncodeSubmit as u64, 8);
        assert_eq!(ActorStage::LockBitstream as u64, 9);
        assert_eq!(ActorStage::EncodeFrame as u64, 10);
    }

    /// 归还/全清/越界：位状态的全部变化
    #[test]
    fn test_busy_lifecycle() {
        let block = SlotStateBlock::new_leaked();
        assert!(!block.is_busy(0), "新块必须全空闲");

        block.busy[0].store(1, Ordering::Release);
        assert!(block.is_busy(0));
        // 越界归还：不 panic、不影响其余位
        block.mark_free(SLOT_STATE_SLOTS as u32 + 1);
        assert!(block.is_busy(0), "越界归还不得波及有效位");

        block.mark_free(0);
        assert!(!block.is_busy(0), "归还即清零");
        block.mark_free(0);
        assert!(!block.is_busy(0), "重复归还无效（CAS 失败路径）");

        block.busy[2].store(1, Ordering::Release);
        block.busy[SLOT_STATE_SLOTS - 1].store(1, Ordering::Release);
        block.mark_all_free();
        assert!(
            (0..SLOT_STATE_SLOTS).all(|i| !block.is_busy(i)),
            "换代全清后所有位归零"
        );
    }

    /// 心跳只增不减；游戏侧超时判定按相等比较，这里验证递增
    #[test]
    fn test_heartbeat_monotonic() {
        let block = SlotStateBlock::new_leaked();
        assert_eq!(block.heartbeat(), 0);
        block.heartbeat_bump();
        block.heartbeat_bump();
        assert_eq!(block.heartbeat(), 2);
    }

    /// 合法槽位描述符策略：句柄与尺寸用全域随机值并声明已接管，
    /// 防止描述符 Drop 把测试进程的真实 fd 当句柄关闭
    fn valid_slot_strategy() -> impl Strategy<Value = VramSlotDescriptor> {
        (
            any::<u32>(),
            prop::num::i64::ANY,
            prop::num::i64::ANY,
            any::<u32>(),
            any::<u32>(),
            prop::num::u64::ANY,
            prop::num::u64::ANY,
        )
            .prop_map(
                |(slot_idx, mem_handle, sem_handle, width, height, size_bytes, offset)| {
                    VramSlotDescriptor {
                        slot_idx,
                        mem_handle,
                        sem_handle,
                        width,
                        height,
                        size_bytes: size_bytes as usize,
                        offset,
                        handles_taken: true,
                    }
                },
            )
    }

    proptest! {
        // 任意描述符按协议顺序拍平后必须无损还原：字段顺序由本测试独立固定，
        // from_flat 与这里任何一侧重排字段都会被打破
        #[test]
        fn parse_batch_round_trips_arbitrary_descriptors(
            slots in prop::collection::vec(valid_slot_strategy(), 1..=8),
        ) {
            let mut flat = Vec::with_capacity(slots.len() * FLAT_STRIDE);
            for d in &slots {
                flat.extend_from_slice(&[
                    d.slot_idx as i64,
                    d.mem_handle,
                    d.sem_handle,
                    d.width as i64,
                    d.height as i64,
                    d.size_bytes as i64,
                    d.offset as i64,
                ]);
            }
            let parsed = VramSlotDescriptor::parse_batch(&flat).expect("整步长形状必须解析成功");
            prop_assert_eq!(parsed.len(), slots.len());
            for (parsed, expected) in parsed.iter().zip(&slots) {
                prop_assert_eq!(parsed.slot_idx, expected.slot_idx);
                prop_assert_eq!(parsed.mem_handle, expected.mem_handle);
                prop_assert_eq!(parsed.sem_handle, expected.sem_handle);
                prop_assert_eq!(parsed.width, expected.width);
                prop_assert_eq!(parsed.height, expected.height);
                prop_assert_eq!(parsed.size_bytes, expected.size_bytes);
                prop_assert_eq!(parsed.offset, expected.offset);
            }
        }

        // 任意长度与内容的输入都不 panic：空与非整步长整批拒绝，整步长按个数接受。
        // 解出的描述符统一接管句柄，避免 Drop 关闭测试进程的真实 fd
        #[test]
        fn parse_batch_never_panics_on_arbitrary_input(
            flat in prop::collection::vec(prop::num::i64::ANY, 0..64),
        ) {
            let mut parsed = VramSlotDescriptor::parse_batch(&flat);
            if let Some(list) = parsed.as_mut() {
                for d in list {
                    d.handles_taken = true;
                }
            }
            if flat.is_empty() || flat.len() % FLAT_STRIDE != 0 {
                prop_assert!(parsed.is_none(), "空输入与非整步长必须整批拒绝");
            } else {
                let list = parsed.expect("整步长形状必须解析成功");
                prop_assert_eq!(list.len(), flat.len() / FLAT_STRIDE);
            }
        }

        // 任意忙位预置与任意归还序列（含越界与重复归还）下：
        // mark_free 只清自己的位、幂等、绝不把 0 变成别的值
        #[test]
        fn mark_free_only_clears_its_own_bit(
            busy_mask in 0u32..(1 << SLOT_STATE_SLOTS),
            frees in prop::collection::vec(0u32..SLOT_STATE_SLOTS as u32 + 3, 0..12),
        ) {
            let block = SlotStateBlock::new_leaked();
            // 直接预置忙位模拟游戏侧认领，claim 路径只存在于游戏侧
            for (i, slot) in block.busy.iter().enumerate() {
                if busy_mask >> i & 1 == 1 {
                    slot.store(1, Ordering::Release);
                }
            }
            let before: Vec<u32> = (0..SLOT_STATE_SLOTS)
                .map(|i| block.busy[i].load(Ordering::Acquire))
                .collect();
            // 每个索引归还两次：第二次 CAS 必然失败，覆盖幂等路径
            for idx in &frees {
                block.mark_free(*idx);
                block.mark_free(*idx);
            }
            for (i, &b) in before.iter().enumerate() {
                let expected = if frees.contains(&(i as u32)) && b == 1 {
                    0
                } else {
                    b
                };
                prop_assert_eq!(
                    block.busy[i].load(Ordering::Acquire),
                    expected,
                    "槽位 {} 的位被误动",
                    i
                );
            }
        }

        // 心跳计数与 bump 次数严格一致
        #[test]
        fn heartbeat_counts_exactly_one_per_bump(bumps in 0u64..2048) {
            let block = SlotStateBlock::new_leaked();
            for _ in 0..bumps {
                block.heartbeat_bump();
            }
            prop_assert_eq!(block.heartbeat(), bumps);
        }
    }
}

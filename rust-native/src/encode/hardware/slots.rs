//! 显存直通槽位的跨平台公共类型：
//! - 导入协议：7 元组描述符解析与句柄生命周期管理
//! - `SlotTable<S>`：硬件编码后端共用的槽位事务容器
//! - `VramBinding` / `OwnedSlot` / `VramBindings`：槽位所有权、帧载荷凭据与当前绑定注册表

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::ThreadId;

use parking_lot::Mutex;

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

impl ActorStage {
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

/// 槽位掩码容量：与游戏侧 `NativeWireContract.MAX_VRAM_SLOTS` 同一条约定，
/// 只限制掩码宽度，不代表实际导入的槽位数
pub(crate) const MAX_VRAM_SLOTS: usize = 8;

/// 编码 Actor 的进度：心跳计数与最后进入的阶段。
///
/// 按引擎持有（旧 Actor 持旧 `Arc`），不需要泄漏分配：被弃线程写不到后继引擎的实例。
pub(crate) struct ActorProgress {
    heartbeat: AtomicU64,
    stage: AtomicU64,
}

impl ActorProgress {
    pub(crate) fn new() -> Self {
        Self {
            heartbeat: AtomicU64::new(0),
            stage: AtomicU64::new(ActorStage::Idle as u64),
        }
    }

    /// 标记 Actor 进入某阶段：只在可能长时间阻塞的调用前后写
    pub(crate) fn set_stage(&self, stage: ActorStage) {
        self.stage.store(stage as u64, Ordering::Release);
    }

    /// 当前阶段码：仅供 JNI 诊断读取
    pub(crate) fn stage(&self) -> u64 {
        self.stage.load(Ordering::Acquire)
    }

    /// Actor 每轮循环开始时 +1：心跳冻结即编码线程停止迭代
    pub(crate) fn heartbeat_bump(&self) {
        self.heartbeat.fetch_add(1, Ordering::Release);
    }

    /// 当前心跳计数：游戏侧按相等比较判定冻结
    pub(crate) fn heartbeat(&self) -> u64 {
        self.heartbeat.load(Ordering::Acquire)
    }
}

/// 进程级绑定句柄分配。句柄必须全局单调：每个引擎从 1 重新开始会让旧引擎的
/// 句柄在新引擎上重新有效，旧绑定的帧就能推进新引擎的槽位表。
static NEXT_BINDING_HANDLE: AtomicU64 = AtomicU64::new(1);

/// 一次显存绑定的全部所有权状态。与 Java 侧 `VramBinding` 是同一个领域对象的两个面：
/// Native 持有事实，Java 只持有 `handle`。
pub(crate) struct VramBinding {
    handle: u64,
    /// 已导入槽位的位掩码：Java 只能使用这些索引
    slot_mask: u64,
    /// 在途位：push 置位、ack 清位
    in_flight: AtomicU64,
    /// 导入调用线程（渲染线程）；push 校验，clear 不校验（停机可能在其它线程）
    owner: ThreadId,
    /// 引擎级心跳与阶段：绑定与引擎同寿命
    progress: Arc<ActorProgress>,
    /// 提交等待超时后置位：在途 GPU 工作可能仍在读槽位纹理，绑定不再接受推送
    poisoned: AtomicBool,
}

impl VramBinding {
    /// 新建绑定并分配进程级唯一句柄
    pub(crate) fn new(slot_mask: u64, owner: ThreadId, progress: Arc<ActorProgress>) -> Self {
        Self {
            handle: NEXT_BINDING_HANDLE.fetch_add(1, Ordering::Relaxed),
            slot_mask,
            in_flight: AtomicU64::new(0),
            owner,
            progress,
            poisoned: AtomicBool::new(false),
        }
    }

    /// 绑定句柄（导入时回送 Java）
    pub(crate) fn handle(&self) -> u64 {
        self.handle
    }

    /// 导入时的渲染线程
    pub(crate) fn owner(&self) -> ThreadId {
        self.owner
    }

    /// 编码 Actor 的当前心跳
    pub(crate) fn heartbeat(&self) -> u64 {
        self.progress.heartbeat()
    }

    /// 编码 Actor 最后进入的阶段码
    pub(crate) fn stage(&self) -> u64 {
        self.progress.stage()
    }

    /// 当前在途掩码（快照）
    pub(crate) fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Acquire)
    }

    /// 是否因提交等待超时毒化
    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// 毒化本绑定：在途提交可能仍在读槽位纹理，后续推送一律拒绝
    pub(crate) fn mark_poisoned(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    /// 置位一个空闲槽位并交出所有权；越界、未导入、已在途都返回 `None`
    ///
    /// 先置位再入收件箱是硬性次序：反过来的话，编码极快时 ack 可能清掉尚未置的位。
    pub(crate) fn try_acquire(self: &Arc<Self>, slot: u32) -> Option<OwnedSlot> {
        let bit = 1u64.checked_shl(slot)?;
        if self.slot_mask & bit == 0 {
            return None;
        }
        if self.in_flight.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
            return None;
        }
        Some(OwnedSlot {
            binding: Arc::clone(self),
            slot,
            suppress: Cell::new(false),
        })
    }

    /// 归还在途位；重复或越界 ack 静默无效
    pub(crate) fn ack(&self, slot: u32) {
        let Some(bit) = 1u64.checked_shl(slot) else {
            return;
        };
        self.in_flight.fetch_and(!bit, Ordering::AcqRel);
    }

    /// 测试专用：按槽位数构造独立绑定
    #[cfg(test)]
    pub(crate) fn new_for_test(slot_count: u32) -> Arc<Self> {
        let mask = (1u64 << slot_count) - 1;
        Arc::new(Self::new(
            mask,
            std::thread::current().id(),
            Arc::new(ActorProgress::new()),
        ))
    }

    #[cfg(test)]
    pub(crate) fn is_in_flight(&self, slot: u32) -> bool {
        (self.in_flight() >> slot) & 1 == 1
    }
}

/// 一次已移交槽位的所有权：Drop 即归还（ack）。
///
/// 命名沿用 `OwnedFd` 的约定——持有即承担归还义务。`suppress` 用于提交仍在途、
/// GPU 可能仍读取槽位的失败路径：此后 Drop 不再归还。
pub(crate) struct OwnedSlot {
    binding: Arc<VramBinding>,
    slot: u32,
    suppress: Cell<bool>,
}

impl std::fmt::Debug for OwnedSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedSlot")
            .field("slot", &self.slot)
            .finish()
    }
}

impl OwnedSlot {
    /// 槽位索引
    pub(crate) fn slot(&self) -> u32 {
        self.slot
    }

    /// 毒化路径：抑制归还并让整个绑定停止接收推送
    pub(crate) fn poison(&self) {
        self.suppress.set(true);
        self.binding.mark_poisoned();
    }
}

impl Drop for OwnedSlot {
    fn drop(&mut self) {
        if !self.suppress.get() {
            self.binding.ack(self.slot);
        }
    }
}

/// 当前生效的显存绑定：同一时刻至多一个，Java 只持有它的句柄。
///
/// 锁包住「取绑定 + 置在途位 + 入收件箱」与「换/摘绑定 + 清收件箱」两侧：
/// 换代时在途帧要么落在换表之前被清掉，要么落进新表，绝不会带着旧槽位索引进入新表。
pub(crate) struct VramBindings {
    current: Mutex<Option<Arc<VramBinding>>>,
}

impl VramBindings {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(None),
        })
    }

    /// 在锁内对指定句柄执行一次操作；句柄不是当前绑定时返回 `None`
    pub(crate) fn with_current<R>(
        &self,
        handle: u64,
        f: impl FnOnce(&Arc<VramBinding>) -> R,
    ) -> Option<R> {
        let guard = self.current.lock();
        guard
            .as_ref()
            .filter(|binding| binding.handle() == handle)
            .map(f)
    }

    /// 安装新绑定，并在同一把锁内清空旧帧（调用方负责随后回收丢弃载荷）
    pub(crate) fn install(&self, binding: Arc<VramBinding>, clear: impl FnOnce()) {
        let mut guard = self.current.lock();
        *guard = Some(binding);
        clear();
    }

    /// 摘除指定绑定，并在同一把锁内清空旧帧；句柄不符时不动
    pub(crate) fn take(&self, handle: u64, clear: impl FnOnce()) -> bool {
        let mut guard = self.current.lock();
        if guard
            .as_ref()
            .is_some_and(|binding| binding.handle() == handle)
        {
            *guard = None;
            clear();
            true
        } else {
            false
        }
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
    fn test_acquire_and_ack_round_trip() {
        let binding = VramBinding::new_for_test(3);
        let slot = binding.try_acquire(1).expect("首选槽位空闲");
        assert_eq!(slot.slot(), 1);
        assert!(binding.is_in_flight(1));
        drop(slot);
        assert!(!binding.is_in_flight(1), "Drop 必须归还");

        let again = binding.try_acquire(1).expect("归还后可再取");
        assert_eq!(again.slot(), 1);
    }

    #[test]
    fn test_double_acquire_is_rejected_without_touching_owner() {
        let binding = VramBinding::new_for_test(3);
        let first = binding.try_acquire(0).expect("首次认领");
        assert!(binding.try_acquire(0).is_none(), "同槽重复认领必须拒绝");
        assert_eq!(binding.in_flight(), 1, "拒绝不得改变在途位");
        drop(first);
    }

    #[test]
    fn test_ack_is_idempotent_and_bounded() {
        let binding = VramBinding::new_for_test(2);
        binding.ack(0);
        binding.ack(9);
        assert_eq!(binding.in_flight(), 0);

        let slot = binding.try_acquire(0).unwrap();
        drop(slot);
        binding.ack(0);
        assert!(!binding.is_in_flight(0));
    }

    #[test]
    fn test_out_of_mask_slot_is_rejected() {
        let binding = VramBinding::new_for_test(3);
        assert!(binding.try_acquire(3).is_none());
        assert!(binding.try_acquire(u32::MAX).is_none());
        assert_eq!(binding.in_flight(), 0, "越界索引不得触碰高位");
    }

    #[test]
    fn test_poison_marks_binding_and_suppresses_drop() {
        let binding = VramBinding::new_for_test(2);
        let slot = binding.try_acquire(0).unwrap();
        slot.poison();
        assert!(binding.is_poisoned(), "毒化必须落在绑定上");
        drop(slot);
        assert!(binding.is_in_flight(0), "毒化后 Drop 不得归还");
    }

    #[test]
    fn test_handles_are_process_globally_unique() {
        let a = VramBinding::new_for_test(1);
        let b = VramBinding::new_for_test(1);
        assert_ne!(a.handle(), b.handle());
        assert!(a.handle() >= 1 && b.handle() >= 1);
    }

    #[test]
    fn test_registry_rejects_stale_handle_and_clears_on_take() {
        let bindings = VramBindings::new();
        let first = VramBinding::new_for_test(1);
        bindings.install(Arc::clone(&first), || {});
        assert!(bindings.with_current(first.handle(), |_| ()).is_some());

        let mut cleared = 0;
        assert!(bindings.take(first.handle(), || cleared += 1));
        assert_eq!(cleared, 1, "摘除必须在锁内清空旧帧");
        assert!(bindings.with_current(first.handle(), |_| ()).is_none());
        assert!(!bindings.take(first.handle(), || {}), "重复摘除无效");
    }

    #[test]
    fn test_registry_install_replaces_and_clears() {
        let bindings = VramBindings::new();
        let old = VramBinding::new_for_test(1);
        bindings.install(Arc::clone(&old), || {});

        let new = VramBinding::new_for_test(1);
        let mut cleared = 0;
        bindings.install(Arc::clone(&new), || cleared += 1);
        assert_eq!(cleared, 1, "换表必须在锁内清空旧帧");
        assert!(bindings.with_current(old.handle(), |_| ()).is_none());
        assert!(bindings.with_current(new.handle(), |_| ()).is_some());
    }

    /// 阶段码只在 Native 内使用；JNI 诊断按它取标签
    #[test]
    fn test_progress_heartbeat_and_stage() {
        let progress = ActorProgress::new();
        assert_eq!(progress.heartbeat(), 0);
        progress.heartbeat_bump();
        progress.heartbeat_bump();
        assert_eq!(progress.heartbeat(), 2);

        progress.set_stage(ActorStage::LockBitstream);
        assert_eq!(progress.stage(), ActorStage::LockBitstream as u64);
        assert_eq!(ActorStage::describe(0), "空闲等待");
        assert_eq!(ActorStage::describe(9), "锁定码流");
        assert_eq!(ActorStage::describe(99), "未知阶段(99)");
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

        // 心跳计数与 bump 次数严格一致
        #[test]
        fn heartbeat_counts_exactly_one_per_bump(bumps in 0u64..2048) {
            let progress = ActorProgress::new();
            for _ in 0..bumps {
                progress.heartbeat_bump();
            }
            prop_assert_eq!(progress.heartbeat(), bumps);
        }
    }
}

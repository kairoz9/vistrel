//! 主机帧池 (Host Frame Pool)
//!
//! 采集侧独占的**唯一**主机帧所有权：渲染线程从中取块写入整帧，编码后端从块里读。
//! 池是长期持有者，编码后端只是租户——块由后端在**自己的设备/上下文**上分配，
//! 经控制面装进池（[`FramePool::attach`]）后即与后端脱钩。
//!
//! 三条不变量：
//! 1. **块的身份随块走**：每块自带所属内存域（CUDA 上下文 / `VideoDevice` 的地址），
//!    池据此把归还的块分回 `current` 或 `stale`，后端再按自己的设备复核。
//! 2. **任何线程都可以归还块**（[`FrameBuf`] 的 `Drop`），但**只进列表、不碰设备**；
//!    设备的等待与释放全部发生在编码线程的 [`FramePool::attach`] 里。
//! 3. **换代只在池内发生**：后端换内存域时把新批交给池，旧批连同全部 `stale` 就在
//!    编码线程上析构。池句柄本身永不更换，逐帧路径看不到「后端换了」。

use std::sync::Arc;

use ash::vk;
use parking_lot::Mutex;

use crate::encode::hardware::nvenc::cuda::PinnedHost;
use crate::encode::hardware::vulkan::{HostBuffer, VideoDevice};
use crate::encode::{MAX_CODED_ALIGN, MAX_HEIGHT, MAX_WIDTH, nv12_len};

/// 池深（同时在途的主机帧数上限）
///
/// 3 帧在 240 FPS 下是 12.5 ms 的在途窗口：采集侧比编码侧快一整帧也不至于取不到块；
/// 更深只是把延迟换成空转，取不到块时丢帧比排队正确。
pub(crate) const POOL_DEPTH: usize = 3;

/// 单块容量：**最大档位 coded 几何**的 NV12，即 2560 × 1472 × 1.5 = 5,652,480 字节
///
/// 用 coded 而不是 display：GPU 侧 CSC 按编码尺寸读那 32 行补齐（生产侧只写了 display
/// 行，差额 120 KB），缓冲必须装得下这些读取。
///
/// 固定最大几何而不是按档位分配：内存域只在换 codec / 换后端时才变，块若随分辨率走，
/// 每次改档位都要换批（等于把 `attach` 的析构点搬到用户点下钻的路径上）。代价是
/// 1080p 档位每块多占约 2.5 MB。
pub(crate) const HOST_SLOT_BYTES: usize = nv12_len(
    MAX_WIDTH,
    MAX_HEIGHT.div_ceil(MAX_CODED_ALIGN) * MAX_CODED_ALIGN,
);

// 内存域身份 = 域对象（CUDA 上下文 / `VideoDevice`）的地址：域对象由块的 `Arc` 持有，
// 块存活期间其地址不会被复用，故地址即稳定身份，无需单调计数。
// 下面两个哨兵是真实分配地址取不到的值。
/// 池尚未装入任何批
const DOMAIN_NONE: usize = 0;
/// 堆块共用的域：堆内存与设备无关，任何软编都能消费
const DOMAIN_HEAP: usize = usize::MAX;

/// 池中的一块主机帧内存
///
/// 三种家族只在「谁读得懂这块内存」上不同，对生产者都是可直写的字节区。
/// 跨线程流转，指针值无诊断意义，故 `Debug` 只报家族。
pub(crate) enum HostBlock {
    /// CUDA 锁页内存：NVENC 后端直接异步 H2D
    Cuda(PinnedHost),
    /// Vulkan 持久映射的 host-visible 缓冲：Vulkan Video 后端可零拷贝直用
    Vulkan(HostBuffer),
    /// 普通堆内存：软编自给自足，不需要设备可读
    Heap(Vec<u8>),
    /// 测试用假设备块：只带域标签，不持有任何真实设备资源
    #[cfg(test)]
    FakeDevice { buf: Vec<u8>, domain: usize },
}

impl HostBlock {
    pub(crate) fn as_ptr(&self) -> *const u8 {
        match self {
            Self::Cuda(host) => host.as_ptr(),
            Self::Vulkan(buf) => buf.as_ptr(),
            Self::Heap(vec) => vec.as_ptr(),
            #[cfg(test)]
            Self::FakeDevice { buf, .. } => buf.as_ptr(),
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        match self {
            Self::Cuda(host) => host.capacity(),
            Self::Vulkan(buf) => buf.size() as usize,
            Self::Heap(vec) => vec.capacity(),
            #[cfg(test)]
            Self::FakeDevice { buf, .. } => buf.capacity(),
        }
    }

    fn domain(&self) -> usize {
        match self {
            Self::Cuda(host) => host.domain_identity(),
            Self::Vulkan(buf) => buf.domain_identity(),
            Self::Heap(_) => DOMAIN_HEAP,
            #[cfg(test)]
            Self::FakeDevice { domain, .. } => *domain,
        }
    }

    /// 测试用：假设备块的域标签；真实块恒为 `None`
    #[cfg(test)]
    fn fake_domain(&self) -> Option<usize> {
        match self {
            Self::FakeDevice { domain, .. } => Some(*domain),
            _ => None,
        }
    }

    pub(crate) fn cuda_host(&self) -> Option<&PinnedHost> {
        match self {
            Self::Cuda(host) => Some(host),
            _ => None,
        }
    }

    /// 本块在此设备上的 `VkBuffer`；家族不符或跨域时 `None`
    ///
    /// 调用方（Vulkan 后端）据此一步完成「是不是我的设备」的复核：跨代的块
    /// 只能走 CPU 路径，绝不能喂给自己的命令缓冲。
    pub(crate) fn vulkan_buffer_of(&self, device: &VideoDevice) -> Option<vk::Buffer> {
        match self {
            Self::Vulkan(buf) if buf.belongs_to(device) => Some(buf.buffer()),
            _ => None,
        }
    }

    /// 本块持有的 `VkBuffer`（不论设备归属）；非 Vulkan 家族为 `None`
    #[cfg(test)]
    pub(crate) fn vulkan_buffer(&self) -> Option<vk::Buffer> {
        match self {
            Self::Vulkan(buf) => Some(buf.buffer()),
            _ => None,
        }
    }
}

impl std::fmt::Debug for HostBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let family = match self {
            Self::Cuda(_) => "Cuda",
            Self::Vulkan(_) => "Vulkan",
            Self::Heap(_) => "Heap",
            #[cfg(test)]
            Self::FakeDevice { .. } => "FakeDevice",
        };
        f.debug_struct("HostBlock")
            .field("family", &family)
            .field("capacity", &self.capacity())
            .finish()
    }
}

/// 堆块批：软编的输入内存，与设备无关
pub(crate) fn heap_blocks() -> Vec<HostBlock> {
    (0..POOL_DEPTH)
        .map(|_| HostBlock::Heap(vec![0u8; HOST_SLOT_BYTES]))
        .collect()
}

/// 测试用假设备块批：模拟后端在自己的设备上分配的一批块
#[cfg(test)]
pub(crate) fn fake_device_blocks(domain: usize) -> Vec<HostBlock> {
    debug_assert!(domain != DOMAIN_NONE && domain != DOMAIN_HEAP);
    (0..POOL_DEPTH)
        .map(|_| HostBlock::FakeDevice {
            buf: vec![0u8; HOST_SLOT_BYTES],
            domain,
        })
        .collect()
}

struct PoolState {
    /// 当前批所属内存域（`DOMAIN_NONE` = 尚未装批）
    domain: usize,
    /// 可再取用的块
    current: Vec<HostBlock>,
    /// 已被换下、等着在编码线程上释放的块
    stale: Vec<HostBlock>,
}

/// 主机帧池
pub(crate) struct FramePool {
    state: Mutex<PoolState>,
}

impl FramePool {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PoolState {
                domain: DOMAIN_NONE,
                current: Vec::with_capacity(POOL_DEPTH),
                stale: Vec::new(),
            }),
        })
    }

    /// 取一块给这一帧用：只加一次锁、只做一次出栈
    ///
    /// 取不到返回 `None`——池里的块全在途，丢这一帧比在渲染线程排队等待正确。
    pub(crate) fn acquire(self: &Arc<Self>) -> Option<FrameBuf> {
        let block = self.state.lock().current.pop()?;
        Some(FrameBuf {
            block: Some(block),
            len: 0,
            pool: Arc::clone(self),
        })
    }

    /// 装新批：后端新建/换域后调用（「只在编码线程碰设备」的不变量见模块文档 2/3）
    ///
    /// 旧批与全部 `stale` 在这里析构（可能含 `vkDeviceWaitIdle`），必须在锁外做。
    /// 空批表示「内存域没变」，保留当前批——但 `stale` 照样释放，所以「任何一次 `attach`
    /// 都收走上一批」是无条件成立的。
    pub(crate) fn attach(&self, blocks: Vec<HostBlock>) {
        // 一批块是同一次分配出来的，域必然一致；取首块的域即整批的域
        let domain = blocks.first().map(HostBlock::domain);
        let stale = {
            let mut state = self.state.lock();
            let mut stale = std::mem::take(&mut state.stale);
            if let Some(domain) = domain {
                state.domain = domain;
                let current = std::mem::replace(&mut state.current, blocks);
                stale.extend(current);
            }
            stale
        };
        // 旧批属于已换下的设备/上下文：就地析构合法——本函数只在编码线程执行
        // （模块文档不变量 2 的落地处）
        drop(stale);
    }

    /// 归还一块：任意线程，只进列表
    ///
    /// 域与当前批一致就回到可再取队列，否则进 `stale` 等下一次 `attach` 释放。
    fn recycle(&self, block: HostBlock) {
        let mut state = self.state.lock();
        if block.domain() == state.domain {
            state.current.push(block);
        } else {
            state.stale.push(block);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.state.lock().current.len()
    }

    #[cfg(test)]
    pub(crate) fn stale_len(&self) -> usize {
        self.state.lock().stale.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.state.lock().current.is_empty()
    }
}

/// 一块已出池的主机帧缓冲
///
/// 析构即归还（任何线程、任何路径：编码成功、编码失败、收件箱覆盖），因此不存在泄漏，
/// 也不需要调用方显式释放。`len` 是生产者写进去的整帧字节数。
pub(crate) struct FrameBuf {
    block: Option<HostBlock>,
    len: usize,
    pool: Arc<FramePool>,
}

impl FrameBuf {
    fn block(&self) -> &HostBlock {
        // 只在本结构析构时取出，此前固定为 Some
        self.block.as_ref().expect("帧缓冲已归还")
    }

    /// 写入整帧：容量不足直接报错，不截断
    pub(crate) fn write(&mut self, src: &[u8]) -> Result<(), String> {
        let block = self.block.as_mut().expect("帧缓冲已归还");
        let capacity = block.capacity();
        if src.len() > capacity {
            return Err(format!(
                "主机帧容量不足: 需要 {} 字节, 容量 {capacity}",
                src.len()
            ));
        }
        match block {
            HostBlock::Cuda(host) => {
                // SAFETY: 块由本结构独占持有，`as_ptr` 指向 cuMemAllocHost_v2 分配的
                // `capacity` 字节且无别名；长度已按容量校验；源切片与它不可能重叠。
                // 刻意不用流式 NT（copy_streaming）：1080p 时目标大部分落在 L3 内，
                // NT 绕缓存即放弃该收益（实测 +44~78%）；1440p 超 L3 后受 NT 存储吞吐
                // 上限限制仍略慢（+11~18%）
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr(),
                        host.as_ptr() as *mut u8,
                        src.len(),
                    );
                }
                Ok(())
            }
            // 持久映射缓冲自带长度校验与流式直写
            HostBlock::Vulkan(buf) => buf.write(src),
            HostBlock::Heap(vec) => {
                vec.clear();
                vec.extend_from_slice(src);
                Ok(())
            }
            #[cfg(test)]
            HostBlock::FakeDevice { buf, .. } => {
                buf.clear();
                buf.extend_from_slice(src);
                Ok(())
            }
        }?;
        self.len = src.len();
        Ok(())
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        let ptr = self.block().as_ptr();
        // SAFETY: ptr 指向本块独占持有的 `capacity` 字节映射/堆内存，`len` 已由
        // `write` 校验为不超过容量且此后不再增长；块的生命周期覆盖本借用
        unsafe { std::slice::from_raw_parts(ptr, self.len) }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn cuda_host(&self) -> Option<&PinnedHost> {
        self.block().cuda_host()
    }

    /// 本帧在此设备上的 `VkBuffer`；跨域或家族不符时为 `None`
    pub(crate) fn vulkan_buffer_of(&self, device: &VideoDevice) -> Option<vk::Buffer> {
        self.block().vulkan_buffer_of(device)
    }

    #[cfg(test)]
    pub(crate) fn vulkan_buffer(&self) -> Option<vk::Buffer> {
        self.block().vulkan_buffer()
    }

    /// 测试用：假设备块的域标签；真实块恒为 `None`
    #[cfg(test)]
    pub(crate) fn fake_domain(&self) -> Option<usize> {
        self.block().fake_domain()
    }
}

impl Drop for FrameBuf {
    fn drop(&mut self) {
        if let Some(block) = self.block.take() {
            self.pool.recycle(block);
        }
    }
}

impl std::fmt::Debug for FrameBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameBuf")
            .field("backing", &self.block)
            .field("len", &self.len)
            .finish()
    }
}

/// 测试共用的池取样：走的就是生产者的逐帧路径（取块 → 整帧写入 → 装载）
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use super::{FramePool, HostBlock};
    use crate::encode::FramePayload;

    /// 建池并装进后端交回的块（生产路径里这件事由 Actor 做）
    pub(crate) fn pool_with(blocks: Vec<HostBlock>) -> Arc<FramePool> {
        let pool = FramePool::new();
        pool.attach(blocks);
        pool
    }

    /// 从池里取一块写进 `bytes`，作为主机帧载荷
    ///
    /// 测试里池的余量与容量都是既定前提，失败直接 panic
    pub(crate) fn host_payload(pool: &Arc<FramePool>, bytes: Vec<u8>) -> FramePayload {
        let mut frame = pool.acquire().expect("池里必须有余量");
        frame.write(&bytes).expect("写入主机帧失败");
        FramePayload::Host { frame }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{StreamingProfile, display_nv12_len};

    /// 归还进当前域后必须能再取到：池深固定，取空即 None
    #[test]
    fn test_acquire_and_recycle_round_trip() {
        let pool = FramePool::new();
        pool.attach(heap_blocks());

        let mut held = Vec::new();
        for _ in 0..POOL_DEPTH {
            held.push(pool.acquire().expect("装批后池里必须有块"));
        }
        assert!(pool.acquire().is_none(), "块全在途时取块必须返回 None");
        drop(held);
        assert_eq!(pool.len(), POOL_DEPTH, "归还后必须全部回到可再取队列");
        assert_eq!(pool.stale_len(), 0);
    }

    /// 写入后字节视图必须是同一次拷贝，容量超标必须报错而不是截断
    /// 容量按 coded 尺寸算而不是 display：GPU 侧 CSC 要读补齐行
    #[test]
    fn test_host_slot_bytes_cover_coded_extent() {
        // 码率与本用例无关（只验几何），占位即可
        let max = StreamingProfile::new(MAX_WIDTH, MAX_HEIGHT, 30, 4_000_000);
        let coded = max.coded_extent((MAX_CODED_ALIGN, MAX_CODED_ALIGN));
        assert_eq!(
            HOST_SLOT_BYTES,
            nv12_len(coded.0, coded.1),
            "单块容量必须恰好是最大档位的 coded NV12"
        );
        assert!(
            HOST_SLOT_BYTES >= display_nv12_len(&max),
            "生产侧按 display 推的帧必须装得下"
        );
    }

    #[test]
    fn test_write_and_slice() {
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        let mut frame = pool.acquire().unwrap();
        let src = vec![7u8; 4096];
        frame.write(&src).expect("容量内写入必须成功");
        assert_eq!(frame.as_slice(), src.as_slice());
        assert_eq!(frame.len(), src.len());

        let too_big = vec![0u8; HOST_SLOT_BYTES + 1];
        assert!(frame.write(&too_big).is_err(), "超容量必须报错");
    }

    /// 同域换批不产生 stale；跨域的 stale 路径由硬件后端用例（真设备换代）覆盖
    #[test]
    fn test_attach_same_domain_keeps_pool_full() {
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        let inflight = pool.acquire().unwrap();
        drop(inflight);
        assert_eq!(pool.len(), POOL_DEPTH);

        pool.attach(heap_blocks());
        assert_eq!(pool.len(), POOL_DEPTH, "同域换批后仍是满池");
        assert_eq!(pool.stale_len(), 0, "同域换批不产生 stale");
    }

    /// 空批是「域没变」，绝不能把当前批清掉
    #[test]
    fn test_attach_empty_keeps_current_batch() {
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        pool.attach(Vec::new());
        assert_eq!(pool.len(), POOL_DEPTH);
        assert!(pool.acquire().is_some());
    }
}

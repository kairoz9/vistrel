//! 主机帧池 (Host Frame Pool)
//!
//! 采集侧取用主机帧的统一入口：渲染线程从中取块写入整帧，编码后端从块里读。
//! 块由后端在**自己的设备/上下文**上分配，经 [`FramePool::attach`] 装进池；池负责取用
//! 与归还，后端保留 `Arc` 用于重建会话。池、后端、在途帧都不再引用时块才真正释放。
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

/// 单块容量：**最大档位 coded 几何**的 NV12，即 3840 × 2176 × 1.5 = 12,533,760 字节
///
/// 用 coded 而不是 display：GPU 侧 CSC 按编码尺寸读取对齐补齐的行（生产侧只写了
/// display 行），缓冲必须装得下这些读取。
///
/// 固定最大几何而不是按档位分配：内存域只在换 codec / 换后端时才变，块若随分辨率走，
/// 每次改档位都要换批（等于把 `attach` 的析构点搬到用户点下钻的路径上）。代价是
/// 1080p 档位每块多占约 9.4 MB。
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
    /// CUDA 锁页内存：NVENC 后端直接异步 H2D；与后端共享同一 `Arc`，池先析构不悬空
    Cuda(Arc<PinnedHost>),
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

    /// 可写视图（长度即容量）：块由 `FrameBuf` 独占，写入期间没有读者
    fn as_mut_slice(&mut self) -> &mut [u8] {
        let capacity = self.capacity();
        match self {
            // SAFETY: 锁页内存由本块独占、覆盖 `capacity` 字节，`&mut self` 排除别名
            Self::Cuda(host) => unsafe {
                std::slice::from_raw_parts_mut(host.as_ptr() as *mut u8, capacity)
            },
            // SAFETY: 持久映射基址覆盖 `size()` 字节，本块存活期内恒有效
            Self::Vulkan(buf) => unsafe { std::slice::from_raw_parts_mut(buf.as_ptr(), capacity) },
            Self::Heap(vec) => vec.as_mut_slice(),
            #[cfg(test)]
            Self::FakeDevice { buf, .. } => buf.as_mut_slice(),
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        match self {
            Self::Cuda(host) => host.capacity(),
            Self::Vulkan(buf) => buf.size() as usize,
            // 堆块按槽长分配且从不缩短，长度即可写容量（与 `as_mut_slice` 同一口径）
            Self::Heap(vec) => vec.len(),
            #[cfg(test)]
            Self::FakeDevice { buf, .. } => buf.len(),
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
            Self::Cuda(host) => Some(host.as_ref()),
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
            display: None,
            pool: Arc::clone(self),
        })
    }

    /// 装新批：后端新建/换域后调用（「只在编码线程碰设备」的不变量见模块文档 2/3）
    ///
    /// 旧批与全部 `stale` 在这里放手（可能含 `vkDeviceWaitIdle`），必须在锁外做；
    /// CUDA 底层分配若仍被后端 `Arc` 持有，会延后到最后一个引用释放。
    /// 空批表示「内存域没变」，保留当前批——但 `stale` 照样放手，所以「任何一次 `attach`
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
        // 旧批属于已换下的设备/上下文：就地放手合法——本函数只在编码线程执行
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
    /// 采集几何：随帧写入声明（`None` = 调用方直给成品布局，见 [`FrameBuf::write`]）
    ///
    /// 判断的是几何本身而非字节数：两种几何可能长度相同而行距不同
    display: Option<(usize, usize)>,
    pool: Arc<FramePool>,
}

impl FrameBuf {
    fn block(&self) -> &HostBlock {
        // 只在本结构析构时取出，此前固定为 Some
        self.block.as_ref().expect("帧缓冲已归还")
    }

    /// 就地补齐到编码尺寸：右侧复制行尾采样、底部复制最后一行
    ///
    /// 编码尺寸只有编码线程知道；校验先于补齐——免补的帧也可能是旧几何的错帧
    pub(crate) fn pad_in_place(&mut self, coded: (usize, usize)) -> Result<(), String> {
        let Some((display_w, display_h)) = self.display else {
            return Err("帧未声明采集几何，无法补齐".to_string());
        };
        let (coded_w, coded_h) = coded;
        if display_w == 0
            || display_h == 0
            || display_w % 2 != 0
            || display_h % 2 != 0
            || coded_w % 2 != 0
            || coded_h % 2 != 0
            || coded_w < display_w
            || coded_h < display_h
        {
            return Err(format!(
                "几何非法: 采集 {display_w}x{display_h} / 编码 {coded_w}x{coded_h}（需偶数，且编码尺寸不小于采集尺寸）"
            ));
        }
        let src_len = nv12_len(display_w, display_h);
        if self.len != src_len {
            return Err(format!(
                "主机帧长度不符: 期望 {src_len}（采集 {display_w}x{display_h}, NV12）, 实际 {}",
                self.len
            ));
        }
        if (display_w, display_h) == (coded_w, coded_h) {
            return Ok(());
        }
        let total = nv12_len(coded_w, coded_h);
        let buf = self.block.as_mut().expect("帧缓冲已归还").as_mut_slice();
        if total > buf.len() {
            return Err(format!(
                "主机帧容量不足: 需要 {total} 字节, 容量 {}",
                buf.len()
            ));
        }
        let buf = &mut buf[..total];
        // 先 UV（高地址）后 Y：Y 的目标会覆写 UV 的源区
        expand_plane(
            buf,
            PlaneMove {
                src_off: display_w * display_h,
                src_w: display_w,
                src_rows: display_h / 2,
                dst_off: coded_w * coded_h,
                dst_w: coded_w,
                dst_rows: coded_h / 2,
                sample: 2,
            },
        );
        expand_plane(
            buf,
            PlaneMove {
                src_off: 0,
                src_w: display_w,
                src_rows: display_h,
                dst_off: 0,
                dst_w: coded_w,
                dst_rows: coded_h,
                sample: 1,
            },
        );
        self.len = total;
        Ok(())
    }

    /// 本帧声明的采集几何；`None` = 尚未写入 / 调用方直给了成品布局
    pub(crate) fn display(&self) -> Option<(usize, usize)> {
        self.display
    }

    /// 写入整帧并声明它的采集几何：容量不足直接报错，不截断
    ///
    /// 几何由写入方一并声明而不是事后补登记——分家就会静默落进「成品布局」那一支。
    pub(crate) fn write(
        &mut self,
        src: &[u8],
        display: Option<(usize, usize)>,
    ) -> Result<(), String> {
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
            // 堆块整块可写（长度即容量）：只覆盖前缀，不缩短可写视图
            HostBlock::Heap(vec) => {
                vec[..src.len()].copy_from_slice(src);
                Ok(())
            }
            #[cfg(test)]
            HostBlock::FakeDevice { buf, .. } => {
                buf[..src.len()].copy_from_slice(src);
                Ok(())
            }
        }?;
        self.len = src.len();
        self.display = display;
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
        frame.write(&bytes, None).expect("写入主机帧失败");
        FramePayload::Host { frame }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{StreamingProfile, host_nv12_len};

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

    /// 长度与采集几何不符时必须拒：短了读不到，长了会被按 display 行距重读而错位
    #[test]
    fn test_pad_in_place_rejects_length_mismatch() {
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        let display = (4usize, 4usize);
        let coded = (8usize, 8usize);
        let exact = nv12_len(display.0, display.1);

        let mut short = pool.acquire().expect("装批后池里必须有块");
        short
            .write(&vec![0u8; exact - 1], Some(display))
            .expect("容量内写入");
        let err = short.pad_in_place(coded).unwrap_err();
        assert!(err.contains("长度不符"), "短帧必须被拒: {err}");
        drop(short);

        let mut long = pool.acquire().expect("归还后必须能再取到");
        long.write(&vec![0u8; exact + 1], Some(display))
            .expect("容量内写入");
        let err = long.pad_in_place(coded).unwrap_err();
        assert!(err.contains("长度不符"), "超长帧必须被拒: {err}");
        drop(long);

        // 采集几何已等于编码尺寸（NVENC / 软编）：免补也不能跳过校验
        let mut aligned = pool.acquire().expect("归还后必须能再取到");
        aligned
            .write(&vec![0u8; exact - 1], Some(display))
            .expect("容量内写入");
        let err = aligned.pad_in_place(display).unwrap_err();
        assert!(err.contains("长度不符"), "免补的路径同样必须校验: {err}");
    }

    /// 未声明采集几何的帧不得就地补齐（成品布局的帧应直接交给编码器）
    #[test]
    fn test_pad_in_place_requires_display() {
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        let mut frame = pool.acquire().expect("装批后池里必须有块");
        frame
            .write(&vec![0u8; nv12_len(4, 4)], None)
            .expect("容量内写入");
        let err = frame.pad_in_place((8, 8)).unwrap_err();
        assert!(err.contains("未声明采集几何"), "未声明几何必须报错: {err}");
    }

    /// 就地补齐：右侧复制行尾采样（UV 成对）、底部复制最后一行，整块恰为编码尺寸
    #[test]
    fn test_pad_in_place_layout_matches_coded_extent() {
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        let mut frame = pool.acquire().expect("装批后池里必须有块");

        // 采集 4x4 → 编码 8x8（两个方向都要补）
        let (display_w, display_h) = (4usize, 4usize);
        let (coded_w, coded_h) = (8usize, 8usize);
        let mut src = vec![0u8; nv12_len(display_w, display_h)];
        for y in 0..display_h {
            for x in 0..display_w {
                src[y * display_w + x] = (y + 1) as u8;
            }
        }
        let uv = &mut src[display_w * display_h..];
        for row in 0..display_h / 2 {
            for i in 0..display_w {
                uv[row * display_w + i] = if i % 2 == 0 {
                    100 + row as u8
                } else {
                    200 + row as u8
                };
            }
        }

        frame
            .write(&src, Some((display_w, display_h)))
            .expect("容量内写入必须成功");
        frame
            .pad_in_place((coded_w, coded_h))
            .expect("就地补齐必须成功");

        let out = frame.as_slice();
        assert_eq!(
            out.len(),
            nv12_len(coded_w, coded_h),
            "落盘长度必须是编码尺寸"
        );
        for row in 0..coded_h {
            let value = (row.min(display_h - 1) + 1) as u8;
            assert!(
                out[row * coded_w..(row + 1) * coded_w]
                    .iter()
                    .all(|b| *b == value),
                "Y 第 {row} 行必须是该行采样铺满（含右侧与底部补齐）"
            );
        }
        let y_bytes = coded_w * coded_h;
        for row in 0..coded_h / 2 {
            let sample = row.min(display_h / 2 - 1) as u8;
            let expected = [
                100 + sample,
                200 + sample,
                100 + sample,
                200 + sample,
                100 + sample,
                200 + sample,
                100 + sample,
                200 + sample,
            ];
            assert_eq!(
                &out[y_bytes + row * coded_w..y_bytes + (row + 1) * coded_w],
                &expected,
                "UV 第 {row} 行必须按采样整对复制，否则 U/V 串位"
            );
        }
    }

    /// 写入后字节视图必须是同一次拷贝，容量超标必须报错而不是截断
    /// 容量按 coded 尺寸算而不是 display：GPU 侧 CSC 要读补齐行
    #[test]
    fn test_host_slot_bytes_cover_coded_extent() {
        // 码率与本用例无关（只验几何），占位即可
        let max = StreamingProfile::new(MAX_WIDTH, MAX_HEIGHT, 30, 4_000_000);
        assert_eq!(
            HOST_SLOT_BYTES,
            host_nv12_len(&max, (MAX_CODED_ALIGN, MAX_CODED_ALIGN)),
            "单块容量必须恰好是最大档位的编码尺寸 NV12"
        );
    }

    #[test]
    fn test_write_and_slice() {
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        let mut frame = pool.acquire().unwrap();
        let src = vec![7u8; 4096];
        frame.write(&src, None).expect("容量内写入必须成功");
        assert_eq!(frame.as_slice(), src.as_slice());
        assert_eq!(frame.len(), src.len());

        let too_big = vec![0u8; HOST_SLOT_BYTES + 1];
        assert!(frame.write(&too_big, None).is_err(), "超容量必须报错");
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

/// 一个平面在块内的迁移与补齐：源布局 → 目标布局
struct PlaneMove {
    src_off: usize,
    src_w: usize,
    src_rows: usize,
    dst_off: usize,
    dst_w: usize,
    dst_rows: usize,
    /// 右补的采样组宽：Y 为 1，交织 UV 为 2（必须成对，否则 U/V 串位）
    sample: usize,
}

/// 把一个平面原地展开到更大的几何：右侧复制行尾采样、底部复制最后一行
///
/// 源与目标同处一块内存：倒序逐行展开时目标偏移 ≥ 源偏移，每行读到的源都还没被覆写
/// （行内重叠交给 `copy_within` 的 memmove 语义）。
fn expand_plane(buf: &mut [u8], m: PlaneMove) {
    // pad_in_place 已校验这些不变量，不成立只可能是调用写错
    assert!(m.dst_off >= m.src_off && m.dst_w >= m.src_w && m.dst_rows >= m.src_rows);
    assert!(m.sample > 0 && m.src_w >= m.sample && m.src_rows > 0);
    // 逐行从后往前：写进高地址的字节只属于已处理过的行
    for y in (0..m.src_rows).rev() {
        let src = m.src_off + y * m.src_w;
        let dst = m.dst_off + y * m.dst_w;
        buf.copy_within(src..src + m.src_w, dst);
        // 右侧补齐：整组复制行尾采样（UV 必须成对，否则 U/V 串位）
        let (visible, pad) = buf[dst..dst + m.dst_w].split_at_mut(m.src_w);
        for (i, byte) in pad.iter_mut().enumerate() {
            *byte = visible[m.src_w - m.sample + i % m.sample];
        }
    }
    // 底部补齐：复制最后一条已写满的行
    let last = m.dst_off + (m.src_rows - 1) * m.dst_w;
    for y in m.src_rows..m.dst_rows {
        buf.copy_within(last..last + m.dst_w, m.dst_off + y * m.dst_w);
    }
}

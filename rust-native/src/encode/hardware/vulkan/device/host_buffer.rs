//! 主机可见、持久映射的缓冲

use std::ptr::NonNull;
use std::sync::Arc;

use ash::vk::{self, TaggedStructure};

use super::VideoDevice;

/// 主机可见、一致、**构造时映射一次**的缓冲
///
/// 码流回读、CSC 的 RGBA 上传、NV12 暂存上传都是逐帧操作，每帧一对
/// `vkMapMemory` / `vkUnmapMemory` 会多出两次驱动往返，而映射本来就可以贯穿整个
/// 结构生命周期。本类型把「创建缓冲 → 分配显存 → 映射」合成一步，析构时逆序归还，
/// 调用方只看到 [`HostBuffer::write`] / [`HostBuffer::read`] 两个主机侧动作。
///
/// 映射范围取**缓冲大小**而不是 `VK_WHOLE_SIZE`：分配大小可能大于缓冲，按缓冲大小映射
/// 能让越界访问落在本类型的长度校验上，而不是悄悄写坏同一块分配里的别的东西。
pub struct HostBuffer {
    device: VideoDevice,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// 持久映射的基址：解映射之前恒有效，`map_memory` 成功即非空
    ptr: NonNull<u8>,
    size: u64,
    /// 落点是否为“可缓存且非设备本地”的系统内存——决定整帧写入用普通还是非暂存存储
    cached_ram: bool,
}

// SAFETY: `HostBuffer` 不实现 `Clone`，可变访问一律要 `&mut self`，所以跨线程移动
// 只是所有权转移、不会产生并发访问；映射地址与显存句柄都是进程内资源，与线程无关。
unsafe impl Send for HostBuffer {}

impl HostBuffer {
    pub(super) fn new(
        device: &VideoDevice,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
    ) -> Result<Self, String> {
        Self::new_inner(device, size, usage, what, None, false)
    }

    /// 缓存堆变体：见 [`VideoDevice::host_buffer_cached`]
    pub(super) fn new_cached(
        device: &VideoDevice,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
    ) -> Result<Self, String> {
        Self::new_inner(device, size, usage, what, None, true)
    }

    /// 视频编码的码流缓冲：创建时必须声明所属 profile
    ///
    /// 否则驱动不把它当视频资源（`VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08201`）。
    /// `profile` 只存指针链，必须活过本次 create 调用。
    pub(super) fn new_video(
        device: &VideoDevice,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
        profile: &vk::VideoProfileInfoKHR<'_>,
    ) -> Result<Self, String> {
        Self::new_inner(device, size, usage, what, Some(profile), true)
    }

    fn new_inner(
        device: &VideoDevice,
        size: u64,
        usage: vk::BufferUsageFlags,
        what: &str,
        video_profile: Option<&vk::VideoProfileInfoKHR<'_>>,
        prefer_cached: bool,
    ) -> Result<Self, String> {
        if size == 0 {
            return Err(format!("{what}大小不能为零"));
        }
        let mut info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // list 必须活到 create 调用之后：`VkVideoProfileListInfoKHR` 里存的是指向
        // 调用方 profile 的指针，而 `info` 又存着指向 list 的指针。写在 `if let` 块里
        // 会在块尾就 drop，借检查直接报 E0597。
        let mut profile_list = video_profile
            .map(|p| vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(p)));
        if let Some(list) = profile_list.as_mut() {
            info = info.push(list);
        }
        // SAFETY: info 已完整初始化；失败时不产生所有权
        let buffer = unsafe { device.device().create_buffer(&info, None) }
            .map_err(|e| format!("{what}创建失败: {e:?}"))?;

        // 码流缓冲是「GPU 写一次、CPU 逐帧读回」：优先落 HOST_CACHED 类型（本机实测
        // 存在 HOST_VISIBLE|HOST_COHERENT|HOST_CACHED 与无 CACHED 两种堆，见设备
        // 构造时的内存类型转储）；纯写暂存不偏向，write-combined 对写更合适
        let preferred = if prefer_cached {
            vk::MemoryPropertyFlags::HOST_CACHED
        } else {
            vk::MemoryPropertyFlags::empty()
        };
        let memory = match device.allocate_buffer(
            buffer,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            preferred,
            what,
        ) {
            Ok((memory, flags)) => (memory, flags),
            Err(e) => {
                // SAFETY: buffer 刚创建且尚未销毁，此处销毁一次；显存未分配成功
                unsafe { device.device().destroy_buffer(buffer, None) };
                return Err(e);
            }
        };
        let (memory, memory_flags) = memory;

        // SAFETY: memory 为刚分配并绑定的显存，属性已确认含 HOST_VISIBLE；
        // 映射起点 0 + 长度 size 不超过分配大小；此后一直保持映射，直到 Drop 解映射
        let ptr = unsafe {
            device
                .device()
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        };
        let ptr = match ptr {
            Ok(ptr) => match NonNull::new(ptr as *mut u8) {
                Some(ptr) => ptr,
                None => {
                    // SAFETY: buffer 与 memory 均已创建且尚未销毁，此处各归还一次
                    unsafe {
                        device.device().destroy_buffer(buffer, None);
                        device.device().free_memory(memory, None);
                    }
                    return Err(format!("{what}映射返回空指针"));
                }
            },
            Err(e) => {
                // SAFETY: buffer 与 memory 均已创建且尚未销毁，此处各归还一次
                unsafe {
                    device.device().destroy_buffer(buffer, None);
                    device.device().free_memory(memory, None);
                }
                return Err(format!("{what}映射失败: {e:?}"));
            }
        };

        Ok(Self {
            device: device.clone(),
            buffer,
            memory,
            ptr,
            size,
            // 设备本地 + 可缓存的堆（若存在）依旧靠非暂存直写；只有纯系统内存才换普通存储
            cached_ram: memory_flags.contains(vk::MemoryPropertyFlags::HOST_CACHED)
                && !memory_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL),
        })
    }

    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    /// 这块缓冲是否创建在 `device` 上
    ///
    /// 池里的块可能跨越一次后端重建：旧域的块属于旧设备，绝不能喂给新设备使用（池按域拦下，这里再判一次）
    /// （`vkCmdCopyBufferToImage` 要求命令缓冲与源缓冲同属一个 `VkDevice`）。
    pub(crate) fn belongs_to(&self, device: &VideoDevice) -> bool {
        Arc::ptr_eq(&self.device.inner, &device.inner)
    }

    /// 所属 `VkDevice` 的身份
    ///
    /// 设备由本结构的 `VideoDevice` 持有，块存活期间该地址不可能被复用，故可直接当域身份用
    pub(crate) fn domain_identity(&self) -> usize {
        Arc::as_ptr(&self.device.inner) as usize
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// 持久映射基址：供生产者线程直写（主机帧池的 Vulkan 块）
    ///
    /// 写入范围与生命周期由持有者（`PinnedBlock::Vulkan`）用容量检查与 RAII 归还保证
    #[inline]
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// 把 `src` 拷到缓冲起始处（主机侧，不产生任何 GPU 命令）
    ///
    /// 堆是“可缓存系统内存”时走普通存储：非暂存直写在那里反而绕过 Cache 白白多一遭
    /// （实测 1440p 整帧 14.75 MB：cached 堆普通写 ≈ 16 GB/s，非暂存写 ≈ 11 GB/s）。
    /// Write-Combined / 设备本地堆仍走 AVX2 非暂存直写，避开写分配开销。
    pub fn write(&mut self, src: &[u8]) -> Result<(), String> {
        self.write_at(0, src)
    }

    /// 从 `offset` 起写入
    ///
    /// 逐行上传（补齐行、行距与源不同）要写进缓冲中间，`write` 只能从头覆盖。
    /// 存储策略与 [`HostBuffer::write`] 完全一致。
    pub fn write_at(&mut self, offset: usize, src: &[u8]) -> Result<(), String> {
        let end = offset as u64 + src.len() as u64;
        if end > self.size {
            return Err(format!(
                "写入区间 {offset}..{end} 超出缓冲大小 {}",
                self.size
            ));
        }
        if src.is_empty() {
            return Ok(());
        }
        // SAFETY: ptr 指向持久映射的基址、覆盖 [0, size)；区间已按 size 校验；
        // 源是 Rust 侧切片，与驱动映射显存不重叠；两条分支都不要求 32B 对齐
        // （copy_streaming 自带对齐与 AVX2 特性检测）
        unsafe {
            let dst = self.ptr.as_ptr().add(offset);
            if self.cached_ram {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
            } else {
                copy_streaming(src.as_ptr(), dst, src.len());
            }
        }
        Ok(())
    }

    /// 读出前 `len` 字节
    ///
    /// 由 GPU 写入的内容必须等提交完成后才读：本后端的编码路径在等过栅栏之后才调用它。
    pub fn read(&self, len: usize) -> Result<Vec<u8>, String> {
        if len as u64 > self.size {
            return Err(format!("读取长度 {len} 超出缓冲大小 {}", self.size));
        }
        // SAFETY: ptr 覆盖 [0, size)，len 已按 size 校验；调用方保证这 len 字节
        // 此刻没有并发写者（GPU 侧提交已完成）
        let src = unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), len) };
        // `to_vec` 直接分配 + 拷贝，不像 `vec![0; len]` 那样先清零再整体覆盖一遍
        Ok(src.to_vec())
    }

    /// 读出前 `len` 字节，并在前面拼一段前缀——单次分配
    ///
    /// 给 IDR 帧用：参数集（SPS/PPS 等）拼在码流最前面，避免「read 一份 + 再拼一份」
    /// 两次分配与拷贝。前置条件同 [`HostBuffer::read`]。
    pub fn read_with_prefix(&self, prefix: &[u8], len: usize) -> Result<Vec<u8>, String> {
        if len as u64 > self.size {
            return Err(format!("读取长度 {len} 超出缓冲大小 {}", self.size));
        }
        // SAFETY: 同 `read`，ptr 覆盖 [0, size)、len 已校验、无并发写者
        let src = unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), len) };
        let mut out = Vec::with_capacity(prefix.len() + len);
        out.extend_from_slice(prefix);
        out.extend_from_slice(src);
        Ok(out)
    }

    /// 用固定字节填前 `len` 字节（测试用来污染暂存缓冲）
    #[cfg(test)]
    pub fn fill(&mut self, byte: u8, len: usize) -> Result<(), String> {
        if len as u64 > self.size {
            return Err(format!("填充长度 {len} 超出缓冲大小 {}", self.size));
        }
        // SAFETY: 同上，写入范围落在映射内
        unsafe { std::ptr::write_bytes(self.ptr.as_ptr(), byte, len) };
        Ok(())
    }
}

impl Drop for HostBuffer {
    fn drop(&mut self) {
        // 析构可能落在任意线程：先等设备空闲，确保没有在途命令还在读写这块缓冲
        let device = self.device.device();
        // SAFETY: 各句柄均由本结构创建且尚未销毁，此处各归还一次；解映射必须在
        // 释放显存之前——`vkFreeMemory` 虽会隐式解映射，但那样指针的失效时机就只
        // 写在规范里，不写在代码里
        unsafe {
            let _ = device.device_wait_idle();
            device.unmap_memory(self.memory);
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// 大块连续内存流式写入（优先 AVX2 非暂存直写，不支持或非对齐时安全回退标准 memcpy）
///
/// # Safety
///
/// 调用方必须保证：`src` 在 `[0, len)` 范围内有效可读；`dst` 在 `[0, len)` 范围内
/// 有效可写；`src` 与 `dst` 的内存区间不得重叠。函数体内逐语句的 SAFETY 论证
/// 依赖上述前置条件。
// SAFETY: `src` 必须在 `[0, len)` 范围内有效可读；
// `dst` 必须在 `[0, len)` 范围内有效可写；
// `src` 与 `dst` 的内存区间不得重叠。
#[inline]
pub(crate) unsafe fn copy_streaming(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        // 目标指针必须 32 字节对齐，且大块写入（>= 128 字节）时非暂存直写才能发挥性能优势
        if is_x86_feature_detected!("avx2") && (dst as usize).is_multiple_of(32) && len >= 128 {
            // SAFETY: 已通过运行时 CPU 特性检测与指针 32B 对齐校验；
            // 长度限制保证至少能完整写入一个 128B chunk
            unsafe { copy_streaming_avx2(src, dst, len) };
            return;
        }
    }
    // SAFETY: 不满足 AVX2 条件时标准拷贝；调用方已保证指针有效与不重叠
    unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
}

/// 128B 流水化非暂存直写拷贝（AVX2 `stream_si256`）
///
/// # Safety
///
/// 调用方必须保证：`src` 与 `dst` 各自 `[0, len)` 范围落在有效缓冲区内且互不重叠；
/// AVX2 支持已由调用方用 `is_x86_feature_detected!` 校验（`target_feature` 只声明
/// 编译期可能性）。函数体内逐语句的 SAFETY 论证依赖这两条前置条件。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn copy_streaming_avx2(src: *const u8, dst: *mut u8, len: usize) {
    use std::arch::x86_64::*;

    unsafe {
        // SAFETY: 本块为 unsafe fn 的实现体，全部语句的安全性依赖上方 # Safety
        // 前置条件（范围有效、互不重叠、AVX2 已检测），逐语句论证见行内注释
        let chunks_128 = len / 128;
        let mut s = src;
        let mut d = dst;

        // 每次迭代写入 128 字节（4 个 256 位寄存器），流水线化非暂存直写
        for _ in 0..chunks_128 {
            // SAFETY: s..s+128 在源范围内，loadu 支持任意对齐的源地址
            let v0 = _mm256_loadu_si256(s as *const __m256i);
            let v1 = _mm256_loadu_si256(s.add(32) as *const __m256i);
            let v2 = _mm256_loadu_si256(s.add(64) as *const __m256i);
            let v3 = _mm256_loadu_si256(s.add(96) as *const __m256i);

            // SAFETY: d 是 32 字节对齐且在 [0, len) 目标有效显存区内；
            // stream_si256 直接写入总线 Write-Combining 缓冲，绕过 CPU 各级 Cache
            _mm256_stream_si256(d as *mut __m256i, v0);
            _mm256_stream_si256(d.add(32) as *mut __m256i, v1);
            _mm256_stream_si256(d.add(64) as *mut __m256i, v2);
            _mm256_stream_si256(d.add(96) as *mut __m256i, v3);

            s = s.add(128);
            d = d.add(128);
        }

        let remainder = len % 128;
        let chunks_32 = remainder / 32;
        for _ in 0..chunks_32 {
            let v = _mm256_loadu_si256(s as *const __m256i);
            _mm256_stream_si256(d as *mut __m256i, v);
            s = s.add(32);
            d = d.add(32);
        }

        let trailing = remainder % 32;
        if trailing > 0 {
            // SAFETY: 末尾不足 32 字节零头标准拷贝
            std::ptr::copy_nonoverlapping(s, d, trailing);
        }

        // SAFETY: sfence 确保弱序的流式直写指令在后续 GPU 访问前全部刷新到总线
        _mm_sfence();
    }
}

//! CUDA 驱动与显存管理 (CUDA Driver & Memory)
//!
//! 用于 CUDA 锁页帧的设备端输入缓冲，以及 Vulkan 外部显存/信号量安全导入。
//!
//! **上下文模型：primary context，直接使用 `cudarc::driver::CudaContext`**——
//! retain/release 引用计数与线程绑定交给它。本文件只补 cudarc 覆盖不到的部分
//!

use std::ffi::c_void;
use std::sync::Arc;

use cudarc::driver::CudaContext;

use super::dl::ensure_cuda_init;
use super::ffi::*;

use crate::encode::hardware::slots::{VramSlotDescriptor, close_raw_handle};

const COLOR_NUM_CHANNELS_RGBA: u32 = 4;

// ============================================================================
// CUDA 上下文(CUDA Context)
// ============================================================================

/// 创建（或复用）设备 `ordinal` 上的 CUDA primary context
///
/// **顺序有硬约束：必须先在本地探路 + 初始化，再交给 cudarc。**
///
/// cudarc 的 `dynamic-loading` 在找不到 `libcuda.so` 时调 `panic_no_lib_found()`
/// （返回 `-> !`，直接 panic），而不是返回错误。`ensure_cuda_init` 先 dlopen 探一次
/// 并缓存结果，无 NVIDIA 驱动的机器（核显 / AMD）在这里就拿到 Err，走不到 cudarc，
/// 「降级到 Vulkan / 软编」的路径才成立。
pub(crate) fn new_cuda_context(ordinal: i32) -> Result<Arc<CudaContext>, String> {
    ensure_cuda_init()?;
    let idx = usize::try_from(ordinal).map_err(|_| format!("CUDA 设备序号非法: {ordinal}"))?;
    // cudarc 的 `new` 走 `cuDevicePrimaryCtxRetain`（进程级共享 + 引用计数），
    // 析构时按其 `is_primary` 调 `cuDevicePrimaryCtxRelease`
    let ctx = CudaContext::new(idx)
        .map_err(|e| format!("CUDA primary context 创建失败 (device {idx}): {e}"))?;
    // `cuCtxCreate` 会把新上下文压到线程栈顶，`cuDevicePrimaryCtxRetain` **不会**，
    // 而 cudarc 的 `new` 也不代劳。这里必须显式补一次，否则后续驱动调用全部落空
    ctx.bind_to_thread()
        .map_err(|e| format!("CUDA 上下文绑定失败: {e}"))?;
    Ok(ctx)
}

/// 在指定流上排队等待外部时间线信号量到达 `value`
pub(crate) fn wait_timeline_sem(
    ctx: &CudaContext,
    sem: CUexternalSemaphore,
    value: u64,
    stream: CUstream,
) -> Result<(), String> {
    // SAFETY: 该结构为 bindgen 生成的 C POD，成员均为整型/指针与信号模式共用体，
    // 全零位模式合法；CUDA 约定调用方零初始化参数结构后按信号类型填写 params
    let mut wait_params: CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS = unsafe { std::mem::zeroed() };
    wait_params.params.fence.value = value;
    let sem_arr = [sem];
    let params_arr = [wait_params];
    ctx.bind_to_thread()
        .map_err(|e| format!("CUDA 上下文绑定失败: {e}"))?;
    unsafe {
        // SAFETY: sem 为已导入的有效外部信号量；params 完全初始化；
        // 等待操作入队进 stream，由 GPU 侧按流序生效
        let res = cuWaitExternalSemaphoresAsync(sem_arr.as_ptr(), params_arr.as_ptr(), 1, stream);
        if res != CUresult::CUDA_SUCCESS {
            return Err(format!("cuWaitExternalSemaphoresAsync failed: {:?}", res));
        }
    }
    Ok(())
}

/// 单个 CUDA 设备的身份信息
#[derive(Debug, Clone)]
pub(crate) struct CudaDeviceInfo {
    pub(crate) ordinal: i32,
    pub(crate) name: String,
}

/// 枚举本机所有 CUDA 设备
///
/// 用于与 Vulkan 物理设备对齐：多显卡机器（如核显输出 + 独显渲染）上若盲目使用 device 0，
/// 会与 Vulkan 实际使用的设备不一致，导致 dma-buf 导入失败。
pub(crate) fn enumerate_devices() -> Vec<CudaDeviceInfo> {
    // cudarc 的 sys 在无驱动时会 panic，先探路；失败就当作「没有 CUDA 设备」
    if ensure_cuda_init().is_err() {
        return Vec::new();
    }
    let mut count: i32 = 0;
    unsafe {
        // SAFETY: 出参 count 校验；cuInit 已由 ensure_cuda_init 完成
        if cuDeviceGetCount(&mut count) != CUresult::CUDA_SUCCESS || count <= 0 {
            return Vec::new();
        }
    }

    let mut devices = Vec::with_capacity(count as usize);
    for ordinal in 0..count {
        let mut device: CUdevice = 0;
        unsafe {
            // SAFETY: ordinal 在驱动报告的合法区间内
            if cuDeviceGet(&mut device, ordinal) != CUresult::CUDA_SUCCESS {
                continue;
            }
        }

        let mut name_buf = [0i8; 256];
        let name = unsafe {
            // SAFETY: name_buf 为 256 字节可写缓冲，长度参数与之匹配
            if cuDeviceGetName(name_buf.as_mut_ptr(), name_buf.len() as i32, device)
                != CUresult::CUDA_SUCCESS
            {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(name_buf.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            }
        };

        devices.push(CudaDeviceInfo { ordinal, name });
    }
    devices
}

// ============================================================================
// 锁页主机内存 (Pinned Host Memory)
// ============================================================================

/// 一块由 CUDA 分配的锁页主机内存，可被 NVENC 作为系统内存输入缓冲直接读取
///
/// 这块内存由帧环持有并**跨帧复用**：生产者把整帧直接写进来，NVENC 引擎经
/// PCIe DMA 从同一个指针读帧（`backend.rs` 的系统内存输入缓冲路径）。
///
/// 分配与释放都自带幂等上下文绑定，是唯一允许脱离编码 actor 线程析构的 CUDA 资源
/// （设备块的最后一个 Arc 可能落在管线拆除线程或在途帧的析构线程）。
pub(crate) struct PinnedHost {
    ptr: *mut c_void,
    cap: usize,
    ctx: Arc<CudaContext>,
}

// SAFETY: ptr 由 cuMemAllocHost_v2 分配并随本结构独占持有，任何时刻只有一个所有者；
// 跨线程移动只转移所有权，不产生共享可变访问
unsafe impl Send for PinnedHost {}

impl std::fmt::Debug for PinnedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedHost")
            .field("cap", &self.cap)
            .finish()
    }
}

impl Drop for PinnedHost {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        // 唯一可能脱离编码 actor 线程析构的 CUDA 资源：设备块的最后一个 Arc 可能落在
        // 管线拆除线程或在途帧的析构线程（见 `PinnedFrame` 文档），幂等绑定兜底
        let _ = self.ctx.bind_to_thread();
        unsafe {
            // SAFETY: ptr 由 cuMemAllocHost_v2 成功分配且尚未释放，此处释放一次
            let _ = cuMemFreeHost(self.ptr);
        }
        self.ptr = std::ptr::null_mut();
    }
}

impl PinnedHost {
    /// 分配 `cap` 字节锁页内存；容量不足或驱动拒绝时返回错误，由调用方决定是否降级
    pub(crate) fn alloc(ctx: &Arc<CudaContext>, cap: usize) -> Result<Self, String> {
        if cap == 0 {
            return Err("锁页缓冲容量不能为 0".to_string());
        }
        ctx.bind_to_thread()
            .map_err(|e| format!("CUDA 上下文绑定失败: {e}"))?;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        unsafe {
            // SAFETY: 出参 ptr 由驱动写入；失败时不接管任何所有权
            let res = cuMemAllocHost_v2(&mut ptr, cap);
            if res != CUresult::CUDA_SUCCESS || ptr.is_null() {
                return Err(format!("cuMemAllocHost_v2 failed: {:?}", res));
            }
        }
        Ok(Self {
            ptr,
            cap,
            ctx: Arc::clone(ctx),
        })
    }

    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.cap
    }

    #[inline]
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.ptr as *const u8
    }

    /// 所属 CUDA 上下文的身份
    ///
    /// 上下文由本结构的 `Arc` 持有，块存活期间该地址不可能被复用，故可直接当域身份用
    pub(crate) fn domain_identity(&self) -> usize {
        Arc::as_ptr(&self.ctx) as usize
    }
}

// ============================================================================
// CUDA 显存槽位导入 (CUDA VRAM Slot)
// ============================================================================

/// 一枚外部显存槽位在 CUDA 侧的导入视图：外部显存、映射出的 RGBA8 2D 数组、
/// 跨进程时间线信号量三者成套
///
/// 句柄所有权：导入成功即归本结构独占，`Drop` 逆序销毁（信号量 → 数组 → 外部
/// 显存）；原始系统句柄（fd / Win32 HANDLE）在驱动接管后即刻关闭。CUDA 调用只
/// 发生在上下文已绑定的编码 actor 线程，`Send` 仅服务一次性所有权移交
pub(crate) struct VramSlot {
    ext_mem: CUexternalMemory,
    mipmap_array: CUmipmappedArray,
    pub(crate) cu_array: CUarray,
    pub(crate) ext_sem: CUexternalSemaphore,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

// SAFETY: 成员均为指针尺寸的驱动侧不透明句柄与整型，不承载 Rust 线程借用不变量；
// CUDA 调用的线程归属不属于内存安全约束，由编码 actor 单线程所有权保证（构造与
// Drop 都只发生在上下文已绑定的编码 actor 线程），Send 仅服务句柄值在线程间的
// 所有权移交
unsafe impl Send for VramSlot {}

impl Drop for VramSlot {
    fn drop(&mut self) {
        // SAFETY: 只在编码 actor 线程析构，CUDA 上下文已绑定；三个句柄均由导入
        // 成功时刻起归本结构独占（失败路径由 SlotImportGuard 收尾），且尚未销毁
        unsafe {
            if !self.ext_sem.is_null() {
                let _ = cuDestroyExternalSemaphore(self.ext_sem);
                self.ext_sem = std::ptr::null_mut();
            }
            if !self.mipmap_array.is_null() {
                let _ = cuMipmappedArrayDestroy(self.mipmap_array);
                self.mipmap_array = std::ptr::null_mut();
            }
            if !self.ext_mem.is_null() {
                let _ = cuDestroyExternalMemory(self.ext_mem);
                self.ext_mem = std::ptr::null_mut();
            }
        }
    }
}

struct SlotImportGuard {
    ext_mem: CUexternalMemory,
    mipmap_array: CUmipmappedArray,
    ext_sem: CUexternalSemaphore,
    mem_handle: Option<i64>,
    sem_handle: Option<i64>,
}

impl SlotImportGuard {
    fn new(mem_handle: i64, sem_handle: i64) -> Self {
        Self {
            ext_mem: std::ptr::null_mut(),
            mipmap_array: std::ptr::null_mut(),
            ext_sem: std::ptr::null_mut(),
            mem_handle: (mem_handle > 0).then_some(mem_handle),
            sem_handle: (sem_handle > 0).then_some(sem_handle),
        }
    }

    fn finish(mut self, cu_array: CUarray, width: u32, height: u32) -> VramSlot {
        let ext_mem = self.ext_mem;
        let mipmap_array = self.mipmap_array;
        let ext_sem = self.ext_sem;
        self.ext_mem = std::ptr::null_mut();
        self.mipmap_array = std::ptr::null_mut();
        self.ext_sem = std::ptr::null_mut();
        self.mem_handle = None;
        self.sem_handle = None;

        VramSlot {
            ext_mem,
            mipmap_array,
            cu_array,
            ext_sem,
            width,
            height,
        }
    }
}

impl Drop for SlotImportGuard {
    fn drop(&mut self) {
        // SAFETY: 只在编码 actor 线程析构，上下文已绑定；守卫登记的句柄归本守卫
        // 独占——mem_handle/sem_handle 是尚未接管的原始系统句柄（close_raw_handle
        // 自带正值守卫），ext_* 是导入成功后取得的 CUDA 句柄，销毁前未重复销毁
        unsafe {
            if let Some(h) = self.sem_handle.take() {
                close_raw_handle(h);
            }
            if let Some(h) = self.mem_handle.take() {
                close_raw_handle(h);
            }
            if !self.ext_sem.is_null() {
                let _ = cuDestroyExternalSemaphore(self.ext_sem);
            }
            if !self.mipmap_array.is_null() {
                let _ = cuMipmappedArrayDestroy(self.mipmap_array);
            }
            if !self.ext_mem.is_null() {
                let _ = cuDestroyExternalMemory(self.ext_mem);
            }
        }
    }
}

/// 把一枚显存槽位描述符整体导入 CUDA 侧：校验 → 接管句柄 → 导入外部显存并关闭
/// 原始 fd/HANDLE → 映射 RGBA8 数组（取 level 0）→ 导入时间线信号量
///
/// 全成全败：任一步失败由 `SlotImportGuard` 逆序回滚并报错；校验先于接管，
/// 校验失败时描述符仍是句柄所有者、由它自己的 Drop 收尾。成功则返回的
/// [`VramSlot`] 独占全部 CUDA 句柄，原始句柄的关闭责任随 `take_handles`
/// 一并移交，全链路恰好多对一，不存在双重关闭
pub(crate) fn import_cuda_slot(slot: &mut VramSlotDescriptor) -> Result<VramSlot, String> {
    let width = slot.width;
    let height = slot.height;
    let size = slot.size_bytes;

    // 先校验再接管：校验失败时描述符仍是句柄所有者，由其 Drop 统一关闭，避免半接管泄漏
    if slot.mem_handle <= 0 || slot.sem_handle <= 0 {
        return Err(format!(
            "Invalid slot handles: mem={}, sem={}",
            slot.mem_handle, slot.sem_handle
        ));
    }
    let (mem_handle, sem_handle) = slot.take_handles();

    let mut guard = SlotImportGuard::new(mem_handle, sem_handle);

    // 1. 导入外部显存 (Linux: dma-buf fd, Windows: Win32 HANDLE)
    //
    // `type_` 用字面值：`CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD` 与
    // `..._OPAQUE_WIN32` 在 CUDA 头里都是 **1**，cudarc 的 rustified enum 因同值把这两个
    // 变体都丢了，所以这里走本地等价布局 `ExternalMemoryHandleDesc`
    #[cfg(unix)]
    let mem_desc = ExternalMemoryHandleDesc {
        type_: 1,
        handle: ExternalMemoryHandleUnion {
            fd: mem_handle as i32,
        },
        size: size as u64,
        // CUDA_EXTERNAL_MEMORY_DEDICATED
        flags: 1,
        reserved: [0; 16],
    };

    #[cfg(windows)]
    let mem_desc = ExternalMemoryHandleDesc {
        type_: 1,
        handle: ExternalMemoryHandleUnion {
            win32: ExternalMemoryWin32Handle {
                handle: mem_handle as *mut c_void,
                name: std::ptr::null(),
            },
        },
        size: size as u64,
        flags: 1,
        reserved: [0; 16],
    };

    let mut ext_mem: CUexternalMemory = std::ptr::null_mut();
    unsafe {
        // SAFETY: mem_desc 完整初始化（`#[repr(C)]` 布局与 CUDA 头逐字段一致，
        // 见 `ffi::ExternalMemoryHandleDesc`），指针 cast 只改变静态类型；
        // 出参 ext_mem 校验非空
        let res = cuImportExternalMemory(
            &mut ext_mem,
            &mem_desc as *const ExternalMemoryHandleDesc as *const CUDA_EXTERNAL_MEMORY_HANDLE_DESC,
        );
        if res != CUresult::CUDA_SUCCESS || ext_mem.is_null() {
            return Err(format!(
                "cuImportExternalMemory failed ({}x{}): {:?}",
                width, height, res
            ));
        }
    }
    guard.ext_mem = ext_mem;

    // SAFETY: 导入成功后驱动全权接管该 fd 的生命周期（见上方导入调用），原始
    // 系统句柄此刻归调用方所有且尚未关闭；close_raw_handle 自带正值守卫
    if let Some(h) = guard.mem_handle.take() {
        unsafe {
            close_raw_handle(h);
        }
    }

    // 2. 映射为 CUDA 2D 纹理数组 (CUmipmappedArray -> CUarray)
    let mipmap_desc = CUDA_EXTERNAL_MEMORY_MIPMAPPED_ARRAY_DESC {
        offset: slot.offset,
        arrayDesc: CUDA_ARRAY3D_DESCRIPTOR {
            Width: width as usize,
            Height: height as usize,
            Depth: 0,
            Format: CUarray_format::CU_AD_FORMAT_UNSIGNED_INT8,
            NumChannels: COLOR_NUM_CHANNELS_RGBA,
            Flags: CUDA_ARRAY3D_COLOR_ATTACHMENT,
        },
        numLevels: 1,
        reserved: [0; 16],
    };

    let mut mipmap_array: CUmipmappedArray = std::ptr::null_mut();
    unsafe {
        // SAFETY: ext_mem 为有效外部显存；mipmap_desc 经格式完全初始化
        let res = cuExternalMemoryGetMappedMipmappedArray(&mut mipmap_array, ext_mem, &mipmap_desc);
        if res != CUresult::CUDA_SUCCESS || mipmap_array.is_null() {
            return Err(format!(
                "cuExternalMemoryGetMappedMipmappedArray failed ({}x{}): {:?}",
                width, height, res
            ));
        }
    }
    guard.mipmap_array = mipmap_array;

    let mut cu_array: CUarray = std::ptr::null_mut();
    unsafe {
        // SAFETY: 提取 mipmap level 0 对应的 CUarray 句柄
        let res = cuMipmappedArrayGetLevel(&mut cu_array, mipmap_array, 0);
        if res != CUresult::CUDA_SUCCESS || cu_array.is_null() {
            return Err(format!(
                "cuMipmappedArrayGetLevel failed ({}x{}): {:?}",
                width, height, res
            ));
        }
    }

    // 3. 导入时间线信号量 (Linux: POSIX fd, Windows: Win32 HANDLE)
    #[cfg(unix)]
    let sem_desc = CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC {
        type_:
            CUexternalSemaphoreHandleType::CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD,
        handle: CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC_st__bindgen_ty_1 {
            fd: sem_handle as i32,
        },
        flags: 0,
        reserved: [0; 16],
    };

    #[cfg(windows)]
    let sem_desc = CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC {
        type_: CUexternalSemaphoreHandleType::CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_WIN32,
        handle: CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC_st__bindgen_ty_1 {
            win32: CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC_st__bindgen_ty_1__bindgen_ty_1 {
                handle: sem_handle as *mut c_void,
                name: std::ptr::null(),
            },
        },
        flags: 0,
        reserved: [0; 16],
    };

    let mut ext_sem: CUexternalSemaphore = std::ptr::null_mut();
    unsafe {
        // SAFETY: sem_desc 为有效信号量描述；校验导入结果
        let res = cuImportExternalSemaphore(&mut ext_sem, &sem_desc);
        if res != CUresult::CUDA_SUCCESS || ext_sem.is_null() {
            return Err(format!(
                "cuImportExternalSemaphore failed ({}x{}): {:?}",
                width, height, res
            ));
        }
    }
    guard.ext_sem = ext_sem;

    // SAFETY: 同显存句柄——导入成功后驱动接管信号量生命周期，原始系统句柄归
    // 调用方所有且尚未关闭
    if let Some(h) = guard.sem_handle.take() {
        unsafe {
            close_raw_handle(h);
        }
    }

    Ok(guard.finish(cu_array, width, height))
}

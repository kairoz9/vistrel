//! 纯手写 NVENC 视频编码模块 (Pure Handwritten NVENC Subsystem)
//!
//! 零外部 FFI crate 依赖，原生动态加载驱动，解耦显存槽位持久预注册与单帧轻量映射。

#![expect(
    non_snake_case,
    reason = "文件内直接使用 NVENC / CUDA 头文件的原生命名 (nvEnc*, NV_ENC_*)"
)]

pub(crate) mod cuda;
pub(crate) mod dl;
#[expect(clippy::all, reason = "bindgen 生成代码按项目规则整体豁免")]
pub(crate) mod ffi;
pub(crate) mod session;
pub(crate) mod session_params;

/// 取出 NVENC 函数表中的必需函数指针。
///
/// 函数表由驱动侧 `NvEncodeAPICreateInstance` 填充，旧版驱动对新增槽位会填 NULL
/// 统一经此宏转换为可诊断错误，避免直接 `unwrap()` 在热路径上 panic。
///
/// 刻意定义在 `pub mod` 之后：子模块必须显式 `use crate::encode::hardware::nvenc::nvfn;`，
/// 依赖关系写在文件头部而不是藏在声明顺序里。
macro_rules! nvfn {
    ($driver:expr, $name:ident) => {
        $driver
            .api()
            .$name
            .ok_or_else(|| format!("当前 NVENC 驱动不支持 {}", stringify!($name)))?
    };
}

use nvfn;

use std::ffi::c_void;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use cudarc::driver::{CudaContext, CudaStream};

#[cfg(test)]
use crate::encode::encoder::BackendStageTimings;
use crate::encode::encoder::{EncodeBackendKind, VideoEncodeBackend};
use crate::encode::hardware::slots::{
    ActorStage, SlotStateBlock, SlotTable, VramSlotDescriptor, ensure_batch_geometry,
};
use crate::encode::pool::{HOST_SLOT_BYTES, HostBlock, POOL_DEPTH};
use crate::encode::{FramePayload, StreamingProfile};

pub(crate) use cuda::new_cuda_context;
use cuda::{PinnedHost, VramSlot, import_cuda_slot, wait_timeline_sem};
pub(crate) use dl::{NvencDriver, ensure_cuda_init, probe_cuda};
use ffi::*;
use session::NvencSession;

/// 一帧送入编码器的画面规格：宽高 + 行距 + 像素格式
///
/// 宽高只能来自 `StreamingProfile`——NVENC 要求 `inputWidth/Height` 与
/// `nvEncInitializeEncoder` 时一致，规格因此由档位派生，不独立给出
#[derive(Debug, Clone, Copy)]
pub(crate) struct InputSpec {
    pub(crate) width: usize,
    pub(crate) height: usize,
    /// 主机行距；CUDA 数组输入不吃它（驱动按数组尺寸取）
    pub(crate) pitch: usize,
    pub(crate) buffer_format: ffi::NV_ENC_BUFFER_FORMAT,
}

/// 一台 NVENC 编码会话的句柄，由 `nvEncOpenEncodeSessionEx` 产生
///
/// 构造成功即独占所有权：交给 [`NvencSession`](session::NvencSession) 之前的任何失败路径上掉出作用域
/// 都会销毁编码器，会话失败中途不再泄漏 NVENC 会话。自带驱动句柄，销毁时驱动
/// 库必定仍在——不依赖外部字段次序。
pub(crate) struct EncoderHandle {
    raw: *mut c_void,
    driver: NvencDriver,
}

impl Drop for EncoderHandle {
    fn drop(&mut self) {
        let Some(destroy) = self.driver.api().nvEncDestroyEncoder else {
            return;
        };
        unsafe {
            // SAFETY: raw 由 nvEncOpenEncodeSessionEx 成功创建且尚未销毁；
            // driver 由本字段持有，函数表在调用期间有效
            let _ = destroy(self.raw);
        }
    }
}

/// 一个输出码流缓冲句柄，由 `nvEncCreateBitstreamBuffer` 产生
///
/// 生命周期严格短于 `EncoderHandle`：`nvEncDestroyBitstreamBuffer` 需要 encoder。
#[derive(Debug, Clone, Copy)]
struct BitstreamHandle(*mut c_void);

/// 一个输入资源的注册句柄，由 `nvEncRegisterResource` 产生
///
/// 与 `CUarray` 在底层同为指针，但互不通用——这正是本类型存在的理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredResource(*mut c_void);

// 编译期守卫：句柄一旦变回 Copy/Clone，就能复制出第二个「所有者」，
// Drop 逻辑随即失效（重复销毁）。冲突即编译错误
const _: () = {
    #[expect(dead_code, reason = "仅作为编译期约束载体，无运行时调用方")]
    trait SingleOwner {}
    impl<T: Copy> SingleOwner for T {}
    impl SingleOwner for EncoderHandle {}
};

impl EncoderHandle {
    /// 出参校验：驱动在失败路径上可能留下未写入的 `null`
    #[inline]
    fn from_raw(driver: NvencDriver, raw: *mut c_void) -> Option<Self> {
        (!raw.is_null()).then_some(Self { raw, driver })
    }

    #[inline(always)]
    pub(crate) fn as_raw(&self) -> *mut c_void {
        self.raw
    }
}

impl BitstreamHandle {
    #[inline]
    fn from_raw(raw: *mut c_void) -> Option<Self> {
        (!raw.is_null()).then_some(Self(raw))
    }

    #[inline(always)]
    pub(crate) fn as_raw(self) -> *mut c_void {
        self.0
    }
}

impl RegisteredResource {
    #[inline]
    fn from_raw(raw: *mut c_void) -> Option<Self> {
        (!raw.is_null()).then_some(Self(raw))
    }

    #[inline(always)]
    pub(crate) fn as_raw(self) -> *mut c_void {
        self.0
    }
}

/// 一个已导入的显存槽位，连同它在当前会话上的 NVENC 注册句柄
struct SlotEntry {
    cuda: VramSlot,
    /// 注册句柄；空表示尚未登记或已归还
    registered: Option<RegisteredResource>,
}

impl SlotEntry {
    fn new(cuda: VramSlot) -> Self {
        Self {
            cuda,
            registered: None,
        }
    }
}

impl Drop for SlotEntry {
    fn drop(&mut self) {
        // 释放点是唯一能检查「句柄是否已归还」的地方：带着注册去释放 CUarray，
        // 驱动就会拿着指向已释放显存的注册继续存活
        debug_assert!(
            self.registered.is_none(),
            "槽位在仍注册时被释放：句柄必须先通过 take 归还"
        );
    }
}

/// NVENC 硬件编码后端门面
///
/// 注册的建立与撤销只发生在控制面：`new` / `import_vram_slots` / `clear_vram_slots` /
/// `reconfigure`。编码路径不判断、不分配、不注册——查不到句柄就报错。
pub(crate) struct NvencBackend {
    cuda_ctx: Arc<CudaContext>,
    session: NvencSession,
    /// 池块基址（池所有权清单）：跨会话存活，重构几何时据此重建输入缓冲
    block_ptrs: Vec<*const u8>,
    slots: SlotTable<SlotEntry>,
    /// 槽位所有权共享块：编码出口归还位，与 Actor 同代
    slot_state: &'static SlotStateBlock,
    /// 必须排在 session / slots 之后（字段按声明顺序析构）：NVENC 在 set_io 时
    /// 拷贝句柄值并在会话存续期间持续使用，流对象必须比会话活得久（先销毁流会导致编码
    /// 路径 SIGSEGV，本机实验证实）；cudarc 流内部持有的 Arc<CudaContext> 也是全后端
    /// CUDA 资源析构期间唯一的上下文锚——排在它们之后才释放。
    stream: Arc<CudaStream>,
    /// 测试并发闸（见 [`crate::testing`]）：声明在最后，字段按声明顺序析构，
    /// 闸位等整套 CUDA 资源清完才归还。
    #[cfg(test)]
    #[allow(dead_code)]
    gpu_slot: Arc<crate::testing::GpuSlot>,
}

impl Drop for NvencBackend {
    fn drop(&mut self) {
        // 固定次序析构：注册句柄 → 槽位显存 → 会话（输入缓冲随会话析构，见
        // `NvencSession::drop`）。只在编码 actor 线程析构，上下文已绑定；逐字段借用：
        // 各处互不相交，比走方法少一层间接
        for entry in self.slots.values_mut() {
            if let Some(registered) = entry.registered.take() {
                self.session.unregister_resource(registered);
            }
        }
        self.slots.clear();
    }
}

/// 排空等待的超时（2s）：已远超一帧的编码时间，超了就是上下文侧出了问题。
/// 与 Vulkan 编码路径的等待超时同一纪律——等待必须有界，失败即弃置
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// 排空轮询间隔：事件查询是纯状态读取的非阻塞调用，轮询期间 CPU 占用可忽略
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

impl NvencBackend {
    /// 按档位构建（主机帧一律 NV12，行距即宽度）
    ///
    /// 帧块必须分配在**本后端自己的 CUDA 上下文**上，且几何一变也不失效
    /// （`cuda_ctx` 不随会话重建），因此构造时分配一次，随构造一并交回，由调用方
    /// 装进帧池。分配失败即整体构造失败。
    pub(crate) fn new(
        profile: StreamingProfile,
        slot_state: &'static SlotStateBlock,
    ) -> Result<(Self, Vec<HostBlock>), String> {
        let driver = NvencDriver::load()?;
        let cuda_ctx = new_cuda_context(crate::encode::hardware::nvenc_device_ordinal())?;
        let stream = cuda_ctx
            .new_stream()
            .map_err(|e| format!("CUDA 流创建失败: {e}"))?;

        // 池块先分配：会话构造期就要用它们的基址包装成系统内存输入缓冲，
        // 会话因此不会出现「已建但还没有输入缓冲」的状态
        let (blocks, block_ptrs) = allocate_host_frames(&cuda_ctx)?;
        let session = NvencSession::new(
            driver,
            cuda_ctx.clone(),
            profile,
            &block_ptrs,
            stream.cu_stream(),
        )?;

        Ok((
            Self {
                cuda_ctx,
                session,
                block_ptrs,
                slots: SlotTable::new(),
                slot_state,
                stream,
                // 测试并发闸：字段按声明顺序析构，声明在最后才能等整套 CUDA
                // 资源清完之后再归还闸位
                #[cfg(test)]
                gpu_slot: crate::testing::GpuSlot::acquire(),
            },
            blocks,
        ))
    }

    /// 显存槽位编码：等待生产者就绪 → 映射 → 提交 → 锁定 → 拷出 → 解映射
    ///
    /// 前置校验失败（查不到槽位/未注册/几何过期）与编码失败统一以 Err 出口；
    /// 所有权归还由调用方在出口统一处理，本方法只写共享块的阶段码
    fn encode_vram_slot(
        &mut self,
        slot_idx: u32,
        sync_value: u64,
        force_idr: bool,
        frame_idx: u64,
    ) -> Result<(Vec<u8>, bool), String> {
        // 1. 取槽位的时间线信号量、注册句柄与几何（都是槽位自己的属性）
        let entry = self
            .slots
            .get(slot_idx)
            .ok_or_else(|| format!("Vram slot {slot_idx} not found in pre-registered slots"))?;
        let registered = entry
            .registered
            .ok_or_else(|| format!("Vram slot {slot_idx} 尚未在当前会话上注册"))?;
        let (ext_sem, slot_width, slot_height) =
            (entry.cuda.ext_sem, entry.cuda.width, entry.cuda.height);
        // 兜底不变量：旧代帧已在 Actor 入口被拒收，导入期整批校验也保证
        // 槽位几何与会话一致，到这里还不符只可能是前两道防线的漏网。核对仍必须在
        // 等待与映射之前，让漏网帧以可诊断的错误丢在入口而不是交给驱动
        let profile = self.session.profile();
        let (profile_width, profile_height) = (profile.width() as u32, profile.height() as u32);
        if slot_width != profile_width || slot_height != profile_height {
            return Err(format!(
                "Vram slot {slot_idx} 几何 {slot_width}x{slot_height} 与编码器 {profile_width}x{profile_height} 不一致"
            ));
        }
        // 2. 时间线信号量等待：异步入队进专属 CUDA Stream，由 GPU 硬件流调度
        self.slot_state.set_stage(ActorStage::WaitTimeline);
        wait_timeline_sem(&self.cuda_ctx, ext_sem, sync_value, self.stream.cu_stream())?;
        // 3. 映射 → 提交 → 锁定 → 拷出 → 解映射
        let spec = InputSpec {
            width: slot_width as usize,
            height: slot_height as usize,
            pitch: 0,
            buffer_format: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR,
        };
        self.session
            .encode_from_resource(registered, spec, force_idr, frame_idx, self.slot_state)
    }
}

/// [`VideoEncodeBackend`] 落点：NVENC 的 CUDA 上下文全部内化于本 adapter，绝不外泄——
/// 门面与调用方只见 trait 方法。
impl VideoEncodeBackend for NvencBackend {
    fn kind(&self) -> EncodeBackendKind {
        EncodeBackendKind::Nvenc
    }

    fn coded_align(&self) -> (u32, u32) {
        // NVENC 会话按显示尺寸建（inputWidth 必须与 nvEncInitializeEncoder 一致），无设备粒度对齐
        (1, 1)
    }

    /// 批量注册导入显存槽位并一次性完成 NVENC 资源预注册
    fn import_vram_slots(&mut self, descriptors: &mut [VramSlotDescriptor]) -> Result<(), String> {
        if descriptors.is_empty() {
            return Err("待导入显存描述符列表为空".to_string());
        }

        // 几何核验先于任何状态变更：不符批次整批拒绝时既有槽位表原样保留，
        // 与 import_all 的全成全败同层
        let profile = self.session.profile();
        ensure_batch_geometry(descriptors, profile.width() as u32, profile.height() as u32)?;

        // 阶段零：整表清空——注册句柄先归还再释放 CUarray，否则驱动会拿着已销毁的
        // 显存去注销。只清槽位：系统内存输入缓冲不经注册表，不受影响。
        // 旧槽位不保留可用性：重导入是整批事务，失败即整批报错降级 RAM 回读
        self.clear_vram_slots();

        // 阶段一：导入外部显存
        if let Err(e) = self.slots.import_all(descriptors, |desc| {
            import_cuda_slot(desc).map(SlotEntry::new)
        }) {
            log::error!("显存槽位导入失败: {}", e);
            return Err(e);
        }
        // 阶段二：为全部新槽位建立预注册，失败即整表清空。
        // 注册期间句柄不落表：全部成功才一次性写入，任一失败则把已建立的逐个归还
        let mut registered: Vec<(u32, RegisteredResource)> = Vec::with_capacity(self.slots.len());
        let mut failure = None;
        for (slot_idx, entry) in self.slots.iter() {
            let slot = &entry.cuda;
            match self.session.register_resource(
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY,
                slot.cu_array as *mut c_void,
                slot.width,
                slot.height,
                0,
                NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR,
            ) {
                Ok(handle) => registered.push((slot_idx, handle)),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }

        // 任一注册失败：归还已建立的句柄后整表清空，保持「槽位只在当前 encoder 上有效」
        if let Some(e) = failure {
            for (_, handle) in registered {
                self.session.unregister_resource(handle);
            }
            self.clear_vram_slots();
            return Err(e);
        }

        for (slot_idx, handle) in registered {
            if let Some(entry) = self.slots.get_mut(slot_idx) {
                entry.registered = Some(handle);
            }
        }
        Ok(())
    }

    /// 清空槽位表并释放底层显存句柄
    ///
    /// 先解注册再释放 CUarray，否则驱动会拿着已销毁的显存去注销。
    /// 只清槽位：系统内存输入缓冲是持久句柄，不挂注册表，原样保留。
    fn clear_vram_slots(&mut self) {
        for entry in self.slots.values_mut() {
            if let Some(registered) = entry.registered.take() {
                self.session.unregister_resource(registered);
            }
        }
        self.slots.clear();
    }

    /// 重新配置编码器分辨率与码率
    ///
    /// 同 codec 的分辨率 / 帧率 / 码率变更全部走会话内 `nvEncReconfigureEncoder`
    /// 只有换 codec 才重建：`NvencSession` 若在重建路径上仍按旧几何持有输入登记，
    /// 旧块仍然有效，因此重配恒返回空批（块绑的是 CUDA 上下文，不是会话）
    fn reconfigure(&mut self, new_profile: StreamingProfile) -> Result<Vec<HostBlock>, String> {
        if self.session.profile().codec() == new_profile.codec() {
            self.session.reconfigure(new_profile)?;
            return Ok(Vec::new());
        }

        // 换 codec 必须重建会话。先把新会话完整建好，成功之后才动输入缓冲与旧会话——
        // 任何一步失败都保持旧状态可用（codec 变更只会发生在会话建立的协商期，
        // 不在逐帧热路径上）
        let driver = self.session.driver();
        let new_session = NvencSession::new(
            driver,
            self.cuda_ctx.clone(),
            new_profile,
            &self.block_ptrs,
            self.stream.cu_stream(),
        )?;

        // 槽位绑的是旧 encoder 的句柄，生产者会在同一轮重建采集管线时
        // `clearVramSlots` + `importVramSlots`。这里全部作废，让
        // 「槽位只在当前 encoder 上有效」成为不变量，而不是靠几何比较去猜哪些还有用
        self.clear_vram_slots();

        // 新会话已带着新几何的输入缓冲建好，此时才切换：旧会话连同它的输入缓冲
        // 一起析构，不存在「旧批先毁、新批未建」的窗口
        self.session = new_session;
        // 上下文未变，老块仍属于本后端的 CUDA 域，继续用
        Ok(Vec::new())
    }

    /// 动态修改目标码率
    fn set_bitrate(&mut self, bps: u32) -> Result<(), String> {
        if bps == 0 {
            return Ok(());
        }
        let mut new_profile = *self.session.profile();
        new_profile.set_bitrate(bps as usize);
        self.session.reconfigure(new_profile)
    }

    /// 编码统一入口：派发 CUDA 主机帧或 Vulkan Vram
    fn encode(
        &mut self,
        payload: &FramePayload,
        force_idr: bool,
        frame_idx: u64,
    ) -> Result<(Bytes, bool), String> {
        let (nalu, is_key) = match payload {
            FramePayload::Host { frame } => {
                let host = frame
                    .cuda_host()
                    .ok_or_else(|| "NVENC 收到非 CUDA 主机帧".to_string())?;
                // 池块已按 CUDADEVICEPTR 登记（UVA 主机别名，零拷贝）：每帧映射后由
                // 引擎经 PCIe 直读帧内容，没有 H2D 拷贝也没有主机同步点
                self.session
                    .encode_host(host.as_ptr(), force_idr, frame_idx, self.slot_state)?
            }
            FramePayload::VramSlot {
                slot_idx,
                sync_value,
            } => {
                // 编码（含前置校验）无论成败，槽位所有权都在出口归还：纹理要么没被
                // 碰过（前置失败），要么已被解映射（同步提交，出口即完成）。
                let result = self.encode_vram_slot(*slot_idx, *sync_value, force_idr, frame_idx);
                self.slot_state.mark_free(*slot_idx);
                result?
            }
        };
        Ok((Bytes::from(nalu), is_key))
    }

    /// 排空：确认本流上的全部在途工作已经完成
    ///
    /// 记录事件后轮询到 deadline，健康路径一次查询即返回。超时与错误都成为
    /// **有界**的失败而不是新的挂死点。失败即上下文不可信：调用方据此整体弃置
    /// （`mem::forget`，永不析构——病态上下文上的释放调用可能同样卡死线程）
    fn drain(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        let event = match self.stream.record_event(None) {
            Ok(event) => event,
            Err(e) => return Err(format!("排空事件记录失败: {e}")),
        };
        loop {
            match event.try_is_complete() {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => {
                    // 上下文粘性错误：事件句柄一并弃置，失败后不再触碰驱动
                    std::mem::forget(event);
                    return Err(format!("排空查询失败: {e}"));
                }
            }
            if Instant::now() >= deadline {
                std::mem::forget(event);
                return Err(format!(
                    "排空 {}s 内未完成（上下文可能挂死）",
                    DRAIN_TIMEOUT.as_secs()
                ));
            }
            std::thread::sleep(DRAIN_POLL_INTERVAL);
        }
    }

    #[cfg(test)]
    fn benchmark_stages(&self) -> BackendStageTimings {
        // 系统内存输入路径没有 H2D：input_copy_ns 固定为 0 是路径事实
        self.session.benchmark_stages()
    }
}

/// 帧块：CUDA 锁页内存，最大几何容量（一次性）
///
/// 必须建在**本后端自己的 CUDA 上下文**上；整批分配失败时已分配的块随 `?` 逆序释放。
/// 基址随批返回：后端据此为每块创建 NVENC 系统内存输入缓冲
fn allocate_host_frames(
    cuda_ctx: &Arc<CudaContext>,
) -> Result<(Vec<HostBlock>, Vec<*const u8>), String> {
    let mut blocks = Vec::with_capacity(POOL_DEPTH);
    let mut ptrs = Vec::with_capacity(POOL_DEPTH);
    for _ in 0..POOL_DEPTH {
        let pinned = PinnedHost::alloc(cuda_ctx, HOST_SLOT_BYTES)?;
        ptrs.push(pinned.as_ptr());
        blocks.push(HostBlock::Cuda(pinned));
    }
    Ok((blocks, ptrs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::encoder::VideoEncodeBackend;
    use crate::encode::pool::FramePool;
    use crate::encode::pool::test_support::host_payload;
    use crate::encode::{StreamingProfile, VideoCodec};

    /// 分辨率切换必须走会话内重配并真的产出新几何
    ///
    /// 回归目标：分辨率切换必须走会话内重配——新建会话会与旧会话并存、撞 NVENC
    /// 并发会话配额（`nvEncInitializeEncoder` 回 NV_ENC_ERR_OUT_OF_MEMORY）；
    /// 会话建立时已声明最大尺寸，`nvEncReconfigureEncoder` 可就地改几何。
    /// 本用例要求每个档位重配后都能立刻编出非空 IDR
    #[test]
    #[ignore = "requires GPU"]
    fn test_reconfigure_across_resolutions_yields_idr() {
        let p1080 = StreamingProfile::new(1920, 1080, 30, VideoCodec::Av1);
        let (mut backend, blocks) = match NvencBackend::new(p1080, SlotStateBlock::new_leaked()) {
            Ok(pair) => pair,
            Err(e) => {
                println!("硬编不可用（{e}），跳过分辨率重配用例");
                return;
            }
        };
        let pool = FramePool::new();
        pool.attach(blocks);

        let mut failures = Vec::new();
        for (w, h) in [
            (1280usize, 720usize),
            (1600, 900),
            (2560, 1440),
            (1920, 1080),
        ] {
            let target = StreamingProfile::new(w, h, 30, VideoCodec::Av1);
            if let Err(e) = backend.reconfigure(target) {
                println!("重配 → {w}x{h}: 失败（{e}）");
                failures.push((w, h, e));
                continue;
            }
            // 重配后必须立刻能编出关键帧：分辨率变了而码流还是旧几何，
            // 解码端就会一直等到序列头
            let payload = host_payload(&pool, vec![128u8; w * h * 3 / 2]);
            match backend.encode(&payload, true, 1) {
                Ok((data, is_key)) => {
                    assert!(!data.is_empty(), "{w}x{h} 重配后码流不得为空");
                    assert!(is_key, "{w}x{h} 强制 IDR 后必须报告关键帧");
                    println!("重配 → {w}x{h}: 成功，IDR {} 字节", data.len());
                }
                Err(e) => {
                    println!("重配 → {w}x{h}: 编码失败（{e}）");
                    failures.push((w, h, e));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "分辨率切换必须全部可用（原地重配），实际失败: {failures:?}"
        );
    }

    #[test]
    fn test_ffi_struct_sizes_match_official_abi() {
        assert_eq!(std::mem::size_of::<ffi::NV_ENC_INITIALIZE_PARAMS>(), 1808);
        assert_eq!(std::mem::size_of::<ffi::NV_ENC_CONFIG>(), 3584);
        assert_eq!(std::mem::size_of::<ffi::NV_ENC_RC_PARAMS>(), 128);
        assert_eq!(
            std::mem::size_of::<ffi::NV_ENC_CONFIG_H264_VUI_PARAMETERS>(),
            112
        );
        assert_eq!(std::mem::size_of::<ffi::NV_ENC_CONFIG_H264>(), 1792);
        assert_eq!(std::mem::size_of::<ffi::NV_ENC_CONFIG_HEVC>(), 1560);
        assert_eq!(std::mem::size_of::<ffi::NV_ENC_CONFIG_AV1>(), 1552);
        assert_eq!(std::mem::size_of::<ffi::NV_ENC_CODEC_CONFIG>(), 1792);
        assert_eq!(std::mem::size_of::<ffi::NV_ENC_PIC_PARAMS>(), 3360);
        assert_eq!(std::mem::size_of::<ffi::NV_ENC_LOCK_BITSTREAM>(), 1552);
    }

    /// CUDA 外部信号量参数：驱动按官方头文件布局读取 `flags`，
    /// `params` 必须是 72 字节（fence 8 + nvSciSync 8 + keyedMutex 16 + reserved[10] 40），
    /// 否则 `flags` 偏移错位。
    ///
    /// 这三个数字来自 `cuda.h` 的 clang 布局计算（生成产物的 layout_tests 亦断言
    /// 72 / 144 / 72）。
    #[test]
    fn test_cuda_external_semaphore_params_abi() {
        use std::mem::offset_of;

        assert_eq!(
            std::mem::size_of::<ffi::CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS_st__bindgen_ty_1>(),
            72
        );
        assert_eq!(
            std::mem::size_of::<ffi::CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS_st__bindgen_ty_1>(),
            72
        );
        assert_eq!(
            std::mem::size_of::<ffi::CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS>(),
            144
        );
        assert_eq!(
            std::mem::size_of::<ffi::CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS>(),
            144
        );
        assert_eq!(
            offset_of!(ffi::CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS, flags),
            72
        );
        assert_eq!(
            offset_of!(ffi::CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS, flags),
            72
        );
        assert_eq!(std::mem::size_of::<ffi::CUuuid>(), 16);
    }

    /// 总尺寸相等不足以证明 ABI 正确：真正被驱动读取的是字段偏移
    #[test]
    fn test_ffi_field_offsets_match_official_abi() {
        use std::mem::offset_of;

        // NV_ENC_PIC_PARAMS：驱动读取的输入/输出描述字段
        assert_eq!(offset_of!(ffi::NV_ENC_PIC_PARAMS, inputWidth), 4);
        assert_eq!(offset_of!(ffi::NV_ENC_PIC_PARAMS, encodePicFlags), 16);
        assert_eq!(offset_of!(ffi::NV_ENC_PIC_PARAMS, inputTimeStamp), 24);
        assert_eq!(offset_of!(ffi::NV_ENC_PIC_PARAMS, inputBuffer), 40);
        assert_eq!(offset_of!(ffi::NV_ENC_PIC_PARAMS, outputBitstream), 48);
        assert_eq!(offset_of!(ffi::NV_ENC_PIC_PARAMS, bufferFmt), 64);
        assert_eq!(offset_of!(ffi::NV_ENC_PIC_PARAMS, pictureType), 72);
        assert_eq!(offset_of!(ffi::NV_ENC_PIC_PARAMS, codecPicParams), 80);

        // NV_ENC_LOCK_BITSTREAM：驱动回填的码流视图
        assert_eq!(offset_of!(ffi::NV_ENC_LOCK_BITSTREAM, outputBitstream), 8);
        assert_eq!(
            offset_of!(ffi::NV_ENC_LOCK_BITSTREAM, bitstreamSizeInBytes),
            36
        );
        assert_eq!(
            offset_of!(ffi::NV_ENC_LOCK_BITSTREAM, bitstreamBufferPtr),
            56
        );
        assert_eq!(offset_of!(ffi::NV_ENC_LOCK_BITSTREAM, pictureType), 64);

        // NV_ENC_REGISTER_RESOURCE：槽位预注册的入参
        assert_eq!(offset_of!(ffi::NV_ENC_REGISTER_RESOURCE, resourceType), 4);
        assert_eq!(offset_of!(ffi::NV_ENC_REGISTER_RESOURCE, width), 8);
        assert_eq!(offset_of!(ffi::NV_ENC_REGISTER_RESOURCE, pitch), 16);
        assert_eq!(
            offset_of!(ffi::NV_ENC_REGISTER_RESOURCE, resourceToRegister),
            24
        );
        assert_eq!(
            offset_of!(ffi::NV_ENC_REGISTER_RESOURCE, registeredResource),
            32
        );
        assert_eq!(offset_of!(ffi::NV_ENC_REGISTER_RESOURCE, bufferFormat), 40);

        // NV_ENC_INITIALIZE_PARAMS：encodeConfig 指针必须在 88 偏移处
        assert_eq!(offset_of!(ffi::NV_ENC_INITIALIZE_PARAMS, encodeWidth), 36);
        assert_eq!(offset_of!(ffi::NV_ENC_INITIALIZE_PARAMS, encodeConfig), 88);
        assert_eq!(offset_of!(ffi::NV_ENC_INITIALIZE_PARAMS, tuningInfo), 136);
    }

    #[test]
    #[ignore = "requires GPU"]
    fn test_raw_nvenc_dynamic_load() {
        if let Err(e) = ensure_cuda_init() {
            println!("Skipping test: CUDA driver not available ({e})");
            return;
        }

        let nvenc_driver = match NvencDriver::load() {
            Ok(drv) => drv,
            Err(e) => {
                println!("Skipping test: NVENC driver not available ({})", e);
                return;
            }
        };
        assert_eq!(
            nvenc_driver.api().version,
            ffi::NV_ENCODE_API_FUNCTION_LIST_VER
        );
        println!("Raw NVENC and CUDA drivers dynamically loaded successfully!");
    }

    #[test]
    #[ignore = "requires GPU"]
    fn test_raw_cuda_context_initialization() {
        let ctx = match new_cuda_context(0) {
            Ok(c) => c,
            Err(e) => {
                println!("Skipping test: RawCudaContext init failed ({})", e);
                return;
            }
        };
        assert!(!ctx.cu_ctx().is_null());
        assert!(ctx.bind_to_thread().is_ok());
        println!("Raw CUDA Context on device 0 initialized and bound successfully!");
    }

    #[test]
    #[ignore = "requires GPU"]
    fn test_raw_nvenc_encode_pinned_ram_codecs() {
        let codecs = [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1];

        for codec in codecs {
            let mut profile = StreamingProfile::new(64, 64, 60, codec);
            profile.set_bitrate(1_000_000);
            // 编码几何的唯一来源是 `session.profile`：64x64 无下限钳制，原样保留，
            // 帧缓冲按档案尺寸分配
            let (width, height) = (profile.width(), profile.height());

            let (mut backend, blocks) =
                match NvencBackend::new(profile, SlotStateBlock::new_leaked()) {
                    Ok(pair) => pair,
                    Err(e) => {
                        println!(
                            "Codec {:?} initialization failed or unsupported on this GPU: {}",
                            codec, e
                        );
                        continue;
                    }
                };
            let pool = FramePool::new();
            pool.attach(blocks);

            // 输入一律 NV12：Y 平面 + 交织 UV 平面
            let frame_bytes = vec![128u8; width * height * 3 / 2];
            let payload = host_payload(&pool, frame_bytes);

            let (data, is_key) = backend
                .encode(&payload, true, 1)
                .expect("Raw NVENC encode failed");
            assert!(!data.is_empty(), "NALU should not be empty");
            assert!(is_key, "force_idr=true 且为首帧必须返回 is_key=true");
            println!(
                "Raw NVENC {:?} successfully encoded host frame! Output NALU size: {} bytes, is_key: {}",
                codec,
                data.len(),
                is_key
            );
        }
    }

    #[test]
    #[ignore = "requires GPU"]
    fn test_raw_nvenc_consecutive_encode_recycles_bitstream() {
        let mut profile = StreamingProfile::new(64, 64, 60, VideoCodec::H264);
        profile.set_bitrate(1_000_000);
        // 几何取自钳制后的 profile，与编码路径的取用点保持一致
        let (width, height) = (profile.width(), profile.height());

        let (mut backend, blocks) = match NvencBackend::new(profile, SlotStateBlock::new_leaked()) {
            Ok(pair) => pair,
            Err(e) => {
                println!("Skipping test: NVENC not available ({})", e);
                return;
            }
        };
        let pool = FramePool::new();
        pool.attach(blocks);

        // 输入一律 NV12：Y 平面 + 交织 UV 平面
        let frame_bytes = vec![128u8; width * height * 3 / 2];
        let payload = host_payload(&pool, frame_bytes);

        // 首帧强制 IDR，随后两帧为 P 帧：验证码流句柄的「取用 → 提交 → 锁定 → 拷出 → 归还」
        // 流程在多帧之间正确复用，且帧型判定取自驱动回填的 pictureType 而非自行解析 NALU
        let (nalu1, is_key1) = backend.encode(&payload, true, 1).expect("首帧编码失败");
        assert!(!nalu1.is_empty(), "首帧 NALU 不应为空");
        assert!(is_key1, "force_idr=true 且为首帧必须返回 is_key=true");

        let (nalu2, is_key2) = backend.encode(&payload, false, 2).expect("第二帧编码失败");
        assert!(!nalu2.is_empty(), "第二帧 NALU 不应为空");
        assert!(!is_key2, "未强制 IDR 的后续帧不应报告为关键帧");

        let (nalu3, is_key3) = backend.encode(&payload, false, 3).expect("第三帧编码失败");
        assert!(!nalu3.is_empty(), "第三帧 NALU 不应为空");
        assert!(!is_key3);
    }
}

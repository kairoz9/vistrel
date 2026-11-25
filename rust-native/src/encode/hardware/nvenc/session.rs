//! NVENC 会话运行时 (Session Runtime)
//!
//! 会话类型的资源注册/映射、码流池、编码提交、IO 流绑定与重配；
//! 构造期流程（开会话、输入格式查询、编码参数构建）见 `session_params`。

use std::ffi::c_void;
use std::sync::Arc;

use cudarc::driver::CudaContext;

use super::cuda::PinnedHost;
use super::dl::NvencDriver;
use super::ffi::*;
use super::session_params::{
    apply_refresh_and_level, build_encoder_config, calculate_dar, open_encoder,
};
use super::{BitstreamHandle, EncoderHandle, InputSpec, RegisteredResource};
#[cfg(test)]
use crate::encode::encoder::BackendStageTimings;
use crate::encode::hardware::nvenc::nvfn;
use crate::encode::hardware::nvenc::session_params::build_pic_params;
use crate::encode::hardware::slots::{ActorStage, SlotStateBlock};
use crate::encode::{StreamingProfile, VideoCodec};

// ============================================================================
// 编码器会话 (NVENC Session)
// ============================================================================

/// 读取驱动侧最近一次错误描述
///
/// 既有会话另有自己的一条查询路径；构造期会话尚未成形，因此单独提成自由
/// 函数，让输入缓冲创建失败也能带上驱动细节
pub(crate) fn last_error(driver: NvencDriver, encoder: &EncoderHandle) -> String {
    let Some(get_error) = driver.api().nvEncGetLastErrorString else {
        return String::new();
    };
    unsafe {
        // SAFETY: encoder 为有效会话句柄；返回值为驱动内部字符串，仅在本调用内读取
        let ptr = get_error(encoder.as_raw());
        if ptr.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

/// 单帧编码耗时告警阈值（毫秒）
///
/// 独立告警阈值，不与任何跨语言常量耦合：持续超过它说明消费已跟不上推送节奏，
/// 槽位归还越拖越久、心跳超时正在逼近。数值取约 3 帧 @60fps 的经验口径
const SLOW_ENCODE_THRESHOLD_MS: u128 = 50;

/// 一次在途编码的收尾凭证：已锁定 ⇒ 必然解锁，已映射 ⇒ 必然解映射
///
/// 不含码流句柄的归还——句柄按值进出，由调用方在本函数返回后放回槽位
struct InflightEncode<'a> {
    encoder: &'a EncoderHandle,
    driver: NvencDriver,
    handle: BitstreamHandle,
    /// 显存槽位与主机帧（登记式输入）路径的每帧映射；无映射的路径为 `None`
    mapped: Option<*mut c_void>,
    /// `nvEncLockBitstream` 成功后置位：缓冲此刻租借给了 CPU，Drop 需先解锁
    locked: bool,
}

impl Drop for InflightEncode<'_> {
    fn drop(&mut self) {
        if self.locked
            && let Some(unlock) = self.driver.api().nvEncUnlockBitstream
        {
            unsafe {
                // SAFETY: handle 为 nvEncLockBitstream 成功锁定且尚未解锁的缓冲；
                // encoder 由借用保证存活
                let _ = unlock(self.encoder.as_raw(), self.handle.as_raw());
            }
        }
        // 两个资源必须成对收尾：只解映射会泄漏码流句柄，只归还句柄会留下永久映射
        if let Some(mapped) = self.mapped {
            unmap_input(self.encoder, self.driver, mapped);
        }
    }
}

/// 一个主机帧块的登记式输入：帧池块与它在本次会话上的注册句柄
///
/// 块按 `CUDADEVICEPTR` 登记——UVA 下锁页内存的基址即设备可访指针，NVENC 经 PCIe
/// 直读帧内容，零拷贝。**不得改回 `nvEncCreateInputBuffer(pSysMemBuffer)` 创建式
/// 输入**：该形态在本机 Linux 驱动上静默产出空壳码流（GL 黑屏排查时的真机实证），
/// 且登记资源类型枚举里本就不存在 SYSMEM。
///
/// 句柄由 `nvEncRegisterResource` 创建、必须由本会话注销；`block` 的 `Arc` 让会话
/// 自己钉住内存，注销先于最后一个引用释放。
struct DeviceInput {
    /// 帧池块：会话持有强引用，保证登记与逐帧映射期间基址不悬空
    block: Arc<PinnedHost>,
    /// 本会话上的登记句柄
    registered: RegisteredResource,
}

/// 一个已开成的 NVENC 编码会话：驱动函数表、会话句柄、登记式输入与编码配置
///
/// 只在 `nvEncOpenEncodeSessionEx` 成功后构造，不存在空会话状态；Drop 按确定性
/// 次序收尾（码流缓冲 → 输入登记 → encoder 句柄随字段析构自收）。同步编码
/// （提交即锁定）决定在途至多一帧，互斥由 `&mut self` 表达；CUDA/NVENC 的线程
/// 亲和约束由编码 actor 单线程所有权保证（`Send` 仅服务一次性移交）
pub(crate) struct NvencSession {
    driver: NvencDriver,
    encoder: EncoderHandle,
    /// 空闲码流缓冲句柄。同步编码（提交即锁定）决定同时在途的至多一个：
    /// 「槽为空」即它正在 `submit` 的收尾守卫手里，互斥由 `&mut self` 表达
    spare_bitstream: Option<BitstreamHandle>,
    /// 主机帧块的登记式输入
    device_inputs: Vec<DeviceInput>,
    profile: StreamingProfile,
    codec: VideoCodec,
    config: Box<NV_ENC_CONFIG>,
    init_params: NV_ENC_INITIALIZE_PARAMS,
    #[cfg(test)]
    benchmark_stages: BackendStageTimings,
}

// SAFETY: 成员均为可跨线程移动的值——函数表是加载后只读的 'static 引用（Send/Sync
// 论证见 `LoadedDriver`），encoder 与登记句柄是指针尺寸的驱动侧不透明值，堆上的
// config/init_params 由 Box 钉死地址（见 `session_params` 的封装说明）。CUDA/NVENC
// 的线程亲和约束不属于 Rust 内存安全不变量，由编码 actor 单线程所有权保证；
// Send 只用于装配点到编码 actor 的一次性所有权移交
unsafe impl Send for NvencSession {}

#[cfg(test)]
impl NvencSession {
    /// 上一帧各阶段耗时（全程由本会话在编码路径上写入）
    pub(super) fn benchmark_stages(&self) -> BackendStageTimings {
        self.benchmark_stages
    }
}

impl Drop for NvencSession {
    fn drop(&mut self) {
        // 不存在「空 encoder」状态：`NvencSession` 只在 `nvEncOpenEncodeSessionEx` 成功之后
        // 才被构造，句柄非空由 `EncoderHandle::from_raw` 在构造处保证。
        //
        // 码流缓冲先销毁：`nvEncDestroyBitstreamBuffer` 不依赖 CUDA 上下文绑定，
        // 先做掉可以让后面的 NVENC 销毁区间更短
        if let Some(handle) = self.spare_bitstream.take()
            && let Some(destroy) = self.driver.api().nvEncDestroyBitstreamBuffer
        {
            unsafe {
                // SAFETY: handle 为 nvEncCreateBitstreamBuffer 成功返回且已解锁的句柄；
                // encoder 由 self 独占持有且此刻存活
                let _ = destroy(self.encoder.as_raw(), handle.as_raw());
            }
        }

        // 注销输入登记要 encoder 存活；系统内存由帧池与会话的 `Arc` 共享，
        // 这里只解除驱动包装
        self.destroy_device_inputs();

        // 编码器本身由 `EncoderHandle` 的 Drop 收尾：字段在本方法体之后才析构，
        // 因此仍在码流缓冲与输入缓冲全部销毁之后
    }
}

impl NvencSession {
    #[inline(always)]
    pub fn driver(&self) -> NvencDriver {
        self.driver
    }

    /// 会话当前档位。分辨率与编解码器构造后不可变（变更须重建会话），
    /// 码率与帧率经 [`Self::reconfigure`] 更新
    #[inline]
    pub fn profile(&self) -> &StreamingProfile {
        &self.profile
    }

    /// 会话 codec：构造期定死（换 codec 走会话重建，由 [`NvencBackend`] 判定）
    #[inline]
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// 会话构造：开会话、建编码参数、把给定池块登记为编码输入资源
    ///
    /// `blocks` 是帧池的锁页内存块；会话为每块保留一份 `Arc`，调用方此后可以放手。
    /// 构造即完整——会话不存在「还没有输入资源」的状态；任一环节失败，已建的部分随
    /// 本会话析构逆序释放
    pub fn new(
        driver: NvencDriver,
        cuda_ctx: Arc<CudaContext>,
        profile: StreamingProfile,
        codec: VideoCodec,
        blocks: &[Arc<PinnedHost>],
        io_stream: CUstream,
    ) -> Result<Self, String> {
        let (width, height) = (profile.width() as u32, profile.height() as u32);
        // 主机帧只有 NV12 一种格式（采集侧已完成 CSC），不存在按布局分叉的余地
        let buffer_format = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;

        let encoder = open_encoder(driver, &cuda_ctx)?;
        let (config, init_params) =
            build_encoder_config(driver, &encoder, &profile, codec, buffer_format)?;

        // 核心优化点：把专用 CUDA Stream 绑为 NVENC 输入与输出端，建立纯 GPU 硬件时间线。
        // 绑定失败必须 fail-closed：退回 legacy default stream 后编码与流上没有隐式同步。
        //
        // 两件事都在本机实测过（RTX 5060 / 驱动 615.78.08）：
        // 1. 参数确实是「指向 CUstream 的指针」——直接传 NULL 会让驱动 SIGSEGV；
        //    FFmpeg 同样传 `&ctx->cu_stream`。
        // 2. 驱动在调用期间读取并拷走句柄，返回后不再回读——绑定后把槽内容改写成
        //    0xdeadbeef，后续编码仍正常。
        // 因此栈槽够用，不需要定址存储；但必须保持「指针」形式。换驱动若出现
        // 花屏/挂死，先重做上面两个探针再决定是否恢复定址槽。
        let mut io_slot = io_stream;
        let slot = &raw mut io_slot;
        let set_streams = nvfn!(driver, nvEncSetIOCudaStreams);
        unsafe {
            // SAFETY: encoder 由本函数持有且存活；slot 在整个调用期间有效
            let res = set_streams(encoder.as_raw(), slot as *mut c_void, slot as *mut c_void);
            if res != NVENCSTATUS::NV_ENC_SUCCESS {
                return Err(format!("nvEncSetIOCudaStreams failed: {res:?}"));
            }
        }

        // 主机帧块按 CUDADEVICEPTR 登记为编码输入（零拷贝，UVA 主机别名）。创建式
        // pSysMemBuffer 输入在本驱动上静默空编，禁用——理由见 `DeviceInput` 文档
        let mut device_inputs = Vec::new();
        let register = nvfn!(driver, nvEncRegisterResource);
        for (i, block) in blocks.iter().enumerate() {
            let base = block.as_ptr();
            // SAFETY: 该结构为 bindgen 生成的 C POD，成员均为整型/指针或 0 值合法的
            // 枚举（UNDEFINED / DIRECTX 等），全零位模式合法；NVENC 约定调用方零初始化
            // 并回填 version，保留字段必须保持 0
            let mut reg_params: NV_ENC_REGISTER_RESOURCE = unsafe { std::mem::zeroed() };
            reg_params.version = NV_ENC_REGISTER_RESOURCE_VER;
            reg_params.resourceType =
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
            reg_params.width = width;
            reg_params.height = height;
            reg_params.pitch = width;
            reg_params.resourceToRegister = base.cast_mut().cast();
            reg_params.bufferFormat = buffer_format;
            let registered = unsafe {
                // SAFETY: reg_params 已完整初始化；block 由本会话的 `Arc` 保证存活；
                // encoder 由本会话独占持有
                let res = register(encoder.as_raw(), &mut reg_params);
                if res != NVENCSTATUS::NV_ENC_SUCCESS {
                    return Err(format!("block {i}: nvEncRegisterResource failed: {res:?}"));
                }
                RegisteredResource::from_raw(reg_params.registeredResource)
            }
            .ok_or_else(|| format!("block {i}: nvEncRegisterResource 返回成功但句柄为空"))?;
            device_inputs.push(DeviceInput {
                block: Arc::clone(block),
                registered,
            });
        }

        let session = Self {
            driver,
            encoder,
            spare_bitstream: None,
            device_inputs,
            profile,
            codec,
            config,
            init_params,
            #[cfg(test)]
            benchmark_stages: BackendStageTimings::default(),
        };
        Ok(session)
    }

    /// 动态重新配置帧率 / 码率 / 分辨率
    ///
    /// 分辨率变更走 Reconfigure API：前提是建会话时声明了
    /// `maxEncodeWidth/maxEncodeHeight`（见 `build_encoder_config`）。
    /// 会话 codec 构造期定死、无参数可改——换 codec 走会话重建，
    /// 由 [`NvencBackend::reconfigure`](super::NvencBackend::reconfigure) 判定。
    pub fn reconfigure(&mut self, profile: StreamingProfile) -> Result<(), String> {
        let resolution_changed =
            profile.width() != self.profile.width() || profile.height() != self.profile.height();

        // 不变量同 `build_encoder_config`：fps/码率由 StreamingProfile 保证落在 u32 且非零
        let fps = profile.fps() as u32;
        let bitrate = profile.bitrate() as u32;

        let mut candidate_config = *self.config;
        candidate_config.rcParams.averageBitRate = bitrate;
        candidate_config.rcParams.maxBitRate = bitrate;
        let frame_size_bits = bitrate / fps;
        candidate_config.rcParams.vbvBufferSize = frame_size_bits;
        candidate_config.rcParams.vbvInitialDelay = frame_size_bits;
        // Level 与 Intra-Refresh 波次随新档位重算，推导与构造期共用同一处实现。
        apply_refresh_and_level(self.codec, &mut candidate_config, fps, bitrate);

        // 输入登记参数里的 width/height/pitch 描述缓冲布局：沿用旧值会让驱动按旧行距
        // 读新几何的帧。先按新几何重登，失败则回滚旧几何，会话保持可用
        if resolution_changed {
            self.reregister_device_inputs(profile.width(), profile.height())?;
        }

        // SAFETY: 该结构为 bindgen 生成的 C POD，嵌入的 INITIALIZE_PARAMS 中枚举字段
        // （tuningInfo）0 值合法（UNDEFINED），其余成员均为整型/指针，全零位模式合法；
        // NVENC 约定零初始化后回填 version，保留字段保持 0
        let mut reconf_params: NV_ENC_RECONFIGURE_PARAMS = unsafe { std::mem::zeroed() };
        reconf_params.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconf_params.reInitEncodeParams = self.init_params;
        reconf_params.reInitEncodeParams.encodeConfig = &mut candidate_config;
        reconf_params.reInitEncodeParams.frameRateNum = fps;
        reconf_params.reInitEncodeParams.frameRateDen = 1;
        if resolution_changed {
            reconf_params.reInitEncodeParams.encodeWidth = profile.width() as u32;
            reconf_params.reInitEncodeParams.encodeHeight = profile.height() as u32;
            let (dar_w, dar_h) = calculate_dar(profile.width() as u32, profile.height() as u32);
            reconf_params.reInitEncodeParams.darWidth = dar_w;
            reconf_params.reInitEncodeParams.darHeight = dar_h;
            // 改分辨率后下一帧必须是 IDR，否则解码端沿用旧序列头
            reconf_params.set_forceIDR(1);
            reconf_params.set_resetEncoder(1);
        }
        let reconfigure = nvfn!(self.driver, nvEncReconfigureEncoder);
        unsafe {
            // SAFETY: reconf_params 已完整初始化（version 与全部输入字段就位）；
            // encoder 由 self 独占持有且此刻存活
            let res = reconfigure(self.encoder.as_raw(), &mut reconf_params);
            if res != NVENCSTATUS::NV_ENC_SUCCESS {
                return Err(format!("nvEncReconfigureEncoder failed: {:?}", res));
            }
        }

        // 成功后同步更新持有状态，防御性确保 init_params.encodeConfig 持续指向固定的堆地址
        *self.config = candidate_config;
        self.init_params = reconf_params.reInitEncodeParams;
        self.init_params.encodeConfig = &mut *self.config;
        self.profile = profile;
        Ok(())
    }

    /// 按新几何重登全部主机帧输入资源
    ///
    /// 登记是一次性的句柄契约，改几何只能注销后重登。全成全败：新几何登记失败时
    /// 回滚到旧几何，保持会话仍可编码（否则一次失败的切换会把整条编码链打死）
    fn reregister_device_inputs(&mut self, width: usize, height: usize) -> Result<(), String> {
        // 先克隆强引用再注销：注销会释放会话侧引用，批必须活到重登完成
        let blocks: Vec<Arc<PinnedHost>> = self
            .device_inputs
            .iter()
            .map(|input| Arc::clone(&input.block))
            .collect();
        let old = (self.profile.width(), self.profile.height());
        // 先全部注销：之后无论成败都没有残留句柄指向旧几何
        self.destroy_device_inputs();
        match self.register_blocks(&blocks, width, height) {
            Ok(inputs) => {
                self.device_inputs = inputs;
                Ok(())
            }
            Err(e) => match self.register_blocks(&blocks, old.0, old.1) {
                Ok(inputs) => {
                    self.device_inputs = inputs;
                    Err(e)
                }
                Err(e2) => Err(format!("{e}；回滚失败: {e2}")),
            },
        }
    }

    /// 把一批池块按给定几何登记为编码输入
    fn register_blocks(
        &self,
        blocks: &[Arc<PinnedHost>],
        width: usize,
        height: usize,
    ) -> Result<Vec<DeviceInput>, String> {
        let register = nvfn!(self.driver, nvEncRegisterResource);
        let mut inputs: Vec<DeviceInput> = Vec::with_capacity(blocks.len());
        for (i, block) in blocks.iter().enumerate() {
            let base = block.as_ptr();
            // SAFETY: 该结构为 bindgen 生成的 C POD，成员均为整型/指针或 0 值合法的
            // 枚举（UNDEFINED / DIRECTX 等），全零位模式合法；NVENC 约定调用方零初始化
            // 并回填 version，保留字段必须保持 0
            let mut reg_params: NV_ENC_REGISTER_RESOURCE = unsafe { std::mem::zeroed() };
            reg_params.version = NV_ENC_REGISTER_RESOURCE_VER;
            reg_params.resourceType =
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
            reg_params.width = width as u32;
            reg_params.height = height as u32;
            reg_params.pitch = width as u32;
            reg_params.resourceToRegister = base.cast_mut().cast();
            reg_params.bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
            let register_res = unsafe {
                // SAFETY: reg_params 已完整初始化；block 由调用方的 `Arc` 保证存活；
                // encoder 由本会话独占持有
                register(self.encoder.as_raw(), &mut reg_params)
            };
            let Some(registered) = (register_res == NVENCSTATUS::NV_ENC_SUCCESS)
                .then(|| RegisteredResource::from_raw(reg_params.registeredResource))
                .flatten()
            else {
                // 已登记的前几块就地注销，全成全败
                for input in inputs {
                    self.unregister_resource(input.registered);
                }
                return Err(format!("block {i}: nvEncRegisterResource failed"));
            };
            inputs.push(DeviceInput {
                block: Arc::clone(block),
                registered,
            });
        }
        Ok(inputs)
    }

    /// 在会话上注册一个编码输入资源，返回驱动句柄
    ///
    /// 资源类型由调用方给定：显存槽位传 Vulkan 导入的 `CUarray`（`CUDAARRAY`），
    /// 主机帧池块传锁页基址（`CUDADEVICEPTR`，UVA 下即设备可访指针）。
    /// 两者同为「注册一次、逐帧映射」的句柄式输入。
    pub(super) fn register_resource(
        &self,
        resource_type: NV_ENC_INPUT_RESOURCE_TYPE,
        resource: *mut c_void,
        width: u32,
        height: u32,
        pitch: u32,
        buffer_format: NV_ENC_BUFFER_FORMAT,
    ) -> Result<RegisteredResource, String> {
        // SAFETY: 该结构为 bindgen 生成的 C POD，成员均为整型/指针或 0 值合法的
        // 枚举（UNDEFINED / DIRECTX 等），全零位模式合法；NVENC 约定调用方零初始化
        // 并回填 version，保留字段必须保持 0
        let mut reg_params: NV_ENC_REGISTER_RESOURCE = unsafe { std::mem::zeroed() };
        reg_params.version = NV_ENC_REGISTER_RESOURCE_VER;
        reg_params.resourceType = resource_type;
        reg_params.width = width;
        reg_params.height = height;
        reg_params.pitch = pitch;
        reg_params.resourceToRegister = resource;
        reg_params.bufferFormat = buffer_format;

        let register = nvfn!(self.driver, nvEncRegisterResource);
        let registered = unsafe {
            // SAFETY: reg_params 已完整初始化；resource 由调用方保证在注销前存续
            let res = register(self.encoder.as_raw(), &mut reg_params);
            if res != NVENCSTATUS::NV_ENC_SUCCESS {
                return Err(format!(
                    "nvEncRegisterResource failed ({:?}, {}x{}, pitch {}): {:?}",
                    resource_type, width, height, pitch, res
                ));
            }
            RegisteredResource::from_raw(reg_params.registeredResource)
        };
        registered
            .ok_or_else(|| format!("nvEncRegisterResource 返回成功但句柄为空 ({resource_type:?})"))
    }

    /// 注销一个注册句柄
    ///
    /// 必须在底层显存被释放**之前**调用。
    /// 返回码被忽略：本方法处在槽位作废与关闭流程里，把失败的句柄留在驱动里，
    /// 比让上层在收尾阶段 panic 更可取。
    pub(super) fn unregister_resource(&self, registered: RegisteredResource) {
        let Some(unregister) = self.driver.api().nvEncUnregisterResource else {
            return;
        };
        unsafe {
            // SAFETY: registered 由 register_resource 成功返回且尚未注销；
            // encoder 由 self 独占持有，此刻必然存活
            let _ = unregister(self.encoder.as_raw(), registered.as_raw());
        }
    }

    /// 取空闲码流句柄；槽空（首个帧或前一个仍在守卫手里）则新建
    fn take_bitstream(&mut self) -> Result<BitstreamHandle, String> {
        match self.spare_bitstream.take() {
            Some(handle) => Ok(handle),
            None => self.create_bitstream(),
        }
    }

    /// 新建一个输出码流缓冲句柄
    fn create_bitstream(&self) -> Result<BitstreamHandle, String> {
        // SAFETY: 该结构为 bindgen 生成的 C POD，成员均为整型/指针或 0 值合法的枚举
        // （MEMORY_HEAP 的 AUTOSELECT），全零位模式合法；NVENC 约定零初始化后回填 version
        let mut params: NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { std::mem::zeroed() };
        params.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;

        let create = nvfn!(self.driver, nvEncCreateBitstreamBuffer);
        let handle = {
            unsafe {
                // SAFETY: params 已完整初始化；encoder 由 self 独占持有且此刻存活
                let res = create(self.encoder.as_raw(), &mut params);
                if res != NVENCSTATUS::NV_ENC_SUCCESS {
                    return Err(format!("nvEncCreateBitstreamBuffer failed: {:?}", res));
                }
                BitstreamHandle::from_raw(params.bitstreamBuffer)
            }
        };
        handle.ok_or_else(|| "nvEncCreateBitstreamBuffer 返回成功但句柄为空".to_string())
    }

    /// 归还句柄供下一帧复用；调用方必须已解锁
    fn recycle_bitstream(&mut self, handle: BitstreamHandle) {
        self.spare_bitstream = Some(handle);
    }

    /// 注销并清空本会话的全部主机帧块登记（幂等；只解除驱动包装，
    /// 系统内存归帧池所有）
    fn destroy_device_inputs(&mut self) {
        let inputs = std::mem::take(&mut self.device_inputs);
        for input in inputs {
            self.unregister_resource(input.registered);
        }
    }

    /// 显存槽位路径：每帧映射外部资源、编码后解映射
    ///
    /// 主机帧走 [`Self::encode_host`]——同为登记式输入，但每帧映射时保留行距
    pub(super) fn encode_from_resource(
        &mut self,
        registered: RegisteredResource,
        spec: InputSpec,
        force_idr: bool,
        frame_idx: u64,
        stage: &SlotStateBlock,
    ) -> Result<(Vec<u8>, bool), String> {
        // 1. 映射。此处的 `?` 是安全的：映射尚未成功，没有需要归还的东西。
        stage.set_stage(ActorStage::MapInput);
        // SAFETY: 该结构为 bindgen 生成的 C POD，成员均为整型/指针或 0 值合法的枚举
        // （BUFFER_FORMAT 的 UNDEFINED），全零位模式合法；version 随后回填
        let mut map_params: NV_ENC_MAP_INPUT_RESOURCE = unsafe { std::mem::zeroed() };
        map_params.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        map_params.registeredResource = registered.as_raw();

        let map = nvfn!(self.driver, nvEncMapInputResource);
        #[cfg(test)]
        {
            self.benchmark_stages = BackendStageTimings::default();
        }
        unsafe {
            // SAFETY: registered 由调用方持有且尚未注销
            let res = map(self.encoder.as_raw(), &mut map_params);
            if res != NVENCSTATUS::NV_ENC_SUCCESS || map_params.mappedResource.is_null() {
                return Err(format!(
                    "nvEncMapInputResource failed ({:?}): {:?}",
                    registered, res
                ));
            }
        }
        let mapped_ptr = map_params.mappedResource;
        // CUDA 数组输入不吃行距；映射返回的像素格式以驱动为准
        let spec = InputSpec {
            pitch: 0,
            buffer_format: map_params.mappedBufferFmt,
            ..spec
        };

        // 2. 提交：句柄取出 → 编码 → 锁定 → 拷出 → 守卫收尾 → 句柄放回
        let handle = self.take_bitstream()?;
        let mut pic_params = build_pic_params(mapped_ptr, spec, force_idr, frame_idx);
        let encode_started = std::time::Instant::now();
        let result = encode_and_lock(
            &self.encoder,
            self.driver,
            handle,
            &mut pic_params,
            Some(mapped_ptr),
            stage,
            #[cfg(test)]
            &mut self.benchmark_stages,
        );
        self.recycle_bitstream(handle);
        probe_slow_encode(encode_started);

        result
    }

    /// 主机帧（NV12）编码：几何与格式由档位决定
    ///
    /// 池块已按 `CUDADEVICEPTR` 登记（UVA 主机别名，零拷贝）：每帧映射 → 编码 →
    /// 解映射，与显存槽位路径同款。行距必须保留——主机内存是 NV12 紧打包
    /// （stride = 宽），不能像 CUDA 数组输入那样置零
    pub(super) fn encode_host(
        &mut self,
        block: *const u8,
        force_idr: bool,
        frame_idx: u64,
        stage: &SlotStateBlock,
    ) -> Result<(Vec<u8>, bool), String> {
        let registered = self.device_input(block)?;
        let spec = InputSpec {
            width: self.profile.width(),
            height: self.profile.height(),
            pitch: self.profile.width(),
            buffer_format: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
        };

        // SAFETY: 该结构为 bindgen 生成的 C POD，成员均为整型/指针或 0 值合法的枚举
        // （BUFFER_FORMAT 的 UNDEFINED），全零位模式合法；version 随后回填
        let mut map_params: NV_ENC_MAP_INPUT_RESOURCE = unsafe { std::mem::zeroed() };
        map_params.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        map_params.registeredResource = registered.as_raw();
        let map = nvfn!(self.driver, nvEncMapInputResource);
        #[cfg(test)]
        {
            self.benchmark_stages = BackendStageTimings::default();
        }
        stage.set_stage(ActorStage::MapInput);
        unsafe {
            // SAFETY: registered 由调用方持有且尚未注销
            let res = map(self.encoder.as_raw(), &mut map_params);
            if res != NVENCSTATUS::NV_ENC_SUCCESS || map_params.mappedResource.is_null() {
                return Err(format!("nvEncMapInputResource failed: {res:?}"));
            }
        }
        let mapped_ptr = map_params.mappedResource;
        let mapped_fmt = map_params.mappedBufferFmt;
        let spec = InputSpec {
            buffer_format: mapped_fmt,
            ..spec
        };

        let handle = self.take_bitstream()?;
        let mut pic_params = build_pic_params(mapped_ptr, spec, force_idr, frame_idx);
        let encode_started = std::time::Instant::now();
        let result = encode_and_lock(
            &self.encoder,
            self.driver,
            handle,
            &mut pic_params,
            Some(mapped_ptr),
            stage,
            #[cfg(test)]
            &mut self.benchmark_stages,
        );
        self.recycle_bitstream(handle);
        probe_slow_encode(encode_started);

        result
    }

    /// 块对应的登记句柄；未登记过的块直接 fail-closed：说明调用方递进来的是
    /// 别的会话的帧池指针
    fn device_input(&self, block: *const u8) -> Result<RegisteredResource, String> {
        self.device_inputs
            .iter()
            .find(|input| input.block.as_ptr() == block)
            .map(|input| input.registered)
            .ok_or_else(|| "主机帧块基址不在本会话的登记表里".to_string())
    }
}

// 解映射：与提交前的 `nvEncMapInputResource` 一一对应
fn unmap_input(encoder: &EncoderHandle, driver: NvencDriver, mapped_ptr: *mut c_void) {
    if mapped_ptr.is_null() {
        return;
    }
    let Some(unmap) = driver.api().nvEncUnmapInputResource else {
        return;
    };
    unsafe {
        // SAFETY: mapped_ptr 由 nvEncMapInputResource 成功返回，与本次映射一一对应；
        // encoder 由调用方借用保证存活
        let _ = unmap(encoder.as_raw(), mapped_ptr);
    }
}

/// 提交一帧、等待锁定、拷出码流字节，返回「字节 + 是否真关键帧」
///
/// 从提交到拷出的整段所有权由 [`InflightEncode`] 扛着：提交失败、锁定失败或
/// panic 展开，都由 Drop 按固定顺序收尾。码流句柄按值进出，失败后由调用方
/// 决定是放回槽位还是丢弃
pub(super) fn encode_and_lock(
    encoder: &EncoderHandle,
    driver: NvencDriver,
    handle: BitstreamHandle,
    pic_params: &mut NV_ENC_PIC_PARAMS,
    mapped: Option<*mut c_void>,
    stage: &SlotStateBlock,
    #[cfg(test)] stages: &mut BackendStageTimings,
) -> Result<(Vec<u8>, bool), String> {
    let encode_picture = nvfn!(driver, nvEncEncodePicture);
    let lock_bitstream = nvfn!(driver, nvEncLockBitstream);

    pic_params.outputBitstream = handle.as_raw();

    let mut inflight = InflightEncode {
        encoder,
        driver,
        handle,
        mapped,
        locked: false,
    };

    // 1. 提交编码（失败路径直接返回，收尾交给 inflight 的 Drop）
    #[cfg(test)]
    let submit_start = std::time::Instant::now();
    stage.set_stage(ActorStage::EncodeSubmit);
    let res = unsafe {
        // SAFETY: pic_params 完全初始化且 outputBitstream 为刚取出的有效句柄；
        // inputBuffer 为已映射资源；inflight 独占 encoder 借用，同一时刻只有本调用在碰这台 encoder
        encode_picture(encoder.as_raw(), pic_params)
    };
    if res != NVENCSTATUS::NV_ENC_SUCCESS {
        return Err(format!(
            "nvEncEncodePicture failed: {:?} ({})",
            res,
            last_error(driver, encoder)
        ));
    }
    #[cfg(test)]
    {
        stages.submit_ns = submit_start.elapsed().as_nanos() as u64;
    }

    // 2. 锁定输出码流（阻塞至本帧完成）
    //
    // SAFETY: NV_ENC_LOCK_BITSTREAM 除 pictureStruct 外的成员均为整型/指针或
    // 0 值合法的枚举（pictureType 的 0 值即 NV_ENC_PIC_TYPE_P），MaybeUninit::zeroed
    // 产生的全零位内存对其合法；pictureStruct 是 1 起始、无 0 值的 C 枚举，零位对其
    // 非法——因此零内存后先显式写入合法变体 FRAME，全部成员初始化完毕才 assume_init；
    // 驱动随后覆写输出字段
    let mut lock_params = std::mem::MaybeUninit::<NV_ENC_LOCK_BITSTREAM>::zeroed();
    unsafe {
        // SAFETY: 写入目标来自 as_mut_ptr，位于本 MaybeUninit 分配内部且未脱离作用域
        (*lock_params.as_mut_ptr()).pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
    }
    // SAFETY: 除 pictureStruct 外的全零位内存合法（见上），pictureStruct 已写入
    // 合法变体，此刻全部成员均已初始化
    let mut lock_params = unsafe { lock_params.assume_init() };
    lock_params.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock_params.outputBitstream = handle.as_raw();

    #[cfg(test)]
    let lock_start = std::time::Instant::now();

    stage.set_stage(ActorStage::LockBitstream);
    let res = unsafe {
        // SAFETY: outputBitstream 为刚提交过编码的有效码流缓冲
        lock_bitstream(inflight.encoder.as_raw(), &mut lock_params)
    };
    #[cfg(test)]
    {
        stages.wait_ns = lock_start.elapsed().as_nanos() as u64;
    }

    if res == NVENCSTATUS::NV_ENC_ERR_NEED_MORE_INPUT {
        return Err("nvEncLockBitstream returned NEED_MORE_INPUT in blocking mode".into());
    }
    if res != NVENCSTATUS::NV_ENC_SUCCESS {
        return Err(format!("nvEncLockBitstream failed: {:?}", res));
    }

    // 锁定成功：缓冲自此租借给 CPU，Drop 需先解锁
    inflight.locked = true;

    // 3. 拷出字节：这是解锁前唯一的读取窗口。尺寸已知，一次分配到位
    let size = lock_params.bitstreamSizeInBytes as usize;
    let mut out = Vec::with_capacity(size);
    if size > 0 {
        unsafe {
            // SAFETY: data_ptr 与 size 来自驱动成功返回的 lock 参数，
            // 在解锁（Drop）之前保持有效
            out.extend_from_slice(std::slice::from_raw_parts(
                lock_params.bitstreamBufferPtr as *const u8,
                size,
            ));
        }
    }
    // WebRTC 仅在 IDR/I 帧时作为自包含关键帧切入点；Intra-Refresh 属于依赖前置参考的增量刷新，绝非随机接入点
    let is_key = matches!(
        lock_params.pictureType,
        NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
    );
    Ok((out, is_key))
}

/// 单帧探针：超过告警阈值（50ms）说明消费已跟不上推送节奏，槽位归还将越拖
/// 越久、心跳超时正在逼近；必须与「帧被收件箱丢弃」区分开
pub(super) fn probe_slow_encode(started: std::time::Instant) {
    let elapsed_ms = started.elapsed().as_millis();
    if elapsed_ms >= SLOW_ENCODE_THRESHOLD_MS {
        log::warn!(
            "单帧编码耗时 {} ms（超过槽位预算 {} ms，槽位回执将逾期）",
            elapsed_ms,
            SLOW_ENCODE_THRESHOLD_MS
        );
    }
}

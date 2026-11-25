//! 统一视频编码器 (Unified Video Encoder)
//!
//! 所有 Encoder Backend（NVENC / Vulkan Video 硬件、OpenH264 软件）之上的唯一
//! 策略层：帧计数、强制 IDR 意图及其失败回滚、档位缓存。策略不进
//! [`VideoEncodeBackend`]——新后端接入是一处 impl 的事，不该重复实现策略；
//! 软编与硬编在此完全同构，能力差异由各自的后端实现诚实作答。

use bytes::Bytes;

use std::sync::Arc;

use super::hardware::nvenc::NvencBackend;
use super::hardware::vulkan::VulkanVideoBackend;
use super::software::SoftwareBackend;
use crate::encode::hardware::slots::{ActorProgress, VramSlotDescriptor};
use crate::encode::pool::{HostBlock, heap_blocks};
use crate::encode::{FramePayload, StreamingProfile, VideoCodec};

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BackendStageTimings {
    pub input_copy_ns: u64,
    pub submit_ns: u64,
    pub wait_ns: u64,
    pub encode_ns: u64,
    /// GPU 侧视频编码作用域的耗时；0 表示该后端/该机器拿不到 GPU 时间戳
    pub gpu_scope_ns: u64,
    /// GPU 侧 `vkCmdEncodeVideoKHR` 本身的耗时；0 表示同上
    pub gpu_encode_ns: u64,
    /// 整帧 GPU 工作（命令缓冲开头到编码作用域结束，含输入图上传与 CSC）；0 表示同上
    pub gpu_frame_ns: u64,
}

/// 编码后端身份
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeBackendKind {
    /// NVIDIA NVENC (ARGB -> NV12)
    Nvenc,
    /// Vulkan Video
    VulkanVideo,
    /// OpenH264 软编：在本 trait 的实现里以 Err / no-op 如实作答
    Software,
}

impl EncodeBackendKind {
    /// 线上词汇：协商后端偏好的请求值、能力清单与控制面摘要共用这一套稳定拼写
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Nvenc => "nvenc",
            Self::VulkanVideo => "vulkan_video",
            Self::Software => "software",
        }
    }

    /// 线上词汇 → 后端身份：与 [`Self::as_str`] 同词汇的解析端，大小写不敏感、无别名；
    /// 识别不出返回 `None`
    pub(crate) fn from_token(token: &str) -> Option<Self> {
        match token.trim().to_lowercase().as_str() {
            "nvenc" => Some(Self::Nvenc),
            "vulkan_video" => Some(Self::VulkanVideo),
            "software" => Some(Self::Software),
            _ => None,
        }
    }
}

/// 编码后端 seam：[`crate::encode::encoder::VideoEncoder`] 门面之下的一切后端都长这个形状。
///
/// 线程约定：实现体被编码线程（Actor）独占，终生不跨线程。
pub(crate) trait VideoEncodeBackend {
    /// 后端身份：探测策略与采集侧粒度发布的真相来源
    fn kind(&self) -> EncodeBackendKind;

    /// 编码像素对齐粒度：采集侧按它建纹理与回读缓冲
    fn coded_align(&self) -> (u32, u32);

    /// 编码一帧，返回 (码流, 是否关键帧)
    ///
    /// 码流以 `Bytes` 交出、一经产出即不可改写：需要拼接前缀（如 IDR 的 SPS/PPS）的后端
    /// 必须在构造 `Bytes` 之前完成拼接。
    /// `force_idr` 只作用于本次调用，失败后的重试意图由门面策略层保管。
    /// 不带宽高：几何由各后端从建编码器时的 profile 自取（NVENC 的 inputWidth 必须与
    /// `nvEncInitializeEncoder` 一致），Vram 分支用的则是槽位自身的纹理尺寸。
    fn encode(
        &mut self,
        payload: &FramePayload,
        force_idr: bool,
        frame_idx: u64,
    ) -> Result<(Bytes, bool), String>;

    /// 重配档位（分辨率/编解码器变化允许内部重建会话）
    ///
    /// `new_codec` 是本次要编码的 codec：实现体据此判断能否就地重配（codec 不可就地
    /// 更换的后端在此报错，由门面/调用方转重建）。
    /// 重建可能换设备/上下文，即换了块所在的内存域：实现体换域时把新批块**交回**调用方，
    /// 由它装进帧池；返回空批＝内存域未变，旧块仍然有效。不吃设备帧的后端恒返回空批。
    fn reconfigure(
        &mut self,
        new_profile: StreamingProfile,
        new_codec: VideoCodec,
    ) -> Result<Vec<HostBlock>, String>;

    /// 跟随 Followed Bitrate 的无状态码率下发
    fn set_bitrate(&mut self, bps: u32) -> Result<(), String>;

    /// 批量导入外部显存直通槽位
    fn import_vram_slots(&mut self, descriptors: &mut [VramSlotDescriptor]) -> Result<(), String>;

    /// 清空槽位表并释放底层显存句柄
    fn clear_vram_slots(&mut self);

    /// 排空后端在途任务并确认 GPU 上下文可用；等待必须有界，失败即上下文不可信——
    /// 调用方必须整体弃置实现体（`mem::forget`，不得正常析构、不再使用）
    fn drain(&mut self) -> Result<(), String>;

    #[cfg(test)]
    fn benchmark_stages(&self) -> BackendStageTimings {
        BackendStageTimings::default()
    }
}

/// 统一编码器门面：一切后端（NVENC / Vulkan Video / OpenH264）之上的唯一策略层
///
/// 持有帧计数、强制 IDR 意图及其失败回滚（编码失败即意图未兑现，还原待重试）与
/// 档位缓存；后端差异由各实现诚实作答，策略不下沉。实现体被编码线程独占，终生不跨线程
pub(crate) struct VideoEncoder {
    backend: Box<dyn VideoEncodeBackend>,
    frame_counter: u64,
    force_keyframe: bool,
    profile: StreamingProfile,
    /// 生效 codec：与档位并列的事实——reconfigure 换 codec 即重建，重配跳过判定用
    codec: VideoCodec,
}

impl VideoEncoder {
    /// 软编入口：OpenH264 + 堆帧块，无硬件依赖、随时就绪（系统默认底座）
    ///
    /// 连同新分配的堆块一并返回，调用方负责装进帧池；软编不进硬件能力快照，
    /// 构造不产生全局发布副作用
    pub(crate) fn new_software(
        profile: StreamingProfile,
        codec: VideoCodec,
    ) -> Result<(Self, Vec<HostBlock>), String> {
        Self::new_backend(profile, codec, EncodeBackendKind::Software, None)
    }

    /// 按显式后端类型构建
    ///
    /// 生产路径经 [`Self::new_software`] 与 Actor 的硬编工厂进入；后端生命周期用例
    /// 借此直接指定目标后端（请求即所得）。硬编种类必须携带编码进度（心跳与阶段），
    /// 缺进度直接报错——那是停滞判定的锚点，不是可选项
    pub(crate) fn new_backend(
        profile: StreamingProfile,
        codec: VideoCodec,
        kind: EncodeBackendKind,
        progress: Option<Arc<ActorProgress>>,
    ) -> Result<(Self, Vec<HostBlock>), String> {
        Self::build(profile, codec, kind, progress)
    }

    /// 测试用：绕过探测与全局后端发布，直接用给定后端装配门面
    #[cfg(test)]
    pub(crate) fn from_backend(
        profile: StreamingProfile,
        codec: VideoCodec,
        backend: Box<dyn VideoEncodeBackend>,
    ) -> Self {
        Self {
            backend,
            frame_counter: 0,
            force_keyframe: true,
            profile,
            codec,
        }
    }

    fn build(
        profile: StreamingProfile,
        codec: VideoCodec,
        kind: EncodeBackendKind,
        progress: Option<Arc<ActorProgress>>,
    ) -> Result<(Self, Vec<HostBlock>), String> {
        // 硬件后端在自己的设备/上下文上分配帧块并随构造一并交回（失败即整体构造失败）；
        // 软编吃堆块，走的是同一个池——帧只有一条路，按内存域区分而已
        let (backend, blocks): (Box<dyn VideoEncodeBackend>, Vec<HostBlock>) = match kind {
            EncodeBackendKind::Nvenc => {
                let progress = progress.ok_or("NVENC 后端缺少编码进度")?;
                let (backend, blocks) = NvencBackend::new(profile, codec, progress)?;
                (Box::new(backend), blocks)
            }
            EncodeBackendKind::VulkanVideo => {
                let progress = progress.ok_or("Vulkan Video 后端缺少编码进度")?;
                let (backend, blocks) = VulkanVideoBackend::new(profile, codec, progress)?;
                (Box::new(backend), blocks)
            }
            EncodeBackendKind::Software => (
                Box::new(SoftwareBackend::new(profile, codec)?),
                heap_blocks(),
            ),
        };

        Ok((
            Self {
                backend,
                frame_counter: 0,
                // 初值 true：新编码器首帧必须是 IDR
                force_keyframe: true,
                profile,
                codec,
            },
            blocks,
        ))
    }

    /// 编码输出 NALU 数据流与真实关键帧标记
    ///
    /// 不带宽高：几何由各后端自己从建编码器时的 profile 取（NVENC 的 inputWidth 必须与
    /// `nvEncInitializeEncoder` 一致），Vram 分支用的则是槽位自身的纹理尺寸。
    pub(crate) fn encode(
        &mut self,
        payload: &FramePayload,
        force_keyframe: bool,
    ) -> Result<(Bytes, bool), String> {
        self.frame_counter += 1;
        let pending_forced = std::mem::take(&mut self.force_keyframe);
        let is_keyframe = force_keyframe || pending_forced || self.frame_counter == 1;
        let result = self
            .backend
            .encode(payload, is_keyframe, self.frame_counter);
        // 编码失败意味着这一帧没产出码流，`force_keyframe` 的意图**没有被兑现**。
        // 不还回去的话，这次 IDR 就永久丢了：NVENC 侧没有「frame_num == 0 就是 IDR」
        // 这种兜底，之后会一直是 P 帧，而新会话（切分辨率/切帧率重建）的参考帧链是空的
        // ——接收端从第一帧起就解不出来，画面永久花屏。
        // Vulkan 侧虽然靠 `frame_num == 0` 兜住首帧，但重建后如果首帧失败、第二帧成功，
        // 这条兜底就不再生效；软编的 `force_intra` 同为一次性意图。所有后端都必须还原。
        if result.is_err() && pending_forced {
            self.force_keyframe = true;
        }
        result
    }

    /// 动态改码率
    ///
    /// 直接改运行中的编码器：NVENC 走 `nvEncReconfigureEncoder`，Vulkan 走重配，
    /// 软编走 openh264 的 `SetOption`——都不重建编码器、不强制关键帧。
    pub(crate) fn set_bitrate(&mut self, bitrate: usize) -> Result<(), String> {
        let bps = bitrate as u32;
        if bps == 0 {
            return Ok(());
        }
        self.backend.set_bitrate(bps)?;
        // 后端已按新码率重配，本层缓存的档位必须同步：`reconfigure` 的「相同即跳过」
        // 条件用的就是它，留旧值会让一次真正需要的重配被吞掉
        self.profile.set_bitrate(bitrate);
        Ok(())
    }

    /// 动态档位/codec 重配
    ///
    /// 返回空批＝内存域未变；非空则调用方必须把新批装进帧池（旧批已失效）。
    pub(crate) fn reconfigure(
        &mut self,
        new_profile: StreamingProfile,
        new_codec: VideoCodec,
    ) -> Result<Vec<HostBlock>, String> {
        if self.profile == new_profile && self.codec == new_codec {
            return Ok(Vec::new());
        }
        log::info!(
            "Streaming profile changed from {:?} ({:?}) to {:?} ({:?}), reconfiguring encoder session",
            self.profile,
            self.codec,
            new_profile,
            new_codec
        );
        let blocks = self.backend.reconfigure(new_profile, new_codec)?;
        self.profile = new_profile;
        self.codec = new_codec;
        // 几何或 codec 变了，旧 GOP 的参考帧不再可靠：下一帧必须是 IDR
        self.force_keyframe = true;
        Ok(blocks)
    }

    /// 查询当前后端是否为硬件编码
    pub(crate) fn is_hardware(&self) -> bool {
        !matches!(self.backend.kind(), EncodeBackendKind::Software)
    }

    /// 排空当前编码后端的所有在途任务并等待硬件闲置
    pub(crate) fn drain(&mut self) -> Result<(), String> {
        self.backend.drain()
    }

    /// 批量导入外部显存槽位
    pub(crate) fn import_vram_slots(
        &mut self,
        descriptors: &mut [VramSlotDescriptor],
    ) -> Result<(), String> {
        self.backend.import_vram_slots(descriptors)
    }

    /// 清空槽位表
    pub(crate) fn clear_vram_slots(&mut self) {
        self.backend.clear_vram_slots();
    }

    #[cfg(test)]
    pub(crate) fn benchmark_stages(&self) -> BackendStageTimings {
        self.backend.benchmark_stages()
    }

    /// 编码尺寸：显示几何按后端 `coded_align` 向上对齐
    ///
    /// 与显示尺寸并列的**事实**，随协商/换档回执交给采集侧（采集侧按它建纹理与
    /// 回读缓冲）。任何别处不得自行推导——同一事实推导两次必然分叉
    pub(crate) fn coded_extent(&self) -> (usize, usize) {
        let align = self.backend.coded_align();
        self.profile
            .coded_extent((align.0 as usize, align.1 as usize))
    }

    /// 后端身份：换档/构建回执如实上报「建成的是哪个后端」的单一事实源
    pub(crate) fn backend_kind(&self) -> EncodeBackendKind {
        self.backend.kind()
    }

    /// 门面缓存的档位（初始档位一致性用例）
    #[cfg(test)]
    pub(crate) fn profile(&self) -> StreamingProfile {
        self.profile
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::encode::VideoCodec;
    use crate::encode::pool::{FramePool, heap_blocks};

    /// 取一块并写满 `color`，交给编码器
    fn host_frame(pool: &Arc<FramePool>, bytes: usize, color: u8) -> FramePayload {
        let mut frame = pool.acquire().expect("池里必须有余量");
        frame
            .write(&vec![color; bytes], None)
            .expect("写入必须成功");
        FramePayload::Host { frame }
    }

    /// 线上词汇解析端与 as_str 互为镜像：规范拼写（大小写不敏感）全可解析，
    /// 别名与噪声一律 None——偏好的词汇纪律不允许「差不多就算」
    #[test]
    fn test_backend_kind_token_roundtrip() {
        for kind in [
            EncodeBackendKind::Nvenc,
            EncodeBackendKind::VulkanVideo,
            EncodeBackendKind::Software,
        ] {
            assert_eq!(EncodeBackendKind::from_token(kind.as_str()), Some(kind));
        }
        assert_eq!(
            EncodeBackendKind::from_token(" NVENC "),
            Some(EncodeBackendKind::Nvenc)
        );
        for noise in ["", "auto", "vulkan", "cuda", "硬编"] {
            assert_eq!(
                EncodeBackendKind::from_token(noise),
                None,
                "词汇无别名: {noise:?}"
            );
        }
    }

    #[test]
    fn test_unified_encoder_software() {
        crate::encode::software::init_api_for_tests();
        let (w, h) = (1280usize, 720usize);
        let profile = StreamingProfile::new(w, h, 60, 4_000_000);
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        let (mut encoder, _blocks) =
            VideoEncoder::new_software(profile, VideoCodec::H264).expect("软编初始化应当成功");
        let payload = host_frame(&pool, w * h * 4, 128);

        let (nalu, is_keyframe) = encoder
            .encode(&payload, true)
            .expect("软编路径应成功输出码流");
        assert!(!nalu.is_empty(), "NALU 数据不应为空");
        assert!(is_keyframe, "强制请求关键帧应输出 is_keyframe = true");
        assert!(!encoder.is_hardware(), "软件编码路径不是硬件后端");
    }

    #[test]
    fn test_unified_encoder_reconfigure() {
        crate::encode::software::init_api_for_tests();
        let profile = StreamingProfile::new(1280, 720, 60, 4_000_000);
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        let (mut encoder, _blocks) =
            VideoEncoder::new_software(profile, VideoCodec::H264).expect("软编初始化应当成功");
        let payload = host_frame(&pool, 1280 * 720 * 4, 0);
        let _ = encoder.encode(&payload, false);

        let new_profile = StreamingProfile::new(1600, 900, 30, 3_000_000);
        assert!(
            encoder
                .reconfigure(new_profile, VideoCodec::H264)
                .expect("软编重配不会换内存域")
                .is_empty(),
            "软编不碰设备，重配不该交回新批"
        );
        assert_eq!(encoder.profile(), new_profile, "重配后门面档位必须同步");
    }

    /// 软编必须接受运行期改码率：`SoftwareBackend` 的 SetOption 走 openh264 官方入口，
    /// 升 / 降两个方向的顺序由它自己保证（见 `software.rs` 的用例）。
    #[test]
    fn test_software_encoder_accepts_bitrate_change() {
        crate::encode::software::init_api_for_tests();
        let (w, h) = (1280usize, 720usize);
        let profile = StreamingProfile::new(w, h, 60, 4_000_000);
        let pool = FramePool::new();
        pool.attach(heap_blocks());
        let (mut encoder, _blocks) =
            VideoEncoder::new_software(profile, VideoCodec::H264).expect("软编初始化应当成功");
        let payload = host_frame(&pool, w * h * 4, 0);
        encoder.encode(&payload, true).expect("首帧编码应当成功");

        encoder
            .set_bitrate(profile.bitrate() * 3 / 2)
            .expect("升码率必须被接受");
        encoder
            .set_bitrate(profile.bitrate() / 2)
            .expect("降码率必须被接受");

        let (nalu, _) = encoder.encode(&payload, false).expect("改码率后仍应能编码");
        assert!(!nalu.is_empty(), "改码率后编码产物不应为空");
    }
}

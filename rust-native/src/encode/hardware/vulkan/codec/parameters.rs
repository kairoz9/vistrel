//! codec 参数集对象 (Video Session Parameters)
//!
//! 编码会话建好之后、发出第一条编码命令之前，必须先把参数集交给驱动：Vulkan Video
//! 不做码流语法解析，序列/图像参数集的每个字段都得由应用填。填错的表现不是编译错误，
//! 而是 `VK_ERROR_INVALID_VIDEO_STD_PARAMETERS_KHR`，或者产出一段没人能解的码流。
//!
//! 参数集构造在 `h264/params.rs` / `h265/params.rs` / `av1/sequence.rs`；本模块只负责
//! 「创建参数对象 → 取序列化字节 → 读取覆盖反馈」这条共同生命周期。
//!
//! **参数集的字节由驱动生成**（`vkGetEncodedVideoSessionParametersKHR`），应用不写码流语法。
//! 驱动可以按 encode quality level 对参数集施加**覆盖**（规范 "encode overrides"），此时
//! 应用手里的 std 结构体与驱动实际编码所依据的并不一致；只有驱动自己序列化出来的那份
//! 字节才与它写出的 slice NAL 同源。

use ash::vk;

use super::super::device::VideoDevice;
use super::super::session::VideoSession;
use super::SequenceConfig;

/// 已创建的会话参数对象
pub struct VideoSessionParameters {
    device: VideoDevice,
    params: vk::VideoSessionParametersKHR,
    /// 驱动序列化出的参数集字节（H.264/H.265 是 Annex-B，AV1 是序列头 OBU），
    /// IDR 时拼在码流前面
    parameter_sets: Vec<u8>,
}

impl VideoSessionParameters {
    /// 按静态序列头配置构造参数对象
    ///
    /// 唯一的 codec 分派点：调用方只传 [`SequenceConfig`]，不按 codec 选方法。
    pub(in crate::encode::hardware::vulkan) fn new(
        device: &VideoDevice,
        session: &VideoSession,
        sequence: &SequenceConfig,
    ) -> Result<Self, String> {
        match sequence {
            SequenceConfig::H264(config) => {
                Self::new_h264(device, session, config.level_idc, config.frame_num_bits)
            }
            SequenceConfig::H265(config) => {
                Self::new_h265(device, session, config.level_idc, config.frame_num_bits)
            }
            SequenceConfig::Av1(config) => Self::new_av1(device, session, config),
        }
    }

    pub fn params(&self) -> vk::VideoSessionParametersKHR {
        self.params
    }

    /// 公共收尾：取驱动序列化的参数集字节并组装 `Self`
    ///
    /// `feedback` 传 null 时不取反馈；非 null 时必须指向顶层的
    /// `VideoEncodeSessionParametersFeedbackInfoKHR`，codec 专属反馈挂在它的 pNext 链上。
    ///
    /// 各 codec 的参数集构造在 `codec/{h264,h265}/params.rs` 与 `codec/av1/sequence.rs`，经本方法与
    /// [`Self::fetch_encoded`] 收尾——字段不外泄，调用方拿不到「半组装」的对象。
    pub(in crate::encode::hardware::vulkan) fn assemble(
        device: &VideoDevice,
        params: vk::VideoSessionParametersKHR,
        info: &vk::VideoEncodeSessionParametersGetInfoKHR<'_>,
        feedback: *mut std::ffi::c_void,
    ) -> Result<Self, String> {
        let parameter_sets = match Self::fetch_encoded(device, info, feedback) {
            Ok(bytes) => bytes,
            Err(e) => {
                // SAFETY: params 由本函数创建且尚未交出，失败路径必须销毁一次
                unsafe {
                    device
                        .video_queue_table()
                        .destroy_video_session_parameters(params, None);
                }
                return Err(e);
            }
        };

        Ok(Self {
            device: device.clone(),
            params,
            parameter_sets,
        })
    }

    /// 让驱动把参数集序列化成码流字节（两段式查询）
    ///
    /// `info` 的 `pNext` 链决定取哪些参数集：H.264 / H.265 必须挂 codec 专属的
    /// `VideoEncode*SessionParametersGetInfoKHR`（VUID 08262 / 08265），AV1 留空。
    ///
    /// `feedback` 为 null 时不取反馈；非 null 时必须指向顶层的
    /// `VideoEncodeSessionParametersFeedbackInfoKHR`，codec 专属反馈挂在它的 pNext 链上。
    pub(in crate::encode::hardware::vulkan) fn fetch_encoded(
        device: &VideoDevice,
        info: &vk::VideoEncodeSessionParametersGetInfoKHR<'_>,
        feedback: *mut std::ffi::c_void,
    ) -> Result<Vec<u8>, String> {
        // 走 `fp()` 暴露的原始指针，不用 ash 的封装：封装把 `pDataSize` 变成了切片长度、
        // 并丢弃驱动写回的实际字节数，而两段式查询要靠它先读回长度、再按长度取字节。
        let fp = device.video_encode_queue_table().fp();
        // `Device::handle` 在这个 ash rev 上是安全方法，不需要 unsafe
        let raw_device = device.device().handle();

        // 第一段：只问长度
        let mut size = 0usize;
        // SAFETY: pData 为 null 是标准两段式查询首调，只写 size；feedback 由调用方保证
        // 指向其声明的 codec 专属反馈结构（或为 null）
        let res = unsafe {
            (fp.get_encoded_video_session_parameters_khr)(
                raw_device,
                info,
                feedback.cast(),
                &mut size,
                std::ptr::null_mut(),
            )
        };
        if res != vk::Result::SUCCESS {
            return Err(format!("查询参数集长度失败: {res:?}"));
        }
        if size == 0 {
            return Err("驱动返回的参数集长度为 0".to_string());
        }

        // 第二段：取字节。缓冲不足时这条命令返回 `VK_INCOMPLETE`（不是 `NOT_ENOUGH_SPACE`），
        // 并把 `pDataSize` 写 0。缓冲大小取自首调，撞上它说明两次调用之间尺寸变了
        let mut buf = vec![0u8; size];
        let mut written = size;
        // SAFETY: buf 长度等于首调给出的 size，written 以同值传入
        let res = unsafe {
            (fp.get_encoded_video_session_parameters_khr)(
                raw_device,
                info,
                feedback.cast(),
                &mut written,
                buf.as_mut_ptr() as *mut std::ffi::c_void,
            )
        };
        if res == vk::Result::INCOMPLETE {
            return Err("参数集字节数在两次查询之间变化".to_string());
        }
        if res != vk::Result::SUCCESS {
            return Err(format!("读取参数集失败: {res:?}"));
        }
        buf.truncate(written);
        Ok(buf)
    }

    /// 参数集字节（H.264/H.265 是 Annex-B，AV1 是序列头 OBU），IDR 时拼在码流最前面
    pub fn parameter_sets(&self) -> &[u8] {
        &self.parameter_sets
    }
}

impl Drop for VideoSessionParameters {
    fn drop(&mut self) {
        // SAFETY: params 由本结构创建且只销毁一次；必须在所属 VideoSession 之前释放，
        // 这一点由持有顺序保证（参数对象持有 device 的 Arc，但不持有会话）
        unsafe {
            self.device
                .video_queue_table()
                .destroy_video_session_parameters(self.params, None);
        }
    }
}

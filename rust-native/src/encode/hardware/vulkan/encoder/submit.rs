//! 一帧编码的执行管线：录制、提交、读回与状态推进
//!
//! 从编码命令描述好之后开始：[`VulkanEncoder::record_and_submit`] 录制全部命令
//! （上传拷贝 → DPB 布局 → begin → 码控 → encode → end）并提交等待，
//! [`VulkanEncoder::submit_encode`] 做失败分诊与 feedback 读取，
//! [`VulkanEncoder::finish_frame`] 读回码流、推进逐帧状态、给 IDR 拼参数集。
//!
//! **等待超时与永久毒化**：提交已落地但时间线等待超时（`InFlight`）时，命令缓冲
//! 仍在途且绝不可复用——对在途命令缓冲 reset 是规范违规。编码器自此永久毒化：
//! 后续录制在入口短路拒绝，排空（`drain`）直接报错；拆构期的风险是游戏侧不得
//! 销毁在途命令缓冲引用的显存，这些资源随 `VkDevice` 在引擎关停时统一回收。

use ash::vk::{self, TaggedStructure};

use super::{PendingUpload, Scope, VulkanEncoder};
use crate::encode::hardware::vulkan::codec::{CodecGopRemaining, CodecRateControl};

// ---- 执行管线：录制、提交、读回 ----

/// `VK_QUERY_TYPE_VIDEO_ENCODE_FEEDBACK_KHR` 的结果布局
///
/// 反馈值按 `encodeFeedbackFlags` 的位序排列，`VkQueryResultStatusKHR` 追加在最后：
/// 只开 `BYTES_WRITTEN` 时是 `[bytes_written, status]`；再开 `HAS_OVERRIDES` 时是
/// `[bytes_written, has_overrides, status]`。
#[derive(Clone, Copy)]
struct EncodeFeedback {
    /// 至多两个反馈值 + 一个状态
    values: [u32; 3],
    /// 查询是否包含 `HAS_OVERRIDES`；决定状态落在 `values[1]` 还是 `values[2]`
    has_overrides: bool,
}

impl EncodeFeedback {
    fn new(has_overrides: bool) -> Self {
        Self {
            values: [0; 3],
            has_overrides,
        }
    }

    /// `dataSize` / `stride`：反馈值个数 + 状态
    fn size(&self) -> usize {
        (1 + usize::from(self.has_overrides) + 1) * std::mem::size_of::<u32>()
    }

    fn bytes_written(&self) -> u64 {
        u64::from(self.values[0])
    }

    fn overrides(&self) -> u32 {
        if self.has_overrides {
            self.values[1]
        } else {
            0
        }
    }

    fn status(&self) -> u32 {
        if self.has_overrides {
            self.values[2]
        } else {
            self.values[1]
        }
    }
}

/// `VK_QUERY_RESULT_STATUS_COMPLETE_KHR`
const QUERY_STATUS_COMPLETE: u32 = 1;

/// 等待编码完成的超时（2s）：已远超一帧的编码时间，超了就是 GPU 侧出了问题。
///
/// **必须有有限超时**：无界等待会让编码 Actor 线程永久阻塞，整条管线只进不出、
/// 不报错、不落盘——排查时完全看不出卡在哪。也刻意不按帧率收紧失败边界：
/// 慢帧（驱动抖动、合成器抢占）等一等就会完成，把「卡顿」误判成「死亡」只会
/// 换来错误帧与对在途命令缓冲的复位 UB。
const ENCODE_WAIT_TIMEOUT_NS: u64 = 2_000_000_000;

/// 提交环节的失败分类：决定布局回滚是否安全
///
/// 失败分两段——提交落地前（命令从未进 GPU，回滚「已录制的布局转换」是正确善后）
/// 与提交落地后等待超时（命令与全部布局转换仍在 GPU 在途，回滚等于谎报状态，
/// 且命令缓冲绝不可复用）。
enum SubmitFailure {
    /// 命令缓冲未能提交：无在途命令，`abort()` 回滚是安全的
    NotSubmitted(String),
    /// 提交已落地但等待超时：命令缓冲仍在途，编码器自此永久毒化
    InFlight(String),
}

// 从 String 的默认转换按「提交未落地」归类：提交落地后的失败必须显式构造
// `InFlight`，不得走 `?` 的隐式转换
impl From<String> for SubmitFailure {
    fn from(e: String) -> Self {
        Self::NotSubmitted(e)
    }
}

/// GPU 时间戳探针：一次提交里写下的四个边界点（顺序即录制顺序）
///
/// 前两个在编码作用域之内、后两个夹住整个作用域，因此
/// `ENCODE_START → ENCODE_END` 就是 `vkCmdEncodeVideoKHR` 本身的 GPU 时间，
/// `SCOPE_START → SCOPE_END` 是 Nsight 里「Video Coding」那一段的等价物。
/// `FRAME_START` 录在命令缓冲最前面，`FRAME_START → SCOPE_END` 才是整帧的 GPU 工作
/// （含输入图上传与 CSC）——Pool 路径的 CSC 在编码作用域之外，只看作用域会漏掉它。
#[cfg(test)]
const TS_SCOPE_START: u32 = 0;
#[cfg(test)]
const TS_ENCODE_START: u32 = 1;
#[cfg(test)]
const TS_ENCODE_END: u32 = 2;
#[cfg(test)]
const TS_SCOPE_END: u32 = 3;
#[cfg(test)]
const TS_FRAME_START: u32 = 4;
/// 时间戳查询池的查询数量
#[cfg(test)]
pub(super) const TS_COUNT: u32 = 5;

/// 码流落盘的开关：环境变量给出路径时才启用（如 `VISTREL_DUMP_IVF=vistrel-av1.ivf`）
///
/// 排障设施，默认关闭。AV1 这条路径出问题时，「码流本身对不对」是最先要回答的问题，
/// 而本项目平时不落盘——`ffprobe` 能从 IVF 里直接读出 `color_space` /
/// `chroma_sample_position`，`ffmpeg` 能逐帧导图，都是纯靠日志做不到的。
/// **每次重测前手动删文件**：IVF 头只在文件不存在时写一次，不删会往旧文件追加。
const DUMP_ENV: &str = "VISTREL_DUMP_IVF";

/// 落盘路径；未设置该环境变量则为 `None`（只读一次并缓存，不占每帧的预算）
fn dump_path() -> Option<&'static str> {
    static PATH: std::sync::LazyLock<Option<String>> =
        std::sync::LazyLock::new(|| std::env::var(DUMP_ENV).ok().filter(|p| !p.is_empty()));
    PATH.as_deref()
}

impl VulkanEncoder {
    /// 编码一帧之后的公共收尾：读码流、推进状态、IDR 拼参数集
    ///
    /// 各 codec 的差别只在「怎么构造编码命令」，收尾完全一致，因此抽出来共用。
    pub(in crate::encode::hardware::vulkan) fn finish_frame(
        &mut self,
        bytes_written: u64,
        is_idr: bool,
    ) -> Result<(Vec<u8>, bool), String> {
        let extent = self.image.extent();
        // 码流缓冲是持久映射的：读回只做一次拷贝，不再有 map/unmap 往返。
        // IDR 必须自带参数集（驱动不写 SPS/PPS，解码器没有它们就无法初始化），
        // 前缀拼接在这一次分配里完成
        let out = if is_idr {
            self.bitstream
                .read_with_prefix(&self.parameter_sets, bytes_written as usize)?
        } else {
            self.bitstream.read(bytes_written as usize)?
        };

        // 状态推进：下一帧写另一个槽，帧号递增（IDR 后从 1 开始）。这三个值都是本函数
        // 所持有状态的直接推导（`setup_idx` 就是当时的 `next_slot`），不再由调用方回传 ——
        // 回传错一次，槽的写入顺序与参考关系就会错位，且要等到下一帧才暴露。
        self.next_slot = 1 - self.next_slot;
        self.frame_num = if is_idr { 1 } else { self.frame_num + 1 };
        // IDR 是全帧内，刷新周期从头开始；其余帧推进一格
        if let Some(cycle) = self.intra_refresh_cycle {
            self.intra_refresh_index = if is_idr {
                0
            } else {
                (self.intra_refresh_index + 1) % cycle
            };
        }
        self.coding.mark_applied();
        self.codec_state.finish_frame(is_idr);

        // 排障：`VISTREL_DUMP_IVF=<path>` 时把码流按 IVF 落盘，供 ffprobe / ffmpeg 读。
        // 必须放在参数集拼接之后，否则 IVF 里的 IDR 缺序列头，ffprobe 认不出 codec。
        // dump_ivf 的 fourcc 写死 AV01，只对 AV1 会话落盘——其它 codec 会产出被
        // ffprobe 认错的文件
        if let Some(path) = dump_path() {
            super::super::codec::dump_ivf(self.codec, path, extent.width, extent.height, &out);
        }

        // 关键帧上报口径：只有真 IDR 才算。渐进刷新的帧虽然是「I 宏块」，但不等于 IDR，
        // 接收端拿到它无法从零开始解码——谎报会让 waiting_for_keyframe 提前放行。
        Ok((out, is_idr))
    }

    /// 录制并提交一次编码，返回驱动写入的码流字节数
    ///
    /// 与 codec 无关的部分全在这里：begin/end coding、feedback query、提交等待、回读。
    /// `codec_rate_control` 与 `gop_remaining` 都由各 codec 构造（结构体是 codec 专属的），
    /// 会被挂进 `vkCmdBeginVideoCodingKHR` 的 pNext —— 见 [`Self::record_and_submit`] 里的说明。
    pub(in crate::encode::hardware::vulkan) fn submit_encode(
        &mut self,
        encode_info: &vk::VideoEncodeInfoKHR<'_>,
        scope: &Scope<'_>,
        layers: &[vk::VideoEncodeRateControlLayerInfoKHR<'_>],
        codec_rate_control: Option<CodecRateControl<'_>>,
        gop_remaining: Option<CodecGopRemaining<'_>>,
    ) -> Result<u64, String> {
        // 永久毒化短路：等待超时过的编码器带着可能在途的命令缓冲，任何后续录制
        // 都必须先 reset 命令缓冲——对在途命令缓冲 reset 是规范违规
        if self.poisoned {
            return Err("编码器已因等待超时毒化：在途命令缓冲不可复用，本会话不再编码".to_string());
        }

        // 录制 + 提交 + 等待。两类失败的善后完全不同：提交未落地才能回滚布局；
        // 在途超时绝不能回滚，并就此毒化编码器
        match self.record_and_submit(
            encode_info,
            scope,
            layers,
            codec_rate_control,
            gop_remaining,
        ) {
            Ok(()) => {}
            Err(SubmitFailure::NotSubmitted(e)) => {
                self.pending_upload = None;
                self.image.abort();
                self.dpb.abort();
                return Err(e);
            }
            Err(SubmitFailure::InFlight(e)) => {
                // 提交已落地：布局转换仍在途，回滚等于谎报状态；毒化后编码器不再
                // 被复用，命令缓冲与 DPB 的最终命运交给关停时的 device_wait_idle
                self.poisoned = true;
                return Err(e);
            }
        }

        // 栅栏已等到：上传与 DPB 的布局转换确实生效了，CSC 的信号量也已被消费
        self.image.commit();
        self.dpb.commit();
        if let Ok(csc) = self.csc.as_mut() {
            csc.finish();
        }

        // 本帧的 GPU 时间戳：提交已完成，四个查询必然可用
        #[cfg(test)]
        self.read_gpu_timings()?;

        // 读 feedback：布局见 `EncodeFeedback`
        let mut feedback = EncodeFeedback::new(self.feedback_has_overrides);
        let size = feedback.size();
        // 走 `fp_v1_0()` 的原始指针而不是封装：这条命令要带 `WITH_STATUS_KHR` 与显式
        // stride，封装版只收一个 `&mut [T]`，表达不了这两个参数。
        let fp = self.device.device().fp_v1_0();
        // SAFETY: feedback.values 是栈上 3 个 u32，容量覆盖「两个反馈值 + 状态」；
        // device 句柄由本结构持有且存活
        let res = unsafe {
            (fp.get_query_pool_results)(
                self.device.device().handle(),
                self.query_pool,
                0,
                1,
                size,
                feedback.values.as_mut_ptr().cast(),
                size as u64,
                vk::QueryResultFlags::WAIT | vk::QueryResultFlags::WITH_STATUS_KHR,
            )
        };
        if res != vk::Result::SUCCESS {
            return Err(format!("读取 feedback 查询失败: {res:?}"));
        }
        let bytes_written = feedback.bytes_written();
        let has_overrides = feedback.overrides();
        let status = feedback.status();
        if status != QUERY_STATUS_COMPLETE {
            return Err(format!(
                "编码查询状态非完成: {}（已写 {bytes_written} 字节）",
                status
            ));
        }
        if bytes_written == 0 || bytes_written > self.bitstream.size() {
            return Err(format!(
                "反馈的码流长度异常: {bytes_written}（缓冲 {}）",
                self.bitstream.size()
            ));
        }
        // 只报一次：逐帧刷同一条日志没有信息量，且会淹没真正的一次性告警
        if has_overrides != 0 && !self.overrides_seen {
            self.overrides_seen = true;
            log::debug!(
                "{:?} 编码器报告逐帧参数被驱动覆写，应用侧 std 字段与码流可能不一致",
                self.codec
            );
        }
        Ok(bytes_written)
    }

    /// 写入一个 GPU 时间戳边界点
    ///
    /// 阶段取 `ALL_COMMANDS`。时间戳的第一同步范围只覆盖**指定阶段**：取最宽阶段才能
    /// 保证「此前录制的命令都已执行完」对上传拷贝、图像屏障与编码命令同时成立；
    /// `VIDEO_ENCODE_KHR` 只覆盖编码命令，把上传未完的时间也会算进来。
    #[cfg(test)]
    fn write_timestamp(&self, query: u32) {
        let Some(pool) = self.ts_pool else {
            return;
        };
        // SAFETY: 命令缓冲处于录制态且由本结构独占；pool 由本结构创建并在整个
        // 结构存活期内有效；query 由四个 `TS_*` 常量给出，均 < TS_COUNT
        unsafe {
            self.device.device().cmd_write_timestamp2(
                self.cmd,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                pool,
                query,
            );
        }
    }

    /// 读回本帧的四个 GPU 时间戳并折算成纳秒
    ///
    /// 视频队列族的时间戳位数常常不足 64（NVIDIA 给 32 位），因此差值必须先按有效位数
    /// 截断再做回绕减法：高位是垃圾，不截断会把几毫秒算成几十秒。回绕周期 = 2^bits 个
    /// tick（本机 1ns/tick 下约 4.3 秒），单帧跨度远小于它，回绕减法不会歧义。
    #[cfg(test)]
    fn read_gpu_timings(&mut self) -> Result<(), String> {
        let Some(pool) = self.ts_pool else {
            return Ok(());
        };
        let mut stamps = [0u64; TS_COUNT as usize];
        let size = std::mem::size_of_val(&stamps);
        let fp = self.device.device().fp_v1_0();
        // 刻意不用 `WAIT`：查询意外不可用时它会永久阻塞编码线程。时间线信号量已经等到，
        // 结果必然可用，这里只做有限次轮询——读不到就报错，绝不挂死。
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        loop {
            // SAFETY: stamps 是 4 个 u64，data_size 与 stride 都按它给出；device 存活
            let res = unsafe {
                (fp.get_query_pool_results)(
                    self.device.device().handle(),
                    pool,
                    0,
                    TS_COUNT,
                    size,
                    stamps.as_mut_ptr().cast(),
                    std::mem::size_of::<u64>() as u64,
                    vk::QueryResultFlags::TYPE_64,
                )
            };
            match res {
                vk::Result::SUCCESS => break,
                vk::Result::NOT_READY if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
                other => return Err(format!("读取 GPU 时间戳失败: {other:?}")),
            }
        }
        let bits = self.device.timestamp_valid_bits();
        let mask = if bits >= 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        let period_ns = f64::from(self.device.timestamp_period_ns());
        let span = |from: u32, to: u32| {
            let ticks = stamps[to as usize].wrapping_sub(stamps[from as usize]) & mask;
            (ticks as f64 * period_ns) as u64
        };
        self.last_timings.gpu_scope_ns = span(TS_SCOPE_START, TS_SCOPE_END);
        self.last_timings.gpu_encode_ns = span(TS_ENCODE_START, TS_ENCODE_END);
        self.last_timings.gpu_frame_ns = span(TS_FRAME_START, TS_SCOPE_END);
        Ok(())
    }

    /// 录制本帧的全部命令，提交一次并等栅栏
    ///
    /// 顺序有硬约束：输入图的拷贝与屏障必须在 `vkCmdBeginVideoCodingKHR` **之前** ——
    /// 视频编码作用域内不允许 `vkCmdPipelineBarrier2`。
    ///
    /// 整帧只有这一次提交：输入图上传、DPB 布局、编码、feedback 查询都在同一个命令缓冲里。
    /// 唯一的跨队列依赖是 CSC（计算族）的完成信号量，挂在 `vkQueueSubmit2` 的等待项上，
    /// 等待阶段收到 `COPY` —— 见提交处的说明。
    fn record_and_submit(
        &mut self,
        encode_info: &vk::VideoEncodeInfoKHR<'_>,
        scope: &Scope<'_>,
        layers: &[vk::VideoEncodeRateControlLayerInfoKHR<'_>],
        codec_rate_control: Option<CodecRateControl<'_>>,
        gop_remaining: Option<CodecGopRemaining<'_>>,
    ) -> Result<(), SubmitFailure> {
        // **begin 作用域必须声明本帧用到的全部槽位，含当前帧的输出槽**——排布规则与
        // 07239 / 08215 两条 VU 的推导见 `begin_scope`：只在 encode 作用域里给槽位会
        // 违反 08215，把 setup 槽按真实槽号绑进 begin 会违反 07239。
        //
        // 码控侧另有三条约束：
        // - `vkCmdControlVideoCodingKHR` 必须在 video coding scope 内
        //   （VUID-vkCmdControlVideoCodingKHR-videocoding）⇒ 只能在 begin 之后；
        // - begin 的 pNext **不带** RC 时，会话状态必须是 DEFAULT（08253）；
        // - begin 的 pNext **带** RC 时，必须与已配置状态逐字段一致（08254），
        //   `averageBitrate` / `maxBitrate` 也在比较范围内。
        //
        // 码率每帧跟着带宽估计变，所以「begin 用当前值」必然与上一帧配置的状态不符。
        // 解法是**一帧滞后**：begin 声明 `CodingControl::applied()`（上次真正配置进
        // 会话的值），作用域内的控制命令再配当前值。首帧状态是 DEFAULT，begin 不带 RC。
        let first = !self.coding.has_applied();
        // 本实现固定为单层码控（三个 codec 都只建一个 layer）；栈上数组避免逐帧堆分配
        assert_eq!(layers.len(), 1, "本实现只支持单层码控");
        let applied = self.coding.applied().unwrap_or_default();
        let mut layers_prev = [layers[0]];
        for l in &mut layers_prev {
            l.average_bitrate = applied.bitrate_bps;
            l.max_bitrate = applied.bitrate_bps;
            l.frame_rate_numerator = applied.fps;
        }
        let mut rc_for_begin = if first {
            None
        } else {
            Some(
                vk::VideoEncodeRateControlInfoKHR::default()
                    .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
                    .layers(&layers_prev)
                    .virtual_buffer_size_in_ms(self.coding.vbv_ms())
                    .initial_virtual_buffer_size_in_ms(self.coding.vbv_ms()),
            )
        };
        let mut codec_for_begin = codec_rate_control;
        let mut rc_for_control = vk::VideoEncodeRateControlInfoKHR::default()
            .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
            .layers(layers)
            .virtual_buffer_size_in_ms(self.coding.vbv_ms())
            .initial_virtual_buffer_size_in_ms(self.coding.vbv_ms());
        let mut codec_for_control = codec_rate_control;

        let mut begin_info = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session.session())
            .video_session_parameters(self.params.params())
            .reference_slots(scope.slots());
        if let Some(rc) = rc_for_begin.as_mut() {
            begin_info = begin_info.push(rc);
            if let Some(ref mut crc) = codec_for_begin {
                begin_info = crc.push_begin(begin_info);
            }
        }
        // `requiresGopRemainingFrames` 时每次 begin 都必须带（VUID 08256 / 10282），
        // 与码控是否重发无关。按值存进局部，begin 只持有它的地址。
        let mut gop_for_begin = gop_remaining;
        if let Some(ref mut gop) = gop_for_begin {
            begin_info = gop.push_begin(begin_info);
        }
        let end_info = vk::VideoEndCodingInfoKHR::default();
        // 唯一的等待来源：计算队列上的 CSC 产物。主机 NV12 路径没有
        let csc_wait = self.csc.as_ref().ok().and_then(|c| c.pending_semaphore());

        // 本帧完成时 signal 时间线信号量
        self.timeline_value += 1;
        let signal_timeline = self.timeline_value;
        let timeline_signal = vk::SemaphoreSubmitInfo::default()
            .semaphore(self.timeline_sem)
            .value(signal_timeline)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS);
        let signal_slice = [timeline_signal];

        let device = self.device.device();
        // SAFETY: 命令缓冲来自本结构独占的命令池，上一次提交已完成（时间线已等到；
        // 等待超时的帧会毒化编码器，毒化后在 `submit_encode` 顶部短路，绝不会带着
        // 在途命令缓冲走到这里）；encode_info 及其 pNext 链、begin_info 的 pNext 链
        // （码控 / GOP 剩余帧数）由调用方与本函数局部持有，在录制期间均存活
        unsafe {
            device
                .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                .map_err(|e| format!("命令缓冲重置失败: {e:?}"))?;
            device
                .begin_command_buffer(
                    self.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| format!("命令缓冲开始失败: {e:?}"))?;

            // 时间戳池必须先重置：所有 `write_timestamp` 都录制在本命令缓冲里，
            // 重置也必须录在这里、且要排在它们之前
            #[cfg(test)]
            if let Some(ts_pool) = self.ts_pool {
                device.cmd_reset_query_pool(self.cmd, ts_pool, 0, TS_COUNT);
            }
            // 全程起点：上传拷贝、CSC 与 DPB 屏障都在此后，故本对齐点覆盖整帧 GPU 工作
            #[cfg(test)]
            self.write_timestamp(TS_FRAME_START);

            // 1. 本帧的输入图上传。与编码同处一次提交，拷贝完 GPU 直接进编码，
            //    中间没有「提交 → 等栅栏 → 再提交」的 CPU 往返
            let cmd = self.cmd;
            match self.pending_upload.take() {
                Some(PendingUpload::Staging) => self.image.record_upload(cmd),
                Some(PendingUpload::Planes { y, uv }) => {
                    self.image.record_upload_from_planes(cmd, y, uv)
                }
                None => {}
            }

            // 2. DPB 图首次使用前必须切到 VIDEO_ENCODE_DPB_KHR 布局。
            //    所有槽位是同一张图的不同数组层，布局状态只有一份，一次屏障覆盖全层
            self.dpb.ensure_layout(cmd)?;

            device.cmd_reset_query_pool(self.cmd, self.query_pool, 0, 1);

            // 作用域起点：此刻上传拷贝与 DPB 屏障都已被 `ALL_COMMANDS` 收进同步范围
            #[cfg(test)]
            self.write_timestamp(TS_SCOPE_START);

            self.device
                .video_queue_table()
                .cmd_begin_video_coding(self.cmd, &begin_info);

            // 码控只在首帧或参数变更后下发。**每帧下发会把驱动状态反复重置**，实测画面全黑。
            // 首帧带 RESET 把会话拉到已知起点；其余帧只同步码率，不动参考状态。
            if self.coding.needs_config() {
                let mut control = vk::VideoCodingControlInfoKHR::default().flags(if first {
                    vk::VideoCodingControlFlagsKHR::RESET
                        | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
                } else {
                    vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
                });
                control = control.push(&mut rc_for_control);
                if let Some(ref mut crc) = codec_for_control {
                    control = crc.push_control(control);
                }
                self.device
                    .video_queue_table()
                    .cmd_control_video_coding(self.cmd, &control);
            }

            // feedback 查询必须在会话已绑定之后才开始（VUID-vkCmdBeginQuery 要求），
            // 否则驱动拿到的是没有会话的编码查询
            device.cmd_begin_query(self.cmd, self.query_pool, 0, vk::QueryControlFlags::empty());

            // 只夹住 `vkCmdEncodeVideoKHR` 本身：feedback 查询的 begin/end 落在两个
            // 时间戳之外，省的把它们几微秒的录制开销算进编码时间
            #[cfg(test)]
            self.write_timestamp(TS_ENCODE_START);

            self.device
                .video_encode_queue_table()
                .cmd_encode_video(self.cmd, encode_info);

            #[cfg(test)]
            self.write_timestamp(TS_ENCODE_END);

            device.cmd_end_query(self.cmd, self.query_pool, 0);

            self.device
                .video_queue_table()
                .cmd_end_video_coding(self.cmd, &end_info);

            // 作用域终点：与 Nsight 的「Video Coding」范围同一口径
            #[cfg(test)]
            self.write_timestamp(TS_SCOPE_END);

            device
                .end_command_buffer(self.cmd)
                .map_err(|e| format!("命令缓冲结束失败: {e:?}"))?;

            let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(self.cmd)];
            // 等待数量是运行时的 0..=1。不能写 `unwrap_or_default()`：`[T; 1]` 的 Default
            // 会给出一个「全默认」元素（空句柄）而不是空数组
            let mut wait_storage = [vk::SemaphoreSubmitInfo::default(); 1];
            let mut wait_count = 0usize;
            if let Some(semaphore) = csc_wait {
                // CSC 的产物是两张平面图，本命令缓冲里消费它们的只有最开头那次拷贝
                wait_storage[wait_count] = vk::SemaphoreSubmitInfo::default()
                    .semaphore(semaphore)
                    .stage_mask(vk::PipelineStageFlags2::COPY);
                wait_count += 1;
            }
            let wait_slice: &[vk::SemaphoreSubmitInfo<'_>] = &wait_storage[..wait_count];
            let submit = vk::SubmitInfo2::default()
                .command_buffer_infos(&cmds)
                .wait_semaphore_infos(wait_slice)
                .signal_semaphore_infos(&signal_slice);
            // SAFETY: submit 及其引用的 cmds / wait_one 在调用期间均存活；
            // 同步通过时间线信号量保证，不再传递 VkFence
            #[cfg(test)]
            let t_sub = std::time::Instant::now();
            device
                .queue_submit2(self.device.video_queue(), &[submit], vk::Fence::null())
                .map_err(|e| format!("编码提交失败: {e:?}"))?;
            #[cfg(test)]
            let submit_ns = t_sub.elapsed().as_nanos() as u64;

            #[cfg(test)]
            let t_wait = std::time::Instant::now();
            let sems = [self.timeline_sem];
            let vals = [signal_timeline];
            let wait_info = vk::SemaphoreWaitInfo::default()
                .semaphores(&sems)
                .values(&vals);
            if let Err(e) = device.wait_semaphores(&wait_info, ENCODE_WAIT_TIMEOUT_NS) {
                let current_val = device
                    .get_semaphore_counter_value(self.timeline_sem)
                    .unwrap_or(0);
                // 提交已落地、等待超时：命令缓冲与全部布局转换仍在途，必须按 InFlight
                // 分类——这里之后的任何 String 错误也不得再走 `?` 的默认归类
                return Err(SubmitFailure::InFlight(format!(
                    "等待编码完成超时（2s）: {e:?}；目标时间线={signal_timeline}，当前值={current_val}"
                )));
            }
            #[cfg(test)]
            let wait_fence_ns = t_wait.elapsed().as_nanos() as u64;

            #[cfg(test)]
            {
                self.last_timings.submit_ns = submit_ns;
                self.last_timings.wait_fence_ns = wait_fence_ns;
            }
        }

        Ok(())
    }
}

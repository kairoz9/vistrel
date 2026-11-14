//! Synchronization2 屏障构造与录制 (Synchronization2 Barriers)
//!
//! 与 sync1 的 `vkCmdPipelineBarrier` 相比，`VkImageMemoryBarrier2` 把
//! `srcStageMask` / `dstStageMask` 收进**每个屏障自身**。由此得到两个直接好处：
//!
//! 1. 一次 `vkCmdPipelineBarrier2` 可以混合不同阶段的依赖，不再被迫按
//!    「阶段对」拆成多次调用；
//! 2. 「纯布局转换、没有访问依赖」有了明确写法 —— `VK_PIPELINE_STAGE_2_NONE`。
//!    sync1 只能用 `TOP_OF_PIPE` 这种「不是真实阶段」的占位去凑，而且把
//!    `srcAccessMask` 写成非空也不会报错，只会得到一个静默失效的依赖。
//!
//! 构造与录制都收在本模块：[`record_image_barriers`] 是全部屏障的唯一录制口，
//! 模块只依赖 ash 本身，不依赖本 crate 的任何类型。

use ash::vk;

/// 屏障一侧的「阶段 + 访问」对
///
/// sync2 之后这两个字段成对出现于屏障内部，打包成一个类型可以避免调用点把
/// stage 和 access 传串位 —— 那类错误编译器不管，表现是偶发花屏。
#[derive(Clone, Copy)]
pub(super) struct StageAccess {
    pub stage: vk::PipelineStageFlags2,
    pub access: vk::AccessFlags2,
}

impl StageAccess {
    /// 传输阶段读
    ///
    /// 平面拷贝的跨队列可见性已并入信号量依赖，生产侧不再声明传输读屏障；
    /// 目前仅测试读回（`record_readback`）使用
    #[cfg(test)]
    pub(super) fn transfer_read() -> Self {
        Self {
            stage: vk::PipelineStageFlags2::TRANSFER,
            access: vk::AccessFlags2::TRANSFER_READ,
        }
    }

    /// 传输阶段写
    pub(super) fn transfer_write() -> Self {
        Self {
            stage: vk::PipelineStageFlags2::TRANSFER,
            access: vk::AccessFlags2::TRANSFER_WRITE,
        }
    }

    /// 计算着色器读
    pub(super) fn compute_read() -> Self {
        Self {
            stage: vk::PipelineStageFlags2::COMPUTE_SHADER,
            access: vk::AccessFlags2::SHADER_READ,
        }
    }

    /// 任意命令、不声明访问
    ///
    /// 外部来源（显存槽位）在本设备上被谁用过不可知，只能保守取最宽阶段；
    /// 目标是编码/解码等视频命令时，用 ALL_COMMANDS 覆盖 `VIDEO_ENCODE_KHR`。
    pub(super) fn all_commands() -> Self {
        Self {
            stage: vk::PipelineStageFlags2::ALL_COMMANDS,
            access: vk::AccessFlags2::empty(),
        }
    }
}

/// 图像布局的「已确认 / 待定」双态跟踪
///
/// `layout` 是**已提交并确认完成**的布局（等过栅栏之后的既成事实），`pending` 是
/// **已录制、尚未确认**的布局。两者不能合并：提交失败时若把待定值当成既成事实，
/// 下一帧就会带着错的 `oldLayout` 发屏障——那是一个不报错的静默错误。
///
/// 编码输入图（`Nv12Image`）与 DPB 数组图（`DpbImages`）共用同一套规则，收在一处。
pub(super) struct TrackedLayout {
    layout: vk::ImageLayout,
    pending: Option<vk::ImageLayout>,
}

impl TrackedLayout {
    /// 新图的起始状态：`UNDEFINED`
    pub(super) fn new() -> Self {
        Self {
            layout: vk::ImageLayout::UNDEFINED,
            pending: None,
        }
    }

    /// 屏障的 `oldLayout`：待定值优先
    ///
    /// 同一命令缓冲里连续录制两次时，后一次的 `oldLayout` 正是前一次录制的产物，
    /// 所以「待定优先」在录制期同样成立。
    pub(super) fn current(&self) -> vk::ImageLayout {
        self.pending.unwrap_or(self.layout)
    }

    /// 录制了一次到 `new_layout` 的转换（尚未提交）
    pub(super) fn record(&mut self, new_layout: vk::ImageLayout) {
        self.pending = Some(new_layout);
    }

    /// 提交已确认完成：把待定布局提升为当前布局
    ///
    /// 调用时机必须在 `vkWaitForFences` 返回之后——早于那一刻，布局转换还没有真正发生。
    pub(super) fn commit(&mut self) {
        if let Some(layout) = self.pending.take() {
            self.layout = layout;
        }
    }

    /// 提交未发生或失败：丢弃待定布局，图像仍处于 `layout`
    pub(super) fn abort(&mut self) {
        self.pending = None;
    }
}

/// 整图（`COLOR` aspect、单 mip、单层）的布局转换屏障
pub(super) fn image_barrier(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src: StageAccess,
    dst: StageAccess,
) -> vk::ImageMemoryBarrier2<'static> {
    plane_barrier(
        image,
        vk::ImageAspectFlags::COLOR,
        old_layout,
        new_layout,
        src,
        dst,
    )
}

/// 覆盖**所有数组层**的整图布局转换屏障
///
/// `image_barrier` 固定 `layer_count: 1`，用在多层 array 图上只会转第 0 层 —— 剩下的层
/// 留在 `UNDEFINED`，之后按 `VIDEO_ENCODE_DPB_KHR` 去写就是布局不符。
///
/// 只在「整图所有层的布局状态一致」时使用：它会把所有层一起改掉。
pub(super) fn image_barrier_all_layers(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src: StageAccess,
    dst: StageAccess,
) -> vk::ImageMemoryBarrier2<'static> {
    let mut barrier = plane_barrier(
        image,
        vk::ImageAspectFlags::COLOR,
        old_layout,
        new_layout,
        src,
        dst,
    );
    barrier.subresource_range.layer_count = vk::REMAINING_ARRAY_LAYERS;
    barrier
}

/// 整张图、单 mip、指定层数的 `COLOR` 子资源范围
///
/// 视图创建与屏障都要它；写成函数而不是在各处抄字面量，改一处就够。
/// 单层图传 1，数组图（DPB）传槽位数 —— 视图必须覆盖全部层。
pub(super) fn color_range_layers(layers: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: layers,
    }
}

/// 单个平面的布局转换屏障
///
/// 多平面图在屏障上可以一次覆盖所有平面（用 `COLOR`），也可以逐平面指定
/// （`PLANE_0` / `PLANE_1`）；DISJOINT 图必须逐平面。
pub(super) fn plane_barrier(
    image: vk::Image,
    aspect: vk::ImageAspectFlags,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src: StageAccess,
    dst: StageAccess,
) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src.stage)
        .src_access_mask(src.access)
        .dst_stage_mask(dst.stage)
        .dst_access_mask(dst.access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: aspect,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        })
}

/// 把一组图像屏障录进命令缓冲（Synchronization2）
///
/// 每个屏障自带 src/dst 阶段与访问掩码，所以一次调用可以混合不同阶段的依赖 ——
/// 这正是 sync1 做不到的地方。没有内存/缓冲屏障需求，`DependencyInfo` 只填图像那一项。
///
/// # Safety
///
/// 调用方必须保证：`cmd` 处于录制状态；`barriers` 及其引用的图像句柄在调用期间
/// 有效。函数体内逐语句的 SAFETY 论证依赖上述前置条件。
// SAFETY: 调用方必须保证 `cmd` 处于可录制状态，且 `barriers` 里引用的图像句柄有效
pub(super) unsafe fn record_image_barriers(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    barriers: &[vk::ImageMemoryBarrier2<'_>],
) {
    let dependency = vk::DependencyInfo::default().image_memory_barriers(barriers);
    // SAFETY: dependency 及其引用的 barriers 在调用期间存活；cmd 由调用方保证
    // 处于可录制状态，barriers 里的图像句柄由调用方保证有效。
    //
    // `vkCmdPipelineBarrier2` 是 1.3 核心命令，`VulkanInstance` 已把设备版本卡在
    // 1.3 以上、`synchronization2` feature 位也显式启用，因此这里不需要查扩展或兜底。
    // 注意 ash 的 `Device::load` 不做空指针检查 —— 上面对版本和 feature 的把关是
    // 这条直调成立的前提。
    unsafe { device.cmd_pipeline_barrier2(cmd, &dependency) };
}

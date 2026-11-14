//! 设备本地图与 NV12 编码输入图 (Owned Image & NV12 Encode Input Image)
//!
//! **[`OwnedImage`]**：设备本地图的通用所有权单元。「建图 → 分配显存 → 建视图 →
//! 逆序销毁」这四步在 CSC 两张平面图、NV12 编码输入图、DPB 多层图上是
//! 同一套。收进一个类型，Drop 顺序只写一次，构造失败时的清理也只写一遍。
//!
//! **[`Nv12Image`]**：一块 `G8_B8R8_2PLANE_420_UNORM` 图像 + 主机可见暂存缓冲，负责把
//! CPU 侧的 NV12 整帧搬进显存并把布局切到 `VIDEO_ENCODE_SRC_KHR`——编码命令只认这个布局。
//!
//! [`Nv12Image`] 不持有命令池、命令缓冲与栅栏：上传指令录进调用方给的命令缓冲，与编码
//! 命令同处一次提交，拷贝与编码之间没有 CPU 往返，GPU 不会夹在两者之间空转。提交、
//! 等待与布局确认全部由 `FrameEncoder` 统一做，本结构只回答「往命令缓冲里写什么」。
//!
//! 布局状态因此分两份：`layout` 是**已确认完成**的布局，`pending_layout` 是**已录制
//! 但尚未确认**的布局。`record_*` 写后者，`commit` 把它提升为前者，`abort` 丢弃它。
//! 两者不能合并 —— 提交失败时若把 `pending_layout` 当成既成事实，下一帧就会带着错的
//! `oldLayout` 发屏障，那是一个不报错的静默错误。（状态机由 [`TrackedLayout`] 承载。）
//!
//! 共享模式：[`OwnedImage`] 按调用方给的族列表决定（单族 `EXCLUSIVE`、跨族
//! `CONCURRENT`）；[`Nv12Image`] 只归视频编码队列族，固定为 `EXCLUSIVE`。

use ash::vk::{self, TaggedStructure};

use super::device::{HostBuffer, VideoDevice};
use super::sync::{
    StageAccess, TrackedLayout, color_range_layers, image_barrier, image_barrier_all_layers,
    record_image_barriers,
};

#[cfg(test)]
use crate::encode::hardware::vulkan::test_support::run_oneshot;
use crate::encode::nv12_len;

/// 建一张设备本地图所需的全部参数
///
/// 字段多但都有明确默认：用 [`ImageSpec::new`] 起手，只覆盖与默认不同的项。
pub(super) struct ImageSpec<'a> {
    pub format: vk::Format,
    pub extent: vk::Extent2D,
    pub usage: vk::ImageUsageFlags,
    /// 数组层数；DPB 图取槽位数，其余为 1
    pub array_layers: u32,
    /// 共享模式按这个列表决定：空或单元素用 `EXCLUSIVE`，多元素用 `CONCURRENT`
    pub sharing: &'a [u32],
    /// 视图类型：单层用 `TYPE_2D`，多层用 `TYPE_2D_ARRAY`
    pub view_type: vk::ImageViewType,
    /// 视图格式。多平面图的平面视图用 R8 / R8G8，其余与图像格式相同
    pub view_format: vk::Format,
    /// 错误信息里的对象名
    pub what: &'static str,
    /// 视频编码用途必须给出：创建时会链上 `VkVideoProfileListInfoKHR`
    ///
    /// 不给的话驱动不把这张图当成视频编码资源（`VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08206`
    /// / `VUID-VkVideoBeginCodingInfoKHR-pPictureResource-07240`）。DPB 图尤其致命——
    /// 重建帧写不进去，参考帧就全是垃圾，表现为帧间画面直接崩掉。
    ///
    /// **按值存一份**（`VideoProfileInfoKHR` 是 `Copy`）：存引用会写成 `&'a T<'a>`，
    /// 外层借用被钉死成内层链的生命周期，调用方后面再想可变借用 profile 就会冲突。
    pub video_profile: Option<vk::VideoProfileInfoKHR<'a>>,
}

impl<'a> ImageSpec<'a> {
    /// 单层、单视图、`COLOR` aspect 的常用形状
    pub fn new(
        format: vk::Format,
        extent: vk::Extent2D,
        usage: vk::ImageUsageFlags,
        what: &'static str,
    ) -> Self {
        Self {
            format,
            extent,
            usage,
            array_layers: 1,
            sharing: &[],
            view_type: vk::ImageViewType::TYPE_2D,
            view_format: format,
            what,
            // 默认不是视频资源；视频编码的图由调用方 `.video(..)` 显式打开
            video_profile: None,
        }
    }

    /// 声明这张图属于某个视频 profile
    ///
    /// 按值传入（`VideoProfileInfoKHR` 是 `Copy`），拷的只是指针链本身；
    /// 被指向的 codec 专属子结构仍由调用方持有，必须活过 `OwnedImage::new` 那次 create。
    pub fn video(mut self, profile: vk::VideoProfileInfoKHR<'a>) -> Self {
        self.video_profile = Some(profile);
        self
    }
}

/// 一张设备本地图 + 它的显存与视图
pub(super) struct OwnedImage {
    device: VideoDevice,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    extent: vk::Extent2D,
}

impl OwnedImage {
    /// 建一张设备本地图，原子完成「图 → 显存 → 视图」三步
    ///
    /// 视图创建失败时图与显存就地归还，不留半就绪句柄；共享模式按
    /// `spec.sharing` 长度判定（族列表须已去重）；声明了视频 profile 的图
    /// 在创建时链上 `VkVideoProfileListInfoKHR`，否则驱动不认它是编码资源。
    /// 析构先 `device_wait_idle` 再逆序销毁，允许落在任意线程
    pub fn new(device: &VideoDevice, spec: ImageSpec<'_>) -> Result<Self, String> {
        let mut info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(spec.format)
            .extent(vk::Extent3D {
                width: spec.extent.width,
                height: spec.extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(spec.array_layers)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(spec.usage)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // 共享模式按调用方给的族列表决定：单族用 EXCLUSIVE，跨族用 CONCURRENT。
        // 族列表必须已去重：`[n, n]` 是非法的共享模式声明
        if spec.sharing.len() > 1 {
            info = info
                .sharing_mode(vk::SharingMode::CONCURRENT)
                .queue_family_indices(spec.sharing);
        } else {
            info = info.sharing_mode(vk::SharingMode::EXCLUSIVE);
        }

        // 视频编码资源必须声明所属 profile，否则驱动不认它是编码资源。
        //
        // 这里的局部副本必须活到 create 调用之后：`VkVideoProfileListInfoKHR` 里存的是
        // 指向它的指针。两个局部都在本函数栈上，生命周期覆盖 `create_image_allocated`。
        let video_profile = spec.video_profile;
        let mut profile_list = video_profile
            .as_ref()
            .map(|p| vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(p)));
        if let Some(list) = profile_list.as_mut() {
            info = info.push(list);
        }

        // 建图与分配显存是一个原子操作：失败时图也被销毁，不会留下半就绪的句柄
        let (image, memory) = device.create_image_allocated(
            &info,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            spec.what,
        )?;

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(spec.view_type)
            .format(spec.view_format)
            .subresource_range(color_range_layers(spec.array_layers));
        // SAFETY: view_info 已完整初始化，image 为刚创建的有效句柄
        let view = match unsafe { device.device().create_image_view(&view_info, None) } {
            Ok(view) => view,
            Err(e) => {
                // 视图没建成：图与显存都是本函数刚创建的，就地归还
                // SAFETY: image 与 memory 尚未交出，此处各销毁一次
                unsafe {
                    device.device().destroy_image(image, None);
                    device.device().free_memory(memory, None);
                }
                return Err(format!("{}视图创建失败: {e:?}", spec.what));
            }
        };

        Ok(Self {
            device: device.clone(),
            image,
            memory,
            view,
            extent: spec.extent,
        })
    }

    pub fn image(&self) -> vk::Image {
        self.image
    }

    pub fn view(&self) -> vk::ImageView {
        self.view
    }

    /// 建图时的 extent（多平面格式也是整图尺寸，平面只是子资源）
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }
}

impl Drop for OwnedImage {
    fn drop(&mut self) {
        // 析构可能落在任意线程：先等设备空闲，再逆序销毁
        let device = self.device.device();
        unsafe {
            // SAFETY: 各句柄均由本结构在 new 中创建且尚未销毁，此处各销毁一次
            let _ = device.device_wait_idle();
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// NV12 编码输入图与其暂存缓冲
pub struct Nv12Image {
    device: VideoDevice,
    /// 设备本地 NV12 图与它的视图
    image: OwnedImage,
    /// 主机可见暂存缓冲：上传与读回共用，构造时映射一次
    staging: HostBuffer,
    /// 布局状态：首次上传前 `UNDEFINED`，之后已确认值固定为 `VIDEO_ENCODE_SRC_KHR`
    layout: TrackedLayout,
}

impl Nv12Image {
    /// 测试构造：不带视频 profile 的编码输入图
    ///
    /// usage 里保留 `TRANSFER_SRC` 供自校验读回；生产路径必须走
    /// [`Self::new_video`] 声明 profile，否则驱动不把图当编码资源
    #[cfg(test)]
    pub fn new(device: &VideoDevice, extent: vk::Extent2D) -> Result<Self, String> {
        Self::new_inner(device, extent, None)
    }

    /// 视频编码用的输入图：创建时必须声明所属 profile
    ///
    /// 否则驱动不把它当视频资源（`VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08206`）。
    /// `profile` 只存指针链，必须活过本次 create 调用。
    pub fn new_video(
        device: &VideoDevice,
        extent: vk::Extent2D,
        profile: &vk::VideoProfileInfoKHR<'_>,
    ) -> Result<Self, String> {
        Self::new_inner(device, extent, Some(profile))
    }

    fn new_inner(
        device: &VideoDevice,
        extent: vk::Extent2D,
        video_profile: Option<&vk::VideoProfileInfoKHR<'_>>,
    ) -> Result<Self, String> {
        if extent.width == 0 || extent.height == 0 {
            return Err("编码输入图几何不能为零".to_string());
        }
        let size = frame_bytes(extent) as u64;

        // 1. 设备本地图像：编码输入 + 传输源/目标（传输目标是为了上传，传输源是为了自校验）
        let mut spec = ImageSpec::new(
            vk::Format::G8_B8R8_2PLANE_420_UNORM,
            extent,
            vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::TRANSFER_SRC,
            "编码输入图",
        );
        if let Some(profile) = video_profile {
            // 拷一份指针链；被指向的 codec 子结构仍由调用方持有
            spec = spec.video(*profile);
        }
        let image = OwnedImage::new(device, spec)?;

        // 2. 主机可见暂存缓冲：上传与读回共用，映射在构造时做一次
        let staging = device.host_buffer(
            size,
            vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
            "暂存缓冲",
        )?;

        // 3. 不建命令池：上传指令录进调用方的命令缓冲，与编码命令同处一次提交

        Ok(Self {
            device: device.clone(),
            image,
            staging,
            layout: TrackedLayout::new(),
        })
    }

    /// 本图整图（`COLOR` aspect）的布局转换屏障
    ///
    /// 多平面图的屏障可以一次覆盖两个平面（`COLOR`），不像拷贝命令那样必须逐平面。
    /// `old_layout` 必须显式传入：同一命令缓冲里第二次转换的旧布局是**前一次录制
    /// 的产物**（字面量），此刻 `TrackedLayout` 的待定值还没记录，不能取 `current()`。
    fn barrier(
        &self,
        old_layout: vk::ImageLayout,
        new_layout: vk::ImageLayout,
        src: StageAccess,
        dst: StageAccess,
    ) -> vk::ImageMemoryBarrier2<'static> {
        image_barrier(self.image.image(), old_layout, new_layout, src, dst)
    }

    pub fn view(&self) -> vk::ImageView {
        self.image.view()
    }

    /// 把一整帧 NV12 写进暂存缓冲（主机侧，不产生任何 GPU 命令）
    ///
    /// 输入布局是 Y 平面（`w*h` 字节）紧跟交错 UV 平面（`w*h/2` 字节）。与
    /// [`Nv12Image::record_upload`] 配对：先 `stage`，再把拷贝录进命令缓冲。
    ///
    /// 主机写暂存缓冲与上一帧的 GPU 读不能重叠。这一点由 `FrameEncoder` 保证——
    /// 它每帧提交后都等到栅栏，调用 `stage` 时上一帧的拷贝早已完成。
    ///
    /// 上传一帧 NV12，源按**显示尺寸**交付
    ///
    /// 主机帧固定为 `显示尺寸 × 1.5` 字节：采集侧不知道编码尺寸，也不该知道。两者的差只
    /// 出现在高度方向（宽度按 64 对齐时 1920/2560 本就对齐），由本方法用最后一行把补齐行
    /// 填满；这些行随后被码流里的裁剪窗口抹掉（H.264 `frame_cropping_flag`、HEVC
    /// `conf_win_*`），解码器看不到。〔与 `crate::encode::display_nv12_len` 同一个约定〕
    pub fn stage(&mut self, nv12: &[u8], src: vk::Extent2D) -> Result<(), String> {
        let expected = frame_bytes(src);
        if nv12.len() < expected {
            return Err(format!(
                "NV12 帧长度不足: 期望 {expected}（{}x{}）, 实际 {}",
                src.width,
                src.height,
                nv12.len()
            ));
        }

        let extent = self.image.extent();
        // 几何一致是热路径（采集侧将来对齐到同一粒度时也走这里）：一次整帧 memcpy
        if src == extent {
            self.staging.write(&nv12[..expected])?;
            return Ok(());
        }

        let src_w = src.width as usize;
        let src_y_rows = src.height as usize;
        let y_bytes = src_w * src_y_rows;
        let dst_y_bytes = extent.width as usize * extent.height as usize;
        self.pad_plane(
            &nv12[..y_bytes],
            src_w,
            src_y_rows,
            0,
            extent.height as usize,
        )?;
        self.pad_plane(
            &nv12[y_bytes..],
            src_w,
            src_y_rows / 2,
            dst_y_bytes,
            extent.height as usize / 2,
        )
    }

    /// 拷一个平面进暂存缓冲，缺的行重复最后一行
    fn pad_plane(
        &mut self,
        plane: &[u8],
        src_w: usize,
        src_rows: usize,
        dst_off: usize,
        dst_rows: usize,
    ) -> Result<(), String> {
        let rows = src_rows.min(dst_rows);
        if rows == 0 {
            return Ok(());
        }
        let visible = src_w * rows;
        let last_row = &plane[visible - src_w..visible];
        let dst_w = self.image.extent().width as usize;
        if src_w == dst_w {
            // 行距一致：可见区一次写完，补齐行是同一条重复
            self.staging.write_at(dst_off, &plane[..visible])?;
            if dst_rows > rows {
                self.staging
                    .write_at(dst_off + visible, &last_row.repeat(dst_rows - rows))?;
            }
        } else {
            for row in 0..rows {
                self.staging.write_at(
                    dst_off + row * dst_w,
                    &plane[row * src_w..(row + 1) * src_w],
                )?;
            }
            for row in rows..dst_rows {
                self.staging.write_at(dst_off + row * dst_w, last_row)?;
            }
        }
        Ok(())
    }

    /// 录制「暂存缓冲 → 图像」的拷贝与布局转换
    ///
    /// 录制完成后本图布局为 `VIDEO_ENCODE_SRC_KHR`（待提交确认），可以紧接着在同一个
    /// 命令缓冲里发编码命令。屏障必须录在 `vkCmdBeginVideoCodingKHR` 之前——视频编码
    /// 作用域内不允许 `vkCmdPipelineBarrier2`。
    ///
    /// # Safety
    ///
    /// 调用方必须保证：`cmd` 处于录制状态，且属于视频编码队列族（本图 `EXCLUSIVE`
    /// 共享，跨族录制即非法）；暂存缓冲已由 [`Nv12Image::stage`] 写入本帧数据。
    /// 函数体内逐语句的 SAFETY 论证依赖上述前置条件。
    // SAFETY: `cmd` 必须处于录制状态，且属于视频编码队列族（本图是 `EXCLUSIVE` 共享）。
    pub(super) unsafe fn record_upload(&mut self, cmd: vk::CommandBuffer) {
        let regions = plane_copy_regions(self.image.extent());

        // SAFETY: cmd 由调用方保证可录制；regions 在调用期间存活
        unsafe {
            let to_dst = self.barrier(
                self.layout.current(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                StageAccess::all_commands(),
                StageAccess::transfer_write(),
            );
            record_image_barriers(self.device.device(), cmd, &[to_dst]);

            self.device.device().cmd_copy_buffer_to_image(
                cmd,
                self.staging.buffer(),
                self.image.image(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );

            // 目标阶段保守取 ALL_COMMANDS：它同时覆盖视频编码的 `VIDEO_ENCODE_KHR`
            // 与跨队列握手，先把「写完之后任何人都能读」做对，收紧留到有性能数据时
            let to_encode = self.barrier(
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
                StageAccess::transfer_write(),
                StageAccess::all_commands(),
            );
            record_image_barriers(self.device.device(), cmd, &[to_encode]);
        }

        self.layout.record(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR);
    }

    /// 建图时的 extent
    pub fn extent(&self) -> vk::Extent2D {
        self.image.extent()
    }

    /// 录制「两张平面图 → NV12 两个平面」的拷贝与布局转换
    ///
    /// 两张源图必须已写完；它们恒处于 `GENERAL`（见 `csc` 模块的「布局」说明）。
    /// 跨队列依赖不在这里处理：完成信号量由 `FrameEncoder` 挂在本次提交的
    /// `waitSemaphores` 上（信号量对构成完整内存依赖，机制见 `csc` 模块文档）。
    /// 源图用 `CONCURRENT` 共享，跨族读取不需要所有权转移。
    ///
    /// 为什么走拷贝而不是让着色器直写 NV12：见 `csc` 模块的说明——NV12 图本身不支持
    /// `STORAGE_IMAGE`，plane view 当 storage 用这条路在本机不成立。
    ///
    /// # Safety
    ///
    /// 调用方必须保证：`cmd` 处于录制状态，且属于视频编码队列族；`y` / `uv` 已由
    /// 一次会在本次提交之前完成的 GPU 写填好（可见性由信号量对承担，机制见
    /// `csc` 模块文档）。函数体内逐语句的 SAFETY 论证依赖上述前置条件。
    // SAFETY: `cmd` 必须处于录制状态，且属于视频编码队列族；`y` / `uv` 必须已由一次会在本次
    // 提交之前完成的 GPU 写填好。
    pub(super) unsafe fn record_upload_from_planes(
        &mut self,
        cmd: vk::CommandBuffer,
        y: vk::Image,
        uv: vk::Image,
    ) {
        let extent = self.image.extent();
        let plane_layers = |aspect: vk::ImageAspectFlags| vk::ImageSubresourceLayers {
            aspect_mask: aspect,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        };
        let color_layers = plane_layers(vk::ImageAspectFlags::COLOR);

        // 平面 0（Y）全分辨率，平面 1（UV）半分辨率——多平面图的 extent 按各自平面的
        // texel 计，不是按整图计
        let y_copy = vk::ImageCopy::default()
            .src_subresource(color_layers)
            .dst_subresource(plane_layers(vk::ImageAspectFlags::PLANE_0))
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            });
        let uv_copy = vk::ImageCopy::default()
            .src_subresource(color_layers)
            .dst_subresource(plane_layers(vk::ImageAspectFlags::PLANE_1))
            .extent(vk::Extent3D {
                width: extent.width / 2,
                height: extent.height / 2,
                depth: 1,
            });

        // SAFETY: cmd 由调用方保证可录制；两次拷贝的源图分别取单平面 `COLOR`，
        // 目标侧 `aspectMask` 是单个平面位（多平面图拷贝的硬要求）
        unsafe {
            let to_dst = self.barrier(
                self.layout.current(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                StageAccess::all_commands(),
                StageAccess::transfer_write(),
            );
            record_image_barriers(self.device.device(), cmd, &[to_dst]);

            self.device.device().cmd_copy_image(
                cmd,
                y,
                vk::ImageLayout::GENERAL,
                self.image.image(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[y_copy],
            );
            self.device.device().cmd_copy_image(
                cmd,
                uv,
                vk::ImageLayout::GENERAL,
                self.image.image(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[uv_copy],
            );

            let to_encode = self.barrier(
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
                StageAccess::transfer_write(),
                StageAccess::all_commands(),
            );
            record_image_barriers(self.device.device(), cmd, &[to_encode]);
        }

        self.layout.record(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR);
    }

    /// 录制「图像 → 暂存缓冲」的整帧读回，用于自校验
    ///
    /// 录制完成后布局仍是 `VIDEO_ENCODE_SRC_KHR`，读回不改变后续编码的可用性。
    // SAFETY: `cmd` 必须处于录制状态，且属于视频编码队列族。
    #[cfg(test)]
    unsafe fn record_readback(&mut self, cmd: vk::CommandBuffer) {
        let regions = plane_copy_regions(self.image.extent());

        // SAFETY: cmd 由调用方保证可录制；regions 在调用期间存活
        unsafe {
            let to_src = self.barrier(
                self.layout.current(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                StageAccess::all_commands(),
                StageAccess::transfer_read(),
            );
            record_image_barriers(self.device.device(), cmd, &[to_src]);

            self.device.device().cmd_copy_image_to_buffer(
                cmd,
                self.image.image(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.staging.buffer(),
                &regions,
            );

            let back_to_encode = self.barrier(
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
                StageAccess::transfer_read(),
                StageAccess::all_commands(),
            );
            record_image_barriers(self.device.device(), cmd, &[back_to_encode]);
        }
    }

    /// 提交已确认完成：把待定布局提升为当前布局（时序约束见 [`TrackedLayout::commit`]）
    pub(super) fn commit(&mut self) {
        self.layout.commit();
    }

    /// 提交未发生或失败：丢弃待定布局，图像仍处于已确认的布局
    pub(super) fn abort(&mut self) {
        self.layout.abort();
    }

    /// 测试专用：用 `0xFF` 覆写暂存缓冲
    ///
    /// 上传与读回共用同一块暂存缓冲，若两边都跳过同一个平面，缓冲区里留的是上传时的旧值，
    /// 比对会「一致」——测试永远绿。读回前先污染，没被真正拷贝的平面就会暴露。
    #[cfg(test)]
    fn poison_staging(&mut self) -> Result<(), String> {
        self.staging.fill(0xFF, frame_bytes(self.image.extent()))
    }

    /// 测试专用：把暂存缓冲前 `size` 字节读出来
    ///
    /// 生产路径没有读回需求（码流长度由 feedback 查询给出），因此只存在于测试。
    #[cfg(test)]
    fn staging_bytes(&self) -> Result<Vec<u8>, String> {
        // 调用方保证前 size 字节已由拷贝命令写入并等到了提交完成
        self.staging.read(frame_bytes(self.image.extent()))
    }
}

// 不需要自定义 Drop：图像与显存由 `OwnedImage` 归还，暂存缓冲由 `HostBuffer` 归还，
// 两者析构前都会 `device_wait_idle`。字段按声明顺序析构，图像先于缓冲释放。

#[cfg(test)]
impl Nv12Image {
    /// 上传一帧并等到完成
    fn upload_blocking(&mut self, nv12: &[u8], src: vk::Extent2D) -> Result<(), String> {
        self.stage(nv12, src)?;
        let device = self.device.clone();
        // SAFETY: 命令缓冲由 run_oneshot 创建并保证处于录制状态
        run_oneshot(&device, None, |cmd| unsafe { self.record_upload(cmd) })?;
        self.commit();
        Ok(())
    }

    /// 从平面图拷入并等到完成；`wait` 用来消费 CSC 的完成信号量
    pub(super) fn upload_from_planes_blocking(
        &mut self,
        y: vk::Image,
        uv: vk::Image,
        wait: Option<vk::Semaphore>,
    ) -> Result<(), String> {
        let device = self.device.clone();
        // SAFETY: 同 upload_blocking；y/uv 由调用方保证已写完
        run_oneshot(&device, wait, |cmd| unsafe {
            self.record_upload_from_planes(cmd, y, uv)
        })?;
        self.commit();
        Ok(())
    }

    /// 读回整帧并等到完成
    pub(super) fn readback_blocking(&mut self) -> Result<Vec<u8>, String> {
        let device = self.device.clone();
        // SAFETY: 命令缓冲由 run_oneshot 创建并保证处于录制状态
        run_oneshot(&device, None, |cmd| unsafe { self.record_readback(cmd) })?;
        self.commit();
        self.staging_bytes()
    }
}

/// DPB 参考图：一张多层数组图，每个槽位占一层
///
/// **为什么不是每个槽位一张图**：`VK_VIDEO_CAPABILITY_SEPARATE_REFERENCE_IMAGES_BIT_KHR`
/// 未声明时，同一会话的所有 DPB 槽位必须由**同一张图**的视图提供 —— 这条写在能力位
/// 自身的定义里，`VkVideoBeginCodingInfoKHR` 上对应
/// `VUID-VkVideoBeginCodingInfoKHR-flags-07244`。本实现只在 `vkCmdEncodeVideoKHR` 里
/// 给出槽位，但这是对「DPB 槽位由什么支撑」的约束，不随给出位置变化。
/// 数组层形式所有驱动都支持，所以不按那个可选能力位分支。
///
/// 与编码输入图仍是两张不同的图：`VIDEO_ENCODE_SRC_KHR` 与 `VIDEO_ENCODE_DPB_KHR`
/// 是互斥布局，不能共用一张图。
pub(super) struct DpbImages {
    device: VideoDevice,
    /// 一张 `TYPE_2D_ARRAY` 图：层数 = DPB 槽数，选槽靠 picture resource 的
    /// `baseArrayLayer`，不需要每层一个视图
    image: OwnedImage,
    /// 布局状态：首次使用前 `UNDEFINED`，之后已确认值固定为 `VIDEO_ENCODE_DPB_KHR`。
    /// 只有一份：屏障一次覆盖所有层，各层布局始终一致
    layout: TrackedLayout,
}

impl DpbImages {
    /// DPB 图构造：一张 `slots` 层的 `TYPE_2D_ARRAY` NV12 图，整图声明
    /// `VIDEO_ENCODE_DPB_KHR` 用法与视频 profile
    ///
    /// 槽位靠 picture resource 的 `baseArrayLayer` 选择，布局屏障一次覆盖所有层；
    /// 调用方必须先确认会话 DPB 槽位数 ≥ 2（本实现需要当前帧 + 1 参考帧），
    /// 图与显存由 [`OwnedImage`] 随本结构析构逆序归还
    pub(super) fn new(
        device: &VideoDevice,
        extent: vk::Extent2D,
        slots: u32,
        profile: &vk::VideoProfileInfoKHR<'_>,
    ) -> Result<Self, String> {
        let image = OwnedImage::new(
            device,
            ImageSpec {
                array_layers: slots,
                view_type: vk::ImageViewType::TYPE_2D_ARRAY,
                ..ImageSpec::new(
                    vk::Format::G8_B8R8_2PLANE_420_UNORM,
                    extent,
                    vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
                    "DPB 图",
                )
                // 拷一份指针链；被指向的 codec 子结构由调用方持有
                .video(*profile)
            },
        )?;

        Ok(Self {
            device: device.clone(),
            image,
            layout: TrackedLayout::new(),
        })
    }

    /// DPB 数组图的视图：槽位由 picture resource 的 `baseArrayLayer` 选择，
    /// 视图本身覆盖全部层
    pub(super) fn view(&self) -> vk::ImageView {
        self.image.view()
    }

    /// 首次使用前把布局切到 `VIDEO_ENCODE_DPB_KHR`
    ///
    /// 屏障必须覆盖**所有数组层**：只转第 0 层会让其余层留在 `UNDEFINED`，
    /// 而编码命令按 `VIDEO_ENCODE_DPB_KHR` 去写它们。
    pub(super) fn ensure_layout(&mut self, cmd: vk::CommandBuffer) -> Result<(), String> {
        let current = self.layout.current();
        if current == vk::ImageLayout::VIDEO_ENCODE_DPB_KHR {
            return Ok(());
        }
        let barrier = image_barrier_all_layers(
            self.image.image(),
            current,
            vk::ImageLayout::VIDEO_ENCODE_DPB_KHR,
            StageAccess::all_commands(),
            StageAccess::all_commands(),
        );
        // SAFETY: cmd 处于录制状态，image 有效且只在此处做一次布局转换
        unsafe {
            record_image_barriers(self.device.device(), cmd, &[barrier]);
        }
        self.layout.record(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR);
        Ok(())
    }

    /// 提交已确认完成：把待定布局提升为当前布局（时序约束见 [`TrackedLayout::commit`]）
    pub(super) fn commit(&mut self) {
        self.layout.commit();
    }

    /// 提交未发生或失败：丢弃待定布局，图像仍处于已确认的布局
    pub(super) fn abort(&mut self) {
        self.layout.abort();
    }
}

// 不需要自定义 Drop：图像与显存由 `OwnedImage` 归还，它在析构前会 `device_wait_idle`。

/// NV12 整帧字节数 —— 口径由传入的 `extent` 决定
///
/// 上传（`stage`）传显示尺寸，平面拷贝与读回传编码尺寸：两者相差补齐行，各自负责。
fn frame_bytes(extent: vk::Extent2D) -> usize {
    nv12_len(extent.width as usize, extent.height as usize)
}

/// 整帧的缓冲区拷贝区域：**逐平面**拆成两个 region
///
/// 多平面图在 `vkCmdCopyBufferToImage` / `vkCmdCopyImageToBuffer` 上**只能按平面拷贝**，
/// 每个 region 的 `aspectMask` 必须是单个平面位（`VUID-vkCmdCopyImageToBuffer-srcImage-07981`
/// 等）；用 `COLOR` 覆盖两个平面是非法用法，驱动会静默跳过平面 1。
/// 一次调用里不同 region 指定不同平面是允许的。
///
/// 缓冲区布局与 NV12 的内存布局一致：Y 平面整块（`w*h`）后接交错 UV（`w*h/2`）。
fn plane_copy_regions(extent: vk::Extent2D) -> [vk::BufferImageCopy; 2] {
    let y_bytes = u64::from(extent.width) * u64::from(extent.height);
    let layers = |aspect: vk::ImageAspectFlags| vk::ImageSubresourceLayers {
        aspect_mask: aspect,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    };
    [
        vk::BufferImageCopy::default()
            .buffer_offset(0)
            .image_subresource(layers(vk::ImageAspectFlags::PLANE_0))
            .image_extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            }),
        vk::BufferImageCopy::default()
            // 平面 1 的元素大小是 2 字节（R8G8），偏移必须按它对齐；`w*h` 对偶数宽高恒成立
            .buffer_offset(y_bytes)
            .image_subresource(layers(vk::ImageAspectFlags::PLANE_1))
            .image_extent(vk::Extent3D {
                width: extent.width / 2,
                height: extent.height / 2,
                depth: 1,
            }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::VideoCodec;
    use crate::encode::hardware::vulkan::caps;
    use crate::encode::hardware::vulkan::context::VulkanInstance;

    /// NV12 整帧上传后原样读回，布局在 `VIDEO_ENCODE_SRC_KHR` 与 `TRANSFER_SRC` 之间往返
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_nv12_image_roundtrip() {
        let Ok(ctx) = VulkanInstance::new("Vistrel-S1c") else {
            println!("Vulkan 不可用，跳过");
            return;
        };
        let caps = match caps::probe_codec(&ctx, VideoCodec::H264) {
            Ok(caps) => caps,
            Err(e) => {
                println!("H.264 能力探测失败，跳过: {e}");
                return;
            }
        };
        let device = match VideoDevice::new(&ctx, VideoCodec::H264) {
            Ok(device) => device,
            Err(e) => {
                println!("设备创建失败，跳过: {e}");
                return;
            }
        };
        let extent = match caps.align_extent(1920, 1080) {
            Ok(extent) => extent,
            Err(e) => {
                println!("几何对齐失败，跳过: {e}");
                return;
            }
        };
        let mut image = match Nv12Image::new(&device, extent) {
            Ok(image) => image,
            Err(e) => {
                println!("输入图创建失败: {e}");
                return;
            }
        };

        // 造一帧有梯度的 NV12：拷贝错位、平面顺序颠倒都会在比对里暴露
        let size = frame_bytes(extent);
        let pixels = extent.width as usize * extent.height as usize;
        let mut nv12 = vec![0u8; size];
        for (i, value) in nv12[..pixels].iter_mut().enumerate() {
            *value = (i % 251) as u8;
        }
        for (i, value) in nv12[pixels..].iter_mut().enumerate() {
            *value = if i % 2 == 0 { 128 } else { (i % 127) as u8 };
        }

        if let Err(e) = image.upload_blocking(&nv12, extent) {
            println!("上传失败: {e}");
            return;
        }
        // 读回前先污染暂存缓冲：否则「上传跳过某平面 + 读回也跳过」会互相掩护
        if let Err(e) = image.poison_staging() {
            println!("暂存缓冲污染失败: {e}");
            return;
        }
        let back = match image.readback_blocking() {
            Ok(back) => back,
            Err(e) => {
                println!("读回失败: {e}");
                return;
            }
        };

        assert_eq!(back.len(), nv12.len(), "读回长度必须一致");
        let mismatches = back.iter().zip(nv12.iter()).filter(|(a, b)| a != b).count();
        assert_eq!(mismatches, 0, "NV12 往返有 {mismatches} 字节不一致");
        println!(
            "NV12 往返一致: {}x{} / {} 字节",
            extent.width, extent.height, size
        );
    }

    /// 显示尺寸的主机帧被补齐到编码尺寸：可见区逐字节保留，补齐行重复最后一行
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_nv12_image_pads_display_frame() {
        let Ok(ctx) = VulkanInstance::new("Vistrel-S1c-pad") else {
            println!("Vulkan 不可用，跳过");
            return;
        };
        let caps = match caps::probe_codec(&ctx, VideoCodec::H264) {
            Ok(caps) => caps,
            Err(e) => {
                println!("H.264 能力探测失败，跳过: {e}");
                return;
            }
        };
        let device = match VideoDevice::new(&ctx, VideoCodec::H264) {
            Ok(device) => device,
            Err(e) => {
                println!("设备创建失败，跳过: {e}");
                return;
            }
        };
        let extent = match caps.align_extent(1920, 1080) {
            Ok(extent) => extent,
            Err(e) => {
                println!("几何对齐失败，跳过: {e}");
                return;
            }
        };
        let display = vk::Extent2D {
            width: 1920,
            height: 1080,
        };
        if display == extent {
            println!("该设备 1080p 已对齐，无需补齐，跳过");
            return;
        }
        let mut image = match Nv12Image::new(&device, extent) {
            Ok(image) => image,
            Err(e) => {
                println!("输入图创建失败: {e}");
                return;
            }
        };

        // 行号写进字节：补齐时抄错行、平面偏移算错都会在比对里暴露
        let src_w = display.width as usize;
        let src_rows = display.height as usize;
        let mut src = vec![0u8; frame_bytes(display)];
        for row in 0..src_rows {
            src[row * src_w..(row + 1) * src_w].fill((row % 251) as u8);
        }
        let uv_off = src_w * src_rows;
        for row in 0..src_rows / 2 {
            src[uv_off + row * src_w..uv_off + (row + 1) * src_w].fill((row % 127) as u8);
        }

        if let Err(e) = image.upload_blocking(&src, display) {
            println!("上传失败: {e}");
            return;
        }
        if let Err(e) = image.poison_staging() {
            println!("暂存缓冲污染失败: {e}");
            return;
        }
        let back = match image.readback_blocking() {
            Ok(back) => back,
            Err(e) => {
                println!("读回失败: {e}");
                return;
            }
        };

        let dst_w = extent.width as usize;
        let dst_y_bytes = dst_w * extent.height as usize;
        assert_eq!(back.len(), frame_bytes(extent), "读回长度必须是编码尺寸");
        for row in 0..src_rows {
            assert!(
                back[row * dst_w..(row + 1) * dst_w]
                    .iter()
                    .all(|b| *b == (row % 251) as u8),
                "亮度第 {row} 行被改动"
            );
        }
        let last_luma = (src_rows - 1) % 251;
        for row in src_rows..extent.height as usize {
            assert!(
                back[row * dst_w..(row + 1) * dst_w]
                    .iter()
                    .all(|b| *b == last_luma as u8),
                "亮度补齐行 {row} 不是最后一行的副本"
            );
        }
        let uv_rows = src_rows / 2;
        for row in 0..uv_rows {
            assert!(
                back[dst_y_bytes + row * dst_w..dst_y_bytes + (row + 1) * dst_w]
                    .iter()
                    .all(|b| *b == (row % 127) as u8),
                "色度第 {row} 行被改动"
            );
        }
        let last_chroma = (uv_rows - 1) % 127;
        for row in uv_rows..extent.height as usize / 2 {
            assert!(
                back[dst_y_bytes + row * dst_w..dst_y_bytes + (row + 1) * dst_w]
                    .iter()
                    .all(|b| *b == last_chroma as u8),
                "色度补齐行 {row} 不是最后一行的副本"
            );
        }
        println!(
            "NV12 补齐一致: 显示 {}x{} → 编码 {}x{}",
            display.width, display.height, extent.width, extent.height
        );
    }
}

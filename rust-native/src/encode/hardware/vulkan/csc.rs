//! RGBA → NV12 计算着色器转换 (Compute Color Space Conversion)
//!
//! 渲染侧给的是 RGBA8，编码器要的是 NV12。转换在计算队列上跑，源图由调用方提供
//! （显存槽位视图），逐像素换算全部交给 GPU。
//!
//! **源图走 sampled image**：绑定 0 是 `texture2D` + `texelFetch`，描述符类型
//! `SAMPLED_IMAGE`（不需要 `VkSampler`）。storage image 要求视图格式与着色器里声明的
//! 格式一致，而 BGRA8 / SRGB 在多数实现上没有 `STORAGE_IMAGE` 能力 —— 显存直通那条路
//! 会直接不可用。采样读取不在着色器里声明格式，通道顺序由视图格式决定。
//!
//! **输出为什么是两张独立平面图，而不是 NV12 图的两个 plane view**：
//! 要让 plane view 当 storage image 用，NV12 图本身必须带 `STORAGE` 用法，也就要求
//! `G8_B8R8_2PLANE_420_UNORM` 在 optimal tiling 下支持 `STORAGE_IMAGE`。实测（S0 探测）
//! 这项能力在 NVIDIA 上**不存在**——R8 / R8G8 有 storage，不代表 NV12 图能有。
//! 所以这里写两张独立平面图（R8 / R8G8），再由调用方用 `vkCmdCopyImage` 拷进 NV12 的
//! 两个平面。多一次设备内拷贝（1080p 约 3MB），换取对可选格式特性的零依赖。
//!
//! **跨队列**：CSC 在计算族，编码在视频编码族（异构硬件上常常不是同一个族）。平面图
//! 用 `CONCURRENT` 共享模式，拷贝侧不需要所有权转移；两个提交之间靠一个二值信号量接力，
//! 由编码侧的提交等待它 —— 主机不在中间阻塞，一帧只有末尾一个同步点。
//!
//! **布局**：平面图常驻 `GENERAL` —— storage 读写只允许 `GENERAL`，而拷贝命令的
//! src/dst 也接受它，布局状态机因此不存在，录制路径不再需要平面屏障。写完交给跨队列
//! 读的可见性由信号量对承担（两侧都是 `ALL_COMMANDS`，构成完整内存依赖）；跨帧次序
//! 由 `finish` 门栅承担。
//!
//! 生产侧的时间线信号量同样交给计算队列等（见 `submit_csc`），所以从「渲染写完」到
//! 「编码提交」之间主机一次都不需要醒来。

use ash::vk;

use super::device::VideoDevice;
use super::image::{ImageSpec, OwnedImage};
use super::sync::{StageAccess, image_barrier, record_image_barriers};

/// `rgba_to_nv12.comp` 的编译产物
///
/// 用 `include_bytes!` 内嵌而不是运行期读文件：编码路径的工作目录不可控，读盘会把
/// 「找不到着色器」变成运行时故障。SPIR-V 是构建产物，随仓库提交。
pub(super) const CSC_SPIRV: &[u8] = include_bytes!("../../../../shaders/rgba_to_nv12.spv");

/// 着色器里的 `local_size_x` / `local_size_y`
pub(super) const LOCAL_SIZE: u32 = 16;

/// 推送常量：目标宽高 + 源宽高（`PushConstants { uint width, height, src_width, src_height; }`）
///
/// 源可以小于目标（槽位按采集尺寸建、编码器要编码尺寸），着色器按源尺寸夹取，
/// 多出来的列/行是边缘复制的补齐边。
pub(super) const PUSH_CONSTANT_SIZE: u32 = 16;

/// 一次分派要绑定的描述符数量：源图 + Y 平面 + UV 平面
const DESCRIPTOR_COUNT: u32 = 3;

/// RGBA → NV12 的转换器
///
/// 一次构造、逐帧复用：持有两张平面输出图与计算管线。
///
/// 描述符不做持久化——每帧随命令缓冲推送（`vkCmdPushDescriptorSet`），所以不存在
/// 「描述符集指向哪个视图」这类需要同步的状态；描述符集布局也在并入管线布局后于
/// 构造期内销毁，不占持有状态。
/// 输出图的生命周期由本结构管理，调用方通过 [`ColorConverter::plane_images`] 拿到句柄
/// 去拷贝，不取得所有权。几何以 Y 平面为准（`self.y.extent()`），不另存字段。
pub struct ColorConverter {
    device: VideoDevice,
    /// Y 平面（R8，全分辨率）
    y: OwnedImage,
    /// UV 平面（R8G8，半分辨率，交错 U/V）
    uv: OwnedImage,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    /// 跨队列完成信号量：计算队列 signal，编码提交 wait
    ///
    /// 二值信号量，一次在途提交对应一次 signal / 一次 wait。等待完成前不能再 signal，
    /// 所以 `pending` 为真时拒绝录制下一帧。
    semaphore: vk::Semaphore,
    /// 有一次提交已发出、尚未确认完成
    pending: bool,
}

/// `ColorConverter` 构造期的资源守卫
///
/// 管线布局、管线、命令池、命令缓冲、信号量在构造成功之前都属于
/// 「已创建、但还没有主人」的状态。任一步失败时由 `Drop` 逆序归还；
/// `finish` 里 `take()` 之后守卫不再持有任何东西。描述符布局是例外——
/// 不交给 `finish`，构造内在管线建成后就地销毁（见 `new` 内注释）。
struct CscBuildGuard {
    device: VideoDevice,
    set_layout: Option<vk::DescriptorSetLayout>,
    pipeline_layout: Option<vk::PipelineLayout>,
    pipeline: Option<vk::Pipeline>,
    pool: Option<vk::CommandPool>,
    cmd: Option<vk::CommandBuffer>,
    semaphore: Option<vk::Semaphore>,
}

impl CscBuildGuard {
    fn new(device: &VideoDevice) -> Self {
        Self {
            device: device.clone(),
            set_layout: None,
            pipeline_layout: None,
            pipeline: None,
            pool: None,
            cmd: None,
            semaphore: None,
        }
    }

    fn finish(mut self) -> CscParts {
        CscParts {
            pipeline_layout: self.pipeline_layout.take().expect("管线布局已创建"),
            pipeline: self.pipeline.take().expect("计算管线已创建"),
            pool: self.pool.take().expect("命令池已创建"),
            cmd: self.cmd.take().expect("命令缓冲已分配"),
            semaphore: self.semaphore.take().expect("信号量已创建"),
        }
    }
}

impl Drop for CscBuildGuard {
    fn drop(&mut self) {
        // 构造期没有提交过任何命令，不需要 `device_wait_idle`。
        // SAFETY: 句柄均由本守卫在构造期创建且尚未交出，此处各销毁一次；
        // 命令缓冲随命令池一并回收
        unsafe {
            let device = self.device.device();
            if let Some(semaphore) = self.semaphore.take() {
                device.destroy_semaphore(semaphore, None);
            }
            if let Some(pool) = self.pool.take() {
                device.destroy_command_pool(pool, None);
            }
            if let Some(pipeline) = self.pipeline.take() {
                device.destroy_pipeline(pipeline, None);
            }
            if let Some(layout) = self.pipeline_layout.take() {
                device.destroy_pipeline_layout(layout, None);
            }
            if let Some(set_layout) = self.set_layout.take() {
                device.destroy_descriptor_set_layout(set_layout, None);
            }
        }
    }
}

/// 构造完成、可以交给 `ColorConverter` 的裸句柄集合
struct CscParts {
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    semaphore: vk::Semaphore,
}

impl ColorConverter {
    /// 转换器构造：准入校验（偶数几何、输出平面 storage 能力、push descriptor 上限）
    /// → 两张平面输出图 → 计算管线与命令资源，全成全败
    ///
    /// 任一环节失败由构造守卫逆序回滚，不留半就绪状态；几何即编码几何
    /// （Y 平面全分辨率），构造后不可变
    pub fn new(device: &VideoDevice, extent: vk::Extent2D) -> Result<Self, String> {
        if extent.width == 0 || extent.height == 0 {
            return Err("CSC 几何不能为零".to_string());
        }
        if !extent.width.is_multiple_of(2) || !extent.height.is_multiple_of(2) {
            return Err(format!(
                "CSC 要求偶数几何（4:2:0 色度抽样），实际 {}x{}",
                extent.width, extent.height
            ));
        }

        // 1. 准入校验：两个输出平面必须能当 storage image，否则整条 GPU 转换路径不成立。
        //    源图走 sampled image，不需要查——RGBA8 / BGRA8 的采样支持是规范保证的
        let support = device.format_support();
        if !support.all() {
            return Err(format!(
                "本设备不支持 CSC 输出所需的 storage image 格式: {support:?}"
            ));
        }

        // push descriptor 准入：1.4 的核心能力（`pushDescriptor` feature 位）或
        // `VK_KHR_push_descriptor` 扩展，两条通路都由设备侧判定并折算成同一个上限。
        // 上限按实际绑定数比，不假设某个下限。
        let max_push = device.max_push_descriptors();
        if max_push < DESCRIPTOR_COUNT {
            return Err(format!(
                "push descriptor 不可用：maxPushDescriptors = {}，需要 {}",
                max_push, DESCRIPTOR_COUNT
            ));
        }
        // 2. 两张平面输出图：跨族共享（计算写、视频编码族读），故用 CONCURRENT
        //
        // 族列表必须去重：某些机器上计算与编码落在同一个族，此时 `[n, n]` 是非法的
        // 共享模式声明，必须退回 EXCLUSIVE。
        let mut families = vec![device.compute_queue_family()];
        if device.video_queue_family() != families[0] {
            families.push(device.video_queue_family());
        }
        let plane_usage = vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC;
        let y = OwnedImage::new(
            device,
            ImageSpec {
                sharing: &families,
                ..ImageSpec::new(vk::Format::R8_UNORM, extent, plane_usage, "CSC Y 平面")
            },
        )?;
        let uv_extent = vk::Extent2D {
            width: extent.width / 2,
            height: extent.height / 2,
        };
        let uv = OwnedImage::new(
            device,
            ImageSpec {
                sharing: &families,
                ..ImageSpec::new(
                    vk::Format::R8G8_UNORM,
                    uv_extent,
                    plane_usage,
                    "CSC UV 平面",
                )
            },
        )?;

        // 3. 描述符布局：绑定 0 是 sampled image（源图），绑定 1/2 是 storage image（输出平面）
        //
        // `PUSH_DESCRIPTOR` 标志让这个布局只能用于推送：用 `vkCmdPushDescriptorSet` 绑定，
        // 不能再 `vkAllocateDescriptorSets`（这正是要的——没有池、没有集）。
        //
        // 设备版本不到 1.4、走 `VK_KHR_push_descriptor` 时这个位与 KHR 的同名位数值相同，
        // 不需要分支。
        //
        // 逐条写出而不是循环构造：`DescriptorSetLayoutBinding` 带生命周期参数，
        // 交给推断比显式标注更容易与 `bindings()` 的签名对齐
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let set_layout_info = vk::DescriptorSetLayoutCreateInfo::default()
            .flags(vk::DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR)
            .bindings(&bindings);
        // 从这里开始是「已创建、还没有主人」的句柄：交给构造期守卫，任一步失败由它归还
        let mut res = CscBuildGuard::new(device);
        // SAFETY: set_layout_info 引用的 bindings 在调用期间存活
        res.set_layout = Some(
            unsafe {
                device
                    .device()
                    .create_descriptor_set_layout(&set_layout_info, None)
            }
            .map_err(|e| format!("CSC 描述符布局创建失败: {e:?}"))?,
        );
        let set_layout = res.set_layout.expect("描述符布局刚创建");

        // 6. 管线布局：一个推送式描述符布局 + 8 字节推送常量
        let push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(PUSH_CONSTANT_SIZE);
        let pipeline_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&set_layout))
            .push_constant_ranges(std::slice::from_ref(&push_range));
        // SAFETY: pipeline_layout_info 引用的对象在调用期间存活
        res.pipeline_layout = Some(
            unsafe {
                device
                    .device()
                    .create_pipeline_layout(&pipeline_layout_info, None)
            }
            .map_err(|e| format!("CSC 管线布局创建失败: {e:?}"))?,
        );
        let pipeline_layout = res.pipeline_layout.expect("管线布局刚创建");

        // 7. 着色器模块。`read_spv` 接管三件事：长度必须是 4 的倍数、校验魔数、按 u32 组装。
        //    第三件是必须的 —— `include_bytes!` 给的对齐只有 1，把 `&[u8]` 直接当 `&[u32]`
        //    用是 UB。它顺带处理了大端文件，并且返回的 `Vec<u32>` 天然对齐。
        //    这里只额外保留「至少放得下 5 个 word 的头」：比它短的文件连魔数都读不全，
        //    早点报出来比让驱动回一个笼统错误好定位。
        const SPIRV_HEADER_BYTES: usize = 5 * 4;
        if CSC_SPIRV.len() < SPIRV_HEADER_BYTES {
            return Err(format!(
                "CSC SPIR-V 长度非法: {} 字节（至少 {SPIRV_HEADER_BYTES}）",
                CSC_SPIRV.len()
            ));
        }
        let words = ash::util::read_spv(&mut std::io::Cursor::new(CSC_SPIRV))
            .map_err(|e| format!("CSC SPIR-V 解析失败: {e}"))?;
        let module_info = vk::ShaderModuleCreateInfo::default().code(&words);
        // SAFETY: module_info 引用的 words 在调用期间存活
        let module = unsafe { device.device().create_shader_module(&module_info, None) }
            .map_err(|e| format!("CSC 着色器模块创建失败: {e:?}"))?;

        let entry = c"main";
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(entry);
        let pipeline_info = vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(pipeline_layout);
        // SAFETY: pipeline_info 及其引用的 module / layout 在调用期间存活
        res.pipeline = Some(
            match unsafe {
                device.device().create_compute_pipelines(
                    vk::PipelineCache::null(),
                    std::slice::from_ref(&pipeline_info),
                    None,
                )
            } {
                Ok(mut pipelines) => pipelines.remove(0),
                Err((_, e)) => {
                    // 管线没建成，模块还没被任何对象捕获，必须在这里销毁；
                    // 描述符布局与管线布局由 `res` 的 Drop 归还
                    // SAFETY: module 由本函数创建且尚未销毁，此处销毁一次
                    unsafe { device.device().destroy_shader_module(module, None) };
                    return Err(format!("CSC 计算管线创建失败: {e:?}"));
                }
            },
        );
        // 模块内容已被管线捕获，可以立刻销毁
        // SAFETY: module 由本函数创建且尚未销毁，此处销毁一次
        unsafe { device.device().destroy_shader_module(module, None) };

        // 描述符布局同理：绑定内容已并入管线布局。Object Lifetime 规定「作为参数传给
        // 创建命令的对象，在该命令结束后不再被访问」，push descriptor 布局又不能分配
        // 描述符集，没有其他生命周期约束——所以构造内就地销毁，不进入 `ColorConverter`
        // 的持有范围
        // SAFETY: 布局由本函数创建且尚未销毁，此处销毁一次
        unsafe {
            device.device().destroy_descriptor_set_layout(
                res.set_layout.take().expect("描述符布局已创建"),
                None,
            );
        }

        // 8. 不建描述符池：描述符每帧随命令缓冲推送，没有需要持久化的集

        // 9. 命令池挂在计算队列族上
        let cmd_pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(device.compute_queue_family());
        // SAFETY: cmd_pool_info 已完整初始化
        res.pool = Some(
            unsafe { device.device().create_command_pool(&cmd_pool_info, None) }
                .map_err(|e| format!("CSC 命令池创建失败: {e:?}"))?,
        );
        let cmd_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(res.pool.expect("命令池刚创建"))
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: cmd_alloc 指向刚创建的命令池
        res.cmd = Some(
            unsafe { device.device().allocate_command_buffers(&cmd_alloc) }
                .map_err(|e| format!("CSC 命令缓冲分配失败: {e:?}"))?[0],
        );

        // 10. 跨队列完成信号量：不建栅栏（理由见模块文档「跨队列」说明——
        // 完成与否由编码侧对信号量的等待传递过来）
        // SAFETY: 默认 SemaphoreCreateInfo，初始为未触发状态
        res.semaphore = Some(
            unsafe {
                device
                    .device()
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }
            .map_err(|e| format!("CSC 信号量创建失败: {e:?}"))?,
        );
        let res = res.finish();

        let mut converter = Self {
            device: device.clone(),
            y,
            uv,
            pipeline_layout: res.pipeline_layout,
            pipeline: res.pipeline,
            pool: res.pool,
            cmd: res.cmd,
            semaphore: res.semaphore,
            pending: false,
        };
        // 失败时 converter 的 Drop 兜底：它先 device_wait_idle 再逆序归还全部资源
        converter.init_plane_layouts()?;
        Ok(converter)
    }

    /// 平面图的一次性布局转换：`UNDEFINED` → `GENERAL`
    ///
    /// 平面图此后常驻 `GENERAL`（理由见模块文档的「布局」一节），本函数是它们唯一一次
    /// 布局变化。冷路径：提交后 `queue_wait_idle` 等完才返回；此时构造仍在进行，
    /// 失败即放弃整个转换器，不存在「半初始化」状态外泄。
    fn init_plane_layouts(&mut self) -> Result<(), String> {
        let to_general = [
            image_barrier(
                self.y.image(),
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::GENERAL,
                StageAccess::all_commands(),
                StageAccess::all_commands(),
            ),
            image_barrier(
                self.uv.image(),
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::GENERAL,
                StageAccess::all_commands(),
                StageAccess::all_commands(),
            ),
        ];
        let device = self.device.device();
        // SAFETY: 构造期尚无任何在途提交，命令缓冲处于可录制的初始状态；
        // to_general 及其引用的句柄在调用期间存活
        unsafe {
            device
                .begin_command_buffer(
                    self.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| format!("CSC 初始布局转换开始失败: {e:?}"))?;
            record_image_barriers(device, self.cmd, &to_general);
            device
                .end_command_buffer(self.cmd)
                .map_err(|e| format!("CSC 初始布局转换结束失败: {e:?}"))?;

            let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(self.cmd)];
            let submit = vk::SubmitInfo2::default().command_buffer_infos(&cmds);
            // SAFETY: submit 引用的 cmds 在调用期间存活；不传栅栏，
            // 以 `queue_wait_idle` 的队列排空为完成依据
            device
                .queue_submit2(self.device.compute_queue(), &[submit], vk::Fence::null())
                .map_err(|e| format!("CSC 初始布局转换提交失败: {e:?}"))?;
            device
                .queue_wait_idle(self.device.compute_queue())
                .map_err(|e| format!("CSC 初始布局转换等待失败: {e:?}"))?;
        }
        Ok(())
    }

    /// 把「源视图 + 两张平面视图」推入当前命令缓冲
    ///
    /// 绑定 0 是 sampled image：`GENERAL` 与 `SHADER_READ_ONLY_OPTIMAL` 都合法，这里沿用
    /// `GENERAL` —— 它与显存槽位跟踪的布局一致，换布局会破坏与生产侧的约定。
    ///
    /// 推送发生在**录制期**：描述符内容被拷进命令缓冲，命令提交时才生效。因此换显存槽位
    /// 不需要任何缓存或改写——每次录制直接推当前帧要用的视图。这也是选 push descriptor
    /// 而不是描述符池的主要理由：它消掉了「集指向哪个视图」这类状态。
    fn push_source_set(&self, source: vk::ImageView) {
        let infos = [
            vk::DescriptorImageInfo::default()
                .image_view(source)
                .image_layout(vk::ImageLayout::GENERAL),
            vk::DescriptorImageInfo::default()
                .image_view(self.y.view())
                .image_layout(vk::ImageLayout::GENERAL),
            vk::DescriptorImageInfo::default()
                .image_view(self.uv.view())
                .image_layout(vk::ImageLayout::GENERAL),
        ];
        // 推送时 `dstSet` 被忽略，保持默认空值即可
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(std::slice::from_ref(&infos[0])),
            vk::WriteDescriptorSet::default()
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(std::slice::from_ref(&infos[1])),
            vk::WriteDescriptorSet::default()
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(std::slice::from_ref(&infos[2])),
        ];
        // SAFETY: writes 及其引用的 infos 在调用期间存活；命令缓冲处于录制状态；
        // 管线布局按 `PUSH_DESCRIPTOR` 标志创建，推送的绑定数与布局声明的绑定数一致。
        //
        // 走 1.4 核心命令还是 `VK_KHR_push_descriptor` 由设备决定，这里不判断——
        // 两条通路的参数完全一致，分支只写在 `device.rs::plan_push_descriptor` 一处。
        unsafe {
            self.device.push_descriptor_set(
                self.cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &writes,
            );
        }
    }

    /// 两张平面输出图的句柄（Y, UV）
    ///
    /// 调用方不得销毁它们；两者恒处于 `GENERAL`，可直接用 `vkCmdCopyImage` 拷进 NV12
    /// 的两个平面。内容就绪的可见性由 CSC 提交的完成信号量保证，拷贝侧只管等它。
    pub fn plane_images(&self) -> (vk::Image, vk::Image) {
        (self.y.image(), self.uv.image())
    }

    /// 源已是 NV12 主机缓冲：跳过色彩转换，把两平面直接摊进两张平面图（同样零 CPU 拷贝）
    ///
    /// 与 RGBA 路径的差别只在中间产物——没有 RGBA 源图，也没有 CSC 分派，
    /// 两条 `cmdCopyBufferToImage` 就地把 Y（偏移 0）与紧打包的交织 UV（偏移 `w*h`）写进平面图。
    /// 缓冲内容须保持到本帧编码完成：槽在编码返回后才可能再次被取用，主机帧池的归还时机恰好满足。
    pub fn copy_nv12_from_buffer(&mut self, buffer: vk::Buffer) -> Result<(), String> {
        self.ensure_idle()?;
        self.nv12_copy_recorded(buffer)
    }

    fn nv12_copy_recorded(&mut self, src: vk::Buffer) -> Result<(), String> {
        let extent = self.y.extent();
        let y_plane_bytes = u64::from(extent.width) * u64::from(extent.height);
        // 紧打包：bufferRowLength 为 0 即「按图像宽度算」，与采集侧 NV12 的行距一致
        let region = |offset: u64, width: u32, height: u32| {
            vk::BufferImageCopy::default()
                .buffer_offset(offset)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
        };
        let y_region = region(0, extent.width, extent.height);
        let uv_region = region(y_plane_bytes, extent.width / 2, extent.height / 2);

        let device = self.device.device();
        // SAFETY: 上一次提交已确认完成（`ensure_idle` 保证 `pending` 为假），命令缓冲可重置；
        // 所有入参结构体在调用期间存活，`src` 的存活由调用方保证
        unsafe {
            device
                .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                .map_err(|e| format!("NV12 平面拷贝命令缓冲重置失败: {e:?}"))?;
            device
                .begin_command_buffer(
                    self.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| format!("NV12 平面拷贝命令缓冲开始失败: {e:?}"))?;

            // 平面图常驻 `GENERAL`：copy 的 dst 允许 `GENERAL`，无需任何屏障。
            // 写完交给跨队列读的可见性由信号量承担（见模块文档的「布局」一节）
            device.cmd_copy_buffer_to_image(
                self.cmd,
                src,
                self.y.image(),
                vk::ImageLayout::GENERAL,
                &[y_region],
            );
            device.cmd_copy_buffer_to_image(
                self.cmd,
                src,
                self.uv.image(),
                vk::ImageLayout::GENERAL,
                &[uv_region],
            );

            device
                .end_command_buffer(self.cmd)
                .map_err(|e| format!("NV12 平面拷贝命令缓冲结束失败: {e:?}"))?;

            // 不传栅栏：完成与否由信号量交给编码提交，主机不在中间阻塞
            self.submit_csc(None)?;
        }

        self.pending = true;
        Ok(())
    }

    fn ensure_idle(&self) -> Result<(), String> {
        if self.pending {
            return Err(
                "上一次 CSC 提交尚未确认完成：请先等待包含其信号量的那次提交，再调用 finish"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// 本次提交的完成信号量；`None` 表示没有在途提交
    ///
    /// 由编码提交放进 `vkQueueSubmit` 的 `waitSemaphores`。二值信号量一次在途提交
    /// 对应一次 signal，因此在 `finish` 之前不能再次提交（`ensure_idle` 会拦）。
    pub fn pending_semaphore(&self) -> Option<vk::Semaphore> {
        self.pending.then_some(self.semaphore)
    }

    /// 编码提交已确认完成：本转换器可以开始下一帧
    ///
    /// 前提是调用方已等待了包含 [`ColorConverter::pending_semaphore`] 的那次提交的栅栏。
    /// 二值信号量只有在等待操作完成后才回到未触发状态，所以这个调用必须发生在
    /// 「栅栏已等到」之后，否则下一次 signal 是非法的。
    pub fn finish(&mut self) {
        self.pending = false;
    }

    /// 提交已录制的 CSC 命令；`wait` 是生产侧的时间线信号量，非空时由 GPU 先等它
    ///
    /// 等待挂在**计算队列**上而不是主机上：CSC 是读源图的一方，挡住它就够，后面的编码
    /// 提交已经通过 [`ColorConverter::pending_semaphore`] 接力。主机因此不必在渲染完成
    /// 前阻塞，GPU 侧也不会出现「渲染完 → 唤醒 CPU → CPU 再提交」的空档。
    ///
    /// 用 `vkQueueSubmit2` 而不是 sync1 + `VkTimelineSemaphoreSubmitInfo`：后者要求
    /// `waitSemaphoreValueCount == waitSemaphoreCount`，而这里的等待数量是运行时的 0 或 1，
    /// 容易配错；`VkSemaphoreSubmitInfo` 把句柄、值、阶段放在一起，没有这个约束。
    ///
    /// `stageMask` 取 `ALL_COMMANDS`：命令缓冲的第一条就是布局转换屏障，而外部图在生产侧
    /// 处于什么布局本设备不可知——只挡 `COMPUTE_SHADER` 会让那次转换有机会抢在生产侧
    /// 写完之前执行。
    fn submit_csc(&self, wait: Option<(vk::Semaphore, u64)>) -> Result<(), String> {
        let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(self.cmd)];
        let signals = [vk::SemaphoreSubmitInfo::default()
            .semaphore(self.semaphore)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        // 等待数量是运行时的 0 或 1。不能写 `unwrap_or_default()`：`[T; 1]` 的 Default
        // 会给出一个「全默认」的元素（空句柄）而不是空数组
        let wait_one = wait.map(|(semaphore, value)| {
            [vk::SemaphoreSubmitInfo::default()
                .semaphore(semaphore)
                .value(value)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)]
        });
        let wait_slice: &[vk::SemaphoreSubmitInfo<'_>] = match &wait_one {
            Some(arr) => arr,
            None => &[],
        };
        let submit = vk::SubmitInfo2::default()
            .command_buffer_infos(&cmds)
            .wait_semaphore_infos(wait_slice)
            .signal_semaphore_infos(&signals);
        // SAFETY: submit 及其引用的 cmds / signals / wait_one 在调用期间均存活；
        // `synchronization2` feature 位在建设备时已显式启用，`vkQueueSubmit2` 的调用前提成立；
        // 二值信号量的等待值固定为 0，由 `SemaphoreSubmitInfo::default()` 给出
        unsafe {
            self.device.device().queue_submit2(
                self.device.compute_queue(),
                &[submit],
                vk::Fence::null(),
            )
        }
        .map_err(|e| format!("CSC 提交失败: {e:?}"))
    }

    /// 从外部来源（显存槽位）转换：不做 CPU 上传，源图已是设备本地内容
    ///
    /// `old_layout` 由调用方给出（槽位自己跟踪），返回值是转换后源图的布局——调用方存回去，
    /// 下一帧作为 `old_layout` 传入。首次调用传 `UNDEFINED`，之后传 `GENERAL`。
    ///
    /// `wait` 是生产侧时间线信号量的（句柄, 值）；非空时交给 `vkQueueSubmit2` 在 GPU 上等，
    /// 主机不在中间阻塞。生产侧没给信号量时传 `None`。
    ///
    /// 与 [`ColorConverter::copy_nv12_from_buffer`] 的差别在于源：那边源已是 NV12，
    /// 只做缓冲拷贝；这边源是 RGBA 显存图，要过一遍着色器。外部图的先前用途本设备
    /// 不可知，所以源 stage 取 `ALL_COMMANDS`、源 access 取空。
    /// @param source 源图（槽位）尺寸：小于目标时右侧与底部按边缘复制补齐
    pub fn convert_from_view(
        &mut self,
        image: vk::Image,
        view: vk::ImageView,
        old_layout: vk::ImageLayout,
        source: vk::Extent2D,
        wait: Option<(vk::Semaphore, u64)>,
    ) -> Result<vk::ImageLayout, String> {
        if self.pending {
            return Err(
                "上一次 CSC 提交尚未确认完成：请先等待包含其信号量的那次提交，再调用 finish"
                    .to_string(),
            );
        }
        // 外部槽位的视图每帧可能不同，而推送发生在录制期——不需要缓存或改写，
        // 直接用当前帧的视图推一次。

        let extent = self.y.extent();
        // 外部源图切到 `GENERAL`：源侧取 `ALL_COMMANDS`（外部图在本设备之前被谁用过
        // 不可知），目标侧是计算着色器。平面图常驻 `GENERAL`，storage 写无需转换，
        // 也没有收尾屏障——写完交给跨队列读的可见性由信号量承担（见模块文档的
        // 「布局」一节）。
        //
        // 源访问掩码留空：`srcAccessMask` 只承载「需要被 flush 的写」，上一帧对源图
        // 的读取是读、无需可用性操作，执行依赖由 `ALL_COMMANDS` 提供。
        let source_to_general = [image_barrier(
            image,
            old_layout,
            vk::ImageLayout::GENERAL,
            StageAccess::all_commands(),
            StageAccess::compute_read(),
        )];
        let mut push_bytes = [0u8; PUSH_CONSTANT_SIZE as usize];
        push_bytes[0..4].copy_from_slice(&extent.width.to_le_bytes());
        push_bytes[4..8].copy_from_slice(&extent.height.to_le_bytes());
        push_bytes[8..12].copy_from_slice(&source.width.to_le_bytes());
        push_bytes[12..16].copy_from_slice(&source.height.to_le_bytes());

        let device = self.device.device();
        // SAFETY: 上一次提交已确认完成（`pending` 为假才走到这里），命令缓冲可以重置；
        // 所有入参结构体在调用期间存活
        unsafe {
            device
                .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                .map_err(|e| format!("CSC 命令缓冲重置失败: {e:?}"))?;
            device
                .begin_command_buffer(
                    self.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| format!("CSC 命令缓冲开始失败: {e:?}"))?;

            record_image_barriers(device, self.cmd, &source_to_general);

            device.cmd_bind_pipeline(self.cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            // 当前帧要用的外部视图直接推入，不做任何缓存
            self.push_source_set(view);
            device.cmd_push_constants(
                self.cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &push_bytes,
            );
            let groups_x = (extent.width / 2).div_ceil(LOCAL_SIZE);
            let groups_y = (extent.height / 2).div_ceil(LOCAL_SIZE);
            device.cmd_dispatch(self.cmd, groups_x, groups_y, 1);

            device
                .end_command_buffer(self.cmd)
                .map_err(|e| format!("CSC 命令缓冲结束失败: {e:?}"))?;

            self.submit_csc(wait)?;
        }

        self.pending = true;
        Ok(vk::ImageLayout::GENERAL)
    }
}

impl Drop for ColorConverter {
    fn drop(&mut self) {
        let device = self.device.device();
        unsafe {
            // SAFETY: 各句柄均由本结构在 new 中创建且尚未销毁，此处各销毁一次；
            // OwnedImage 的 Drop 会再各自销毁图像与显存
            let _ = device.device_wait_idle();
            device.destroy_semaphore(self.semaphore, None);
            device.destroy_command_pool(self.pool, None);
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::VideoCodec;
    use crate::encode::hardware::vulkan::caps;
    use crate::encode::hardware::vulkan::context::VulkanInstance;
    use crate::encode::hardware::vulkan::image::Nv12Image;
    use crate::encode::hardware::vulkan::rgba_to_nv12;
    use crate::encode::hardware::vulkan::test_support::run_oneshot;

    /// 外部视图路径：`convert_from_view` 的产物必须与 CPU 参考实现逐像素一致
    ///
    /// 源图是本测试自建的 RGBA 图（经暂存缓冲上传一次，两轮转换复用），不需要真实的
    /// 显存槽位也能覆盖外部路径的三件事：外部视图推送、源图屏障、平面布局往返
    /// （第一轮结束平面在 `TRANSFER_SRC_OPTIMAL`，第二轮要切回 `GENERAL`——
    /// 第一轮跑通、第二轮布局错的实现很常见）。
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_csc_from_view_matches_cpu() {
        let Ok(ctx) = VulkanInstance::new("Vistrel-S3") else {
            println!("Vulkan 不可用，跳过");
            return;
        };
        let Ok(caps) = caps::probe_codec(&ctx, VideoCodec::H264) else {
            println!("H.264 能力探测失败，跳过");
            return;
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
        let mut converter = match ColorConverter::new(&device, extent) {
            Ok(c) => c,
            Err(e) => {
                // 硬件已满足准入条件却仍构造失败，那是代码问题（例如着色器没重编、
                // 描述符类型与 SPIR-V 声明不一致），不能当成「环境不支持」跳过 ——
                // 那样这条测试会假绿
                assert!(
                    !device.format_support().all(),
                    "硬件满足 CSC 准入条件，但构造失败: {e}"
                );
                println!("CSC 不可用，跳过: {e}");
                return;
            }
        };
        let mut nv12_image = match Nv12Image::new(&device, extent) {
            Ok(i) => i,
            Err(e) => {
                println!("输入图创建失败: {e}");
                return;
            }
        };

        let w = extent.width as usize;
        let h = extent.height as usize;
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                rgba[i] = (x * 7 % 256) as u8;
                rgba[i + 1] = (y * 13 % 256) as u8;
                rgba[i + 2] = ((x + y) * 3 % 256) as u8;
                rgba[i + 3] = 255;
            }
        }

        // 自建源图并经暂存缓冲上传一次：外部路径的输入本就是「生产侧写好的设备本地图」
        let source = OwnedImage::new(
            &device,
            ImageSpec::new(
                vk::Format::R8G8B8A8_UNORM,
                extent,
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
                "CSC 测试源图",
            ),
        )
        .expect("测试源图创建不应失败");
        let mut staging = device
            .host_buffer(
                (w * h * 4) as u64,
                vk::BufferUsageFlags::TRANSFER_SRC,
                "CSC 测试暂存",
            )
            .expect("测试暂存缓冲创建不应失败");
        staging.write(&rgba).expect("测试暂存写入不应失败");
        // 句柄先取出来：上传闭包按值捕获（vk::Image / vk::ImageView 都是 Copy），
        // 两轮转换还要继续用同一对句柄
        let (source_image, source_view) = (source.image(), source.view());

        let upload_to_dst = image_barrier(
            source_image,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            StageAccess::all_commands(),
            StageAccess::transfer_write(),
        );
        let upload_to_general = image_barrier(
            source_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::GENERAL,
            StageAccess::transfer_write(),
            StageAccess::compute_read(),
        );
        let region = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            });
        let raw = device.device();
        // SAFETY: 屏障与拷贝仅引用本测试创建的源图与暂存缓冲；命令缓冲的生命周期
        // 由 `run_oneshot` 全权管理，提交与等待都在它内部完成
        if let Err(e) = run_oneshot(&device, None, move |cmd| unsafe {
            record_image_barriers(raw, cmd, &[upload_to_dst]);
            raw.cmd_copy_buffer_to_image(
                cmd,
                staging.buffer(),
                source_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            record_image_barriers(raw, cmd, &[upload_to_general]);
        }) {
            println!("测试源图上传失败: {e}");
            return;
        }

        // 两轮转换复用同一张源图（布局固定为 GENERAL，与生产 VRAM 路径逐帧相同）
        for pass in 1..=2 {
            let layout = match converter.convert_from_view(
                source_image,
                source_view,
                vk::ImageLayout::GENERAL,
                extent,
                None,
            ) {
                Ok(layout) => layout,
                Err(e) => {
                    println!("外部视图路径转换失败: {e}");
                    return;
                }
            };
            assert_eq!(
                layout,
                vk::ImageLayout::GENERAL,
                "外部路径返回的源图布局应为 GENERAL"
            );

            let (y_img, uv_img) = converter.plane_images();
            let wait = converter.pending_semaphore();
            if let Err(e) = nv12_image.upload_from_planes_blocking(y_img, uv_img, wait) {
                println!("平面拷贝失败: {e}");
                return;
            }
            converter.finish();
            let gpu = match nv12_image.readback_blocking() {
                Ok(v) => v,
                Err(e) => {
                    println!("读回失败: {e}");
                    return;
                }
            };

            let reference = reference_nv12(&rgba, extent);
            assert_nv12_matches(&gpu, &reference, &format!("外部视图路径 第{pass}轮"));
        }
    }

    /// 源小于目标时，右侧与底部必须是边缘复制式补齐
    ///
    /// 显存直通的槽位由游戏侧按采集尺寸建，编码器输入是编码尺寸（对齐后更大）。
    /// 补齐在着色器里按源尺寸夹取完成，产物必须与主机帧补齐（`write_padded`）一致，
    /// 否则两条路径的画面会在右/下边缘出现不同接缝
    #[test]
    #[ignore = "requires GPU"]
    fn test_vulkan_video_csc_pads_smaller_source() {
        let Ok(ctx) = VulkanInstance::new("Vistrel-S3-pad") else {
            println!("Vulkan 不可用，跳过");
            return;
        };
        let Ok(caps) = caps::probe_codec(&ctx, VideoCodec::H264) else {
            println!("H.264 能力探测失败，跳过");
            return;
        };
        let device = match VideoDevice::new(&ctx, VideoCodec::H264) {
            Ok(device) => device,
            Err(e) => {
                println!("设备创建失败，跳过: {e}");
                return;
            }
        };
        // 采集 1920x1080 → 编码（对齐后）更大；无对齐则无事可测
        let coded = match caps.align_extent(1920, 1080) {
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
        if coded == display {
            println!("驱动不对齐 1920x1080，补齐路径无可测，跳过");
            return;
        }
        let mut converter = match ColorConverter::new(&device, coded) {
            Ok(c) => c,
            Err(e) => {
                assert!(
                    !device.format_support().all(),
                    "硬件满足 CSC 准入条件，但构造失败: {e}"
                );
                println!("CSC 不可用，跳过: {e}");
                return;
            }
        };
        let mut nv12_image = match Nv12Image::new(&device, coded) {
            Ok(i) => i,
            Err(e) => {
                println!("输入图创建失败: {e}");
                return;
            }
        };

        let (dw, dh) = (display.width as usize, display.height as usize);
        let (cw, ch) = (coded.width as usize, coded.height as usize);
        let mut source_rgba = vec![0u8; dw * dh * 4];
        for y in 0..dh {
            for x in 0..dw {
                let i = (y * dw + x) * 4;
                source_rgba[i] = (x * 7 % 256) as u8;
                source_rgba[i + 1] = (y * 13 % 256) as u8;
                source_rgba[i + 2] = ((x + y) * 3 % 256) as u8;
                source_rgba[i + 3] = 255;
            }
        }
        // CPU 参考：先把源按边缘复制铺满编码尺寸，再走与正常路径相同的参考实现
        let mut padded_rgba = vec![0u8; cw * ch * 4];
        for y in 0..ch {
            for x in 0..cw {
                let sx = x.min(dw - 1);
                let sy = y.min(dh - 1);
                let src = (sy * dw + sx) * 4;
                let dst = (y * cw + x) * 4;
                padded_rgba[dst..dst + 4].copy_from_slice(&source_rgba[src..src + 4]);
            }
        }

        let source = OwnedImage::new(
            &device,
            ImageSpec::new(
                vk::Format::R8G8B8A8_UNORM,
                display,
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
                "CSC 补齐测试源图",
            ),
        )
        .expect("测试源图创建不应失败");
        let mut staging = device
            .host_buffer(
                (dw * dh * 4) as u64,
                vk::BufferUsageFlags::TRANSFER_SRC,
                "CSC 补齐测试暂存",
            )
            .expect("测试暂存缓冲创建不应失败");
        staging.write(&source_rgba).expect("测试暂存写入不应失败");
        let (source_image, source_view) = (source.image(), source.view());
        let upload_to_dst = image_barrier(
            source_image,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            StageAccess::all_commands(),
            StageAccess::transfer_write(),
        );
        let upload_to_general = image_barrier(
            source_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::GENERAL,
            StageAccess::transfer_write(),
            StageAccess::compute_read(),
        );
        let region = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: display.width,
                height: display.height,
                depth: 1,
            });
        let raw = device.device();
        // SAFETY: 屏障与拷贝仅引用本测试创建的源图与暂存缓冲；命令缓冲的生命周期
        // 由 `run_oneshot` 全权管理，提交与等待都在它内部完成
        if let Err(e) = run_oneshot(&device, None, move |cmd| unsafe {
            record_image_barriers(raw, cmd, &[upload_to_dst]);
            raw.cmd_copy_buffer_to_image(
                cmd,
                staging.buffer(),
                source_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            record_image_barriers(raw, cmd, &[upload_to_general]);
        }) {
            println!("测试源图上传失败: {e}");
            return;
        }

        // 环境不支持在上面的准入判断里已跳过；走到这里后的失败都是代码问题，必须响亮
        let layout = converter
            .convert_from_view(
                source_image,
                source_view,
                vk::ImageLayout::GENERAL,
                display,
                None,
            )
            .unwrap_or_else(|e| panic!("补齐转换失败: {e}"));
        assert_eq!(layout, vk::ImageLayout::GENERAL);

        let (y_img, uv_img) = converter.plane_images();
        let wait = converter.pending_semaphore();
        nv12_image
            .upload_from_planes_blocking(y_img, uv_img, wait)
            .unwrap_or_else(|e| panic!("平面拷贝失败: {e}"));
        converter.finish();
        let gpu = nv12_image
            .readback_blocking()
            .unwrap_or_else(|e| panic!("读回失败: {e}"));
        let reference = reference_nv12(&padded_rgba, coded);
        assert_nv12_matches(&gpu, &reference, "源小于目标的补齐路径");
    }

    /// 与 CPU 参考逐像素比对，容差 1（UNORM 舍入方向可能不同）
    fn assert_nv12_matches(gpu: &[u8], reference: &[u8], label: &str) {
        assert_eq!(gpu.len(), reference.len(), "{label}: NV12 长度必须一致");
        let mut max_diff = 0i32;
        let mut bad = 0usize;
        let mut first_bad = None;
        for (i, (a, b)) in gpu.iter().zip(reference.iter()).enumerate() {
            let diff = (*a as i32 - *b as i32).abs();
            if diff > max_diff {
                max_diff = diff;
            }
            if diff > 1 {
                bad += 1;
                if first_bad.is_none() {
                    first_bad = Some((i, *a, *b));
                }
            }
        }
        println!(
            "{label}: 最大偏差 {max_diff} / 超差 {bad} 字节{}",
            match first_bad {
                Some((i, a, b)) => format!(" / 首个超差 idx={i} gpu={a} cpu={b}"),
                None => String::new(),
            }
        );
        assert_eq!(bad, 0, "{label}: GPU 结果与 CPU 参考实现不一致");
    }

    /// BT.709 Limited Range 的浮点参考实现，逐行对齐 `rgba_to_nv12.comp`
    ///
    /// 色度取 2x2 块在 **RGB 空间**的均值再转，和着色器一致（转换是线性的，两种顺序
    /// 数学等价，但浮点舍入不同，所以这里刻意照抄着色器的顺序）。
    fn reference_nv12(rgba: &[u8], extent: vk::Extent2D) -> Vec<u8> {
        let w = extent.width as usize;
        let h = extent.height as usize;
        let mut out = vec![0u8; w * h + w * h / 2];
        let (y_plane, uv_plane) = out.split_at_mut(w * h);

        let px = |x: usize, y: usize| -> [f32; 3] {
            let i = (y * w + x) * 4;
            [
                rgba[i] as f32 / 255.0,
                rgba[i + 1] as f32 / 255.0,
                rgba[i + 2] as f32 / 255.0,
            ]
        };
        let y_of = |c: [f32; 3]| 16.0 / 255.0 + 0.182586 * c[0] + 0.614231 * c[1] + 0.062007 * c[2];
        let u_of =
            |c: [f32; 3]| 128.0 / 255.0 - 0.100644 * c[0] - 0.338572 * c[1] + 0.439216 * c[2];
        let v_of =
            |c: [f32; 3]| 128.0 / 255.0 + 0.439216 * c[0] - 0.398942 * c[1] - 0.040274 * c[2];
        let to_u8 = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;

        for y in 0..h {
            for x in 0..w {
                y_plane[y * w + x] = to_u8(y_of(px(x, y)));
            }
        }
        let half_w = w / 2;
        for by in 0..h / 2 {
            for bx in 0..half_w {
                let c00 = px(bx * 2, by * 2);
                let c10 = px(bx * 2 + 1, by * 2);
                let c01 = px(bx * 2, by * 2 + 1);
                let c11 = px(bx * 2 + 1, by * 2 + 1);
                let avg = [
                    (c00[0] + c10[0] + c01[0] + c11[0]) * 0.25,
                    (c00[1] + c10[1] + c01[1] + c11[1]) * 0.25,
                    (c00[2] + c10[2] + c01[2] + c11[2]) * 0.25,
                ];
                let idx = by * half_w * 2 + bx * 2;
                uv_plane[idx] = to_u8(u_of(avg));
                uv_plane[idx + 1] = to_u8(v_of(avg));
            }
        }
        out
    }

    /// CPU 兜底路径：`rgba_to_nv12` 的输出必须与浮点参考一致，且填充区是边缘复制
    ///
    /// 形状刻意取 64x48 可见 → 80x64 编码：同时覆盖横向与纵向填充。真实场景里
    /// 1920x1080 会被对齐成 1920x1088（纵向填充），宽高都可能不一致。
    ///
    /// 这条用例不需要 Vulkan：它验的是 CPU 路径本身，也是唯一能证明「CPU 与 GPU
    /// 两条路径色彩参数一致」的东西 —— 两者都必须是 BT.709 + Limited Range。
    #[test]
    fn test_cpu_rgba_to_nv12_matches_reference() {
        let visible = vk::Extent2D {
            width: 64,
            height: 48,
        };
        let coded = vk::Extent2D {
            width: 80,
            height: 64,
        };
        let (vw, vh) = (visible.width as usize, visible.height as usize);
        let (cw, ch) = (coded.width as usize, coded.height as usize);

        let mut rgba = vec![0u8; vw * vh * 4];
        for y in 0..vh {
            for x in 0..vw {
                let i = (y * vw + x) * 4;
                rgba[i] = (x * 7 % 256) as u8;
                rgba[i + 1] = (y * 13 % 256) as u8;
                rgba[i + 2] = ((x + y) * 3 % 256) as u8;
                rgba[i + 3] = 255;
            }
        }

        let mut nv12 = vec![0u8; cw * ch + cw * ch / 2];
        if let Err(e) = rgba_to_nv12(&rgba, visible, coded, &mut nv12) {
            panic!("CPU 转换失败: {e}");
        }

        let (y_plane, uv_plane) = nv12.split_at(cw * ch);

        // 1. 可见区域与浮点参考逐像素比对（容差 1，与 GPU 那条用例同一口径）。
        //    目标缓冲是编码 stride，先按可见宽度收紧成紧凑布局再比
        let mut packed = Vec::with_capacity(vw * vh + vw * vh / 2);
        for row in 0..vh {
            packed.extend_from_slice(&y_plane[row * cw..row * cw + vw]);
        }
        for row in 0..vh / 2 {
            packed.extend_from_slice(&uv_plane[row * cw..row * cw + vw]);
        }
        let reference = reference_nv12(&rgba, visible);
        assert_nv12_matches(&packed, &reference, "CPU 转换");

        // 2. 填充区必须是边缘复制。留 0 或留上一帧的残留会进 DPB 参考帧，
        //    表现为后续 P 帧边缘出现暗带
        for row in 0..ch {
            let src_row = row.min(vh - 1);
            for col in vw..cw {
                assert_eq!(
                    y_plane[row * cw + col],
                    y_plane[src_row * cw + vw - 1],
                    "Y 平面 ({col},{row}) 未按边缘复制填充"
                );
            }
        }
        for row in 0..ch / 2 {
            let src_row = row.min(vh / 2 - 1);
            for col in (vw..cw).step_by(2) {
                assert_eq!(
                    uv_plane[row * cw + col],
                    uv_plane[src_row * cw + vw - 2],
                    "UV 平面 U ({col},{row}) 未按边缘复制填充"
                );
                assert_eq!(
                    uv_plane[row * cw + col + 1],
                    uv_plane[src_row * cw + vw - 1],
                    "UV 平面 V ({col},{row}) 未按边缘复制填充"
                );
            }
        }
    }
}

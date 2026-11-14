import java.io.File

plugins {
    id("dev.kikugie.stonecutter")
    // see https://maven.fabricmc.net/fabric-loom/fabric-loom.gradle.plugin/maven-metadata.xml for new versions
    alias(libs.plugins.fabric.loom).apply(false)
    alias(libs.plugins.fabric.loom.remap).apply(false)
    // see https://projects.neoforged.net/neoforged/moddevgradle for new versions
    alias(libs.plugins.neoforged.moddev).apply(false)
    alias(libs.plugins.spotless)
}
stonecutter active "26.3"

// 渲染栈代际常量与双栈类名改写（按节点求值）。门名 = 该版本线新增的那个东西：
// gpu_device = GpuDevice/CommandEncoder 抽象设备 API 代（26.1+，blaze3d/renderpearl 皆属之，对应 Java 25 与未混淆时代）
// renderpearl_era = renderpearl 命名空间代（26.3+）
// vulkan_backend = 游戏自带 Vulkan 后端（26.2 引入，26.1 为纯 OpenGL；驱动扩展支持由运行期另判）
stonecutter parameters {
    constants["gpu_device"] = current.parsed >= "26.1"
    constants["renderpearl_era"] = current.parsed >= "26.3"
    constants["vulkan_backend"] = current.parsed >= "26.2"

    // 双栈类名改写表：源码恒以 renderpearl(26.3) 落笔，blaze3d 代（26.1/26.2）按此表做
    // 限定名整串替换。表内规则自上而下应用，后条作用于前条的产物。
    val gpuClasses = listOf(
        "com.mojang.renderpearl.api.device.GpuDevice" to "com.mojang.blaze3d.systems.GpuDevice",
        "com.mojang.renderpearl.api.commands.CommandEncoder" to "com.mojang.blaze3d.systems.CommandEncoder",
        "com.mojang.renderpearl.api.commands.RenderPass" to "com.mojang.blaze3d.systems.RenderPass",
        "com.mojang.renderpearl.api.buffers.GpuBufferSlice" to "com.mojang.blaze3d.buffers.GpuBufferSlice",
        "com.mojang.renderpearl.api.buffers.GpuBuffer" to "com.mojang.blaze3d.buffers.GpuBuffer",
        "com.mojang.renderpearl.api.pipeline.BindGroupLayout" to "com.mojang.blaze3d.pipeline.BindGroupLayout",
        "com.mojang.renderpearl.api.pipeline.ColorTargetState" to "com.mojang.blaze3d.pipeline.ColorTargetState",
        "com.mojang.renderpearl.api.pipeline.RenderPipeline" to "com.mojang.blaze3d.pipeline.RenderPipeline",
        "com.mojang.renderpearl.api.textures.GpuTextureView" to "com.mojang.blaze3d.textures.GpuTextureView",
        "com.mojang.renderpearl.api.textures.GpuTexture" to "com.mojang.blaze3d.textures.GpuTexture",
        "com.mojang.renderpearl.api.textures.FilterMode" to "com.mojang.blaze3d.textures.FilterMode",
        "com.mojang.renderpearl.api.vertex.VertexFormat" to "com.mojang.blaze3d.vertex.VertexFormat",
        "com.mojang.renderpearl.backend.vulkan.VulkanDevice" to "com.mojang.blaze3d.vulkan.VulkanDevice",
        "com.mojang.renderpearl.backend.vulkan.VulkanCommandEncoder" to "com.mojang.blaze3d.vulkan.VulkanCommandEncoder",
        "com.mojang.renderpearl.backend.vulkan.VulkanGpuTexture" to "com.mojang.blaze3d.vulkan.VulkanGpuTexture",
        "com.mojang.renderpearl.backend.api.GpuDeviceBackend" to "com.mojang.blaze3d.systems.GpuDeviceBackend",
        // GpuFormat 在 26.2 仅换包不改名（26.1 的 TextureFormat 改名随 26.1 行落地时单独设计）
        "com.mojang.renderpearl.api.GpuFormat" to "com.mojang.blaze3d.GpuFormat",
    )
    replacements {
        for ((from, to) in gpuClasses) {
            string(current.parsed < "26.3") { replace(from, to) }
        }
        // 26.1 增量：TextureFormat 枚举缺 RG8（色度平面以 RGBA8 承载，NV12 组装侧配合）。
        // GpuFormat 的类型名不在此改写（import 走三态条件块；裸名改写会与限定名规则成环），
        // 仅改写常量与参数声明的简单名用法，规则输入互不重叠、输出不含任何规则输入
        string(current.parsed < "26.2") {
            replace("GpuFormat.R8_UNORM", "TextureFormat.RED8")
            replace("GpuFormat.RGBA8_UNORM", "TextureFormat.RGBA8")
            replace("GpuFormat format", "TextureFormat format")
        }
    }
}


// Spotless 用独立 configuration 解析格式化器，需从 mavenCentral 拉取
repositories {
    mavenCentral()
}

spotless {
    java {
        // 只格式化根源码；versions/ 与 build/ 是生成目录
        target("common/src/**/*.java", "fabric/src/**/*.java", "neoforge/src/**/*.java", "forge/src/**/*.java")
        // 必须选不重排注释的格式化器：GJF、palantir、Eclipse JDT 都会破坏 Stonecutter 的 //? 条件标记
        princeOfSpace("2.3.1").indentSize(4).lineLength(120).javaLanguageLevel(25)
    }
}

// web 前端（web/）由 Vite 构建，产物 web/dist 与版本无关
val webDir = file("web")

fun Exec.pnpmInWeb(vararg args: String) {
    val dir = webDir
    setWorkingDir(dir)
    commandLine("pnpm", *args)
    doFirst {
        if (!File(dir, "node_modules").isDirectory()) {
            throw GradleException("web 前端依赖未安装：请先在 web/ 目录执行 pnpm install")
        }
    }
}

tasks.register<Exec>("webAssets") {
    group = "build"
    description = "Vite 构建 web 前端产物到 web/dist"
    pnpmInWeb("run", "build")
    // 增量判定只看源码与清单，node_modules 与 dist 不作输入
    inputs.files(
        File(webDir, "index.html"),
        File(webDir, "vite.config.js"),
        File(webDir, "package.json"),
        File(webDir, "pnpm-lock.yaml"),
        File(webDir, "src"),
        File(webDir, "public"),
    )
    outputs.dir(File(webDir, "dist"))
}

// 根 check 聚合全部检查：Spotless 由插件挂入，各版本节点测试经 Stonecutter 任务映射汇入
tasks.named("check") {
    dependsOn(stonecutter.tasks.named("check"))
}

tasks.register("buildAndCollect") {
    group = "build"
    description = "Builds all Stonecutter versions and loaders with the check gate and collects the distributable jars"
    dependsOn(stonecutter.tasks.named("build"))
    dependsOn(stonecutter.tasks.named("buildAndCollect") { branch.id != "common" })
    dependsOn(tasks.named("check"))
    dependsOn(tasks.named("webAssets"))
}

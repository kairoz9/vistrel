import net.fabricmc.loom.api.LoomGradleExtensionAPI

plugins {
    id("multiloader-common")
    id("dev.kikugie.loom-back-compat")
}

dependencies {
    add("minecraft", "com.mojang:minecraft:${sc.current.version}")
    // 官方映射只在混淆变体（loom-remap）上生效，非混淆侧 loomx 静默跳过
    loomx.applyMojangMappings()
    // Fabric 和 NeoForge 均内置 Fabric Mixin
    compileOnly(libs.sponge.mixin)
    // 混淆变体（fabric-loom-remap）不填充 loaderLibraries 时，Loom 的 JarManifestService
    // 会把 Fabric-Mixin-Version 写成 unknown；补一条让清单元数据准确（运行期仍由 loader 自带）
    if (sc.properties.get<Boolean>("loomx.obfuscated")) {
        add("loaderLibraries", libs.sponge.mixin)
    }
    // Fabric / NeoForge 内置 MixinExtras；Forge 1.20 系不内置，由 forge 模块 JIJ 内嵌保证运行期可用
    compileOnly(libs.mixinextras.common)
    annotationProcessor(libs.mixinextras.common)
    compileOnly(libs.fabric.loader)

    // Loom interface injection 在 MC 与 fabric-api 类上写入 @Environment(EnvType.CLIENT)，
    // 测试编译读取这些类时必须能解析 EnvType，否则 javac 对每处注解告警并静默丢弃
    testCompileOnly(libs.fabric.loader)
    testImplementation(libs.junit.jupiter)
    testRuntimeOnly(libs.junit.platform.launcher)
}

tasks.test {
    useJUnitPlatform()
    jvmArgs("--enable-native-access=ALL-UNNAMED")
    testLogging {
        events("skipped", "failed")
    }
}

configure<LoomGradleExtensionAPI> {
    mixin {
        useLegacyMixinAp = false
    }
}

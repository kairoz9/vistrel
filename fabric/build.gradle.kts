import net.fabricmc.loom.api.LoomGradleExtensionAPI
import org.gradle.api.tasks.Copy

plugins {
    id("multiloader-loader")
    id("dev.kikugie.loom-back-compat")
}

dependencies {
    add("minecraft", "com.mojang:minecraft:${sc.current.version}")
    // 官方映射只在混淆变体（loom-remap）上生效，非混淆侧 loomx 静默跳过
    loomx.applyMojangMappings()
    // modImplementation 由 loom-back-compat 双向供给：混淆侧是 loom 原生配置，非混淆侧重定向到 implementation
    modImplementation("net.fabricmc:fabric-loader:${sc.properties.get<String>("deps.loader_version")}")
    modImplementation("net.fabricmc.fabric-api:fabric-api:${sc.properties.get<String>("deps.api_version")}")
}

// 产物归拢：可分发 jar 由 loom 变体决定——混淆侧 remapJar，非混淆侧 jar
tasks.named<Copy>("buildAndCollect") {
    from(loomx.modJar.flatMap { it.archiveFile }, loomx.modSourcesJar.flatMap { it.archiveFile })
}

configure<LoomGradleExtensionAPI> {
    val modId = providers.gradleProperty("mod.id").get()
    val aw = project(":common").file("src/main/resources/$modId.accesswidener")
    if (aw.exists()) {
        accessWidenerPath.set(aw)
    }
    runs {
        maybeCreate("client").apply {
            client()
            displayName.set("Fabric Client")
            generateRunConfig.set(true)
            runDirectory.set(project.file("runs/client"))
        }
        maybeCreate("server").apply {
            server()
            displayName.set("Fabric Server")
            generateRunConfig.set(true)
            runDirectory.set(project.file("runs/server"))
        }
    }
}

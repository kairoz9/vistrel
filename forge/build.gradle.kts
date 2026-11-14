import java.nio.file.Files
import kotlinx.serialization.json.Json
import org.gradle.api.artifacts.ExternalModuleDependency
import org.gradle.api.tasks.Copy
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonObject

plugins {
    id("multiloader-loader")
    alias(libs.plugins.forge.gradle)
    alias(libs.plugins.forge.jarjar)
    alias(libs.plugins.forge.renamer)
}

val modId = providers.gradleProperty("mod.id").get()

// Include generated resources
sourceSets.named("main") { resources.srcDir("src/generated/resources") }

minecraft {
    runs {
        configureEach {
            workingDir = layout.projectDirectory.dir("runs/$name")
            systemProperty("forge.enabledGameTestNamespaces", modId)
        }

        register("client")

        register("server") {
            args("--nogui")
        }

        register("data") {
            workingDir = layout.projectDirectory.dir("runs/data")
            args("--mod", modId, "--all", "--output", layout.projectDirectory.dir("src/generated/resources"), "--existing", layout.projectDirectory.dir("src/main/resources"))
        }
    }
}

repositories {
    minecraft.mavenizer(this)
    maven(fg.forgeMaven)
    maven(fg.minecraftLibsMaven)
    mavenCentral()
}

val mixinExtrasVersion = libs.versions.mixinextras.get()

dependencies {
    val forgeVersion = sc.properties.get<String>("deps.api_version")
    implementation(minecraft.dependency("net.minecraftforge:forge:${sc.current.version}-$forgeVersion"))
    // Forge 1.20 系运行期不提供 MixinExtras（官方 1.21.10+ 才内置），编译需显式声明；
    compileOnly(libs.mixinextras.common)
    annotationProcessor(libs.mixinextras.common)
    runtimeOnly(libs.mixinextras.forge)
    // Mixin 注解处理器：renamer 的 refmap 流水线靠它在编译期生成 refmap；
    // mixin 的 POM 不带依赖，ASM 需按编译类路径同版本显式补齐
    annotationProcessor(libs.mixin.ap)
    annotationProcessor(libs.asm)
    annotationProcessor(libs.asm.tree)
    annotationProcessor(libs.asm.commons)
    annotationProcessor(libs.gson)
    annotationProcessor(libs.guava)
}

jarJar.register {
    // with(jar) 与 withType<Jar> 的 LICENSE 块各带一份 → EXCLUDE 去重
    duplicatesStrategy = DuplicatesStrategy.EXCLUDE
}
val mixinExtrasDep = dependencies.create(libs.mixinextras.forge.get()) as ExternalModuleDependency
mixinExtrasDep.isTransitive = false // 只嵌本体，不带传递依赖
dependencies { add("jarJar", mixinExtrasDep) }
jarJar.configure(mixinExtrasDep) {
    setVersion(mixinExtrasVersion)
    setRange("[$mixinExtrasVersion,)")
}

tasks.jar {
    manifest {
        attributes("MixinConfigs" to "$modId.mixins.json,$modId.forge.mixins.json")
    }
}

// SRG 重映射：1.20 系生产运行时是 SRG 命名，官方映射编译的 jar 必须经 renamer 重映射才能发布，
// 发布以 -srg 产物为准；JIJ 内嵌随 jarJar 进 -srg
renamer.mappings.from(minecraft.dependency.toSrgFile)
val fatJar = tasks.named<Jar>("jarJar")
val srgJar = renamer.classes(fatJar) {
    archiveClassifier = "srg"
}

// 只收 -srg 发布产物与 sources jar
tasks.named<Copy>("buildAndCollect") {
    from(srgJar.flatMap { it.output }, tasks.named<Jar>("sourcesJar").flatMap { it.archiveFile })
}

// mixin @At 目标走 refmap：编译期 Mixin AP 生成，dev 经 remapRefMap 换算，生产随 jar 重映射生效；
// source 必须先于 jar 注册，configureJar 只在执行时遍历已登记的 sourceSet
val mixinRefmaps = renamer.enableMixinRefmaps {
    config("$modId.mixins.json")
    config("$modId.forge.mixins.json")
    source(sourceSets.main.get(), "$modId-refmap.json")
    jar(fatJar)
}
minecraft.runs.configureEach { mixinRefmaps.run(this) }

// refmap 键只进 forge 产物
tasks.named("processResources") {
    // 值局部化到任务配置作用域：doLast 闭包只捕获局部值，捕获脚本引用会与配置缓存不兼容
    val refmapFileName = "$modId-refmap.json.refmap.json"
    val mixinConfigNames = listOf("$modId.mixins.json", "$modId.forge.mixins.json")
    val prettyJson = Json { prettyPrint = true }
    doLast {
        val root = outputs.files.firstOrNull { it.isDirectory } ?: outputs.files.first()
        for (name in mixinConfigNames) {
            val src = Files.walk(root.toPath())
                .filter { p -> p.toFile().name == name }
                .findFirst().orElse(null)
                ?: throw IllegalStateException("forge 资源输出中缺少 mixin 配置: $name")
            val cfg = Json.parseToJsonElement(src.toFile().readText()).jsonObject.toMutableMap()
            cfg["refmap"] = JsonPrimitive(refmapFileName)
            src.toFile().writeText(prettyJson.encodeToString(JsonElement.serializer(), JsonObject(cfg)) + "\n")
        }
    }
}

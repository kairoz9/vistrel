import org.apache.tools.ant.taskdefs.condition.Os
import org.gradle.api.tasks.Copy
import org.gradle.api.tasks.Exec
import org.gradle.api.tasks.JavaExec
import org.gradle.api.tasks.SourceSetContainer
import org.gradle.jvm.tasks.Jar
import org.gradle.language.jvm.tasks.ProcessResources

plugins {
    id("multiloader-common")
}

val commonPath = ":common:${project.name}".takeIf { findProject(it) != null } ?: ":common"
val commonProject = evaluationDependsOn(commonPath)
val commonMain = commonProject.the<SourceSetContainer>().getByName("main")

// IDE 的 Gradle 模型只认 source set；原先把 common 只挂在 compileJava.source 上，
// 编辑器（VS Code 的 Java 扩展、IntelliJ）看不到这份源码，loader 里的 common 引用
// 会显示为未解析。把 common 的 source set 直接并入 main，编译、sourcesJar 与 javadoc
// 都沿用同一份源码。
// src/generated/resources 是 NeoForge/Forge datagen 的产物目录；fabric 下为空目录，无副作用。
sourceSets.named("main") {
    java.srcDir(commonMain.java)
    resources.srcDir("src/generated/resources")
}

// 产物归拢目录统一为根 build/libs
// 各 loader 脚本补齐 from 规格（可分发 jar 因 loader 而异：fabric 走 loom 变体任务，forge 以 -srg 为准）
tasks.register<Copy>("buildAndCollect") {
    group = "build"
    description = "Copies the distributable jar and sources into the root build/libs directory"
    into(rootProject.layout.buildDirectory.dir("libs"))
}

tasks.named<Jar>("sourcesJar") {
    from(commonMain.resources)
}

val linuxTriple = providers.gradleProperty("vistrel.linuxTriple").getOrElse("")
val linuxNative: File = if (linuxTriple.isEmpty()) {
    rootProject.file("rust-native/target/release/libvistrel.so")
} else {
    rootProject.file("rust-native/target/$linuxTriple/release/libvistrel.so")
}

val windowsTriple = providers.gradleProperty("vistrel.windowsTriple").getOrElse("")
val windowsNative: File = if (windowsTriple.isEmpty()) {
    rootProject.file("rust-native/target/release/vistrel.dll")
} else {
    rootProject.file("rust-native/target/$windowsTriple/release/vistrel.dll")
}

val hostNative = if (Os.isFamily(Os.FAMILY_WINDOWS)) windowsNative else linuxNative

tasks.named<Jar>("jar") {
    from(linuxNative) {
        into("natives")
    }
    from(windowsNative) {
        into("natives")
    }
    // 显式输入声明：产物缺失时任务以缺输入失败，而非静默打出无 natives 的 jar
    inputs.file(linuxNative).withPropertyName("nativeLinux")
    inputs.file(windowsNative).withPropertyName("nativeWindows")
}


// 资源统一在 loader 侧展开打包：common 的源资源 + 根 web/dist。
tasks.named<ProcessResources>("processResources") {
    from(commonMain.resources)
    from(rootProject.tasks.named<Exec>("webAssets")) {
        into("web")
    }
}

// runClient 注入宿主产物路径
val nativeLib = providers.gradleProperty("vistrel.nativeLib").getOrElse(hostNative.absolutePath)
tasks.matching { it.name == "runClient" }.configureEach {
    (this as? JavaExec)?.let { run ->
        run.jvmArgs("-Dvistrel.nativeLib=" + rootProject.file(nativeLib).absolutePath)
    }
}
import org.gradle.api.publish.maven.MavenPublication
import org.gradle.external.javadoc.CoreJavadocOptions

plugins {
    `java-library`
    `maven-publish`
}

// 版本矩阵唯一事实源是 stonecutter.properties.toml（Stonecutter 内建结构化属性）
fun Project.prop(key: String): String =
    findProperty(key)?.toString()?: throw GradleException("[${project.path}] 属性 '$key' 缺失")

fun Project.optionalProp(key: String): String = findProperty(key)?.toString().orEmpty()

// Fabric 不兼容 Maven 区间, 需转换
fun Project.fabricVersionRange(mavenRange: String): String {
    val match = Regex("""\[([^,]+), (.+)\)""").matchEntire(mavenRange)
        ?: throw GradleException("[${project.path}] minecraft.version_range '$mavenRange' 不是 [a, b) 形式，无法推导 Fabric 范围")
    return ">=${match.groupValues[1]} <${match.groupValues[2]}"
}

val minecraftVersion = project.name
val modId = prop("mod.id")
val modName = prop("mod.name")
val modAuthor = prop("mod.author")
val modLicense = prop("mod.license")
val modCredits = prop("mod.credits")

val expandProps = mapOf(
    "mod_id" to modId,
    "mod_name" to modName,
    "mod_version" to project.version,
    "mod_group" to project.group,
    "mod_author" to modAuthor,
    "mod_credits" to modCredits,
    "mod_license" to modLicense,
    "mod_description" to project.description,
    "minecraft_version" to minecraftVersion,
    "minecraft_version_range" to prop("minecraft.version_range"),
    "minecraft_version_range_fabric" to fabricVersionRange(prop("minecraft.version_range")),
    "java_version" to prop("java.version"),
    "api_version" to optionalProp("deps.api_version"),
    "loader_version" to optionalProp("deps.loader_version"),
)

val loaderName = parent?.takeIf { it != rootProject }?.name ?: project.name

// 归档名的唯一计算点：manifest 与 Maven 发布坐标都复用该值
val projectArchivesName = "$modId-$loaderName-$minecraftVersion"

base {
    archivesName = projectArchivesName
}

java {
    toolchain.languageVersion.set(JavaLanguageVersion.of(prop("java.version").toInt()))
    withSourcesJar()
    withJavadocJar()
}

tasks.withType<JavaCompile>().configureEach {
    options.encoding = "UTF-8"
    options.compilerArgs.add("-Xlint:all,-processing,-classfile")
}

// 配置期取值：任务执行期读 project.ext 动态属性与配置缓存不兼容
val licensePrefix = modName

tasks.withType<Jar>().configureEach {
    from(rootProject.file("LICENSE")) {
        // 闭包写法会捕获脚本引用与配置缓存不兼容，改用配置期定值的模式替换（等价于重命名为 LICENSE_<modName>）
        rename(Regex("(.*)").toPattern(), "\$1_$licensePrefix")
    }
}

tasks.withType<Javadoc>().configureEach {
    classpath += configurations.getByName("compileClasspath").filter { it.name.contains("fabric-loader") }
    (options as CoreJavadocOptions).addBooleanOption("Xdoclint:none", true)
    options.quiet()
    isFailOnError = false
}

tasks.jar {
    manifest {
        attributes(
            "Specification-Title" to modName,
            "Specification-Vendor" to modAuthor,
            "Specification-Version" to version,
            "Implementation-Title" to project.name,
            "Implementation-Version" to version,
            "Implementation-Vendor" to modAuthor,
            "Built-On-Minecraft" to minecraftVersion,
        )
    }
}

tasks.processResources {
    // 局部化展开表：filesMatching 闭包只捕获局部值，捕获脚本引用会与配置缓存不兼容
    val props = expandProps
    val jsonExpandProps = props.entries.associate { (key, value) ->
        key to if (value is String) value.replace("\n", "\\\\n") else value
    }

    filesMatching(listOf("META-INF/mods.toml", "META-INF/neoforge.mods.toml")) {
        expand(props)
    }

    filesMatching(listOf("fabric.mod.json", "*.mixins.json")) {
        expand(jsonExpandProps)
    }

    inputs.properties(props)
}

publishing {
    publications {
        register("mavenJava", MavenPublication::class) {
            artifactId = projectArchivesName
            from(components["java"])
        }
    }
    repositories {
        maven {
            // 环境变量未设时不设 URL
            System.getenv("local_maven_url")?.let { setUrl(it) }
        }
    }
}

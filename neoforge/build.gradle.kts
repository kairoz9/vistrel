import org.gradle.api.tasks.Copy
import org.gradle.jvm.tasks.Jar

plugins {
    id("multiloader-loader")
    id("net.neoforged.moddev")
}

val modId = providers.gradleProperty("mod.id").get()

// 产物归拢：MDG 的 jar 即可分发产物
tasks.named<Copy>("buildAndCollect") {
    from(tasks.named<Jar>("jar").flatMap { it.archiveFile }, tasks.named<Jar>("sourcesJar").flatMap { it.archiveFile })
}

neoForge {
    version = sc.properties.get<String>("deps.api_version")
    // Automatically enable neoforge AccessTransformers if the file exists
    val at = project(":common").file("src/main/resources/META-INF/accesstransformer.cfg")
    if (at.exists()) {
        accessTransformers.from(at.absolutePath)
    }
    runs {
        configureEach {
            systemProperty("neoforge.enabledGameTestNamespaces", modId)
            ideName = "NeoForge ${name.replaceFirstChar { it.uppercase() }} (${project.path})" // Unify the run config names with fabric
        }
        register("client") {
            client()
            gameDirectory = mkdir(file("runs/client"))
        }
        register("data") {
            if (project.name == "1.21") {
                data()
            } else {
                clientData()
            }

            gameDirectory = mkdir(file("runs/data"))
            // DataGen can be run by - "./gradlew :neoforge:runData" in Terminal.
            // Specify the modid for data generation, where to output the resulting resource, and where to look for existing resources.
            programArguments.addAll("--mod", modId, "--all", "--output", file("src/generated/resources/").absolutePath, "--existing", file("src/main/resources/").absolutePath)
        }
        register("server") {
            server()
            file("runs/server").parentFile.mkdirs()
            gameDirectory = mkdir(file("runs/server"))
        }
    }
    mods {
        create(modId) {
            sourceSet(sourceSets.main.get())
        }
    }
}

sourceSets.named("main") { resources.srcDir("src/generated/resources") }

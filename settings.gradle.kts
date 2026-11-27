pluginManagement {
    repositories {
        gradlePluginPortal()
        mavenCentral()
        exclusiveContent {
            forRepository {
                maven {
                    name = "Fabric"
                    url = uri("https://maven.fabricmc.net")
                }
            }
            filter {
                includeGroupAndSubgroups("net.fabricmc")
            }
        }
        exclusiveContent {
            forRepository {
                maven {
                    name = "Forge"
                    url = uri("https://maven.minecraftforge.net")
                }
            }
            filter {
                includeGroupAndSubgroups("net.minecraftforge")
            }
        }
        exclusiveContent {
            forRepository {
                maven {
                    name = "KikuGie"
                    url = uri("https://maven.kikugie.dev/releases")
                }
            }
            filter {
                includeGroupAndSubgroups("dev.kikugie")
            }
        }
    }
}

// 依赖仓库的单一来源；ForgeGradle 仍会向项目注入自己的仓库，故不启用 FAIL_ON_PROJECT_REPOS
dependencyResolutionManagement {
    repositories {
        mavenCentral()
    }
}

plugins {
    id("org.gradle.toolchains.foojay-resolver-convention") version "1.0.0"
    id("dev.kikugie.stonecutter") version "0.9.8"
    id("dev.kikugie.loom-back-compat") version "0.4.2"
}

rootProject.name = "vistrel"
includeBuild("build-logic")

stonecutter {
    kotlinController = true
    centralScript = "build.gradle.kts"

    create(rootProject) {
        branch("common") {
            versions("1.20", "1.21", "26.1", "26.2", "26.3")
        }
        branch("fabric") {
            versions("1.20", "1.21", "26.1", "26.2", "26.3")
        }
        branch("neoforge") {
            versions("1.21", "26.1", "26.2", "26.3")
        }
        branch("forge") {
            versions("1.20")
        }
        // vcsVersion 的键空间是纯版本名（0.9.8 无 branch:node 限定语法，跨分支同名节点由插件校验同值）；
        // 显式钉住，避免退回按注册顺序取首位的兜底行为
        vcsVersion = "26.3"
    }
}

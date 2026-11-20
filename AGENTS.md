# AGENTS.md - Vistrel 智能体协作指南

> 本文档专为在此代码库中工作的 AI Agent 编写，旨在系统性阐述 **Vistrel** 的项目架构、设计哲学、多加载器（Multi-Loader）规范、Rust 原生多媒体管线、渲染捕获机制、以及维护准则。在对本项目进行任何功能扩展、Bug 修复或性能优化前，请务必完整阅读本文档。

---

## 1. 项目定位与核心设计哲学 (Project Overview & Philosophy)

**Vistrel** 是一个基于 WebRTC 与硬件加速视频编码的 Minecraft 单模组（Single-Mod）像素流（Pixel Streaming）串流系统。远程玩家只需通过现代桌面浏览器（Chrome/Edge/Firefox/Safari）访问 `http://<IP>:<Port>`，即可享受零客户端安装、超低输入延迟、鼠标指针锁定（Pointer Lock）的高品质 Minecraft 畅玩体验。

### 核心设计原则 (Guiding Principles)
1. **浏览器零插件即点即玩 (Zero Client Installation)**: 客户端 100% 运行于标准 HTML5/WebRTC 浏览器环境，无需安装任何客户端软件、Java 运行环境或额外应用。
2. **跨语言高性能架构 (Java Common Core + Rust Native Engine)**: 游戏逻辑、事件注入与 ModLoader 适配采用现代 Java 25 编写；计算密集的视频硬编码、音频回环与 WebRTC Peer 直连管线全部由底层 Rust 原生动态库（`libvistrel.so` / `vistrel.dll`）接管。
3. **Stonecutter 统一多版本多加载器架构 (Stonecutter Multi-Version Matrix)**: 采用 Stonecutter 多版本架构与 MultiLoader 分层规范，一套单一源码基准同时覆盖 **1.20** (Java 17 / Forge & Fabric)、**1.21 ~ 1.21.11** (Java 21 / NeoForge & Fabric) 以及 **26.3** (Java 25 / NeoForge & Fabric) 主流游戏生态。
4. **事件级低延迟输入映射 (Event-Driven Raw Input Pass-Through)**: 浏览器端捕获的原始物理键鼠移动量（`movementX`/`movementY`）与键盘事件，通过直连 WebRTC DataChannel 投递至 Minecraft 的 `client.mouse` 与 `KeyMapping`，精准模拟实体硬件操作。

---

## 2. 统一领域术语 (Ubiquitous Language)

全项目领域术语已统一定义并维护于 [CONTEXT.md](CONTEXT.md)。在代码实现、技术文档、注释、提交信息以及交互中，**必须**严格遵守该词典所规范的正式术语，严禁混用传统模糊词汇。

---

## 3. AI Agent 协作规则 (Rules for AI Agents)

在为 **Vistrel** 编写或重构任何代码时，AI Agent 必须严格恪守以下红线：

### 🚫 严禁违反的架构红线 (Invariants)
1. **绝对禁止阻塞渲染主线程**: 任何涉及视频帧读取（PBO 抓取除外）、图像缩放、NV12 转换、硬件编码以及网络 I/O 的操作，**必须**全部置于后台工作线程或 Rust Native 内部的异步运行时中执行。
2. **遵守 MultiLoader 分层规范**:
   - 核心像素流、信令服务、音频环回、Native 交互及输入转发代码必须 **100% 编写在 `common` 模块**。
   - `fabric`、`neoforge` 与 `forge` 仅作为轻量引导层（Bootstrap），仅负责注册平台专属事件（如 Fabric `ModInitializer`、Forge/NeoForge `@Mod` 构造器）与 SPI 服务实现。严禁在特定 Loader 模块内编写重复的核心业务逻辑。
3. **严格遵守 JNI 内存生命周期**:
   - 在 JNI 中读取 Java Direct Buffer 指针时，必须确保该 Buffer 在 Native 消费期间不被垃圾回收（GC）或被 OpenGL 解除映射（Unmap）。
   - 禁止在 Rust Native 中长期持有已经脱离生命周期的 JNI 本地引用（LocalRef）。
4. **保持零依赖轻量化设计**:
   - Java 端禁止引入重型外部 Web 容器（如 Tomcat / Spring 等）。
   - 信令服务端与静态网页服务器必须保持轻量嵌入式实现（通过轻量 WebSocket 或标准 NIO HTTP 静态托管）。
5. **代码生成**:
   - 各模块下的 `versions/` 目录（如 `common/versions/1.21.1/`、`fabric/versions/1.20.1/`）为 Stonecutter 动态生成的版本工作区。
   - 任何代码改动**必须且只能在根源码目录**（如 `common/src/`, `fabric/src/`）进行，跨版本差异必须使用 `//? if >=26.3` 条件注释语法实现。
6. **Git 规则**:
   - 提交任何变更到 Git 之前，必须执行 `./gradlew "Reset active project"` 重置为 26.3 基准视图，严禁将反转后的 `<26.3` 条件注释状态暂存或提交入库。
   - 提交信息采用 `Conventional Commits` 格式，语言使用简体中文。
7. **Rust unsafe 必须说明**:
   - `rust-native` 中每一处 `unsafe` 块、`unsafe fn` 与 `unsafe impl` 必须附带安全性注释，统一使用 `// SAFETY:` 前缀（`ffi_nvenc.rs` 等生成物豁免），说明：为何必须使用 unsafe、所依赖的不变量（如指针有效性、生命周期、线程归属、fd 所有权、驱动上下文绑定），以及为何在当前调用点这些不变量成立。
   - 注释必须陈述安全依据，不得仅复述操作内容；`unsafe impl Send / Sync` 等声明性实现同样必须说明守护机制（如 `CpuFramePool` 的三态状态机）。

### 🛠️ 通用工程与编码范式 (General Engineering Paradigms)

#### 1. 生命周期与资源管理范式 (Lifecycle & Resource Management)
- **RAII 与作用域自闭环**: 所有跨语言底层资源（fd、显存句柄、PBO 映射内存）必须绑定于对象生命周期或方法作用域内，捕获、投递与解映射必须自闭环，严禁跨帧暴露裸句柄或依赖外部手动释放。
- **全成全败事务原子性 (All-or-Nothing)**: 批量槽位与设备资源导入要么全量成功，任一环节失败必须逆序全量回滚并报错，严禁残留“部分就绪”的半死不活状态。
- **确定性级联析构 (Deterministic Teardown)**: 模块注销必须严格遵循“停止写者 → 排空在途任务与等待 ACK → 释放底层设备显存”的确定性次序，杜绝并发竞争与 Use-After-Free。

#### 2. 容错与防御性设计范式 (Error Handling & Resilience)
- **快速失败与安全闭环 (Fail-Fast & Fail-Closed)**: 必须在构造期、硬件探测期或握手首包完成金丝雀前置校验，不满足条件时立即显式报错并阻断，禁止带着隐患进入热路径。
- **断路器与永久降级 (Circuit Breaker & Permanent Demotion)**: 加速管线一旦在初始化或运行期发生单帧失败，断路器立即熔断并永久锁定在 RAM 回读等兜底管线上，本次会话绝不逐帧重试。
- **严禁静默妥协 (No Silent Compromise)**: 当底层不支持标称格式（如 RGBA/ABGR）时必须直接 fail-closed 报错降级，严禁采用导致红蓝通道颠倒（如静默回退 ARGB）或截断数据的“假妥协”方案。

#### 3. 模块与架构设计范式 (Architecture & Modular Design)
- **深模块架构 (Deep Module)**: 严格恪守“接口极简、实现极深”，将 VMA 分配、跨 API 句柄导入等底层细节完全封装在具体管线内部，对外仅暴露单一自闭环方法（如 `boolean capture()`）。
- **控制面推式倒置与不可变快照 (Control Push & Immutable Snapshot)**: 会话主控状态由 Native 单向推式回调通知，Java 侧以不可变 Record 原子替换快照，实现渲染主线程零锁、零 JNI 穿透读取且杜绝多字段数据撕裂。
- **空闲期绝对静默 (Idle Full-Silence)**: 无远程活动连接时，画面采集 Blit、输入轮询与窗口保活必须在纯 Java 顶层立即短路返回，底层系统与 JNI 调用保持 100% 绝对静默。

#### 4. 注释与文档范式 (Comments & Documentation)
- **注释只描述当前世界 (Timeless Comments)**: 注释只承载代码本身表达不出来的信息——契约（参数含义与单位、返回值语义、`@throws` 异常契约）、不变量与所有权（谁拥有 buffer/句柄、线程归属、生命周期）、设计依据（为什么这样做、为什么不那样做）。实现演变写进 Git 提交信息，设计决策写进 ADR/CONTEXT.md，未完成事项挂 issue 并以 `TODO(<编号>)` 锚定。
- **禁止开发过程叙事 (No Dev-Diary)**: 注释内严禁“原来 / 此前 / 换成 X 之后”式迁移叙事、注释掉的旧实现（Stonecutter `/* */` 条件壳除外），以及“此处不再重复 / 不再需要”式自指收敛动作的元叙事。判别金标准：删掉该注释后丢的是“历史”就直接删，丢的是“为什么”就改写为 timeless 的“为什么”再保留；错误或过时的注释比没有注释更糟。
- **导出 API 契约完整 (Complete API Contracts)**: common 模块导出 API 必须有 Javadoc 并逐参数使用 `@param` / `@return` / `@throws`（简单 getter 豁免）；裸 JNI native 方法必须文档化 buffer 所有权、内存布局与返回语义（与红线 3 的 JNI 生命周期约束对齐）。
- **分层不重复 (Single-Source Layering)**: 同一设计事实只出现在最能承载它的一层——类文档讲抽象与线程模型，方法文档讲契约，行内注释讲当场不变量；Rust 侧沿用 `// SAFETY:` 规范（红线 7）。

---

## 4. 开发、测试与调试准则 (Development & Operations)

### 4.1 环境基线与工具链要求

- **操作系统**: Linux x86_64（主要开发平台） / Windows 10/11 x64。
- **JDK 版本**: **Java 25 (LTS)**
- **Rust 版本**: **Rust 1.98+ (Edition 2024)**，包含 `cargo`（与 `rust-native/Cargo.toml` 的 `rust-version` 保持一致）。
- **Node.js 版本**: **Node 24+ (LTS)**，包含 `pnpm`。仅用于仓库根 `web/` 前端工程（Vite 构建 + ESLint）。

### 4.2 核心构建与调试命令一览

```bash
# 1. 编译 Rust 原生动态库
cd rust-native && cargo build --release

# 2. 批量编译并打包全版本全加载器 (1.20 / 1.21 / 26.3) 的最终 Jar 包
#    产物统一归拢到 build/libs/<MC 版本>/（forge 取 -all-srg 重映射发布产物）
#    消费指定 target 编译产物时注入 -Pvistrel.windowsTriple=<target> / -Pvistrel.linuxTriple=<target>
./gradlew buildAndCollect

# 3. 独立编译指定 Loader 和指定版本的产物
./gradlew :fabric:26.3:assemble
./gradlew :neoforge:1.21:assemble
./gradlew :forge:1.20:assemble

# 3.5 单独归拢指定版本的产物到根 build/libs/<MC 版本>/
./gradlew :fabric:26.3:buildAndCollect

# 4. 启动指定版本的目标客户端进行联调
./gradlew :fabric:26.3:runClient     # 启动 Fabric 26.3 客户端
./gradlew :fabric:1.21:runClient      # 启动 Fabric 1.21 客户端
./gradlew :neoforge:26.3:runClient   # 启动 NeoForge 26.3 客户端
./gradlew :neoforge:1.21:runClient   # 启动 NeoForge 1.21 客户端
./gradlew :forge:1.20:runClient      # 启动 Forge 1.20 客户端

# 5. Stonecutter IDE 源码视图切换 (自动原地反转条件注释)
./gradlew "Set active project to 1.21"   # 切至 1.21 视图
./gradlew "Set active project to 1.20"   # 切至 1.20 视图
./gradlew "Reset active project"        # 重置回 26.3 基准版本

# 6. 检查门（根 check = Spotless + 各版本节点测试）
./gradlew check
# 单独校验/修复 Java 源码格式 (Spotless)
./gradlew spotlessCheck
./gradlew spotlessApply
```

---

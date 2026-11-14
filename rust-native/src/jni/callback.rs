// ============================================================
// 跨线程 JNI 下行回调（ICE trickle 与日志桥）
// ============================================================
//
// 会话/档位状态推送已删除：Java 信令层用「offer 成功 / WS 关闭」自持状态，
// 档位只来自 `applyProfile` 阻塞 ask 的返回值与 offer answer。

use std::sync::OnceLock;

use jni::Env;
use jni::bind_java_type;
use jni::objects::JClass;
use jni::refs::LoaderContext;
use jni::sys::jint;

// Java 侧下行入口的编译期绑定：类引用与方法 ID 由 `NativeBridgeAPI::get` 一次缓存
// 之后每次分发不再重复 GetStaticMethodID；签名错位在编译期即报错
bind_java_type! {
    rust_type = NativeBridge,
    java_type = com.kairo.vistrel.streaming.natives.NativeBridge,
    methods {
        static fn on_local_ice_candidate(candidate_json: JString) -> void,

        static fn native_log(level: jint, message: JString) -> void,
    },
}

static JVM: OnceLock<jni::JavaVM> = OnceLock::new();

/// 捕获 JavaVM 与类绑定（幂等初始化）
pub fn init(env: &Env, class: &JClass) {
    if JVM.get().is_none()
        && let Ok(jvm) = env.get_java_vm()
    {
        let _ = JVM.set(jvm);
    }
    // 用 Java 线程传入的类对象解析类加载器，一次性缓存类引用与方法 ID：
    // AttachCurrentThread 附加的原生线程经 SystemClassLoader 找不到 Mod 类，
    // 所以这次解析必须锚在 Java 侧给的那个 Class 上
    let loader = LoaderContext::FromObject(class.as_ref());
    if let Err(e) = NativeBridgeAPI::get(env, &loader) {
        log::error!("NativeBridge 绑定初始化失败: {:?}", e);
    }
}

/// 跨线程向 Java 上抛本端 ICE 候选（服务端 trickle）
///
/// `candidate_json` 是 `RTCIceCandidateInit` 的 JSON；封装成浏览器识别的信令帧由 Java 侧完成
pub fn dispatch_local_ice_candidate(candidate_json: String) {
    let Some(jvm) = JVM.get() else {
        return;
    };
    let outcome: jni::errors::Result<()> = jvm.attach_current_thread_for_scope(|env| {
        let j_candidate = match env.new_string(candidate_json.as_str()) {
            Ok(s) => s,
            Err(e) => {
                log::error!("创建候选 JString 失败: {:?}", e);
                return Ok(());
            }
        };
        if let Err(e) = NativeBridge::on_local_ice_candidate(env, &j_candidate) {
            log::error!("JNI onLocalIceCandidate 调用失败: {:?}", e);
            env.exception_clear();
        }
        Ok(())
    });
    if let Err(e) = outcome {
        log::debug!("跨线程附加 JVM 失败（候选上抛跳过）: {:?}", e);
    }
}

/// 日志桥是否已就绪：就绪后 stdout 通道静默
pub fn bridge_ready() -> bool {
    JVM.get().is_some()
}

/// 跨线程把一条 native 日志转发进 Java 的 log4j（latest.log 可见）
///
/// 由日志出口逐条调用，本函数内部**禁止**再打日志：任何失败路径都会从 log 调用栈里
/// 再次进入日志系统，无限递归直至栈溢出，失败只能静默吞掉（stdout 已有同内容兜底）
pub fn dispatch_native_log(level: log::Level, message: String) {
    let Some(jvm) = JVM.get() else {
        // 桥未就绪（引擎启动前的极少量日志）：env_logger 已写 stdout，这里静默丢弃
        return;
    };
    // log 门面的级别判别式就是 1..=5（Error=1..Trace=5），与 Java 侧映射约定一致
    let jvm_level = level as jint;
    let outcome: jni::errors::Result<()> = jvm.attach_current_thread_for_scope(|env| {
        let j_message = match env.new_string(message.as_str()) {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        if NativeBridge::native_log(env, jvm_level, &j_message).is_err() {
            env.exception_clear();
        }
        Ok(())
    });
    let _ = outcome;
}

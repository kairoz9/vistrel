// jni-macros 的 `native_method!` 展开内部使用已弃用的 `Atomic::fetch_update`，弃用
// 告警落在每个导出项的宏调用点上（本文件自身代码不产生弃用告警）；依赖侧修复前在此豁免。
#![allow(deprecated)]

use crate::encode::control::ControlStatus;
use crate::encode::hardware::slots::{ActorStage, MAX_VRAM_SLOTS};
use crate::encode::hardware::{FLAT_STRIDE, VramSlotDescriptor};
use crate::encode::pipeline::{pack_reply, pack_status};
use crate::encode::{self, VideoPipeline};
use crate::jni::callback;
use crate::jni::{Reply, elapsed_birth_nanos, get_global};
use crate::streaming::StreamingEngine;
use crate::streaming::audio_loopback::AudioLoopback;
use jni::Env;
use jni::errors::LogErrorAndDefault;
use jni::native_method;
use jni::objects::{JByteBuffer, JClass, JPrimitiveArray, JString};
use jni::sys::{jfloat, jint, jlong};
use rtc::peer_connection::sdp::RTCSessionDescription;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use webrtc::peer_connection::RTCIceCandidateInit;

// JNI 边界护栏：`native_method!` 生成的包装器自带 catch_unwind —— panic 一旦越过
// `extern "system"` 边界会 abort 整个 JVM（不是抛异常），所以每个导出面统一用
// `LogErrorAndDefault` 收口：记日志后按零值返回。导出符号串仍逐字保留（不用宏的自动
// mangle），确保 JVM 按短名解析到与本文件外历史版本一致的符号。
//
// 所有可能失败的请求/应答导出面都返回统一信封（见 `Reply`）：panic 或 JNI
// 失败记日志后返回空串，Java 解析为空即按 retry 处理，不会把零值伪装成成功。

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_startEngine",
    error_policy = LogErrorAndDefault,
    static fn start_engine(openh264_lib: JString, udp_bind: JString) -> void,
};

fn start_engine<'caller>(
    env: &mut Env<'caller>,
    class: JClass<'caller>,
    openh264_lib: JString<'caller>,
    udp_bind: JString<'caller>,
) -> jni::errors::Result<()> {
    let state = get_global();
    if state.is_running.load(Ordering::SeqCst) {
        // 引擎仍存活说明 Java 侧拆除未先行落地：拒绝重复启动并让 Java 走失败清理。
        // 静默跳过会让新服务顶着旧引擎伪装成启动成功，随后旧引擎一拆即成空壳
        log::error!("startEngine 在引擎仍存活时被调用：拆除未先行落地，拒绝重复启动");
        env.throw_new(
            jni::jni_str!("java/lang/IllegalArgumentException"),
            jni::jni_str!("startEngine: 引擎未拆除，拒绝重复启动"),
        )?;
        return Ok(());
    }

    // 缺失即失败：`bindAddress:port` 必须由 Java 提供。回退通配临时端口会让媒体面在
    // 全部网卡上监听，与配置项收窄监听范围的意图相反
    let udp_bind_addr = udp_bind
        .try_to_string(env)
        .ok()
        .filter(|text| !text.trim().is_empty());
    let Some(udp_bind_addr) = udp_bind_addr else {
        log::error!("startEngine 缺少 UDP 绑定地址，拒绝启动媒体引擎");
        env.throw_new(
            jni::jni_str!("java/lang/IllegalArgumentException"),
            jni::jni_str!("startEngine 缺少 UDP 绑定地址：Java 侧需传入 bindAddress:port"),
        )?;
        return Ok(());
    };

    // 输入环随会话生灭（会话器官，非全局状态）：旧会话的环连同残留事件一起
    // 析构，不存在跨重启残留，也就没有清环动作

    state.is_running.store(true, Ordering::SeqCst);

    // 注入软件编码库路径（Java 已从 jar 提取到配置目录）。必须先于 VideoPipeline::spawn：
    // EncoderActor 启动即构造软件编码。路径缺失只降级软编，硬件路径照常。
    let openh264_path = if openh264_lib.is_null() {
        String::new()
    } else {
        openh264_lib.try_to_string(env).unwrap_or_default()
    };
    if openh264_path.is_empty() {
        log::warn!("startEngine 未携带 openh264 库路径，软件编码不可用，仅剩硬件编码");
    }
    encode::software::init_library(&openh264_path);

    callback::init(env, &class);

    // 硬件能力探测的唯一发生地：必须是引擎启动路径，不能是「谁先查询谁探测」。
    // 探测会创建并销毁一个 CudaContext / 开关一次 NVENC session，代价落在本线程上。
    encode::bootstrap_hardware_caps();

    // 构造编码管线组合根并拉起编码线程（资源全部归属 Actor 线程）
    let pipeline = VideoPipeline::spawn();
    let audio = AudioLoopback::new();

    let engine = Arc::new(StreamingEngine::new(
        pipeline,
        Arc::clone(&audio),
        udp_bind_addr,
    ));

    // 注册跨线程回调出口：档位状态已改由 Java 信令层自持（offer 成功 / WS 关闭），
    // native 不再向 Java 推送会话状态；这里只剩 ICE 候选 trickle
    engine.set_candidate_sink(Arc::new(|candidate_json| {
        callback::dispatch_local_ice_candidate(candidate_json);
    }));

    state.streaming_engine.store(Some(Arc::clone(&engine)));

    Ok(())
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_stopEngine",
    error_policy = LogErrorAndDefault,
    static fn stop_engine() -> void,
};

fn stop_engine<'caller>(
    _env: &mut Env<'caller>,
    _class: JClass<'caller>,
) -> jni::errors::Result<()> {
    let state = get_global();
    state.is_running.store(false, Ordering::SeqCst);

    let engine = state.streaming_engine.swap(None);

    if let Some(engine) = engine {
        state.runtime.block_on(engine.stop());
    }
    Ok(())
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_negotiateOffer",
    error_policy = LogErrorAndDefault,
    static fn negotiate_offer(offer_json: JString, width: jint, height: jint, fps: jint, bpp: jfloat, codec: JString, backend: JString) -> JString,
};

// 参数表是 Java native 方法的镜像，由 JNI ABI 定死，不归本侧的参数预算管
#[allow(clippy::too_many_arguments)]
fn negotiate_offer<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    offer_json: JString<'caller>,
    width: jint,
    height: jint,
    fps: jint,
    bpp: jfloat,
    codec: JString<'caller>,
    backend: JString<'caller>,
) -> jni::errors::Result<JString<'caller>> {
    if offer_json.is_null() {
        return Ok(JString::default());
    }
    let offer_str = match offer_json.try_to_string(env) {
        Ok(s) => s,
        Err(_) => return Ok(JString::default()),
    };
    let codec_token = if codec.is_null() {
        None
    } else {
        codec.try_to_string(env).ok()
    };
    let backend_token = if backend.is_null() {
        None
    } else {
        backend.try_to_string(env).ok()
    };
    let state = get_global();
    let is_running = state.is_running.load(Ordering::SeqCst);
    let engine = state.streaming_engine.load_full();
    let resp_str = state
        .runtime
        .block_on(handle_negotiate_offer(
            engine,
            is_running,
            &offer_str,
            width,
            height,
            fps,
            bpp,
            codec_token.as_deref(),
            backend_token.as_deref(),
        ))
        .encode();
    match env.new_string(resp_str) {
        Ok(output) => Ok(output),
        Err(_) => {
            log::error!("negotiateOffer 创建 Java 字符串失败");
            Ok(JString::default())
        }
    }
}

/// negotiateOffer 线上请求的处理函数：存活守卫、offer 反序列化、几何与编码偏好
/// 透传、应答整形；出口只做 JString/Env 搬运，测试直接驱动本函数钉线契约。
/// `codec_token` / `backend_token` 缺省按自动；词表外令牌与不可行的偏好一律拒绝，
/// 偏好从不静默降级
// 参数表与 JNI 包装层一一对应（后者由 Java native 签名定死），不归本侧的参数预算管
#[allow(clippy::too_many_arguments)]
async fn handle_negotiate_offer(
    engine: Option<Arc<StreamingEngine>>,
    is_running: bool,
    offer_json: &str,
    width: i32,
    height: i32,
    fps: i32,
    bpp: f32,
    codec_token: Option<&str>,
    backend_token: Option<&str>,
) -> Reply {
    if !is_running {
        return Reply::retry("串流引擎正在关停");
    }
    let Some(engine) = engine else {
        log::error!("StreamingEngine 未启动，无法协商 Offer");
        return Reply::retry("串流引擎未启动");
    };
    let offer: RTCSessionDescription = match serde_json::from_str(offer_json) {
        Ok(sdp) => sdp,
        Err(e) => {
            log::error!("无法反序列化 Offer SDP: {e}");
            return Reply::fatal(format!("无法解析 Offer SDP: {e}"));
        }
    };

    // 几何原样透传：0×0 是暂停信号，由引擎按契约在 handle_offer 入口显式拒绝，
    // 缝上不做「修正成最小档位」；负数钳成 0 只为保证 i32→usize 不回绕
    let w = width.max(0) as usize;
    let h = height.max(0) as usize;
    let f = fps.max(0) as usize;
    let initial_bpp = if bpp.is_finite() && bpp > 0.0 {
        Some(bpp)
    } else {
        None
    };
    // 词表外的偏好令牌 = 契约漂移：显式拒绝协商，不做静默降级
    let preferred_codec = match codec_token {
        None => None,
        Some(token) => match encode::VideoCodec::from_token(token) {
            Some(codec) => Some(codec),
            None => {
                log::warn!("negotiateOffer 的 codec 偏好 {token:?} 无法识别，协商拒绝");
                return Reply::fatal(format!("无法识别的 codec 偏好 {token:?}"));
            }
        },
    };
    let preferred_backend = match backend_token {
        None => None,
        Some(token) => match encode::EncodeBackendKind::from_token(token) {
            Some(kind) => Some(kind),
            None => {
                log::warn!("negotiateOffer 的后端偏好 {token:?} 无法识别，协商拒绝");
                return Reply::fatal(format!("无法识别的编码后端偏好 {token:?}"));
            }
        },
    };

    let (answer, profile, codec, backend) = match engine
        .handle_offer(
            offer,
            w,
            h,
            f,
            initial_bpp,
            crate::streaming::EncodePreferences {
                backend: preferred_backend,
                codec: preferred_codec,
            },
        )
        .await
    {
        Ok(parts) => parts,
        Err(reply) => {
            log::error!(
                "WebRTC Offer 协商失败（可重试={}）: {}",
                reply.retryable(),
                reply.message()
            );
            return reply;
        }
    };
    Reply::ok(serde_json::json!({
        "type": "answer",
        "width": profile.width(),
        "height": profile.height(),
        "fps": profile.fps(),
        // 生效画质输入：与派生码率消费的钳制值一致（偏好本身不在档位事实里）
        "bpp": encode::clamp_bpp(initial_bpp.unwrap_or(encode::MIN_BPP)),
        "bitrate": profile.bitrate(),
        "codec": codec.as_str(),
        "backend": backend.as_str(),
        "backends": encode::encoder_caps_options_json(),
        "bppMin": encode::MIN_BPP,
        "bppMax": encode::MAX_BPP,
        "sdp": answer,
    }))
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_applyProfile",
    error_policy = LogErrorAndDefault,
    static fn apply_profile(
        width: jint,
        height: jint,
        fps: jint,
        bpp: jfloat,
    ) -> JString,
};

/// 阻塞应用新的画质档位（Java 后台 worker / 信令 executor 调用，渲染线程绝不调用）
///
/// 成功时 `data` 是实际生效档位（`width/height/fps/bpp/bitrate`）；被拒绝按
/// `fatal`、无会话按 `retry` 返回，Java 保持自己的生效基线。codec 不在线合同里：
/// 它是协商定死的会话常量。
fn apply_profile<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    width: jint,
    height: jint,
    fps: jint,
    bpp: jfloat,
) -> jni::errors::Result<JString<'caller>> {
    let state = get_global();
    let engine = state.streaming_engine.load_full();
    let resp_str = state
        .runtime
        .block_on(handle_apply_profile(engine, width, height, fps, bpp))
        .encode();
    match env.new_string(resp_str) {
        Ok(s) => Ok(s),
        Err(_) => Ok(JString::default()),
    }
}

/// applyProfile 线上请求的处理函数：几何透传与应答整形都在这里，
/// 出口只做 JString/Env 搬运，测试不经 Env 直接驱动本函数钉住线契约。
async fn handle_apply_profile(
    engine: Option<Arc<StreamingEngine>>,
    width: i32,
    height: i32,
    fps: i32,
    bpp: f32,
) -> Reply {
    let Some(engine) = engine else {
        log::warn!("applyProfile 时引擎未启动，拒绝档位请求");
        return Reply::retry("applyProfile 时引擎未启动");
    };

    // 几何透传同协商缝：0 由引擎按暂停信号契约拒绝，负数钳成 0 只为保证不回绕
    let outcome = engine
        .apply_profile(
            width.max(0) as usize,
            height.max(0) as usize,
            fps.max(0) as usize,
            bpp,
        )
        .await;
    let profile = match outcome {
        crate::streaming::ProfileApply::Applied(profile) => profile,
        crate::streaming::ProfileApply::Rejected => {
            return Reply::fatal("档位重配被编码器拒绝");
        }
        crate::streaming::ProfileApply::NoSession => {
            return Reply::retry("无活跃会话");
        }
    };
    Reply::ok(serde_json::json!({
        "width": profile.width(),
        "height": profile.height(),
        "fps": profile.fps(),
        // 生效画质输入：与派生码率消费的钳制值一致（偏好本身不在档位事实里）
        "bpp": encode::clamp_bpp(bpp),
        "bitrate": profile.bitrate(),
    }))
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_addIceCandidate",
    error_policy = LogErrorAndDefault,
    static fn add_ice_candidate(candidate_json: JString) -> void,
};

fn add_ice_candidate<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    candidate_json: JString<'caller>,
) -> jni::errors::Result<()> {
    if candidate_json.is_null() {
        return Ok(());
    }
    let cand_str = match candidate_json.try_to_string(env) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let candidate: RTCIceCandidateInit = match serde_json::from_str(&cand_str) {
        Ok(cand) => cand,
        Err(e) => {
            log::error!("无法反序列化 ICE Candidate: {e}");
            return Ok(());
        }
    };

    let state = get_global();
    if let Some(engine) = state.streaming_engine.load_full() {
        state.runtime.spawn(async move {
            engine.add_ice_candidate(candidate).await;
        });
    }
    Ok(())
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_closeSession",
    error_policy = LogErrorAndDefault,
    static fn close_session() -> void,
};

fn close_session<'caller>(
    _env: &mut Env<'caller>,
    _class: JClass<'caller>,
) -> jni::errors::Result<()> {
    let state = get_global();
    if let Some(engine) = state.streaming_engine.load_full() {
        state.runtime.spawn(async move {
            engine.close_session().await;
        });
    }
    Ok(())
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_pollInputEvents",
    error_policy = LogErrorAndDefault,
    static fn poll_input_events(out_buffer: JByteBuffer, max_events: jint) -> jint,
};

fn poll_input_events<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    out_buffer: JByteBuffer<'caller>,
    max_events: jint,
) -> jni::errors::Result<jint> {
    let state = get_global();
    if !state.is_running.load(Ordering::Relaxed) {
        return Ok(0);
    }

    let ptr = match env.get_direct_buffer_address(&out_buffer) {
        Ok(p) => p,
        Err(_) => return Ok(0),
    };
    let cap = match env.get_direct_buffer_capacity(&out_buffer) {
        Ok(c) => c,
        Err(_) => return Ok(0),
    };

    // SAFETY: ptr 来自 get_direct_buffer_address 的成功返回，长度即 JVM 报告
    // 的缓冲容量 cap；可变切片仅在本 JNI 调用栈内被会话输入环提取同步写入
    // 读取，不会逃逸到 Buffer 失效之后
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr, cap) };
    let engine = state.streaming_engine.load();
    let Some(engine) = engine.as_ref() else {
        return Ok(0);
    };
    Ok(engine.drain_input_events(slice, max_events as usize) as jint)
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_queryEncoderCaps",
    error_policy = LogErrorAndDefault,
    static fn query_encoder_caps() -> JString,
};

fn query_encoder_caps<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
) -> jni::errors::Result<JString<'caller>> {
    // 纯读内存，不碰设备。data 是本机全部可用后端及其 codec 与显卡名，由 Java 侧解析缓存
    // ——诊断界面按会话后端查表，显存直通门控按有无硬编判定，都从这一份快照派生。
    let json = Reply::ok(encode::encoder_caps()).encode();
    match env.new_string(json) {
        Ok(s) => Ok(s),
        Err(_) => Ok(JString::default()),
    }
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_importVramSlots",
    error_policy = LogErrorAndDefault,
    static fn import_vram_slots(flat: jlong[]) -> JString,
};

fn import_vram_slots<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    flat: JPrimitiveArray<'caller, jlong>,
) -> jni::errors::Result<JString<'caller>> {
    let json = import_vram_slots_reply(env, flat).encode();
    match env.new_string(json) {
        Ok(s) => Ok(s),
        Err(_) => Ok(JString::default()),
    }
}

/// 槽位整批导入：Native 读当前轮次交给 Actor（stale 时不消费句柄），
/// 成功回送 lease 供 Java 取同一代次的块。
fn import_vram_slots_reply<'caller>(
    env: &mut Env<'caller>,
    flat: JPrimitiveArray<'caller, jlong>,
) -> Reply {
    // 0. 硬编能力预检：读启动期建立的快照，本机无硬编时直接拒绝
    if encode::hardware_backends().is_none() {
        return Reply::ok(serde_json::json!({ "code": ControlStatus::NoHardwareEncoder as i32 }));
    }

    // 整批处理：Java 决定槽位数量，Native 只按协议切分收到的数组
    let Ok(len) = flat.len(env) else {
        return Reply::ok(serde_json::json!({ "code": ControlStatus::MalformedBatch as i32 }));
    };
    if len == 0 || !len.is_multiple_of(FLAT_STRIDE) {
        return Reply::ok(serde_json::json!({ "code": ControlStatus::MalformedBatch as i32 }));
    }

    let mut buf = vec![0i64; len];
    if flat.get_region(env, 0, &mut buf).is_err() {
        return Reply::ok(serde_json::json!({ "code": ControlStatus::MalformedBatch as i32 }));
    }

    let Some(slots) = VramSlotDescriptor::parse_batch(&buf) else {
        return Reply::ok(serde_json::json!({ "code": ControlStatus::MalformedBatch as i32 }));
    };
    if slots
        .iter()
        .any(|descriptor| descriptor.slot_idx as usize >= MAX_VRAM_SLOTS)
    {
        // 未进入导入：描述符在此析构并关闭原始句柄
        return Reply::ok(serde_json::json!({ "code": ControlStatus::MalformedBatch as i32 }));
    }

    // 派发控制命令给 Actor；绑定句柄由 Native 在导入成功时分配并回送
    let engine = get_global().streaming_engine.load();
    let Some(engine) = engine.as_ref() else {
        return Reply::ok(serde_json::json!({ "code": ControlStatus::ActorDead as i32 }));
    };
    let pipeline = engine.video();
    let owner = std::thread::current().id();
    match pipeline.import_vram_slots(owner, slots) {
        Ok(handle) => Reply::ok(serde_json::json!({
            "code": ControlStatus::Ok as i32,
            "handle": handle,
        })),
        Err(status) => Reply::ok(serde_json::json!({ "code": status as i32 })),
    }
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_pushVramSlot",
    error_policy = LogErrorAndDefault,
    static fn push_vram_slot(handle: jlong, slot: jint, sync_value: jlong) -> jint,
};

fn push_vram_slot<'caller>(
    _env: &mut Env<'caller>,
    _class: JClass<'caller>,
    handle: jlong,
    slot: jint,
    sync_value: jlong,
) -> jni::errors::Result<jint> {
    let birth_nanos = elapsed_birth_nanos();

    // 句柄、线程与槽位校验全在 Native 侧：旧绑定的帧在入口丢弃，不进入收件箱
    let engine = get_global().streaming_engine.load();
    let Some(engine) = engine.as_ref() else {
        return Ok(pack_status(ControlStatus::ActorDead));
    };
    Ok(pack_status(engine.video().push_vram_slot(
        handle as u64,
        slot as u32,
        birth_nanos,
        sync_value as u64,
    )))
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_vramSlotState",
    error_policy = LogErrorAndDefault,
    static fn vram_slot_state(handle: jlong) -> jlong,
};

fn vram_slot_state<'caller>(
    _env: &mut Env<'caller>,
    _class: JClass<'caller>,
    handle: jlong,
) -> jni::errors::Result<jlong> {
    // 每 tick 一次：权威在途快照 + 心跳。游戏侧据此选槽并判定停滞
    let engine = get_global().streaming_engine.load();
    let Some(engine) = engine.as_ref() else {
        return Ok(pack_reply(ControlStatus::ActorDead, 0, 0) as jlong);
    };
    Ok(engine.video().vram_slot_state(handle as u64) as jlong)
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_clearVramSlots",
    error_policy = LogErrorAndDefault,
    static fn clear_vram_slots(handle: jlong) -> JString,
};

fn clear_vram_slots<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    handle: jlong,
) -> jni::errors::Result<JString<'caller>> {
    let code = clear_vram_slots_code(handle);
    let json = Reply::ok(serde_json::json!(code)).encode();
    match env.new_string(json) {
        Ok(s) => Ok(s),
        Err(_) => Ok(JString::default()),
    }
}

/// 槽位注销的领域结局：返回 `ControlStatus` 判别值，由外层统一装进 data。
fn clear_vram_slots_code(handle: jlong) -> i32 {
    // 引擎未运行即无导入方：注销实际是空操作，Vacuous ACK 同样成立。
    // 这里返回非 OK 会让 Java 按「未获确认」保留导出资源，而编码器早已随引擎析构，
    // 保留只会白白丢掉三张导出纹理。
    let engine = get_global().streaming_engine.load();
    let Some(engine) = engine.as_ref() else {
        return ControlStatus::Ok.code();
    };
    let pipeline = engine.video();

    // 清空待编帧、排空 GPU 在途任务并注销显存槽位：ClearVramSlots 单命令完成
    // 「停写者 → 排空异步尾巴 → 释放导入」的固定次序，一次控制往返
    // 注意：不回退软编！硬件编码器仍保持就绪，可无缝接收随后的主机帧池块 (RAM DMA 双缓冲) 并用 StagingBuffer 进行硬编
    let code = match pipeline.clear_vram_slots(handle as u64) {
        Ok(()) => ControlStatus::Ok.code(),
        Err(e) => e.code(),
    };
    if code != ControlStatus::Ok as i32 {
        // 未获确认：编码器可能仍持有导出资源的跨 API 导入，Java 侧不得销毁，
        // 结果码必须原样上报而不是装作成功
        log::warn!("显存槽位注销未获 Actor 确认 (code={code})，游戏侧导出资源暂缓销毁");
    }
    code
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_stallDiagnostics",
    error_policy = LogErrorAndDefault,
    static fn stall_diagnostics(handle: jlong) -> JString,
};

fn stall_diagnostics<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    handle: jlong,
) -> jni::errors::Result<JString<'caller>> {
    // 冷路径：只在心跳超时后调用一次。阶段码表只留在 Native，Java 不再维护第二份映射
    let label = get_global()
        .streaming_engine
        .load()
        .as_ref()
        .and_then(|engine| engine.video().stall_stage(handle as u64))
        .map(ActorStage::describe)
        .unwrap_or_else(|| "引擎未运行".to_string());
    match env.new_string(label) {
        Ok(s) => Ok(s),
        Err(_) => Ok(JString::default()),
    }
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_pushSystemFrame",
    error_policy = LogErrorAndDefault,
    static fn push_system_frame(buffer: JByteBuffer, width: jint, height: jint) -> void,
};

fn push_system_frame<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    buffer: JByteBuffer<'caller>,
    width: jint,
    height: jint,
) -> jni::errors::Result<()> {
    let state = get_global();
    let birth_nanos = elapsed_birth_nanos();

    let ptr = match env.get_direct_buffer_address(&buffer) {
        Ok(p) => p,
        Err(e) => {
            log::error!("获取 DirectBuffer 地址失败: {:?}", e);
            return Ok(());
        }
    };
    let cap = match env.get_direct_buffer_capacity(&buffer) {
        Ok(c) => c,
        Err(e) => {
            log::error!("获取 DirectBuffer 容量失败: {:?}", e);
            return Ok(());
        }
    };
    if cap == 0 {
        return Ok(());
    }

    // 帧数据拷进 CPU 帧池的可复用缓冲后即刻返回，软编在 Actor 线程异步执行；
    // PBO DirectBuffer 仅在本 JNI 调用期内有效，拷贝必须就地完成。
    // 宽高来自调用方的采集缓冲：非法或字节数与几何不符就整帧拒绝
    let display = match validate_display_extent(width, height, cap) {
        Ok(display) => display,
        Err(e) => {
            log::warn!("pushSystemFrame 几何非法（{width}x{height}, {cap} 字节）: {e}");
            return Ok(());
        }
    };
    // SAFETY: ptr 来自 get_direct_buffer_address 的成功返回，长度取 DirectBuffer
    // 的有效容量 cap；切片仅在本 JNI 调用栈内被同步拷贝消费，不会逃逸到 Buffer 失效之后
    let slice = unsafe { std::slice::from_raw_parts(ptr, cap) };
    // 帧池与收件箱都属于管线：引擎未启动时连拷贝都不必做
    let Some(engine) = state.streaming_engine.load_full() else {
        return Ok(());
    };
    engine.video().push_host_frame(birth_nanos, slice, display);
    Ok(())
}

/// 采集几何的信任边界校验：偶数、非零，且字节数与该几何的 NV12 长度恰好相等
fn validate_display_extent(
    width: jint,
    height: jint,
    bytes: usize,
) -> Result<(usize, usize), String> {
    let (w, h) = (width.max(0) as usize, height.max(0) as usize);
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err(format!("宽高必须为偶数且非零（{w}x{h}）"));
    }
    let expected = crate::encode::nv12_len(w, h);
    if bytes != expected {
        return Err(format!("字节数不符：期望 {expected}，实际 {bytes}"));
    }
    Ok((w, h))
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_setAudioBitrate",
    error_policy = LogErrorAndDefault,
    static fn set_audio_bitrate(bitrate_kbps: jint) -> void,
};

fn set_audio_bitrate<'caller>(
    _env: &mut Env<'caller>,
    _class: JClass<'caller>,
    bitrate_kbps: jint,
) -> jni::errors::Result<()> {
    let state = get_global();
    if let Some(sessions) = state.streaming_engine.load().as_ref() {
        sessions.audio().set_bitrate(bitrate_kbps);
    }
    Ok(())
}

const _: jni::NativeMethod = native_method! {
    java_type = "kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_kairo_vistrel_streaming_natives_NativeBridge_pushAudioPcm",
    error_policy = LogErrorAndDefault,
    static fn push_audio_pcm(buffer: JByteBuffer, samples_per_channel: jint) -> void,
};

fn push_audio_pcm<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    buffer: JByteBuffer<'caller>,
    samples_per_channel: jint,
) -> jni::errors::Result<()> {
    let state = get_global();
    let engine = state.streaming_engine.load();
    let Some(engine) = engine.as_ref() else {
        return Ok(());
    };

    let ptr = match env.get_direct_buffer_address(&buffer) {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };
    let cap = match env.get_direct_buffer_capacity(&buffer) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let total_samples = (samples_per_channel as usize) * crate::encode::AUDIO_CHANNELS;
    let expected_bytes = total_samples * size_of::<i16>();
    if cap < expected_bytes {
        log::warn!(
            "pushAudioPcm: DirectBuffer 容量不足 (期望 {}, 实际 {})",
            expected_bytes,
            cap
        );
        return Ok(());
    }

    // SAFETY:
    // 1. ptr 来自 env.get_direct_buffer_address 的有效返回，且已校验 cap >= expected_bytes；
    // 2. 指针按 16-bit PCM 对齐，在当前 JNI 同步调用生命周期内由 Java 调用栈保证缓冲区有效且不被 GC 回收；
    // 3. 切片仅在当前栈内被同步传入 Opus 编码器消费，绝不逃逸出当前函数生命周期。
    let pcm = unsafe { std::slice::from_raw_parts(ptr as *const i16, total_samples) };
    engine.audio().push_sample(pcm);
    Ok(())
}

/// JNI 线契约用例：不经 Env 直接驱动 handle_negotiate_offer / handle_apply_profile
#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::test_support::{make_test_engine, make_test_offer};

    /// make_test_offer 的线协议形态：Java 信令层透传的就是这份 JSON
    fn offer_wire_json() -> String {
        serde_json::to_string(&make_test_offer()).expect("测试 SDP 必可序列化")
    }

    /// 0×0 是暂停信号，协商缝不得把它「修正」成最小档位：0 与负数都必须被显式拒绝
    #[tokio::test]
    async fn test_negotiate_rejects_zero_geometry_as_pause_signal() {
        let (engine, _rx) = make_test_engine(false);
        for geometry in [(0, 0, 0), (-1, -1, -1)] {
            let reply = handle_negotiate_offer(
                Some(Arc::clone(&engine)),
                true,
                &offer_wire_json(),
                geometry.0,
                geometry.1,
                geometry.2,
                1.0,
                None,
                None,
            )
            .await;
            assert!(
                reply.message().contains("内容档位为 0"),
                "几何 {geometry:?} 必须被拒绝而非修正成 1×1@1"
            );
            assert!(reply.retryable(), "暂停是瞬时状态，内容恢复后可重试");
        }
    }

    /// 关停开始后拒新协商，与引擎是否仍在位无关
    #[tokio::test]
    async fn test_negotiate_refuses_while_shutting_down() {
        let (engine, _rx) = make_test_engine(false);
        let reply = handle_negotiate_offer(
            Some(engine),
            false,
            &offer_wire_json(),
            1920,
            1080,
            60,
            1.0,
            None,
            None,
        )
        .await;
        assert!(reply.message().contains("引擎"));
        assert!(reply.retryable(), "引擎重启后同一 offer 可重试");
    }

    /// 引擎未启动 = 结构化拒绝，不是 panic
    #[tokio::test]
    async fn test_negotiate_without_engine_returns_typed_error() {
        let reply = handle_negotiate_offer(
            None,
            true,
            &offer_wire_json(),
            1920,
            1080,
            60,
            1.0,
            None,
            None,
        )
        .await;
        assert!(reply.message().contains("未启动"));
        assert!(reply.retryable(), "引擎启动后同一 offer 可重试");
    }

    #[tokio::test]
    async fn test_apply_profile_without_engine_is_retryable() {
        let reply = handle_apply_profile(None, 1600, 900, 90, 0.2).await;
        assert!(reply.retryable(), "引擎重启后档位请求可重试");
    }

    /// 全线往返：协商建会话 → 档位生效 → 0 几何档位被拒。应答 JSON 的键集是 Java
    /// 解析器锚定的线契约，多键少键都算漂移。指定的 codec 走规范令牌（大小写不敏感），
    /// 能力清单在未探测环境只含软件编码
    #[tokio::test]
    async fn test_negotiate_then_apply_roundtrip_pin_line_contract() {
        let (engine, _rx) = make_test_engine(false);

        let reply = handle_negotiate_offer(
            Some(Arc::clone(&engine)),
            true,
            &offer_wire_json(),
            1920,
            1080,
            60,
            1.0,
            Some("H264"),
            None,
        )
        .await;
        let answer_json = reply.data().expect("假后端上的合法协商必须得到应答");
        assert_eq!(answer_json["type"], "answer");
        assert_eq!(answer_json["width"], 1920);
        assert_eq!(answer_json["height"], 1080);
        assert_eq!(answer_json["fps"], 60);
        // sdp 原样透传给浏览器，保持 W3C 形态（嵌套对象携带 type 与 SDP 文本）
        assert_eq!(answer_json["sdp"]["type"], "answer");
        assert!(
            !answer_json["sdp"]["sdp"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
        for key in [
            "width", "height", "fps", "bpp", "bitrate", "codec", "backend", "backends", "bppMin",
            "bppMax", "sdp",
        ] {
            assert!(answer_json.get(key).is_some(), "offer 应答缺 {key} 键");
        }
        assert_eq!(
            answer_json["backend"], "software",
            "回执后端是假 Actor 实际建成的身份，不是偏好值"
        );
        assert_eq!(
            answer_json["backends"],
            serde_json::json!([{ "kind": "software", "codecs": ["H264"], "gpu": "" }]),
            "未探测环境的能力清单只含软件编码（软件编码恒收尾）"
        );

        let reply = handle_apply_profile(Some(Arc::clone(&engine)), 1600, 900, 90, 0.2).await;
        let applied = reply.data().expect("活跃会话上的合法档位必须生效");
        assert_eq!(applied["width"], 1600);
        for key in ["width", "height", "fps", "bpp", "bitrate"] {
            assert!(applied.get(key).is_some(), "档位应答缺 {key} 键");
        }
        // codec 与后端身份都是协商定死的会话常量：档位应答一概不携带
        for absent in ["codec", "backend"] {
            assert!(
                applied.get(absent).is_none(),
                "档位应答不得携带 {absent} 键（会话常量只在协商时确定）"
            );
        }

        let reply = handle_apply_profile(Some(Arc::clone(&engine)), 0, 0, 0, 0.2).await;
        assert!(reply.data().is_none(), "拒绝不得携带 data");
        assert!(!reply.retryable(), "同一档位重试不会成功");
    }

    /// 编码偏好的硬约束契约：词表外令牌、清单外后端、本机无法满足的 codec 一律协商
    /// 拒绝，绝不静默降级——「无法满足即报错」
    #[tokio::test]
    async fn test_negotiate_rejects_unhonorable_preferences() {
        let (engine, _rx) = make_test_engine(false);

        // 词表外的令牌：契约漂移，拒绝
        let reply = handle_negotiate_offer(
            Some(Arc::clone(&engine)),
            true,
            &offer_wire_json(),
            1920,
            1080,
            60,
            1.0,
            Some("wmv"),
            None,
        )
        .await;
        assert!(reply.message().contains("codec"));
        assert!(!reply.retryable(), "同一偏好重试不会成功");

        // 可识别但不可行：单测无探测快照（等价离线软编机），指定硬编必须拒绝
        let reply = handle_negotiate_offer(
            Some(Arc::clone(&engine)),
            true,
            &offer_wire_json(),
            1920,
            1080,
            60,
            1.0,
            None,
            Some("Vulkan_Video"),
        )
        .await;
        assert!(reply.message().contains("编码后端"));
        assert!(!reply.retryable(), "同一偏好重试不会成功");

        // 可识别但不可行：软编机指定 AV1——「没有 AV1 的机器上远程玩家指定 AV1 就返回错误」
        let reply = handle_negotiate_offer(
            Some(Arc::clone(&engine)),
            true,
            &offer_wire_json(),
            1920,
            1080,
            60,
            1.0,
            Some("AV1"),
            None,
        )
        .await;
        assert!(reply.message().contains("codec"));
        assert!(!reply.retryable(), "同一偏好重试不会成功");

        // 指定软编在能力清单内（软件编码恒可选）：照常协商成功，回执如实携带软编身份
        let reply = handle_negotiate_offer(
            Some(Arc::clone(&engine)),
            true,
            &offer_wire_json(),
            1920,
            1080,
            60,
            1.0,
            None,
            Some("software"),
        )
        .await;
        let answer_json = reply.data().expect("能力清单内的软编偏好必须被满足");
        assert_eq!(answer_json["backend"], "software");
    }
}

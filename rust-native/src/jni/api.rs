// jni-macros 的 `native_method!` 展开内部使用已弃用的 `Atomic::fetch_update`，弃用
// 告警落在每个导出项的宏调用点上（本文件自身代码不产生弃用告警）；依赖侧修复前在此豁免。
#![allow(deprecated)]

use crate::encode::control::ControlStatus;
use crate::encode::hardware::{FLAT_STRIDE, VramSlotDescriptor};
use crate::encode::{self, VideoPipeline};
use crate::jni::callback;
use crate::jni::{elapsed_birth_nanos, get_global};
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
// 例外：`importVramSlots` / `clearVramSlots` 的零值恰是 `ControlStatus::Ok`，按零值
// 返回会把 panic 伪装成「已导入 / 已注销」，Java 侧据此销毁游戏侧导出资源即显存 UAF；
// 这两个出口沿用默认的 `ThrowRuntimeExAndDefault` 抛异常，由 Java 按失败走熔断。

const _: jni::NativeMethod = native_method! {
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_startEngine",
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
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_stopEngine",
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
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_negotiateOffer",
    error_policy = LogErrorAndDefault,
    static fn negotiate_offer(offer_json: JString, width: jint, height: jint, fps: jint, bpp: jfloat) -> JString,
};

fn negotiate_offer<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    offer_json: JString<'caller>,
    width: jint,
    height: jint,
    fps: jint,
    bpp: jfloat,
) -> jni::errors::Result<JString<'caller>> {
    if offer_json.is_null() {
        return Ok(JString::default());
    }
    let offer_str = match offer_json.try_to_string(env) {
        Ok(s) => s,
        Err(_) => return Ok(JString::default()),
    };
    let state = get_global();
    let is_running = state.is_running.load(Ordering::SeqCst);
    let engine = state.streaming_engine.load_full();
    let answer = state.runtime.block_on(handle_negotiate_offer(
        engine, is_running, &offer_str, width, height, fps, bpp,
    ));
    let Some(resp_str) = answer else {
        return Ok(JString::default());
    };
    match env.new_string(resp_str) {
        Ok(output) => Ok(output),
        Err(_) => {
            log::error!("negotiateOffer 创建 Java 字符串失败");
            Ok(JString::default())
        }
    }
}

/// negotiateOffer 线上请求的处理函数：存活守卫、offer 反序列化、几何透传与应答
/// 整形都在这里，出口只做 JString/Env 搬运，测试不经 Env 直接驱动本函数钉住线契约。
async fn handle_negotiate_offer(
    engine: Option<Arc<StreamingEngine>>,
    is_running: bool,
    offer_json: &str,
    width: i32,
    height: i32,
    fps: i32,
    bpp: f32,
) -> Option<String> {
    if !is_running {
        return None;
    }
    let Some(engine) = engine else {
        log::error!("StreamingEngine 未启动，无法协商 Offer");
        return None;
    };
    let offer: RTCSessionDescription = match serde_json::from_str(offer_json) {
        Ok(sdp) => sdp,
        Err(e) => {
            log::error!("无法反序列化 Offer SDP: {e}");
            return None;
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

    let (answer, profile, backend) = match engine.handle_offer(offer, w, h, f, initial_bpp).await {
        Ok(parts) => parts,
        Err(e) => {
            log::error!("WebRTC Offer 协商失败: {e}");
            return None;
        }
    };
    Some(
        serde_json::json!({
            "type": "answer",
            "width": profile.width(),
            "height": profile.height(),
            "fps": profile.fps(),
            "bpp": profile.bpp(),
            "bitrate": profile.bitrate(),
            "codec": profile.codec().as_str(),
            "backend": backend.as_str(),
            "bppMin": encode::MIN_BPP,
            "bppMax": encode::MAX_BPP,
            "sdp": answer,
        })
        .to_string(),
    )
}

const _: jni::NativeMethod = native_method! {
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_applyProfile",
    error_policy = LogErrorAndDefault,
    static fn apply_profile(
        width: jint,
        height: jint,
        fps: jint,
        bpp: jfloat,
        codec: JString,
    ) -> JString,
};

/// 阻塞应用新的画质档位（Java 后台 worker / 信令 executor 调用，渲染线程绝不调用）
///
/// codec 随请求携带：编码器与 SDP 载荷类型都锚在它上，Rust 侧不再保留档位快照，
/// Java 从 offer 应答里落账的值原样传回。应答只在**生效**时携带实际档位（
/// `width/height/fps/bpp/bitrate/codec`），被拒绝或无会话返回空串。
/// 返回空串，Java 按「无应答」保持自己的生效基线。
fn apply_profile<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    width: jint,
    height: jint,
    fps: jint,
    bpp: jfloat,
    codec: JString<'caller>,
) -> jni::errors::Result<JString<'caller>> {
    let state = get_global();
    let engine = state.streaming_engine.load_full();
    let codec_token = if codec.is_null() {
        None
    } else {
        codec.try_to_string(env).ok()
    };
    let answer = state.runtime.block_on(handle_apply_profile(
        engine,
        codec_token.as_deref(),
        width,
        height,
        fps,
        bpp,
    ));
    let Some(resp_str) = answer else {
        return Ok(JString::default());
    };
    match env.new_string(resp_str) {
        Ok(s) => Ok(s),
        Err(_) => Ok(JString::default()),
    }
}

/// applyProfile 线上请求的处理函数：codec 令牌解析、几何透传与应答整形都在这里，
/// 出口只做 JString/Env 搬运，测试不经 Env 直接驱动本函数钉住线契约。
/// 返回 `None` = 无应答，Java 按「无应答」保持自己的生效基线。
async fn handle_apply_profile(
    engine: Option<Arc<StreamingEngine>>,
    codec_token: Option<&str>,
    width: i32,
    height: i32,
    fps: i32,
    bpp: f32,
) -> Option<String> {
    let Some(engine) = engine else {
        log::warn!("applyProfile 时引擎未启动，拒绝档位请求");
        return None;
    };
    // codec 决定后端与会话已协商的载荷类型：解析不出就不猜，按拒绝处理
    let Some(token) = codec_token else {
        log::warn!("applyProfile 的 codec 缺失，拒绝档位请求");
        return None;
    };
    let Some(codec) = encode::VideoCodec::from_token(token) else {
        log::warn!("applyProfile 的 codec 无法识别，拒绝档位请求");
        return None;
    };

    // 几何透传同协商缝：0 由引擎按暂停信号契约拒绝，负数钳成 0 只为保证不回绕
    let outcome = engine
        .apply_profile(
            width.max(0) as usize,
            height.max(0) as usize,
            fps.max(0) as usize,
            bpp,
            codec,
        )
        .await;
    let (profile, backend) = match outcome {
        crate::streaming::ProfileApply::Applied(profile, backend) => (profile, backend),
        crate::streaming::ProfileApply::Rejected | crate::streaming::ProfileApply::NoSession => {
            return None;
        }
    };
    Some(
        serde_json::json!({
            "width": profile.width(),
            "height": profile.height(),
            "fps": profile.fps(),
            "bpp": profile.bpp(),
            "bitrate": profile.bitrate(),
            "codec": profile.codec().as_str(),
            // 后端未就绪的采纳态为 null，Java 按未知处理
            "backend": backend.map(|b| b.as_str()),
        })
        .to_string(),
    )
}

const _: jni::NativeMethod = native_method! {
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_addIceCandidate",
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
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_closeSession",
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
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_pollInputEvents",
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
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_queryEncoderCaps",
    error_policy = LogErrorAndDefault,
    static fn query_encoder_caps() -> JString,
};

fn query_encoder_caps<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
) -> jni::errors::Result<JString<'caller>> {
    // 纯读常驻能力快照，无副作用。Unknown（引擎未启动）与
    // Unavailable（探测过、无硬件）都是合法应答而非错误——status 命令在停服状态也
    // 会查询，由 Java 侧按 `unknown` / `software` 分别展示。
    let summary = encode::encoder_caps_summary();
    match env.new_string(summary) {
        Ok(s) => Ok(s),
        Err(_) => Ok(JString::default()),
    }
}

const _: jni::NativeMethod = native_method! {
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_importVramSlots",
    static fn import_vram_slots(flat: jlong[]) -> jint,
};

fn import_vram_slots<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    flat: JPrimitiveArray<'caller, jlong>,
) -> jni::errors::Result<jint> {
    // 0. 硬编能力预检：读启动期建立的快照，本机无硬编时直接拒绝
    if encode::hardware_caps().is_none() {
        return Ok(ControlStatus::NoHardwareEncoder as i32);
    }

    // 整批处理：Java 决定槽位数量，Native 只按协议切分收到的
    // 数组。长度不是每槽位 long 数的整数倍即整批拒绝，绝不按错位布局解析。
    let Ok(len) = flat.len(env) else {
        return Ok(ControlStatus::MalformedBatch as i32);
    };
    if len == 0 || !len.is_multiple_of(FLAT_STRIDE) {
        return Ok(ControlStatus::MalformedBatch as i32);
    }

    let mut buf = vec![0i64; len];
    if flat.get_region(env, 0, &mut buf).is_err() {
        return Ok(ControlStatus::MalformedBatch as i32);
    }

    let Some(slots) = VramSlotDescriptor::parse_batch(&buf) else {
        return Ok(ControlStatus::MalformedBatch as i32);
    };

    // 派发控制命令给 Actor 进行槽位导入与激活硬编
    let code = if let Some(engine) = get_global().streaming_engine.load().as_ref() {
        match engine.video().import_vram_slots(slots) {
            Ok(()) => ControlStatus::Ok.code(),
            Err(e) => e.code(),
        }
    } else {
        ControlStatus::ActorDead as i32
    };
    Ok(code)
}

const _: jni::NativeMethod = native_method! {
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_pushVramSlot",
    error_policy = LogErrorAndDefault,
    static fn push_vram_slot(slot_idx: jint, sync_value: jlong) -> void,
};

fn push_vram_slot<'caller>(
    _env: &mut Env<'caller>,
    _class: JClass<'caller>,
    slot_idx: jint,
    sync_value: jlong,
) -> jni::errors::Result<()> {
    let birth_nanos = elapsed_birth_nanos();

    // 零锁极速投递：直接构建 VramSlot payload 投递到帧收件箱，由编码 Actor 处理槽位映射
    if let Some(engine) = get_global().streaming_engine.load().as_ref() {
        engine
            .video()
            .push_vram_slot(birth_nanos, slot_idx as u32, sync_value as u64);
    }
    Ok(())
}

const _: jni::NativeMethod = native_method! {
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_slotStateBlock",
    static fn slot_state_block() -> JByteBuffer,
};

fn slot_state_block<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
) -> jni::errors::Result<JByteBuffer<'caller>> {
    // 槽位所有权块随当前活跃引擎走：块指针在引擎重建时换代（Box::leak 新块），
    // Java 每次构建显存管线时经本出口取一次，旧的缓冲视图随旧管线一并废弃。
    // 引擎未运行返回 null，调用方按「无所有权块」走构造期降级
    let engine = get_global().streaming_engine.load_full();
    let Some(engine) = engine.as_ref() else {
        return Ok(JByteBuffer::default());
    };
    let block = engine.video().slot_state();
    unsafe {
        // SAFETY: new_direct_byte_buffer 要求指针有效且容量范围内可读——
        // 块由 Box::leak 持有，进程内地址恒定；DirectBuffer 不拥有该内存，
        // 生命周期由泄漏语义保证长于任何 Java 侧视图
        env.new_direct_byte_buffer(
            block.as_ptr() as *mut u8,
            crate::encode::hardware::slots::SLOT_STATE_BLOCK_BYTES,
        )
    }
}

const _: jni::NativeMethod = native_method! {
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_clearVramSlots",
    static fn clear_vram_slots() -> jint,
};

fn clear_vram_slots<'caller>(
    _env: &mut Env<'caller>,
    _class: JClass<'caller>,
) -> jni::errors::Result<jint> {
    // 引擎未运行即无导入方：注销实际是空操作，Vacuous ACK 同样成立。
    // 这里返回非 OK 会让 Java 按「未获确认」保留导出资源，而编码器早已随引擎析构，
    // 保留只会白白丢掉三张导出纹理。
    let engine = get_global().streaming_engine.load();
    let Some(engine) = engine.as_ref() else {
        return Ok(ControlStatus::Ok.code());
    };
    let pipeline = engine.video();

    // 清空待编帧、排空 GPU 在途任务并注销显存槽位：ClearVramSlots 单命令完成
    // 「停写者 → 排空异步尾巴 → 释放导入」的固定次序，一次控制往返
    // 注意：不回退软编！硬件编码器仍保持就绪，可无缝接收随后的主机帧池块 (RAM DMA 双缓冲) 并用 StagingBuffer 进行硬编
    let code = match pipeline.clear_vram_slots() {
        Ok(()) => ControlStatus::Ok.code(),
        Err(e) => e.code(),
    };
    if code != ControlStatus::Ok as i32 {
        // 未获确认：编码器可能仍持有导出资源的跨 API 导入，Java 侧不得销毁，
        // 结果码必须原样上报而不是装作成功
        log::warn!("显存槽位注销未获 Actor 确认 (code={code})，游戏侧导出资源暂缓销毁");
    }
    Ok(code)
}

const _: jni::NativeMethod = native_method! {
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_pushSystemFrame",
    error_policy = LogErrorAndDefault,
    static fn push_system_frame(buffer: JByteBuffer) -> void,
};

fn push_system_frame<'caller>(
    env: &mut Env<'caller>,
    _class: JClass<'caller>,
    buffer: JByteBuffer<'caller>,
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
    // SAFETY: ptr 来自 get_direct_buffer_address 的成功返回，长度取 DirectBuffer
    // 的有效容量 cap；切片仅在本 JNI 调用栈内被同步拷贝消费，不会逃逸到 Buffer 失效之后
    let slice = unsafe { std::slice::from_raw_parts(ptr, cap) };
    // 帧池与收件箱都属于管线：引擎未启动时连拷贝都不必做
    let Some(engine) = state.streaming_engine.load_full() else {
        return Ok(());
    };
    engine.video().push_host_frame(birth_nanos, slice);
    Ok(())
}

const _: jni::NativeMethod = native_method! {
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_setAudioBitrate",
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
    java_type = "com.kairo.vistrel.streaming.natives.NativeBridge",
    export = "Java_com_kairo_vistrel_streaming_natives_NativeBridge_pushAudioPcm",
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
            let answer = handle_negotiate_offer(
                Some(Arc::clone(&engine)),
                true,
                &offer_wire_json(),
                geometry.0,
                geometry.1,
                geometry.2,
                1.0,
            )
            .await;
            assert!(
                answer.is_none(),
                "几何 {geometry:?} 必须被拒绝而非修正成 1×1@1"
            );
        }
    }

    /// 关停开始后拒新协商，与引擎是否仍在位无关
    #[tokio::test]
    async fn test_negotiate_refuses_while_shutting_down() {
        let (engine, _rx) = make_test_engine(false);
        let answer =
            handle_negotiate_offer(Some(engine), false, &offer_wire_json(), 1920, 1080, 60, 1.0)
                .await;
        assert!(answer.is_none(), "关停期协商必须无应答");
    }

    /// 引擎未启动 = 无应答，不是 panic 也不是错误应答
    #[tokio::test]
    async fn test_negotiate_without_engine_returns_no_answer() {
        let answer =
            handle_negotiate_offer(None, true, &offer_wire_json(), 1920, 1080, 60, 1.0).await;
        assert!(answer.is_none(), "无引擎协商必须无应答");
    }

    #[tokio::test]
    async fn test_apply_profile_without_engine_returns_no_answer() {
        let answer = handle_apply_profile(None, Some("h264"), 1600, 900, 90, 0.2).await;
        assert!(answer.is_none(), "无引擎档位请求必须无应答");
    }

    /// codec 令牌解析不出就不猜：缺失与无法识别同为拒绝
    #[tokio::test]
    async fn test_apply_profile_rejects_unresolvable_codec() {
        let (engine, _rx) = make_test_engine(false);
        for token in [None, Some("wmv")] {
            let answer =
                handle_apply_profile(Some(Arc::clone(&engine)), token, 1600, 900, 90, 0.2).await;
            assert!(answer.is_none(), "codec {token:?} 必须被拒绝");
        }
    }

    /// 全线往返：协商建会话 → 档位生效 → 0 几何档位被拒。应答 JSON 的键集是 Java
    /// 解析器锚定的线契约，多键少键都算漂移
    #[tokio::test]
    async fn test_negotiate_then_apply_roundtrip_pin_line_contract() {
        let (engine, _rx) = make_test_engine(false);

        let answer = handle_negotiate_offer(
            Some(Arc::clone(&engine)),
            true,
            &offer_wire_json(),
            1920,
            1080,
            60,
            1.0,
        )
        .await;
        let Some(answer_json) = answer else {
            panic!("假后端上的合法协商必须得到应答");
        };
        let answer: serde_json::Value = serde_json::from_str(&answer_json).unwrap();
        assert_eq!(answer["type"], "answer");
        assert_eq!(answer["width"], 1920);
        assert_eq!(answer["height"], 1080);
        assert_eq!(answer["fps"], 60);
        // sdp 原样透传给浏览器，保持 W3C 形态（嵌套对象携带 type 与 SDP 文本）
        assert_eq!(answer["sdp"]["type"], "answer");
        assert!(!answer["sdp"]["sdp"].as_str().unwrap_or_default().is_empty());
        for key in [
            "width", "height", "fps", "bpp", "bitrate", "codec", "backend", "bppMin", "bppMax",
            "sdp",
        ] {
            assert!(answer.get(key).is_some(), "offer 应答缺 {key} 键");
        }

        let applied =
            handle_apply_profile(Some(Arc::clone(&engine)), Some("h264"), 1600, 900, 90, 0.2).await;
        let Some(applied_json) = applied else {
            panic!("活跃会话上的合法档位必须生效");
        };
        let applied: serde_json::Value = serde_json::from_str(&applied_json).unwrap();
        assert_eq!(applied["width"], 1600);
        // 请求令牌大小写不敏感地解析，应答回规范拼写
        assert_eq!(applied["codec"], "H264");
        for key in [
            "width", "height", "fps", "bpp", "bitrate", "codec", "backend",
        ] {
            assert!(applied.get(key).is_some(), "档位应答缺 {key} 键");
        }

        assert!(
            handle_apply_profile(Some(Arc::clone(&engine)), Some("h264"), 0, 0, 0, 0.2)
                .await
                .is_none(),
            "0 几何档位必须按暂停信号契约被拒"
        );
    }
}

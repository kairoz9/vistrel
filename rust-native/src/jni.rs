//! JNI 互操作层 (Java Native Interface Integration)
//!
//! 统一汇聚 Java `NativeBridge` 与底层 Rust 原生引擎的双向调用与生命周期管理：
//! - `api`: Java -> Rust 的 C-ABI 导出函数（以 `Java_com_kairo_vistrel_streaming_natives_NativeBridge_` 命名）
//! - `callback`: Rust -> Java 的跨线程事件与状态反向推送

pub mod api;
pub mod callback;
use crate::streaming::StreamingEngine;
use arc_swap::ArcSwapOption;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::runtime::Runtime;

/// 首次投递的惰性锚点，全部出生纳秒相对它计量（单调，进程级不回拨）
static STREAMING_EPOCH: OnceLock<Instant> = OnceLock::new();

/// 计算相对 `STREAMING_EPOCH` 的单调时钟纳秒
pub fn elapsed_birth_nanos() -> u64 {
    let now = Instant::now();
    let epoch = STREAMING_EPOCH.get_or_init(|| now);
    now.duration_since(*epoch).as_nanos() as u64
}

/// 进程级全局状态：tokio 运行时、当前串流引擎槽位与运行中标志
///
/// 经 [`get_global`] 以 `&'static` 获取，全进程单例；引擎槽位以
/// [`ArcSwapOption`] 原子替换，读侧无锁、无数据撕裂
pub struct GlobalState {
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) streaming_engine: ArcSwapOption<StreamingEngine>,
    pub(crate) is_running: AtomicBool,
}

static GLOBAL: OnceLock<GlobalState> = OnceLock::new();

/// 取全局单例，首次调用时惰性初始化
///
/// 名称掩盖了首调副作用：装配全局日志器（`RUST_LOG` 可覆盖，默认 info；桥就绪前
/// stdout 兜底、就绪后转 JNI→log4j）并构建多线程 tokio 运行时（构建失败即 panic）。
/// 返回 `&'static`，此后调用零开销且无副作用；任意线程可调，OnceLock 保证仅首个
/// 到达线程执行初始化闭包
pub fn get_global() -> &'static GlobalState {
    GLOBAL.get_or_init(|| {
        // 默认 info；RUST_LOG 可覆盖。
        // 日志通道：stdout 仅作桥未就绪时的兜底（引擎启动前的极少量日志）；
        // 桥就绪后只走 JNI→log4j
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .format(|buf, record| {
                use std::io::Write as _;
                if !callback::bridge_ready() {
                    writeln!(
                        buf,
                        "{} {:<5} [{}] {}",
                        buf.timestamp(),
                        record.level(),
                        record.target(),
                        record.args()
                    )?;
                }
                // 消息带 target 前缀进 log4j：latest.log 里可按子系统检索
                callback::dispatch_native_log(
                    record.level(),
                    format!("[{}] {}", record.target(), record.args()),
                );
                Ok(())
            })
            .try_init();

        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .thread_name("vistrel-tokio")
                .enable_all()
                .build()
                .expect("Failed to initialize Tokio runtime"),
        );

        GlobalState {
            runtime,
            streaming_engine: ArcSwapOption::from(None),
            is_running: AtomicBool::new(false),
        }
    })
}

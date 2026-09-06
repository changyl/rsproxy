// 日志初始化:双通道输出(滚动文件 + stderr)
//
// 目标:让日志真正可排查 —— 配置里的 log_dir/log_level 不再是摆设。
//
// 通道:
//   - 文件:  <log_dir>/newproxy.YYYY-MM-DD.log,每日滚动,最多保留 MAX_LOG_FILES 个
//   - 终端:  stderr
//
// 级别语义:
//   - RUST_LOG 显式设置(非空)→ 两个通道都遵循 RUST_LOG(尊重用户意图)
//   - RUST_LOG 未设置     → 终端默认 info;文件遵循配置 log_level(默认 info)
//
// 返回的 LogGuard 必须存活到进程退出(non-blocking writer 的 worker 线程句柄),
// 否则日志会丢。调用方(如 main)把它作为局部变量持有到进程结束即可。

use std::io;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::Layer as _;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry;
use tracing_subscriber::EnvFilter;

use crate::config::LogLevel;

/// 文件日志保留的最大数量(每日滚动,超出自动清理最旧文件)
const MAX_LOG_FILES: usize = 30;
/// 日志文件前缀(文件名形如 newproxy.2026-07-31.log)
const LOG_FILE_PREFIX: &str = "newproxy";

/// 日志初始化结果:持有 non-blocking writer 的 guard(进程存活期间不得 drop)
pub struct LogGuard {
    _guard: WorkerGuard,
    /// 日志输出目录
    pub log_dir: String,
}

impl LogGuard {
    /// 日志文件通配名(供展示/排查,如 "logs/newproxy.*.log")
    pub fn log_file_glob(&self) -> String {
        format!("{}/{}.*.log", self.log_dir, LOG_FILE_PREFIX)
    }
}

/// 初始化日志系统。失败仅在日志目录不可创建/不可写时发生。
pub fn init(log_dir: &str, log_level: LogLevel) -> io::Result<LogGuard> {
    // 目录不存在则创建(嵌套目录一并创建)
    std::fs::create_dir_all(log_dir)?;

    // RUST_LOG 显式设置(非空)→ 双通道都遵循;否则用各自默认级别
    let rust_log = std::env::var("RUST_LOG")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let make_filter = |fallback: &str| -> EnvFilter {
        match &rust_log {
            Some(v) => EnvFilter::try_new(v).unwrap_or_else(|e| {
                eprintln!("warning: 无效的 RUST_LOG `{v}` ({e}),回退到 `{fallback}`");
                EnvFilter::new(fallback)
            }),
            None => EnvFilter::new(fallback),
        }
    };

    // ─── 文件通道:每日滚动 + 非阻塞 writer ───
    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(log_dir)
        .map_err(|e| io::Error::other(format!("rolling appender: {e}")))?;
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false) // 文件里不要颜色转义序列
        .with_target(true)
        .with_filter(make_filter(log_level.as_str()));

    // ─── 终端通道:stderr ───
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .with_ansi(true)
        .with_target(true)
        .with_filter(make_filter("info"));

    registry()
        .with(file_layer)
        .with(stderr_layer)
        .init();

    Ok(LogGuard {
        _guard: guard,
        log_dir: log_dir.to_string(),
    })
}

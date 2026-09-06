use std::process::exit;
use std::sync::Arc;

use tracing::{error, info};

use newproxy::app::AppCtx;
use newproxy::config;
use newproxy::conn::front;
use newproxy::logging;
use newproxy::pool::backend::{self, SrvPool};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ─── 命令行帮助 / 版本 ───

fn print_help() {
    println!("newproxy v{VERSION}");
    println!("MySQL 分库分表代理 — Rust 原生实现");
    println!();
    println!("USAGE:");
    println!("    newproxy [OPTIONS] -c <CONFIG>");
    println!("    newproxy [OPTIONS] <CONFIG>");
    println!();
    println!("ARGS:");
    println!("    <CONFIG>    配置文件路径(INI 格式),必填。");
    println!("                可用位置参数或 -c/--config 提供,二选一。");
    println!();
    println!("OPTIONS:");
    println!("    -c, --config <PATH>    指定配置文件路径(与位置参数二选一)");
    println!("    -h, --help             打印帮助信息并退出");
    println!("    -V, --version          打印版本号并退出");
    println!();
    println!("示例:");
    println!("    newproxy conf/newproxy.conf          # 位置参数指定配置文件");
    println!("    newproxy -c conf/newproxy-test.conf  # 等价的显式写法");
}

fn print_version() {
    println!("newproxy {VERSION}");
}

// ─── 参数解析(零依赖手写:支持 -h/--help、-V/--version、-c/--config、位置参数)───

#[derive(Debug)]
enum ParseOutcome {
    Run { config_path: String },
    Help,
    Version,
    Error(String),
}

fn parse_args() -> ParseOutcome {
    parse_args_from(std::env::args().skip(1))
}

/// 参数解析(纯函数,便于单元测试全分支):输入不含程序名的参数序列
fn parse_args_from<I: Iterator<Item = String>>(args: I) -> ParseOutcome {
    let mut args = args;
    let mut config_path: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return ParseOutcome::Help,
            "-V" | "--version" => return ParseOutcome::Version,
            "-c" | "--config" => {
                if config_path.is_some() {
                    return ParseOutcome::Error(format!(
                        "配置文件已指定为 `{}`,不可重复提供",
                        config_path.as_deref().unwrap_or("")
                    ));
                }
                match args.next() {
                    Some(p) => config_path = Some(p),
                    None => {
                        return ParseOutcome::Error(format!(
                            "`{arg}` 需要一个参数(配置文件路径)"
                        ))
                    }
                }
            }
            s if s.starts_with("--config=") => {
                if config_path.is_some() {
                    return ParseOutcome::Error(format!(
                        "配置文件已指定为 `{}`,不可重复提供",
                        config_path.as_deref().unwrap_or("")
                    ));
                }
                config_path = Some(s["--config=".len()..].to_string());
            }
            // 未知短/长选项:报错并提示 --help(单独的 `-` 视为位置参数)
            s if s.starts_with('-') && s != "-" => {
                return ParseOutcome::Error(format!(
                    "未知参数: `{s}`\n用 --help 查看可用选项"
                ))
            }
            _ => {
                if config_path.is_some() {
                    return ParseOutcome::Error(format!(
                        "配置文件已指定为 `{}`,不可重复提供(`-c` 与位置参数二选一)",
                        config_path.as_deref().unwrap_or("")
                    ));
                }
                config_path = Some(arg);
            }
        }
    }

    match config_path {
        Some(path) => ParseOutcome::Run { config_path: path },
        None => ParseOutcome::Error("未指定配置文件:用 -c <PATH> 或直接给出配置文件路径".into()),
    }
}

fn main() -> anyhow::Result<()> {
    // 先解析参数:--help/--version/错误都不应触发日志初始化,直接处理后退出
    match parse_args() {
        ParseOutcome::Help => {
            print_help();
            exit(0);
        }
        ParseOutcome::Version => {
            print_version();
            exit(0);
        }
        ParseOutcome::Error(msg) => {
            eprintln!("error: {msg}");
            eprintln!("用 --help 查看用法");
            exit(2);
        }
        ParseOutcome::Run { config_path } => {
            // tokio worker 线程数取配置 `max_threads`(默认 4,至少 1)。
            // 原 `#[tokio::main]` 用 CPU 核数作 worker,容器内核数往往小于
            // 配置期望值,高并发下代理吞吐受限;此处显式按配置构建运行时。
            let threads = crate::config::load_config(&config_path)
                .map(|c| c.max_threads.max(1))
                .unwrap_or(4);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(threads)
                .enable_all()
                .build()?;
            rt.block_on(run(config_path))
        }
    }
}

async fn run(config_path: String) -> anyhow::Result<()> {
    // 配置文件必须存在且可解析,否则禁止启动。
    // 一个无配置的分库分表代理会绑定端口却无法路由任何查询,
    // 静默回退到内置默认值是危险行为,故此处直接报错退出。
    if !std::path::Path::new(&config_path).exists() {
        eprintln!("error: 配置文件不存在: {config_path}");
        eprintln!("用 --help 查看用法");
        exit(2);
    }

    // 先加载配置:日志系统需要 log_dir / log_level 才能初始化
    let config = config::load_config(&config_path)?;

    // 双通道日志:滚动文件(log_dir/newproxy.*.log) + stderr。
    // guard 必须存活到进程退出,否则文件日志会丢——run() 的 accept 循环
    // 永不返回,局部变量 _log 因此天然存活整个进程生命周期。
    let _log = logging::init(&config.log_dir, config.log_level)?;
    info!("newproxy v{VERSION}");
    info!("loading config from {config_path}");
    info!("log file: {}", _log.log_file_glob());

    let port = config.port;
    let mng_port = config.mng_port;
    let reload_interval = config.reload_interval_secs;
    let config_center_cfg = config.config_center.clone();
    let srv_pool = Arc::new(SrvPool::new());

    // 空闲回收器
    let reaper_pool = srv_pool.clone();
    tokio::spawn(async move {
        backend::idle_reaper(reaper_pool, 60).await;
    });

    let ctx = Arc::new(AppCtx::new(config, srv_pool, config_path.clone()));

    // ─── 配置中心(拓扑热更新):etcd / zookeeper,按分片增量生效 ───
    if config_center_cfg.kind != "none" {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            match newproxy::config_center::build_store(&config_center_cfg).await {
                Ok(store) => {
                    info!(
                        "config center connected: {} ({})",
                        store.name(),
                        config_center_cfg.endpoints.join(",")
                    );
                    let ctx_pool = ctx.clone();
                    let on_applied: newproxy::config_center::ChangeCallback =
                        std::sync::Arc::new(move |change| {
                            let (cid, tid) = change.shard_key();
                            ctx_pool.srv_pool.invalidate_shard(&cid, &tid);
                        });
                    newproxy::config_center::run_watcher(
                        store.as_ref(),
                        ctx.topology.clone(),
                        on_applied,
                    )
                    .await;
                }
                Err(e) => error!("config center connect failed: {e}"),
            }
        });
    }

    // ─── 热加载触发 1: SIGHUP 信号(kill -HUP <pid>) ───
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mut sig =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("SIGHUP handler init failed: {e}");
                        return;
                    }
                };
            loop {
                sig.recv().await;
                info!("SIGHUP received, reloading config");
                match ctx.reload_config() {
                    Ok(diff) => info!("config reloaded: {diff}"),
                    Err(e) => error!("config reload failed: {e}"),
                }
            }
        });
    }

    // ─── 热加载触发 2: 配置文件变更自动热加载(mtime 轮询;reload_interval=0 关闭) ───
    if reload_interval > 0 {
        let ctx = ctx.clone();
        let path = config_path.clone();
        tokio::spawn(async move {
            let mut last_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(reload_interval));
            // 跳过首个立即触发的 tick(启动时配置刚加载过)
            interval.tick().await;
            loop {
                interval.tick().await;
                let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                if mtime.is_some() && mtime != last_mtime {
                    last_mtime = mtime;
                    info!("config file changed, auto-reloading");
                    match ctx.reload_config() {
                        Ok(diff) => info!("config auto-reloaded: {diff}"),
                        Err(e) => error!("config auto-reload failed: {e}"),
                    }
                }
            }
        });
    }

    // 管理 HTTP 服务(Web 面板 / JSON API / Prometheus / 健康检查,Basic Auth):
    // 独立 task,失败不影响业务;mng_port=0 时不启动。
    if mng_port > 0 {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = newproxy::mgmt::http::serve(ctx, mng_port).await {
                error!("mgmt http server failed: {e}");
            }
        });
        info!("mgmt http starting on 0.0.0.0:{mng_port}");
    }

    // 大 backlog:快速建连/断开 churn(连接池客户端、健康检查)下,
    // 默认 SOMAXCONN 会溢出导致 ECONNREFUSED——实测 8 并发高频重连
    // 出现数千次客户端错误,提升后消失。
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    let addr: std::net::SocketAddr = (std::net::Ipv4Addr::UNSPECIFIED, port).into();
    socket.bind(addr)?;
    let listener = socket.listen(4096)?;
    info!("listening on 0.0.0.0:{port}");

    // SIGTERM/SIGINT 优雅退出:停止接受新连接并正常返回。
    // 1) 进程正常退出时 LLVM 覆盖率 profraw 才落盘(cov e2e 依赖);
    // 2) systemd/docker stop 能干净收尾,不再依赖默认信号终止。
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("SIGTERM received, shutting down");
                return Ok(());
            }
            _ = sigint.recv() => {
                info!("SIGINT received, shutting down");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, peer_addr) = match accepted {
                    Ok(conn) => conn,
                    Err(e) => {
                        error!("accept error: {e}");
                        continue;
                    }
                };
                info!("new connection from {peer_addr}");
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    front::conn_task(stream, peer_addr, ctx).await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> ParseOutcome {
        parse_args_from(args.iter().map(|s| s.to_string()))
    }

    fn err_msg(o: ParseOutcome) -> String {
        match o {
            ParseOutcome::Error(m) => m,
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn parse_help_version() {
        assert!(matches!(run(&["-h"]), ParseOutcome::Help));
        assert!(matches!(run(&["--help"]), ParseOutcome::Help));
        assert!(matches!(run(&["-V"]), ParseOutcome::Version));
        assert!(matches!(run(&["--version"]), ParseOutcome::Version));
    }

    #[test]
    fn parse_config_forms() {
        match run(&["-c", "/tmp/a.conf"]) {
            ParseOutcome::Run { config_path } => assert_eq!(config_path, "/tmp/a.conf"),
            o => panic!("got {o:?}"),
        }
        match run(&["--config", "/tmp/b.conf"]) {
            ParseOutcome::Run { config_path } => assert_eq!(config_path, "/tmp/b.conf"),
            o => panic!("got {o:?}"),
        }
        match run(&["--config=/tmp/c.conf"]) {
            ParseOutcome::Run { config_path } => assert_eq!(config_path, "/tmp/c.conf"),
            o => panic!("got {o:?}"),
        }
        // 位置参数
        match run(&["/tmp/pos.conf"]) {
            ParseOutcome::Run { config_path } => assert_eq!(config_path, "/tmp/pos.conf"),
            o => panic!("got {o:?}"),
        }
    }

    #[test]
    fn parse_config_errors() {
        // 未指定配置
        assert!(err_msg(run(&[])).contains("未指定配置文件"));
        // -c 缺值
        assert!(err_msg(run(&["-c"])).contains("需要一个参数"));
        // 重复指定
        assert!(err_msg(run(&["-c", "a", "-c", "b"])).contains("不可重复"));
        assert!(err_msg(run(&["-c", "a", "--config=b"])).contains("不可重复"));
        assert!(err_msg(run(&["-c", "a", "/tmp/pos.conf"])).contains("不可重复"));
        assert!(err_msg(run(&["--config=a", "/tmp/pos.conf"])).contains("不可重复"));
        // 未知选项
        assert!(err_msg(run(&["-x"])).contains("未知参数"));
        assert!(err_msg(run(&["--bogus"])).contains("未知参数"));
        // 单独的 `-` 视为位置参数
        match run(&["-"]) {
            ParseOutcome::Run { config_path } => assert_eq!(config_path, "-"),
            o => panic!("got {o:?}"),
        }
    }
}

//! 内置性能基准:in-process proxy + mock backend,不依赖 Docker/MySQL。
//!
//! 目的:把「代理层开销」从「MySQL 服务端性能」中隔离出来,回答两个问题:
//!   1. 代理相对直连慢多少(proxy overhead)
//!   2. 瓶颈在前端解析、转发、还是协议细节(按场景/并发拆分)
//!
//! 架构(全部在进程内):
//!   [virtual client] ×N ──TCP──▶ [newproxy proxy] ──TCP──▶ [mock backend]
//!
//! 用法:
//!   cargo test --release --test perf_proxy -- --ignored --nocapture
//!
//! 可选环境变量:
//!   PERF_SECS  每个场景时长(默认 3s)
//!   PERF_CONNS 并发列表,空格分隔(默认 "1 4 16 64")
//!
//! 注意:必须用 --release 跑才有意义(debug 下 tokio 与分配开销失真)。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Buf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

use newproxy::app::AppCtx;
use newproxy::config::model::{
    AppConfig, Cluster, ClusterTablet, Database, DatabaseGroup, DbUser, LogLevel, ProductUser,
};
use newproxy::conn::front;
use newproxy::pool::backend::SrvPool;
use newproxy::proto::error::{build_ok, OK_HEADER};
use newproxy::proto::handshake::scramble_native;

// ─── 常量 ───

const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const SCRAMBLE_LEN: usize = 20;
const PROD_USER: &str = "bench";
const PROD_PASS: &str = "benchpass";

/// 静态 scramble(代理不校验 scramble 随机性,固定值即可)
const SCRAMBLE: [u8; SCRAMBLE_LEN] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14,
];

/// 旧式 5B EOF(warnings=0, status=AUTOCOMMIT)
const OLD_EOF: [u8; 5] = [0xFE, 0x00, 0x00, 0x02, 0x00];

// ─── 帧工具(仅用于 bench 端,2 次 read_exact 足够精确)───

async fn read_packet(s: &mut TcpStream, out: &mut Vec<u8>) -> std::io::Result<()> {
    let mut hdr = [0u8; 4];
    s.read_exact(&mut hdr).await?;
    let len = (hdr[0] as usize) | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
    out.clear();
    out.resize(len, 0);
    s.read_exact(out).await?;
    Ok(())
}

async fn send_packet(s: &mut TcpStream, seq: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(payload.len() + 4);
    buf.push((payload.len() & 0xFF) as u8);
    buf.push(((payload.len() >> 8) & 0xFF) as u8);
    buf.push(((payload.len() >> 16) & 0xFF) as u8);
    buf.push(seq);
    buf.extend_from_slice(payload);
    s.write_all(&buf).await
}

/// 拼接一个完整 wire packet(含 header)
fn wire(seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 4);
    v.push((payload.len() & 0xFF) as u8);
    v.push(((payload.len() >> 8) & 0xFF) as u8);
    v.push(((payload.len() >> 16) & 0xFF) as u8);
    v.push(seq);
    v.extend_from_slice(payload);
    v
}

// ─── Mock Backend:零思考速度的 MySQL 服务端 ───

struct MockBackend {
    port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl MockBackend {
    async fn start() -> std::io::Result<Self> {
        let lsock = TcpSocket::new_v4()?;
        lsock.set_reuseaddr(true)?;
        let addr: std::net::SocketAddr = (std::net::Ipv4Addr::LOCALHOST, 0).into();
        lsock.bind(addr)?;
        let listener = lsock.listen(4096)?;
        let port = listener.local_addr()?.port();

        // 预构建响应字节流(含 header,一次性 write,消除后端开销)
        let greeting = wire(0, &build_greeting()); // seq=0,server greeting 必须带 4B header
        let ok = wire(2, &build_ok(0, 0, 0x0002, 0, None));
        let select1 = build_resultset(1, 1, b"1");
        let rows100 = build_resultset(100, 1, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
        let prepare_ok = build_prepare_response(1, 1, 1);

        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    // 与 proxy accept 循环同理:偶发 accept 错误不能终止监听
                    Err(_) => continue,
                };
                let _ = sock.set_nodelay(true);
                let greeting = greeting.clone();
                let ok = ok.clone();
                let select1 = select1.clone();
                let rows100 = rows100.clone();
                let prepare_ok = prepare_ok.clone();
                tokio::spawn(async move {
                    let _ = sock.write_all(&greeting).await;
                    let mut pkt = Vec::with_capacity(4096);
                    // 读 auth(seq=1)
                    if read_packet(&mut sock, &mut pkt).await.is_err() {
                        return;
                    }
                    let _ = sock.write_all(&ok).await;
                    // 命令阶段
                    loop {
                        pkt.clear();
                        if read_packet(&mut sock, &mut pkt).await.is_err() {
                            return;
                        }
                        let cmd = pkt.first().copied().unwrap_or(0xFF);
                        let resp: &[u8] = match cmd {
                            0x02 => &ok, // COM_INIT_DB
                            0x03 => {
                                // COM_QUERY:按 SQL 文本选择响应
                                let sql = String::from_utf8_lossy(&pkt[1..]);
                                if sql.starts_with("SELECT bench_100_rows") {
                                    &rows100
                                } else {
                                    &select1
                                }
                            }
                            0x16 => &prepare_ok, // COM_STMT_PREPARE
                            0x17 => &select1,    // COM_STMT_EXECUTE
                            0x1A => &ok,         // COM_STMT_RESET
                            0x0E => &ok,         // COM_PING
                            0x19 | 0x18 => continue, // CLOSE / SEND_LONG_DATA:无响应
                            _ => &ok,
                        };
                        if sock.write_all(resp).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        Ok(Self { port, handle })
    }

    async fn stop(self) {
        self.handle.abort();
    }
}

/// 构造后端 Server Greeting(协议 v10,mysql_native_password)
fn build_greeting() -> Vec<u8> {
    let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | 0x0008_0000; // + PLUGIN_AUTH
    let mut v = Vec::with_capacity(64);
    v.push(10);
    v.extend_from_slice(b"5.7.40-bench-mock\0");
    v.extend_from_slice(&1u32.to_le_bytes()); // connection_id
    v.extend_from_slice(&SCRAMBLE[..8]); // part 1
    v.push(0); // filler
    v.extend_from_slice(&(caps as u16).to_le_bytes()); // cap_lower
    v.push(33); // charset
    v.extend_from_slice(&2u16.to_le_bytes()); // status
    v.extend_from_slice(&((caps >> 16) as u16).to_le_bytes()); // cap_upper
    v.push((8 + 12 + 1) as u8); // auth_plugin_data_len
    v.extend_from_slice(&[0u8; 10]); // reserved
    v.extend_from_slice(&SCRAMBLE[8..]); // part 2
    v.push(0); // NUL
    v.extend_from_slice(b"mysql_native_password\0");
    v
}

/// 构造 COM_QUERY 结果集响应:colcount + nc×coldef + EOF + nrows×row + EOF
fn build_resultset(rows: usize, cols: usize, cell: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(rows * cols * 16 + 128);
    let mut seq: u8 = 1;
    // column_count
    v.extend_from_slice(&wire(seq, &[cols as u8]));
    seq = seq.wrapping_add(1);
    // coldefs
    for i in 0..cols {
        let mut cd = vec![0x03u8];
        cd.extend_from_slice(b"def"); // catalog
        cd.push(0x00); // schema
        cd.push(0x00); // table
        cd.push(0x00); // org_table
        cd.push(1);
        cd.push(b'c' + i as u8); // name
        cd.push(0x00); // org_name
        cd.push(0x0C); // fixed len
        cd.extend_from_slice(&33u16.to_le_bytes()); // charset utf8
        cd.extend_from_slice(&0u32.to_le_bytes()); // length
        cd.push(0xFD); // VAR_STRING
        cd.extend_from_slice(&[0x00, 0x00]); // flags
        cd.push(0x00); // decimals
        cd.extend_from_slice(&[0x00, 0x00]); // filler
        v.extend_from_slice(&wire(seq, &cd));
        seq = seq.wrapping_add(1);
    }
    v.extend_from_slice(&wire(seq, &OLD_EOF));
    seq = seq.wrapping_add(1);
    // rows
    for _ in 0..rows {
        let mut row = Vec::with_capacity(cell.len() + 1);
        row.push(cell.len() as u8);
        row.extend_from_slice(cell);
        v.extend_from_slice(&wire(seq, &row));
        seq = seq.wrapping_add(1);
    }
    v.extend_from_slice(&wire(seq, &OLD_EOF));
    v
}

/// COM_STMT_PREPARE 响应:PREPARE_OK + np×pdef + EOF + nc×coldef + EOF
fn build_prepare_response(stmt_id: u32, nc: u16, np: u16) -> Vec<u8> {
    let mut ok = vec![0x00u8];
    ok.extend_from_slice(&stmt_id.to_le_bytes());
    ok.extend_from_slice(&nc.to_le_bytes());
    ok.extend_from_slice(&np.to_le_bytes());
    ok.push(0x00); // reserved
    ok.extend_from_slice(&0u16.to_le_bytes()); // warnings
    let pdef = [0x03u8, b'?', 0x00]; // 简化 ColumnDef
    let cdef = [0x03u8, b'1', 0x00];
    let mut v = Vec::with_capacity(64);
    v.extend_from_slice(&wire(1, &ok));
    for _ in 0..np {
        v.extend_from_slice(&wire(2, &pdef));
    }
    v.extend_from_slice(&wire(2, &OLD_EOF));
    for _ in 0..nc {
        v.extend_from_slice(&wire(3, &cdef));
    }
    v.extend_from_slice(&wire(3, &OLD_EOF));
    v
}

// ─── 虚拟客户端 ───

struct BenchClient {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl BenchClient {
    /// 连接 + 完整前端握手(mysql_native_password)
    async fn connect(port: u16) -> std::io::Result<Self> {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await?;
        sock.set_nodelay(true)?;
        // 压测客户端:drop 时用 RST 关闭(linger=0)跳过 TIME_WAIT。
        // 否则高频 connect/断开 churn 会把 macOS 仅 16384 个临时端口耗尽
        // (EADDRNOTAVAIL, os error 49)——实测 8 并发即可触发,且负载越低
        // (connect 越快)越严重;直连路径同样受影响,非代理缺陷。
        let _ = sock.set_linger(Some(Duration::ZERO));
        let mut buf = Vec::with_capacity(4096);
        read_packet(&mut sock, &mut buf).await?; // greeting
        let scramble = parse_greeting_scramble(&buf);
        let auth_resp = scramble_native(PROD_PASS.as_bytes(), &scramble);

        let mut auth = Vec::with_capacity(64);
        auth.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION).to_le_bytes());
        auth.extend_from_slice(&(16 * 1024 * 1024u32).to_le_bytes()); // max_packet
        auth.push(33); // charset
        auth.extend_from_slice(&[0u8; 23]); // reserved
        auth.extend_from_slice(PROD_USER.as_bytes());
        auth.push(0);
        auth.push(SCRAMBLE_LEN as u8);
        auth.extend_from_slice(&auth_resp);
        send_packet(&mut sock, 1, &auth).await?;

        buf.clear();
        read_packet(&mut sock, &mut buf).await?;
        if buf.first() != Some(&OK_HEADER) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("auth failed: {:02x?}", &buf[..buf.len().min(16)]),
            ));
        }
        Ok(Self { sock, buf })
    }

    /// 执行一个命令并读到终止包。返回 (包数, 是否 ERR)
    async fn roundtrip(&mut self, cmd: u8, payload: &[u8]) -> std::io::Result<(usize, bool)> {
        let mut pkt = Vec::with_capacity(payload.len() + 1);
        pkt.push(cmd);
        pkt.extend_from_slice(payload);
        send_packet(&mut self.sock, 0, &pkt).await?;

        let mut packets = 0usize;
        let mut err = false;
        loop {
            self.buf.clear();
            read_packet(&mut self.sock, &mut self.buf).await?;
            packets += 1;
            let b0 = self.buf.first().copied().unwrap_or(0);
            if b0 == 0xFF {
                err = true;
                return Ok((packets, err));
            }
            if b0 == OK_HEADER || (b0 == 0xFE && self.buf.len() < 7) {
                return Ok((packets, err)); // OK 或旧式 EOF 终止
            }
            // 结果集:读列定义 → EOF → 行 → EOF
            let nc = if b0 < 0xFB { b0 as usize } else { 1 };
            for _ in 0..nc {
                self.buf.clear();
                read_packet(&mut self.sock, &mut self.buf).await?;
            }
            self.buf.clear();
            read_packet(&mut self.sock, &mut self.buf).await?; // 列后 EOF
            loop {
                self.buf.clear();
                read_packet(&mut self.sock, &mut self.buf).await?;
                packets += 1;
                let b = self.buf.first().copied().unwrap_or(0);
                if b == 0xFF || (b == 0xFE && self.buf.len() < 7) || b == OK_HEADER {
                    return Ok((packets, err));
                }
            }
        }
    }

    /// COM_STMT_PREPARE 专用:固定 5 包(PREPARE_OK + pdef + EOF + coldef + EOF)
    async fn prepare(&mut self) -> std::io::Result<()> {
        send_packet(&mut self.sock, 0, &[0x16]).await?;
        for _ in 0..5 {
            self.buf.clear();
            read_packet(&mut self.sock, &mut self.buf).await?;
        }
        Ok(())
    }
}

/// 从 greeting payload 提取 20 字节 scramble
fn parse_greeting_scramble(payload: &[u8]) -> [u8; SCRAMBLE_LEN] {
    let mut r = payload;
    assert_eq!(r.get_u8(), 10, "protocol version");
    let pos = r.iter().position(|&b| b == 0).expect("version NUL");
    r = &r[pos + 1..];
    assert!(r.len() >= 4);
    r = &r[4..]; // connection_id
    assert!(r.len() >= 8);
    let mut s = [0u8; SCRAMBLE_LEN];
    s[..8].copy_from_slice(&r[..8]);
    r = &r[8..];
    r = &r[1..]; // filler
    r = &r[2..]; // cap_lower
    r = &r[1..]; // charset
    r = &r[2..]; // status
    r = &r[2..]; // cap_upper
    assert!(r.len() >= 1);
    let auth_data_len = r[0] as usize;
    r = &r[1..];
    r = &r[10..]; // reserved
    let part2 = (auth_data_len - 8 - 1).min(12);
    s[8..8 + part2].copy_from_slice(&r[..part2]);
    s
}

// ─── 统计 ───

struct Stats {
    qps: f64,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    avg_us: f64,
    errors: u64,
}

fn percentile(v: &[u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let idx = ((s.len() as f64) * p).ceil() as usize - 1;
    s[idx.min(s.len() - 1)]
}

fn summarize(dur: Duration, lat: &[u64], errors: u64) -> Stats {
    let ops = lat.len() as f64;
    Stats {
        qps: ops / dur.as_secs_f64(),
        p50_us: percentile(lat, 0.50) as f64 / 1e3,
        p95_us: percentile(lat, 0.95) as f64 / 1e3,
        p99_us: percentile(lat, 0.99) as f64 / 1e3,
        avg_us: if ops > 0.0 { lat.iter().sum::<u64>() as f64 / ops / 1e3 } else { 0.0 },
        errors,
    }
}

// ─── 场景 ───

#[derive(Clone, Copy)]
enum Workload {
    Ping,
    Select1,
    Rows100,
    PrepareExec,
    Connect,
}

fn workload_name(w: Workload) -> &'static str {
    match w {
        Workload::Ping => "ping(内联,无后端)",
        Workload::Select1 => "select1(透传,5包)",
        Workload::Rows100 => "rows100(透传,102包)",
        Workload::PrepareExec => "prepare+execute",
        Workload::Connect => "连接+认证(握手开销)",
    }
}

async fn client_loop(port: u16, w: Workload, dur: Duration, lat: &mut Vec<u64>) -> u64 {
    // Connect 场景:每次迭代 = 一次完整「连接 + 握手 + 认证」,直接测量连接建立开销
    if matches!(w, Workload::Connect) {
        let deadline = Instant::now() + dur;
        let mut errors = 0u64;
        loop {
            let t0 = Instant::now();
            match BenchClient::connect(port).await {
                Ok(_c) => lat.push(t0.elapsed().as_nanos() as u64),
                Err(_) => errors += 1,
            }
            if Instant::now() >= deadline {
                return errors;
            }
        }
    }

    let mut c = match BenchClient::connect(port).await {
        Ok(c) => c,
        Err(_) => return 0,
    };
    if matches!(w, Workload::PrepareExec) {
        if c.prepare().await.is_err() {
            return 0;
        }
    }
    let deadline = Instant::now() + dur;
    let mut errors = 0u64;
    loop {
        let t0 = Instant::now();
        let r = match w {
            Workload::Ping => c.roundtrip(0x0E, b"").await,
            Workload::Select1 => c.roundtrip(0x03, b"SELECT 1").await,
            Workload::Rows100 => c.roundtrip(0x03, b"SELECT bench_100_rows").await,
            Workload::PrepareExec => c.roundtrip(0x17, &[0x01, 0, 0, 0, 0, 1, 0, 0, 0]).await,
            Workload::Connect => unreachable!(),
        };
        match r {
            Ok((_, err)) => {
                if err {
                    errors += 1;
                }
            }
            Err(_) => {
                errors += 1;
                // 连接断开:重连一次再继续
                match BenchClient::connect(port).await {
                    Ok(nc) => {
                        c = nc;
                        if matches!(w, Workload::PrepareExec) && c.prepare().await.is_err() {
                            return errors;
                        }
                    }
                    Err(_) => return errors,
                }
            }
        }
        lat.push(t0.elapsed().as_nanos() as u64);
        if Instant::now() >= deadline {
            break;
        }
    }
    errors
}

async fn run_scenario(
    w: Workload,
    via: &str,
    conns: usize,
    secs: u64,
    port: u16,
    out: &mut Vec<String>,
) {
    // 预热:建立连接 + 少量请求,排除连接建立/池冷启动影响
    if let Ok(mut c) = BenchClient::connect(port).await {
        if matches!(w, Workload::PrepareExec) {
            let _ = c.prepare().await;
        }
        for _ in 0..20 {
            let _ = c.roundtrip(0x03, b"SELECT 1").await;
        }
    }

    let dur = Duration::from_secs(secs);
    let mut handles = Vec::new();
    for _ in 0..conns {
        let port = port;
        handles.push(tokio::spawn(async move {
            let mut lat = Vec::with_capacity(8192);
            let errs = client_loop(port, w, dur, &mut lat).await;
            (lat, errs)
        }));
    }
    let mut all = Vec::new();
    let mut errors = 0u64;
    for h in handles {
        if let Ok((lat, errs)) = h.await {
            all.extend(lat);
            errors += errs;
        }
    }
    let s = summarize(dur, &all, errors);
    out.push(format!(
        "| {} | {} | {} | {:>10.0} | {:>8.2} | {:>8.2} | {:>8.2} | {:>8.2} | {} |",
        workload_name(w),
        via,
        conns,
        s.qps,
        s.avg_us,
        s.p50_us,
        s.p95_us,
        s.p99_us,
        if errors > 0 { format!("{errors}") } else { "0".to_string() },
    ));
    eprintln!(
        "  {:<28} via={:<4} conns={:<3} QPS={:>10.0}  p50={:>7.2}µs p95={:>7.2}µs p99={:>7.2}µs",
        workload_name(w),
        via,
        conns,
        s.qps,
        s.p50_us,
        s.p95_us,
        s.p99_us
    );
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_conns() -> Vec<usize> {
    std::env::var("PERF_CONNS")
        .ok()
        .map(|v| {
            v.split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect()
        })
        .filter(|v: &Vec<usize>| !v.is_empty())
        .unwrap_or_else(|| vec![1, 4, 16, 64])
}

// ─── 测试入口(#[ignore]:正常 cargo test 跳过,显式运行)───

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "性能基准:用 cargo test --release --test perf_proxy -- --ignored --nocapture 运行"]
async fn proxy_perf_bench() {
    let secs = env_u64("PERF_SECS", 3);
    let conns = env_conns();
    println!("\n# NewProxy Proxy 内置性能基准 (secs={secs}, conns={conns:?}, release)");
    println!();

    let mock = MockBackend::start().await.expect("mock backend");
    let proxy_port = start_proxy(mock.port).await.expect("proxy");

    println!("环境:mock backend :{}  proxy :{}", mock.port, proxy_port);
    println!();
    println!("| 场景 | 路径 | 并发 | QPS | avg(µs) | p50(µs) | p95(µs) | p99(µs) | 错误 |");
    println!("|------|------|------|------:|--------:|--------:|--------:|--------:|------|");

    let mut rows: Vec<String> = Vec::new();

    // 直连基线:select1 / rows100 / ping / 连接建立 不经代理
    // (rows100 直连用于隔离「客户端读包成本」与「代理转发成本」)
    for n in [1usize, 16, 64] {
        if conns.contains(&n) {
            let _ = run_scenario(Workload::Select1, "直连", n, secs, mock.port, &mut rows).await;
            if n == 16 {
                let _ = run_scenario(Workload::Rows100, "直连", n, secs, mock.port, &mut rows).await;
                let _ = run_scenario(Workload::Ping, "直连", n, secs, mock.port, &mut rows).await;
            }
        }
    }
    // 代理路径:全部场景 × 全部并发
    for w in [
        Workload::Ping,
        Workload::Select1,
        Workload::Rows100,
        Workload::PrepareExec,
    ] {
        for n in &conns {
            let _ = run_scenario(w, "代理", *n, secs, proxy_port, &mut rows).await;
        }
    }
    // 连接建立开销(握手密集型,时长取 max(secs,2) 保证样本量)
    let conn_secs = secs.max(2);
    let _ = run_scenario(Workload::Connect, "直连", 8, conn_secs, mock.port, &mut rows).await;
    let _ = run_scenario(Workload::Connect, "代理", 8, conn_secs, proxy_port, &mut rows).await;

    for r in &rows {
        println!("{r}");
    }
    println!();
    mock.stop().await;
}

/// 启动 in-process proxy(镜像 main.rs 的 accept 循环)
async fn start_proxy(mock_port: u16) -> std::io::Result<u16> {
    let mut cfg = AppConfig::default();
    cfg.reload_interval_secs = 0;
    cfg.log_level = LogLevel::Error;
    cfg.conn_pool_socket_max_serve_client_times = 1_000_000_000; // 基准内不回收连接

    let db = Database {
        host: "127.0.0.1".into(),
        port: mock_port,
        max_pool_size: 128,
        max_connections: 4096,
        connect_timeout: 5,
        weight: 1,
        tablet_name: None,
    };
    let tablet = ClusterTablet {
        cluster_id: "c1".into(),
        tablet_id: "t1".into(),
        index: 0,
        groups: vec![DatabaseGroup {
            group_id: "g1".into(),
            master: Some(db),
            slave: None,
        }],
        routes: vec![],
    };
    let mut clusters: HashMap<String, Cluster> = HashMap::new();
    clusters.insert(
        "c1".into(),
        Cluster {
            id: "c1".into(),
            name: "c1".into(),
            tablets: vec![tablet],
        },
    );
    cfg.clusters = clusters;

    let mut db_users = HashMap::new();
    db_users.insert(
        "dbu".into(),
        DbUser {
            username: "dbu".into(),
            password: "dbpass".into(),
            default_db: None,
            cluster_name: "c1".into(),
        },
    );
    cfg.db_users = db_users;

    let mut product_users = HashMap::new();
    product_users.insert(
        PROD_USER.into(),
        ProductUser {
            username: PROD_USER.into(),
            password: PROD_PASS.into(),
            db_username: "dbu".into(),
            max_connections: 1_000_000,
            cluster_name: "c1".into(),
            scramble_password: None,
        },
    );
    cfg.product_users = product_users;

    let ctx = Arc::new(AppCtx::new(cfg, Arc::new(SrvPool::new()), "perf-bench".into()));

    let listener = TcpSocket::new_v4()?;
    listener.set_reuseaddr(true)?;
    let addr: std::net::SocketAddr = (std::net::Ipv4Addr::LOCALHOST, 0).into();
    listener.bind(addr)?;
    let listener = listener.listen(4096)?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            // 注意:accept 偶发错误(高频 churn 下的 ECONNABORTED/EMFILE)必须
            // continue 而非 break——break 会让代理静默停止接受连接,后续所有
            // 客户端 connect 被拒(基准曾因此复现 connect QPS 掉到个位数)。
            let (stream, peer) = match listener.accept().await {
                Ok(x) => x,
                // 偶发 accept 错误(高频 churn 下的 ECONNABORTED 等)必须
                // continue:break 会让代理静默停止接受连接(曾复现于基准)。
                Err(_) => continue,
            };
            let ctx = ctx.clone();
            tokio::spawn(async move {
                front::conn_task(stream, peer, ctx).await;
            });
        }
    });
    Ok(port)
}

// ─── 长跑场景:配合系统 profiler(sample / Instruments)定位 CPU 热点 ───

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "长跑: perf_proxy::proxy_perf_long_run --ignored --nocapture"]
async fn proxy_perf_long_run() {
    let mock = MockBackend::start().await.expect("mock backend");
    let proxy_port = start_proxy(mock.port).await.expect("proxy");
    eprintln!("[long-run] mock={} proxy={} — 开始 60s select1 @16conns(可用 sample <pid> 采集)", mock.port, proxy_port);
    let mut lat = Vec::with_capacity(1_000_000);
    let errs = client_loop(proxy_port, Workload::Select1, Duration::from_secs(60), &mut lat).await;
    let s = summarize(Duration::from_secs(60), &lat, errs);
    eprintln!("[long-run] QPS={:.0} p50={:.1}µs p95={:.1}µs p99={:.1}µs errors={}", s.qps, s.p50_us, s.p95_us, s.p99_us, s.errors);
    mock.stop().await;
}

//! front.rs 错误路径测试:进程内代理 + 可注入故障的 mock 后端(错误注入)。
//!
//! 覆盖 E2E 难以触发的分支:
//! - 后端连接拒绝 / 认证失败 / 查询返回 ERR(解析失败记录)
//! - 后端挂起 + kill 中断
//! - 慢查询记录、COM_INIT_DB 失败
//! - 前端认证:错密码 / 未知用户
//! - 会话内多查询(连接池复用)、`SELECT DATABASE()` 本地应答

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

use newproxy::app::AppCtx;
use newproxy::config::model::{
    AppConfig, Cluster, ClusterTablet, Database, DatabaseGroup, DbUser, LogLevel, ProductUser,
};
use newproxy::conn::front;
use newproxy::pool::backend::SrvPool;
use newproxy::proto::error::{build_error, build_ok, ERR_HEADER, OK_HEADER};
use newproxy::proto::handshake::scramble_native;

const SCRAMBLE_LEN: usize = 20;
const PROD_USER: &str = "u";
const PROD_PASS: &str = "p";
const SCRAMBLE: [u8; SCRAMBLE_LEN] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14,
];
const OLD_EOF: [u8; 5] = [0xFE, 0x00, 0x00, 0x02, 0x00];

// ─── 帧工具 ───

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

fn wire(seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 4);
    v.push((payload.len() & 0xFF) as u8);
    v.push(((payload.len() >> 8) & 0xFF) as u8);
    v.push(((payload.len() >> 16) & 0xFF) as u8);
    v.push(seq);
    v.extend_from_slice(payload);
    v
}

fn build_greeting() -> Vec<u8> {
    let caps = 0x0000_0200 | 0x0000_8000 | 0x0008_0000; // PROTOCOL_41|SECURE_CONNECTION|PLUGIN_AUTH
    let mut v = Vec::with_capacity(64);
    v.push(10);
    v.extend_from_slice(b"5.7.40-mock\0");
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&SCRAMBLE[..8]);
    v.push(0);
    v.extend_from_slice(&(caps as u16).to_le_bytes());
    v.push(33);
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&((caps >> 16) as u16).to_le_bytes());
    v.push(21);
    v.extend_from_slice(&[0u8; 10]);
    v.extend_from_slice(&SCRAMBLE[8..]);
    v.push(0);
    v.extend_from_slice(b"mysql_native_password\0");
    v
}

fn build_select1() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&wire(1, &[1u8])); // column_count
    let mut cd = vec![0x03u8];
    cd.extend_from_slice(b"def\0\0\0\0\0");
    cd.push(1);
    cd.push(b'1');
    cd.push(0x00);
    cd.push(0x0C);
    cd.extend_from_slice(&33u16.to_le_bytes());
    cd.extend_from_slice(&0u32.to_le_bytes());
    cd.push(0xFD);
    cd.extend_from_slice(&[0x00, 0x00]);
    cd.push(0x00);
    cd.extend_from_slice(&[0x00, 0x00]);
    v.extend_from_slice(&wire(2, &cd));
    v.extend_from_slice(&wire(3, &OLD_EOF));
    v.extend_from_slice(&wire(4, &[1u8, b'1']));
    v.extend_from_slice(&wire(5, &OLD_EOF));
    v
}

// ─── 可注入故障的 mock 后端 ───

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    /// 正常:握手成功,查询返回 SELECT 1 结果集
    None,
    /// 认证请求返回 ERR 1045
    AuthFail,
    /// 查询返回 ERR 1064
    QueryErr,
    /// 查询前 sleep(制造慢查询/挂起)
    Slow(Duration),
    /// COM_INIT_DB 返回 ERR
    InitDbErr,
    /// 认证请求后返回 AuthSwitchRequest(要求切换 mysql_native_password)
    AuthSwitch,
    /// 认证请求后返回 AUTH_MORE_DATA(caching_sha2 fast-auth 失败 → 明文密码)
    AuthMoreData,
}

struct MockBackend {
    port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl MockBackend {
    async fn start(fault: Fault) -> std::io::Result<Self> {
        let lsock = TcpSocket::new_v4()?;
        lsock.set_reuseaddr(true)?;
        let addr: std::net::SocketAddr = (std::net::Ipv4Addr::LOCALHOST, 0).into();
        lsock.bind(addr)?;
        let listener = lsock.listen(4096)?;
        let port = listener.local_addr()?.port();

        let greeting = wire(0, &build_greeting());
        let ok = wire(2, &build_ok(0, 0, 0x0002, 0, None));
        let auth_err = wire(2, &build_error(1045, "28000", "Access denied for mock"));
        let query_err = wire(1, &build_error(1064, "42000", "You have an error in your SQL syntax"));
        let initdb_err = wire(1, &build_error(1049, "42000", "Unknown database 'nope'"));

        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                let _ = sock.set_nodelay(true);
                let (greeting, ok, auth_err, query_err, initdb_err) =
                    (greeting.clone(), ok.clone(), auth_err.clone(), query_err.clone(), initdb_err.clone());
                tokio::spawn(async move {
                    let _ = sock.write_all(&greeting).await;
                    let mut pkt = Vec::with_capacity(4096);
                    if read_packet(&mut sock, &mut pkt).await.is_err() {
                        return;
                    }
                    // auth 阶段(seq=1 的 Client Auth Response)
                    match fault {
                        Fault::AuthFail => {
                            let _ = sock.write_all(&auth_err).await;
                            return;
                        }
                        Fault::AuthSwitch => {
                            // 0xFE + plugin + NUL + auth data
                            let mut sw = vec![0xFEu8];
                            sw.extend_from_slice(b"mysql_native_password\0");
                            sw.extend_from_slice(&SCRAMBLE);
                            let _ = sock.write_all(&wire(2, &sw)).await;
                            // 读取 switch response,然后回 OK
                            if read_packet(&mut sock, &mut pkt).await.is_err() {
                                return;
                            }
                            let _ = sock.write_all(&ok).await;
                        }
                        Fault::AuthMoreData => {
                            // 0x01 + 0x04(perform full auth)→ 代理发明文密码 → 回 OK
                            let _ = sock.write_all(&wire(2, &[0x01, 0x04])).await;
                            if read_packet(&mut sock, &mut pkt).await.is_err() {
                                return;
                            }
                            let _ = sock.write_all(&ok).await;
                        }
                        _ => {
                            let _ = sock.write_all(&ok).await;
                        }
                    }
                    // 命令阶段
                    loop {
                        pkt.clear();
                        if read_packet(&mut sock, &mut pkt).await.is_err() {
                            return;
                        }
                        let cmd = pkt.first().copied().unwrap_or(0xFF);
                        match cmd {
                            0x02 => {
                                // COM_INIT_DB
                                if fault == Fault::InitDbErr {
                                    if sock.write_all(&initdb_err).await.is_err() {
                                        return;
                                    }
                                } else if sock.write_all(&ok).await.is_err() {
                                    return;
                                }
                            }
                            0x03 => {
                                // COM_QUERY
                                match fault {
                                    Fault::QueryErr => {
                                        if sock.write_all(&query_err).await.is_err() {
                                            return;
                                        }
                                    }
                                    Fault::Slow(d) => {
                                        tokio::time::sleep(d).await;
                                        if sock.write_all(&build_select1()).await.is_err() {
                                            return;
                                        }
                                    }
                                    _ => {
                                        if sock.write_all(&build_select1()).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                            0x0E => {
                                // COM_PING
                                if sock.write_all(&ok).await.is_err() {
                                    return;
                                }
                            }
                            _ => {
                                if sock.write_all(&ok).await.is_err() {
                                    return;
                                }
                            }
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

/// 获取一个"无人监听"的端口(用于连接拒绝场景)
fn unused_port() -> u16 {
    use std::net::TcpListener;
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// ─── 进程内代理 ───

fn make_cfg(backend_port: u16, default_db: Option<&str>) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.max_threads = 4;
    cfg.log_level = LogLevel::Error;
    let mut clusters = HashMap::new();
    clusters.insert(
        "c1".into(),
        Cluster {
            id: "c1".into(),
            name: "c1".into(),
            tablets: vec![ClusterTablet {
                cluster_id: "c1".into(),
                tablet_id: "t0".into(),
                index: 0,
                groups: vec![DatabaseGroup {
                    group_id: "g0".into(),
                    master: Some(Database {
                        host: "127.0.0.1".into(),
                        port: backend_port,
                        max_pool_size: 16,
                        max_connections: 256,
                        connect_timeout: 5,
                        weight: 1,
                        tablet_name: Some("t0".into()),
                    }),
                    slave: None,
                }],
                routes: vec![],
            }],
        },
    );
    cfg.clusters = clusters;
    let mut db_users = HashMap::new();
    db_users.insert(
        "dbu".into(),
        DbUser {
            username: "dbu".into(),
            password: "dbpass".into(),
            default_db: default_db.map(|s| s.to_string()),
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
            max_connections: 1000,
            cluster_name: "c1".into(),
            scramble_password: None,
        },
    );
    cfg.product_users = product_users;
    cfg
}

async fn start_proxy(cfg: AppConfig) -> std::io::Result<(u16, Arc<AppCtx>)> {
    let ctx = Arc::new(AppCtx::new(cfg, Arc::new(SrvPool::new()), "front-test".into()));
    let lsock = TcpSocket::new_v4()?;
    lsock.set_reuseaddr(true)?;
    let addr: std::net::SocketAddr = (std::net::Ipv4Addr::LOCALHOST, 0).into();
    lsock.bind(addr)?;
    let listener = lsock.listen(4096)?;
    let port = listener.local_addr()?.port();
    let ctx_task = ctx.clone();
    tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => continue,
            };
            let ctx = ctx_task.clone();
            tokio::spawn(async move {
                front::conn_task(stream, peer, ctx).await;
            });
        }
    });
    Ok((port, ctx))
}

// ─── 测试客户端 ───

/// 从 greeting payload 提取 20 字节 scramble(与代理每次生成的随机值一致)
fn parse_greeting_scramble(payload: &[u8]) -> [u8; SCRAMBLE_LEN] {
    use bytes::Buf;
    let mut r = payload;
    assert_eq!(r.get_u8(), 10, "protocol version");
    let pos = r.iter().position(|&b| b == 0).expect("version NUL");
    r = &r[pos + 1..];
    r = &r[4..]; // connection_id
    let mut s = [0u8; SCRAMBLE_LEN];
    s[..8].copy_from_slice(&r[..8]);
    r = &r[8..];
    r = &r[1..]; // filler
    r = &r[2..]; // cap_lower
    r = &r[1..]; // charset
    r = &r[2..]; // status
    r = &r[2..]; // cap_upper
    let auth_data_len = r[0] as usize;
    r = &r[1..];
    r = &r[10..]; // reserved
    let part2 = (auth_data_len - 8 - 1).min(12);
    s[8..8 + part2].copy_from_slice(&r[..part2]);
    s
}

struct Client {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl Client {
    async fn connect(port: u16, user: &str, pass: &str) -> std::io::Result<Self> {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await?;
        sock.set_nodelay(true)?;
        let mut buf = Vec::with_capacity(4096);
        read_packet(&mut sock, &mut buf).await?; // greeting
        let scramble = parse_greeting_scramble(&buf);
        let auth_resp = scramble_native(pass.as_bytes(), &scramble);
        let mut auth = Vec::with_capacity(64);
        auth.extend_from_slice(&(0x0000_0200u32 | 0x0000_8000).to_le_bytes());
        auth.extend_from_slice(&(16 * 1024 * 1024u32).to_le_bytes());
        auth.push(33);
        auth.extend_from_slice(&[0u8; 23]);
        auth.extend_from_slice(user.as_bytes());
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

    /// 执行 COM_QUERY,返回 (是否 ERR, 错误码)
    async fn query(&mut self, sql: &str) -> std::io::Result<(bool, u16)> {
        let mut pkt = Vec::with_capacity(sql.len() + 1);
        pkt.push(0x03);
        pkt.extend_from_slice(sql.as_bytes());
        send_packet(&mut self.sock, 0, &pkt).await?;
        self.buf.clear();
        read_packet(&mut self.sock, &mut self.buf).await?;
        let b0 = self.buf.first().copied().unwrap_or(0);
        if b0 == ERR_HEADER {
            let code = u16::from_le_bytes([self.buf[1], self.buf[2]]);
            return Ok((true, code));
        }
        // 结果集:跳过列定义与行(本测试只关心终止与否)
        let nc = if b0 < 0xFB { b0 as usize } else { 1 };
        for _ in 0..nc {
            self.buf.clear();
            read_packet(&mut self.sock, &mut self.buf).await?;
        }
        loop {
            self.buf.clear();
            read_packet(&mut self.sock, &mut self.buf).await?;
            let b = self.buf.first().copied().unwrap_or(0);
            if b == OK_HEADER || b == ERR_HEADER || (b == 0xFE && self.buf.len() < 7) {
                return Ok((false, 0));
            }
        }
    }

}

// ─── 场景测试 ───

/// 等待异步连接 task 记录解析失败,避免固定 sleep 在慢 CI 上偶发失败。
async fn wait_parse_failures(ctx: &Arc<AppCtx>, min: u64) -> bool {
    for _ in 0..40 {
        if ctx.metrics.parse_failures.get() >= min {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// 基础链路:同一会话连续查询(验证会话粘滞后端连接)
#[tokio::test(flavor = "multi_thread")]
async fn basic_query_session_sticky() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let (port, _ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    let (err, _code) = c.query("SELECT 1").await.unwrap();
    assert!(!err, "查询不应出错");
    // 同一会话第二条查询(复用 session-sticky self.backend,不重新 acquire 池)
    let (err, _code) = c.query("SELECT 2").await.unwrap();
    assert!(!err);
    mock.stop().await;
}

/// 后端拒绝连接:前端握手成功,首个查询时后端建连失败 → 客户端收到错误
#[tokio::test(flavor = "multi_thread")]
async fn backend_connection_refused() {
    let dead_port = unused_port();
    let (port, ctx) = start_proxy(make_cfg(dead_port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    let r = c.query("SELECT 1").await;
    assert!(r.is_err() || r.unwrap().0, "后端拒绝连接时首个查询应失败");
    let _ = &ctx;
}

/// 后端认证失败:前端握手成功,后端握手失败 → 客户端首个查询收到 1049 类错误
#[tokio::test(flavor = "multi_thread")]
async fn backend_auth_failure() {
    let mock = MockBackend::start(Fault::AuthFail).await.unwrap();
    let (port, _ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    match c.query("SELECT 1").await {
        Ok((err, code)) => assert!(err, "后端认证失败应返回 ERR(code={code})"),
        Err(e) => panic!("后端认证失败路径应由代理返回 ERR,不应直接断开: {e}"),
    }
    mock.stop().await;
}

/// 查询返回 ERR:客户端收到 1064,且代理记录后端错误统计
#[tokio::test(flavor = "multi_thread")]
async fn backend_query_err_recorded() {
    let mock = MockBackend::start(Fault::QueryErr).await.unwrap();
    let (port, ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    let (err, code) = c.query("SELEC * FROM t").await.unwrap();
    assert!(err);
    assert_eq!(code, 1064);
    // 代理先把 ERR 发给客户端，再写后端错误指标；轮询避免两个 task 的调度竞争。
    for _ in 0..40 {
        if ctx.metrics.backend_errors.get() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ctx.metrics.backend_errors.get() >= 1, "后端 ERR 应记录 backend_errors");
    let list = ctx.metrics.backend_errors_snapshot();
    assert!(!list.is_empty());
    assert_eq!(list[0].code, "1064");
    mock.stop().await;
}

/// 慢查询:延迟 400ms 的查询被记为慢查询
#[tokio::test(flavor = "multi_thread")]
async fn slow_query_recorded() {
    let mock = MockBackend::start(Fault::Slow(Duration::from_millis(400))).await.unwrap();
    let (port, ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    let (err, _) = c.query("SELECT SLEEP(0.4)").await.unwrap();
    assert!(!err);
    assert!(ctx.metrics.queries_slow.get() >= 1, "慢查询应被计数");
    let slow = ctx.metrics.slow_queries_snapshot();
    assert!(!slow.is_empty(), "慢查询列表应有记录");
    assert!(!slow[0].shard.is_empty(), "慢查询应带分片: {}", slow[0].shard);
    // 最近查询记录同样带分片
    let recent = ctx.metrics.recent_queries_snapshot();
    assert!(!recent.is_empty());
    mock.stop().await;
}

/// kill 中断挂起的查询:后端不响应,运维 kill 后连接被关闭
#[tokio::test(flavor = "multi_thread")]
async fn kill_hangs_query() {
    let mock = MockBackend::start(Fault::Slow(Duration::from_secs(30))).await.unwrap();
    let (port, ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    // 找到该连接的 cid
    let cid = {
        let mut found = None;
        for e in ctx.connections.iter() {
            found = Some(*e.key());
        }
        found.expect("应有注册连接")
    };
    // 发起查询(后端 30s 后才响应),同时 kill
    let mut pkt = vec![0x03u8];
    pkt.extend_from_slice(b"SELECT 1");
    send_packet(&mut c.sock, 0, &pkt).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    ctx.kill_connection(cid).unwrap();
    // 客户端连接应被关闭(读返回 EOF/错误)
    let mut buf = Vec::new();
    let r = tokio::time::timeout(Duration::from_secs(3), read_packet(&mut c.sock, &mut buf)).await;
    assert!(r.is_err() || r.unwrap().is_err(), "kill 后客户端应收到连接关闭");
    mock.stop().await;
}

/// COM_INIT_DB 失败(默认库不存在):代理不崩溃,继续可用
#[tokio::test(flavor = "multi_thread")]
async fn init_db_failure_tolerated() {
    let mock = MockBackend::start(Fault::InitDbErr).await.unwrap();
    let (port, ctx) = start_proxy(make_cfg(mock.port, Some("nope"))).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    // 默认库选择失败 → current_db 为 None,但查询仍可用
    let (err, _) = c.query("SELECT 1").await.unwrap();
    assert!(!err, "COM_INIT_DB 失败不应阻断后续查询");
    mock.stop().await;
    let _ = &ctx;
}

/// 前端认证:错密码 / 未知用户被拒绝
#[tokio::test(flavor = "multi_thread")]
async fn front_auth_rejects() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let (port, _ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    // 错密码 → 拒绝
    let r = Client::connect(port, PROD_USER, "wrong").await;
    assert!(r.is_err(), "错密码应被拒绝");
    // 未知用户 → 拒绝
    let r = Client::connect(port, "nobody", PROD_PASS).await;
    assert!(r.is_err(), "未知用户应被拒绝");
    mock.stop().await;
}

/// 多查询会话:认证后连续查询,验证会话级后端粘滞与指标记录
#[tokio::test(flavor = "multi_thread")]
async fn multi_query_session_metrics() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let (port, ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    for _ in 0..3 {
        let (err, _) = c.query("SELECT 1").await.unwrap();
        assert!(!err);
    }
    assert_eq!(ctx.metrics.queries_total.get(), 3);
    // 分片维度计数:全部落在 0.t0 等价键(cluster c1 → "c1.t0")
    let shards = ctx.metrics.shard_queries_snapshot();
    assert_eq!(shards.len(), 1, "单分片环境只有一个分片有计数: {shards:?}");
    assert_eq!(shards[0].1, 3);
    mock.stop().await;
}

// ─── 2 分片路由切换(尾号路由 + ensure_backend 切换)───

/// 构建 2 分片配置:两个 tablet 各挂一个 mock 后端,声明 hash_mod 路由
fn make_two_tablet_cfg(ports: (u16, u16)) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.max_threads = 4;
    cfg.log_level = LogLevel::Error;
    let mut clusters = HashMap::new();
    let tablet = |cid: &str, tid: &str, idx: usize, port: u16| ClusterTablet {
        cluster_id: cid.into(),
        tablet_id: tid.into(),
        index: idx,
        groups: vec![DatabaseGroup {
            group_id: format!("g{idx}").into(),
            master: Some(Database {
                host: "127.0.0.1".into(),
                port,
                max_pool_size: 16,
                max_connections: 256,
                connect_timeout: 5,
                weight: 1,
                tablet_name: Some(tid.into()),
            }),
            slave: None,
        }],
        routes: vec![],
    };
    clusters.insert(
        "c1".into(),
        Cluster {
            id: "c1".into(),
            name: "c1".into(),
            tablets: vec![
                tablet("c1", "t0", 0, ports.0),
                tablet("c1", "t1", 1, ports.1),
            ],
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
            max_connections: 1000,
            cluster_name: "c1".into(),
            scramble_password: None,
        },
    );
    cfg.product_users = product_users;
    cfg
}

/// 尾号路由:同一会话内 sbtest_1 → 分片1,再 sbtest_0 → 分片0(后端逐查询切换)
#[tokio::test(flavor = "multi_thread")]
async fn tail_routing_switches_backend_per_query() {
    let m0 = MockBackend::start(Fault::None).await.unwrap();
    let m1 = MockBackend::start(Fault::None).await.unwrap();
    let (port, ctx) = start_proxy(make_two_tablet_cfg((m0.port, m1.port))).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();

    // 无尾号表 → 默认第一个分片(c1.t0)
    let (err, _) = c.query("SELECT 1").await.unwrap();
    assert!(!err);
    assert_eq!(ctx.metrics.queries_total.get(), 1);
    // 尾号 1 → 分片 1
    let (err, _) = c.query("SELECT * FROM sbtest_1").await.unwrap();
    assert!(!err);
    // 尾号 0 → 分片 0(切换回)
    let (err, _) = c.query("SELECT * FROM sbtest_0").await.unwrap();
    assert!(!err);

    // 分片计数:两个分片都有查询,且 0/1 分布正确
    let shards = ctx.metrics.shard_queries_snapshot();
    let get = |k: &str| shards.iter().find(|(s, _)| s == k).map(|(_, n)| *n).unwrap_or(0);
    assert_eq!(get("c1.t0"), 2, "sbtest_0 + 无尾号 → 分片0 各 1 次: {shards:?}");
    assert_eq!(get("c1.t1"), 1, "sbtest_1 → 分片1: {shards:?}");
    // 最近查询也记录真实分片
    let recent = ctx.metrics.recent_queries_snapshot();
    let shards_seen: std::collections::HashSet<_> = recent.iter().map(|q| q.shard.clone()).collect();
    assert!(shards_seen.contains("c1.t0") && shards_seen.contains("c1.t1"), "最近查询应记录两个分片: {shards_seen:?}");
    m0.stop().await;
    m1.stop().await;
}

/// IP 黑名单拒绝:客户端 IP 命中 ignore_ips → 握手被拒(连接拒绝计数)
#[tokio::test(flavor = "multi_thread")]
async fn ip_blacklist_rejects() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let mut cfg = make_cfg(mock.port, None);
    // 黑名单包含整个 127/8(本机回环被拒);白名单(命中才限制)不会拦截非命中 IP
    cfg.ignore_ips = vec![("127.0.0.0".parse().unwrap(), 8)];
    let (port, ctx) = start_proxy(cfg).await.unwrap();
    let r = Client::connect(port, PROD_USER, PROD_PASS).await;
    assert!(r.is_err(), "命中黑名单的 IP 应被拒绝");
    assert!(ctx.metrics.connections_rejected.get() >= 1, "拒绝连接计数应增加");
    mock.stop().await;
}

/// 畸形 auth 包:客户端发送非法 payload → 代理返回 access denied
#[tokio::test(flavor = "multi_thread")]
async fn malformed_auth_rejected() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let (port, _ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    sock.set_nodelay(true).unwrap();
    let mut buf = Vec::new();
    read_packet(&mut sock, &mut buf).await.unwrap(); // greeting
    // 发送过短/非法 auth payload(不足 32 字节)
    send_packet(&mut sock, 1, b"garbage").await.unwrap();
    buf.clear();
    read_packet(&mut sock, &mut buf).await.unwrap();
    assert_eq!(buf.first(), Some(&ERR_HEADER), "畸形 auth 应被拒绝: {:02x?}", &buf[..buf.len().min(8)]);
    mock.stop().await;
}

/// 畸形命令包:auth 成功后发送空命令包(解析失败)→ 记录解析失败,连接关闭
#[tokio::test(flavor = "multi_thread")]
async fn malformed_command_recorded() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let (port, ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    // 空 payload 的命令包 → parse_command_packet 失败 → 记录解析失败
    send_packet(&mut c.sock, 0, &[]).await.unwrap();
    assert!(wait_parse_failures(&ctx, 1).await, "畸形命令应记录解析失败");
    // 连接被关闭(读返回错误)
    let mut buf = Vec::new();
    let r = tokio::time::timeout(Duration::from_secs(2), read_packet(&mut c.sock, &mut buf)).await;
    assert!(r.is_err() || r.unwrap().is_err(), "畸形命令后连接应关闭");
    mock.stop().await;
}

// ─── 本地应答与管理命令(SQL 路径)───

/// 发送一条查询并只读首个响应包(本地应答场景用)
async fn query_first_packet(c: &mut Client, sql: &str) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(sql.len() + 1);
    pkt.push(0x03);
    pkt.extend_from_slice(sql.as_bytes());
    send_packet(&mut c.sock, 0, &pkt).await.unwrap();
    c.buf.clear();
    read_packet(&mut c.sock, &mut c.buf).await.unwrap();
    c.buf.clone()
}

/// help 查询 → 代理本地应答帮助文本
#[tokio::test(flavor = "multi_thread")]
async fn help_local_answer() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let (port, _ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    let resp = query_first_packet(&mut c, "help").await;
    // answer_help 返回 2 列结果集;未被拦截而透传到 mock 时首包会是 1 列。
    assert_eq!(resp, vec![2u8], "help 应命中本地 answer_help: {:02x?}", &resp[..resp.len().min(8)]);
    mock.stop().await;
}

/// checkproxy kill(不存在的连接)→ 返回错误(1094),走 SQL 管理命令路径
#[tokio::test(flavor = "multi_thread")]
async fn checkproxy_kill_sql_path() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let (port, ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    // kill 一个不存在的 cid → 错误分支
    let resp = query_first_packet(&mut c, "checkproxy kill 99999").await;
    assert_eq!(resp.first(), Some(&ERR_HEADER), "kill 未知连接应报错: {:02x?}", &resp[..resp.len().min(10)]);
    // kill 自己的真实连接 → OK 分支
    let cid = ctx.connections.iter().next().map(|e| *e.key()).expect("有注册连接");
    let resp = query_first_packet(&mut c, &format!("checkproxy kill {cid}")).await;
    assert_eq!(resp.first(), Some(&OK_HEADER), "kill 自身应返回 OK: {:02x?}", &resp[..resp.len().min(10)]);
    mock.stop().await;
}

/// checkproxy reload → 配置路径不存在 → 错误分支(RELOAD failed)
#[tokio::test(flavor = "multi_thread")]
async fn checkproxy_reload_sql_path() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let (port, _ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    // make_cfg 的 config_path 是 "front-test"(不存在)→ reload 失败分支
    let resp = query_first_packet(&mut c, "checkproxy reload").await;
    assert_eq!(resp.first(), Some(&ERR_HEADER), "reload 不存在配置应报错: {:02x?}", &resp[..resp.len().min(10)]);
    mock.stop().await;
}

/// 未知命令码 → 代理回 unknown command 错误
#[tokio::test(flavor = "multi_thread")]
async fn unknown_command_response() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let (port, _ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    // 0x1D 是未支持命令
    send_packet(&mut c.sock, 0, &[0x1D]).await.unwrap();
    c.buf.clear();
    read_packet(&mut c.sock, &mut c.buf).await.unwrap();
    assert_eq!(c.buf.first(), Some(&ERR_HEADER), "未知命令应返回错误");
    mock.stop().await;
}

/// 非 UTF-8 查询 → 记录解析失败,不影响连接
#[tokio::test(flavor = "multi_thread")]
async fn non_utf8_query_recorded() {
    let mock = MockBackend::start(Fault::None).await.unwrap();
    let (port, ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    // 非法 UTF-8 字节序列作为查询
    let mut pkt = vec![0x03u8];
    pkt.extend_from_slice(&[0xff, 0xfe, 0xc3, 0x28]);
    send_packet(&mut c.sock, 0, &pkt).await.unwrap();
    assert!(wait_parse_failures(&ctx, 1).await, "非 UTF-8 应记录解析失败");
    mock.stop().await;
}

/// 后端在命令阶段关闭连接 → 代理写失败 → 连接标记损坏并关闭
#[tokio::test(flavor = "multi_thread")]
async fn backend_write_error_marks_broken() {
    // 认证后立刻断开的 mock:代理下次向后端写命令失败 → mark_broken
    let lsock = TcpSocket::new_v4().unwrap();
    lsock.set_reuseaddr(true).unwrap();
    let addr: std::net::SocketAddr = (std::net::Ipv4Addr::LOCALHOST, 0).into();
    lsock.bind(addr).unwrap();
    let listener = lsock.listen(4096).unwrap();
    let port = listener.local_addr().unwrap().port();
    let greeting = wire(0, &build_greeting());
    let ok = wire(2, &build_ok(0, 0, 0x0002, 0, None));
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let _ = sock.write_all(&greeting).await;
        let mut pkt = Vec::new();
        if read_packet(&mut sock, &mut pkt).await.is_ok() {
            let _ = sock.write_all(&ok).await;
            // 认证后立即关闭(代理下次写后端失败 → mark_broken)
            let _ = sock.shutdown().await;
        }
    });
    let (pport, ctx) = start_proxy(make_cfg(port, None)).await.unwrap();
    let mut c = Client::connect(pport, PROD_USER, PROD_PASS).await.unwrap();
    // 查询:代理向后端写命令 → 失败 → 客户端收到错误
    let mut pkt = vec![0x03u8];
    pkt.extend_from_slice(b"SELECT 1");
    send_packet(&mut c.sock, 0, &pkt).await.unwrap();
    c.buf.clear();
    let r = tokio::time::timeout(Duration::from_secs(3), read_packet(&mut c.sock, &mut c.buf)).await;
    assert!(r.is_err() || r.unwrap().is_err(), "后端断开后客户端应收到错误或关闭");
    handle.abort();
    let _ = &ctx;
}

/// 认证切换:后端要求切 mysql_native_password → 代理应答并完成认证
#[tokio::test(flavor = "multi_thread")]
async fn backend_auth_switch() {
    let mock = MockBackend::start(Fault::AuthSwitch).await.unwrap();
    let (port, _ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    let (err, _) = c.query("SELECT 1").await.unwrap();
    assert!(!err, "AuthSwitch 后应完成认证并可查询");
    mock.stop().await;
}

/// caching_sha2 fast-auth 失败(perform full auth)→ 代理发明文密码并完成认证
#[tokio::test(flavor = "multi_thread")]
async fn backend_auth_more_data_full_auth() {
    let mock = MockBackend::start(Fault::AuthMoreData).await.unwrap();
    let (port, _ctx) = start_proxy(make_cfg(mock.port, None)).await.unwrap();
    let mut c = Client::connect(port, PROD_USER, PROD_PASS).await.unwrap();
    let (err, _) = c.query("SELECT 1").await.unwrap();
    assert!(!err, "full-auth 后应完成认证并可查询");
    mock.stop().await;
}

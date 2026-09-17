// 读写分离(一致性档位)端到端测试:进程内代理 + 两个 mock MySQL(fake leader/follower)
//
// 场景:
// 1. causal 档 + 屏障就绪(WAIT 返回 1):可分流纯 SELECT 落到 follower;
//    写(INSERT)与 FOR UPDATE 等强制语句落到 leader;
// 2. causal 档 + 屏障超时(WAIT 返回 0):读自动回落 leader;
// 3. eventual 档:纯 SELECT 直接走 follower(无屏障 WAIT)。
//
// 运行:`cargo test --test ha_readsplit -- --nocapture`

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

use newproxy::app::AppCtx;
use newproxy::config::model::{
    AppConfig, Cluster, ClusterTablet, Database, DatabaseGroup, DbUser, LogLevel, ProductUser,
    ReadConsistency, RaftMember, XenonRaft,
};
use newproxy::conn::front;
use newproxy::pool::backend::SrvPool;
use newproxy::proto::error::{build_ok, OK_HEADER};
use newproxy::proto::handshake::scramble_native;

const SCRAMBLE_LEN: usize = 20;
const SCRAMBLE: [u8; SCRAMBLE_LEN] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14,
];
const GTID: &str = "01234567-89ab-cdef-0123-456789abcdef:1-7";

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
    let caps = 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
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

fn lenenc_str(s: &str) -> Vec<u8> {
    let mut o = vec![s.len() as u8];
    o.extend_from_slice(s.as_bytes());
    o
}

/// 文本结果集:单列单行(现代布局:列数 → 列定义 → 行 → OK 终止,
/// 对应代理后端握手协商 CLIENT_DEPRECATE_EOF 后的真实返回;经典列间 EOF 会
/// 干扰代理侧屏障解码,故不使用)
fn text_result(seq: &mut u8, col_name: &str, value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&wire(*seq, &[1u8])); // column_count
    *seq += 1;
    let mut cd = vec![0x03u8, 0x03, b'd', b'e', b'f'];
    // catalog 已含 def;schema/table/org_table 各为空字符串(共 3 个 0x00)
    cd.extend_from_slice(&[0u8; 3]);
    cd.extend_from_slice(&lenenc_str(col_name));
    cd.push(0); // org_name 空
    cd.push(0x0C);
    cd.extend_from_slice(&33u16.to_le_bytes());
    cd.extend_from_slice(&0u32.to_le_bytes());
    cd.push(0xFD);
    cd.extend_from_slice(&[0x00, 0x00]);
    cd.push(0x00);
    cd.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(&wire(*seq, &cd));
    *seq += 1;
    // 经典协议:列后 5B EOF
    let eof: [u8; 5] = [0xFE, 0x00, 0x00, 0x02, 0x00];
    out.extend_from_slice(&wire(*seq, &eof));
    *seq += 1;
    // 真实文本行:无列数前缀
    let row = lenenc_str(value);
    out.extend_from_slice(&wire(*seq, &row));
    *seq += 1;
    out.extend_from_slice(&wire(*seq, &eof));
    *seq += 1;
    out
}

// ─── 可记录查询的 mock 后端 ───

enum Kind {
    Leader,
    Follower,
}

struct FakeBackend {
    port: u16,
    queries: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeBackend {
    /// `barrier_ok`:follower 对 WAIT_FOR_EXECUTED_GTID_SET 返回 1(true)/0(false)
    async fn start(kind: Kind, barrier_ok: bool) -> std::io::Result<Self> {
        let lsock = TcpSocket::new_v4()?;
        lsock.set_reuseaddr(true)?;
        let addr: std::net::SocketAddr = (std::net::Ipv4Addr::LOCALHOST, 0).into();
        lsock.bind(addr)?;
        let listener = lsock.listen(4096)?;
        let port = listener.local_addr()?.port();
        let queries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let q = queries.clone();
        let greeting = wire(0, &build_greeting());
        let auth_ok = wire(2, &build_ok(0, 0, 0x0002, 0, None));
        let is_leader = matches!(kind, Kind::Leader);
        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let q = q.clone();
                let greeting = greeting.clone();
                let auth_ok = auth_ok.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_fake_conn(&mut sock, &greeting, &auth_ok, &q, is_leader, barrier_ok)
                            .await
                    {
                        eprintln!("fake handler error: {e}");
                    }
                });
            }
        });
        Ok(Self { port, queries, handle })
    }

    async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }

    fn received(&self) -> Vec<String> {
        self.queries.lock().unwrap().clone()
    }
}


async fn handle_fake_conn(
    sock: &mut TcpStream,
    greeting: &[u8],
    auth_ok: &[u8],
    q: &Arc<Mutex<Vec<String>>>,
    is_leader: bool,
    barrier_ok: bool,
) -> std::io::Result<()> {
    sock.write_all(greeting).await?;
    let mut buf = Vec::with_capacity(4096);
    read_packet(sock, &mut buf).await?; // auth
    sock.write_all(auth_ok).await?;
    loop {
        read_packet(sock, &mut buf).await?;
        if buf.is_empty() {
            continue;
        }
        match buf[0] {
            0x01 => return Ok(()), // COM_QUIT
            0x0e => {
                let _ = send_packet(sock, 1, &build_ok(0, 0, 0x0002, 0, None)).await;
            }
            0x03 => {
                let sql = String::from_utf8_lossy(&buf[1..]).into_owned();
                q.lock().unwrap().push(sql.clone());
                let lower = sql.to_ascii_lowercase();
                if lower.contains("wait_for_executed_gtid_set") {
                    let v = if barrier_ok { "1" } else { "0" };
                    let mut seq = 1u8;
                    let r = text_result(&mut seq, "wait_result", v);
                    sock.write_all(&r).await?;
                } else if is_leader && lower.contains("@@global.gtid_executed") {
                    let mut seq = 1u8;
                    let r = text_result(&mut seq, "@@GLOBAL.gtid_executed", GTID);
                    sock.write_all(&r).await?;
                } else {
                    let _ = send_packet(sock, 1, &build_ok(0, 0, 0x0002, 0, None)).await;
                }
            }
            _ => {}
        }
    }
}

// ─── 客户端与代理 ───

fn parse_greeting_scramble(payload: &[u8]) -> [u8; SCRAMBLE_LEN] {
    use bytes::Buf;
    let mut r = payload;
    assert_eq!(r.get_u8(), 10);
    let pos = r.iter().position(|&b| b == 0).expect("version NUL");
    r = &r[pos + 1..];
    r = &r[4..];
    let mut s = [0u8; SCRAMBLE_LEN];
    s[..8].copy_from_slice(&r[..8]);
    r = &r[8..];
    r = &r[1..];
    r = &r[2..];
    r = &r[1..];
    r = &r[2..];
    r = &r[2..];
    let auth_data_len = r[0] as usize;
    r = &r[1..];
    r = &r[10..];
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
        let mut buf = Vec::with_capacity(4096);
        read_packet(&mut sock, &mut buf).await?;
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
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "auth failed"));
        }
        Ok(Self { sock, buf })
    }

    /// 执行 COM_QUERY;返回是否 ERR(不解析结果内容)
    async fn query(&mut self, sql: &str) -> std::io::Result<bool> {
        let mut pkt = Vec::with_capacity(sql.len() + 1);
        pkt.push(0x03);
        pkt.extend_from_slice(sql.as_bytes());
        send_packet(&mut self.sock, 0, &pkt).await?;
        loop {
            self.buf.clear();
            read_packet(&mut self.sock, &mut self.buf).await?;
            let b = self.buf.first().copied().unwrap_or(0);
            if b == 0xFF {
                return Ok(true);
            }
            if b == OK_HEADER || (b == 0xFE && self.buf.len() < 9) {
                return Ok(false);
            }
            // 结果集包:继续读直到 EOF/OK
        }
    }
}

fn make_cfg(leader_port: u16, follower_port: u16, consistency: ReadConsistency) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.log_level = LogLevel::Error;
    let mut clusters = HashMap::new();
    let tablet = ClusterTablet {
        cluster_id: "c1".into(),
        tablet_id: "t0".into(),
        index: 0,
        groups: vec![DatabaseGroup {
            group_id: "g0".into(),
            master: Some(Database {
                host: "127.0.0.1".into(),
                port: leader_port,
                max_pool_size: 16,
                max_connections: 256,
                connect_timeout: 5,
                weight: 1,
                tablet_name: Some("t0".into()),
            }),
            slave: Some(Database {
                host: "127.0.0.1".into(),
                port: follower_port,
                max_pool_size: 16,
                max_connections: 256,
                connect_timeout: 5,
                weight: 1,
                tablet_name: Some("t0".into()),
            }),
        }],
        routes: vec![],
        xenon: Some(XenonRaft {
            members: vec![
                RaftMember { host: "127.0.0.1".into(), mysql_port: leader_port, raft_endpoint: None },
                RaftMember { host: "127.0.0.1".into(), mysql_port: follower_port, raft_endpoint: None },
            ],
            read_consistency: consistency,
            probe_interval_ms: 3600_000,
            barrier_wait_ms: 300,
            ..XenonRaft::default()
        }),
    };
    clusters.insert("c1".into(), Cluster { id: "c1".into(), name: "c1".into(), tablets: vec![tablet] });
    cfg.clusters = clusters;
    let mut db_users = HashMap::new();
    db_users.insert(
        "dbu".into(),
        DbUser { username: "dbu".into(), password: "dbpass".into(), default_db: None, cluster_name: "c1".into() },
    );
    cfg.db_users = db_users;
    let mut product_users = HashMap::new();
    product_users.insert(
        "u".into(),
        ProductUser {
            username: "u".into(),
            password: "p".into(),
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
    let ctx = Arc::new(AppCtx::new(cfg, Arc::new(SrvPool::new()), "ha-test".into()));
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

fn wait_until<F: Fn() -> bool>(f: F) -> bool {
    for _ in 0..60 {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

// ─── 场景 ───

/// causal 档 + follower 屏障就绪:纯 SELECT 走 follower;写与 FOR UPDATE 走 leader
#[tokio::test(flavor = "multi_thread")]
async fn causal_reads_go_follower_writes_leader() {
    let leader = FakeBackend::start(Kind::Leader, true).await.unwrap();
    let follower = FakeBackend::start(Kind::Follower, true).await.unwrap();
    let (port, ctx) =
        start_proxy(make_cfg(leader.port, follower.port, ReadConsistency::Causal)).await.unwrap();
    let mut c = Client::connect(port, "u", "p").await.unwrap();

    assert!(!c.query("INSERT INTO t (id) VALUES (1)").await.unwrap());
    assert!(!c.query("SELECT * FROM t WHERE id=1").await.unwrap());
    assert!(!c.query("SELECT * FROM t WHERE id=1 FOR UPDATE").await.unwrap());

    // leader 应收到:INSERT、FOR UPDATE 的 SELECT、以及屏障采样 @@GLOBAL.gtid_executed
    let leader_ok = wait_until(|| {
        let q = leader.received();
        q.iter().any(|s| s.contains("INSERT")) && q.iter().any(|s| s.contains("FOR UPDATE"))
    });
    assert!(leader_ok, "leader 应收到写与 FOR UPDATE: leader={:?} follower={:?}", leader.received(), follower.received());
    // follower 应收到屏障 WAIT + 可分流 SELECT
    let follower_ok = wait_until(|| {
        let q = follower.received();
        q.iter().any(|s| s.to_ascii_lowercase().contains("wait_for_executed_gtid_set"))
            && q.iter().any(|s| s.contains("SELECT * FROM t WHERE id=1") && !s.contains("UPDATE"))
    });
    assert!(follower_ok, "follower 应收到 WAIT + 可分流 SELECT: {:?}", follower.received());
    // 指标:分流读 ≥1
    assert!(ctx.metrics.ha_reads_follower.get() >= 1);
    assert!(ctx.metrics.ha_reads_leader_fallback.get() == 0);
    c.sock.shutdown().await.ok();
    follower.stop().await;
    leader.stop().await;
}

/// causal 档 + follower 追不平(WAIT 返回 0):读自动回落 leader
#[tokio::test(flavor = "multi_thread")]
async fn barrier_timeout_falls_back_to_leader() {
    let leader = FakeBackend::start(Kind::Leader, false).await.unwrap();
    let follower = FakeBackend::start(Kind::Follower, false).await.unwrap();
    let (port, ctx) =
        start_proxy(make_cfg(leader.port, follower.port, ReadConsistency::Causal)).await.unwrap();
    let mut c = Client::connect(port, "u", "p").await.unwrap();

    assert!(!c.query("SELECT * FROM t WHERE id=1").await.unwrap());

    // follower 应收到 WAIT 但不应收到 SELECT;SELECT 应落到 leader
    let all_ok = wait_until(|| {
        let fl = follower.received();
        let ld = leader.received();
        fl.iter()
            .any(|s| s.to_ascii_lowercase().contains("wait_for_executed_gtid_set"))
            && !fl.iter().any(|s| s.contains("SELECT * FROM t"))
            && ld.iter().any(|s| s.contains("SELECT * FROM t"))
    });
    assert!(all_ok, "追不平应回 leader:\n  leader={:?}\n  follower={:?}", leader.received(), follower.received());
    assert!(ctx.metrics.ha_barrier_timeouts.get() >= 1);
    assert!(ctx.metrics.ha_reads_leader_fallback.get() >= 1);
    c.sock.shutdown().await.ok();
    follower.stop().await;
    leader.stop().await;
}

/// eventual 档:纯 SELECT 直接走 follower(无 WAIT)
#[tokio::test(flavor = "multi_thread")]
async fn eventual_reads_no_barrier() {
    let leader = FakeBackend::start(Kind::Leader, true).await.unwrap();
    let follower = FakeBackend::start(Kind::Follower, true).await.unwrap();
    let (port, _ctx) =
        start_proxy(make_cfg(leader.port, follower.port, ReadConsistency::Eventual)).await.unwrap();
    let mut c = Client::connect(port, "u", "p").await.unwrap();

    assert!(!c.query("SELECT * FROM t WHERE id=1").await.unwrap());
    let ok = wait_until(|| {
        let q = follower.received();
        q.iter().any(|s| s.contains("SELECT * FROM t WHERE id=1"))
    });
    assert!(ok, "eventual 读应直接到 follower: {:?}", follower.received());
    // follower 不应收到任何 WAIT
    assert!(!follower
        .received()
        .iter()
        .any(|s| s.to_ascii_lowercase().contains("wait_for_executed_gtid_set")));
    c.sock.shutdown().await.ok();
    follower.stop().await;
    leader.stop().await;
}

/// Session 档:会话未写时读无需屏障;写过后读需要屏障(读己之写)
#[tokio::test(flavor = "multi_thread")]
async fn session_consistency_barrier_only_after_write() {
    let leader = FakeBackend::start(Kind::Leader, true).await.unwrap();
    let follower = FakeBackend::start(Kind::Follower, true).await.unwrap();
    let (port, _ctx) =
        start_proxy(make_cfg(leader.port, follower.port, ReadConsistency::Session)).await.unwrap();
    let mut c = Client::connect(port, "u", "p").await.unwrap();

    // 会话尚未写过:读直接走 follower,无 WAIT
    assert!(!c.query("SELECT * FROM t WHERE id=1").await.unwrap());
    let first_ok = wait_until(|| {
        let q = follower.received();
        q.iter().any(|s| s.contains("SELECT * FROM t WHERE id=1"))
    });
    assert!(first_ok, "首次纯读应直达 follower");
    let waits_before = follower
        .received()
        .iter()
        .filter(|s| s.to_ascii_lowercase().contains("wait_for_executed_gtid_set"))
        .count();
    assert_eq!(waits_before, 0, "会话未写过不应有屏障");

    // 写后:读走 follower 且带 WAIT(读己之写)
    assert!(!c.query("UPDATE t SET id=1 WHERE id=2").await.unwrap());
    let write_seen = wait_until(|| leader.received().iter().any(|s| s.contains("UPDATE")));
    assert!(write_seen, "写应到 leader");
    assert!(!c.query("SELECT * FROM t WHERE id=1").await.unwrap());
    let wait_ok = wait_until(|| {
        follower
            .received()
            .iter()
            .filter(|s| s.to_ascii_lowercase().contains("wait_for_executed_gtid_set"))
            .count()
            >= 1
    });
    assert!(wait_ok, "写后读应带 GTID 屏障");
    c.sock.shutdown().await.ok();
    follower.stop().await;
    leader.stop().await;
}

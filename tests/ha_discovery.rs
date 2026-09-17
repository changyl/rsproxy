// Xenon raft 发现端到端测试:进程内代理(不连前端)+ 两个成员 fake MySQL
//
// 验证核心链路:周期探测(手动触发 force_probe)→ 解析 mysql.xenon_raft_status
// 各节点行 → judge 判主 → topology 覆盖 + 分片连接池失效;随后模拟 raft 漂移
// (状态行切换 leader)再次探测 → 自动跟随新主。
//
// 运行:`cargo test --test ha_discovery -- --nocapture`

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

use newproxy::app::AppCtx;
use newproxy::config::model::{
    AppConfig, Cluster, ClusterTablet, DbUser, LogLevel, ProductUser, RaftMember, XenonRaft,
};
use newproxy::pool::backend::SrvPool;
use newproxy::proto::error::build_ok;

const SCRAMBLE_LEN: usize = 20;
const SCRAMBLE: [u8; SCRAMBLE_LEN] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14,
];
const GTID: &str = "01234567-89ab-cdef-0123-456789abcdef:1-7";

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

/// 单行多列文本结果(现代布局)
/// 首列协议完整版:多列支持统一使用标准列定义
fn multi_result(seq: &mut u8, col_names: &[&str], values: &[&str]) -> Vec<u8> {
    let n = col_names.len();
    let mut out = Vec::new();
    out.extend_from_slice(&wire(*seq, &[n as u8]));
    *seq += 1;
    for cname in col_names {
        let mut cd = vec![0x03u8, 0x03, b'd', b'e', b'f'];
        // catalog 已含 def;schema/table/org_table 各为空(共 3 个 0x00)
        cd.extend_from_slice(&[0u8; 3]);
        cd.extend_from_slice(&lenenc_str(cname));
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
    }
    // 真实文本行:无列数前缀,逐列 lenenc
    let mut row = Vec::new();
    for v in values {
        row.extend_from_slice(&lenenc_str(v));
    }
    let eof: [u8; 5] = [0xFE, 0x00, 0x00, 0x02, 0x00];
    // 经典协议:列后 5B EOF
    out.extend_from_slice(&wire(*seq, &eof));
    *seq += 1;
    out.extend_from_slice(&wire(*seq, &row));
    *seq += 1;
    out.extend_from_slice(&wire(*seq, &eof));
    *seq += 1;
    out
}

/// 单列单值文本结果
fn single_col_result(seq: &mut u8, col: &str, value: &str) -> Vec<u8> {
    multi_result(seq, &[col], &[value])
}

/// 成员 fake:可切换 raft 状态行中的 leader
struct Member {
    port: u16,
    /// leader 端点(Member 内设置):false=自身 127.0.0.1,true=对端 127.0.0.2
    leader_switch: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

async fn bind_on(ip: std::net::Ipv4Addr) -> std::io::Result<(TcpListener4, u16)> {
    let lsock = TcpSocket::new_v4()?;
    lsock.set_reuseaddr(true)?;
    let addr: std::net::SocketAddr = (ip, 0).into();
    lsock.bind(addr)?;
    let listener = lsock.listen(4096)?;
    let port = listener.local_addr()?.port();
    Ok((TcpListener4(listener), port))
}

async fn bind_on_v6() -> std::io::Result<(TcpListener4, u16)> {
    let lsock = TcpSocket::new_v6()?;
    lsock.set_reuseaddr(true)?;
    let addr: std::net::SocketAddr = (std::net::Ipv6Addr::LOCALHOST, 0).into();
    lsock.bind(addr)?;
    let listener = lsock.listen(4096)?;
    let port = listener.local_addr()?.port();
    Ok((TcpListener4(listener), port))
}

struct TcpListener4(tokio::net::TcpListener);

impl Member {
    /// ip:绑定地址;leader_endpoint_other:切换开关置 true 时自述的 leader 端点
    async fn start(
        ip: std::net::Ipv4Addr,
        v6: bool,
        self_endpoint: &'static str,
        other_endpoint: &'static str,
    ) -> std::io::Result<Self> {
        let (listener, port) = if v6 { bind_on_v6().await? } else { bind_on(ip).await? };
        let leader_switch = Arc::new(AtomicBool::new(false));
        let ls = leader_switch.clone();
        let greeting = wire(0, &build_greeting());
        let auth_ok = wire(2, &build_ok(0, 0, 0x0002, 0, None));
        let handle = tokio::spawn(async move {
            let listener = listener.0;
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let greeting = greeting.clone();
                let auth_ok = auth_ok.clone();
                let ls = ls.clone();
                tokio::spawn(async move {
                    let _ = sock.set_nodelay(true);
                    let mut buf = Vec::with_capacity(4096);
                    if sock.write_all(&greeting).await.is_err()
                        || read_packet(&mut sock, &mut buf).await.is_err()
                        || sock.write_all(&auth_ok).await.is_err()
                    {
                        return;
                    }
                    loop {
                        if read_packet(&mut sock, &mut buf).await.is_err() {
                            return;
                        }
                        if buf.is_empty() {
                            continue;
                        }
                        match buf[0] {
                            0x01 => return,
                            0x0e => {
                                let _ =
                                    send_packet(&mut sock, 1, &build_ok(0, 0, 0x0002, 0, None)).await;
                            }
                            0x03 => {
                                let sql = String::from_utf8_lossy(&buf[1..]).into_owned();
                                let lower = sql.to_ascii_lowercase();
                                let mut seq = 1u8;
                                if lower.contains("xenon_raft_status") {
                                    let leader_ep = if ls.load(Ordering::Relaxed) {
                                        other_endpoint
                                    } else {
                                        self_endpoint
                                    };
                                    let now_s = SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .map(|d| d.as_secs())
                                        .unwrap_or(0)
                                        .to_string();
                                    let r = multi_result(
                                        &mut seq,
                                        &["id", "leader", "view_id", "epoch_id", "updated_at"],
                                        &["1", leader_ep, "7", "1", &now_s],
                                    );
                                    let _ = sock.write_all(&r).await;
                                } else if lower.contains("@@global.gtid_executed") {
                                    let r = single_col_result(&mut seq, "@@GLOBAL.gtid_executed", GTID);
                                    let _ = sock.write_all(&r).await;
                                } else if lower.contains("rpl_semi_sync_master_status") {
                                    let r = single_col_result(&mut seq, "Value", "ON");
                                    let _ = sock.write_all(&r).await;
                                } else {
                                    let _ =
                                        send_packet(&mut sock, 1, &build_ok(0, 0, 0x0002, 0, None))
                                            .await;
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        Ok(Self { port, leader_switch, handle })
    }

    async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

/// 带 xenon 管理的单分片配置(members = 127.0.0.1:a / 127.0.0.2:b)
fn make_cfg(ports: (u16, u16)) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.log_level = LogLevel::Error;
    let mut clusters = HashMap::new();
    let tablet = ClusterTablet {
        cluster_id: "0".into(),
        tablet_id: "t0".into(),
        index: 0,
        groups: vec![],
        routes: vec![],
        xenon: Some(XenonRaft {
            members: vec![
                RaftMember { host: "127.0.0.1".into(), mysql_port: ports.0, raft_endpoint: None },
                RaftMember { host: "::1".into(), mysql_port: ports.1, raft_endpoint: None },
            ],
            probe_interval_ms: 3600_000, // 不自动跑;手动 force_probe
            leader_stale_ms: 10_000,
            ..XenonRaft::default()
        }),
    };
    clusters.insert(
        "0".into(),
        Cluster { id: "0".into(), name: "c0".into(), tablets: vec![tablet] },
    );
    cfg.clusters = clusters;
    let mut db_users = HashMap::new();
    db_users.insert(
        "dbu".into(),
        DbUser { username: "dbu".into(), password: "dbpass".into(), default_db: None, cluster_name: "c0".into() },
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
            cluster_name: "c0".into(),
            scramble_password: None,
        },
    );
    cfg.product_users = product_users;
    cfg
}

/// 发现 → 跟随;状态行切换 → 自动跟随新 leader;池随分片失效
#[tokio::test(flavor = "multi_thread")]
async fn discovery_follows_raft_leader_change() {
    let a = Member::start(
        std::net::Ipv4Addr::new(127, 0, 0, 1),
        false,
        "127.0.0.1:3306",
        "::1:3306",
    )
    .await
    .unwrap();
    let b = Member::start(
        std::net::Ipv4Addr::LOCALHOST,
        true,
        "::1:3306",
        "127.0.0.1:3306",
    )
    .await
    .unwrap();
    // 初始状态:两成员一致指向 A(a 自述自身;b 报告对端 A)
    b.leader_switch.store(true, Ordering::Relaxed);
    let cfg = make_cfg((a.port, b.port));
    let ctx = Arc::new(AppCtx::new(cfg, Arc::new(SrvPool::new()), "ha-disc".into()));

    // 预置一个分片桶,验证 leader 变更会按分片失效连接池
    let bkt = ctx.srv_pool.get_or_create_bucket("0", "t0", "dbu", "db", Default::default());
    assert!(ctx.srv_pool.all_buckets().len() >= 1);

    // 1) 初始 leader = A(127.0.0.1:3306 所在成员)
    newproxy::ha::center::HaCenter::force_probe_async(ctx.clone(), "0".into(), "t0".into()).await;
    let st0 = ctx.ha.snapshot("0", "t0");
    eprintln!(
        "probe errors={} st={:?}",
        ctx.metrics.ha_probe_errors.get(),
        st0.map(|s| (s.leader.clone(), s.last_probe_errors, s.degraded))
    );
    let st = ctx.ha.snapshot("0", "t0").expect("state after probe");
    assert_eq!(st.leader.as_ref().map(|m| m.endpoint()).as_deref(), Some(format!("127.0.0.1:{}", a.port).as_str()));
    assert!(!st.degraded);
    assert_eq!(st.semisync_on, Some(true));
    // topology 覆盖已生效
    let topo = ctx.topology.get("0", "t0").expect("topology after probe");
    assert_eq!(
        topo.master.as_ref().map(|m| m.endpoint()).as_deref(),
        Some(format!("127.0.0.1:{}", a.port).as_str())
    );
    // followers = 成员减 leader(保留另一成员)
    assert_eq!(topo.slaves.len(), 1);
    assert_eq!(topo.slaves[0].endpoint(), format!("::1:{}", b.port));
    // 池已被失效(分片前缀桶清空)
    assert!(!ctx.srv_pool.all_buckets().iter().any(|(k, _)| k.starts_with("0.t0.")));
    let _ = bkt;

    // 2) raft 漂移:两个成员都自述 leader=B
    a.leader_switch.store(true, Ordering::Relaxed);
    b.leader_switch.store(false, Ordering::Relaxed);
    let changes_before = ctx.metrics.ha_leader_changes.get();
    newproxy::ha::center::HaCenter::force_probe_async(ctx.clone(), "0".into(), "t0".into()).await;
    let st = ctx.ha.snapshot("0", "t0").expect("state after switch");
    assert_eq!(
        st.leader.as_ref().map(|m| m.endpoint()).as_deref(),
        Some(format!("::1:{}", b.port).as_str())
    );
    assert_eq!(ctx.metrics.ha_leader_changes.get(), changes_before + 1);
    // 幂等探测不重复计数
    newproxy::ha::center::HaCenter::force_probe_async(ctx.clone(), "0".into(), "t0".into()).await;
    assert_eq!(ctx.metrics.ha_leader_changes.get(), changes_before + 1);

    b.stop().await;
    a.stop().await;
}

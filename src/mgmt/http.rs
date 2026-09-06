//! 管理 HTTP 服务(挂在 mng_port):Web 面板 + JSON API + Prometheus 指标 + 健康检查
//!
//! - 零新依赖:tokio 手写极简 HTTP/1.1(仅 GET/POST,无 body 解析)
//! - 鉴权:Basic Auth(配置 `mng_user` / `mng_password`,未配置则关闭鉴权)
//! - 端点:
//!   - `GET  /`              Web 管理面板(内嵌 HTML/JS,Canvas 手绘图表)
//!   - `GET  /metrics`       Prometheus 文本格式(监控系统采集)
//!   - `GET  /healthz`       存活探针(恒 200,不鉴权)
//!   - `GET  /readyz`        就绪探针(主端口/配置健康,不鉴权)
//!   - `GET  /api/status`     运行指标 JSON
//!   - `GET  /api/connections` 活跃连接列表
//!   - `GET  /api/sql`        Top SQL 统计
//!   - `GET  /api/pool`       后端连接池状态
//!   - `GET  /api/config`     配置摘要(不含任何密码)
//!   - `POST /api/kill?cid=N` kill 连接
//!   - `POST /api/reload`     热加载配置
//!
//! 管理端口故障不影响业务:server 以独立 task 运行,失败仅记日志。

use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::app::AppCtx;

/// 内嵌管理面板(单文件 HTML+JS+CSS)
const DASHBOARD_HTML: &str = include_str!("dashboard.html");
/// 内嵌登录页(未认证时 GET / 返回该页,替代浏览器 Basic Auth 弹窗)
const LOGIN_HTML: &str = include_str!("login.html");

/// 会话有效期(小时)
const SESSION_TTL_HOURS: u64 = 8;
/// 会话 cookie 名
const SESSION_COOKIE: &str = "newproxy_session";
/// 会话 cookie 前缀(避免每次 format 分配)
const SESSION_COOKIE_EQ: &str = "newproxy_session=";

/// 登录会话存储:token → 过期时间(内存态;服务重启即失效,可接受)
type SessionStore = std::sync::Arc<dashmap::DashMap<String, std::time::Instant>>;

/// 启动管理 HTTP 服务(阻塞;失败时调用方记日志即可,不阻断业务)
pub async fn serve(ctx: Arc<AppCtx>, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    info!("mgmt http listening on 0.0.0.0:{port}");
    let sessions: SessionStore = std::sync::Arc::new(dashmap::DashMap::new());
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                // 偶发 accept 错误(EMFILE/ECONNABORTED)继续监听,不终止服务
                warn!("mgmt http accept error: {e}");
                continue;
            }
        };
        let ctx = ctx.clone();
        let sessions = sessions.clone();
        tokio::spawn(async move {
            let _ = handle_conn(stream, ctx, sessions).await;
        });
    }
}

// ─── 连接处理 ───

/// 单连接处理:读请求行 + header + body(如有),认证,路由
async fn handle_conn(
    mut stream: TcpStream,
    ctx: Arc<AppCtx>,
    sessions: SessionStore,
) -> std::io::Result<()> {
    // 简化解析:一次读入请求头(限制 64KB,防止滥用)
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        // 请求头结束(空行)或超限
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 64 * 1024 {
            break;
        }
    }

    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    // 收集 header(小写 key)
    let mut headers: Vec<(String, String)> = Vec::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let auth = headers
        .iter()
        .find(|(k, _)| k == "authorization")
        .map(|(_, v)| v.clone());
    let cookie = headers
        .iter()
        .find(|(k, _)| k == "cookie")
        .map(|(_, v)| v.clone());
    // POST body:Content-Length 指定时读剩余字节(urlencoded 表单)
    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = Vec::new();
    if content_length > 0 && content_length <= 64 * 1024 {
        let have = buf.len().saturating_sub(head_end + 4);
        if have < content_length {
            let mut rest = vec![0u8; content_length - have];
            stream.read_exact(&mut rest).await?;
            body = rest;
        } else {
            body = buf[head_end + 4..head_end + 4 + content_length].to_vec();
        }
    }

    // 解析请求行:"METHOD /path?query HTTP/1.1"
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_uppercase();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };

    // 认证:Basic 头 或 会话 cookie 有效,任一即通过;未配置 mng_user 则免认证
    let authenticated =
        authorized(&ctx, auth.as_deref()) || session_valid(&ctx, &sessions, cookie.as_deref());

    // POST /login:免认证(本身用于建立会话)
    if path == "/login" && method == "POST" {
        let resp = do_login(&ctx, &sessions, &body);
        stream.write_all(&resp).await?;
        stream.flush().await?;
        return Ok(());
    }

    // GET /api/check:一键健康检测(async:需探测后端连通性)
    if path == "/api/check" && method == "GET" {
        let (status, ctype, body) = api_check(&ctx).await;
        let resp = response(status, ctype, body.as_bytes(), None);
        stream.write_all(&resp).await?;
        stream.flush().await?;
        return Ok(());
    }

    // POST /api/config/slow_query_ms:修改慢查询阈值配置并立即 reload 生效
    if path == "/api/config/slow_query_ms" && method == "POST" {
        let (status, ctype, body) = api_set_slow_query_ms(&ctx, &query);
        let resp = response(status, ctype, body.as_bytes(), None);
        stream.write_all(&resp).await?;
        stream.flush().await?;
        return Ok(());
    }

    // 健康探针不鉴权(编排平台无凭据)
    if path == "/healthz" || path == "/readyz" {
        let (status, ctype, body) = route(&ctx, &method, &path, &query, true);
        let resp = response(status, ctype, body.as_bytes(), None);
        stream.write_all(&resp).await?;
        stream.flush().await?;
        return Ok(());
    }

    // 其余端点:未认证
    if !authenticated {
        // 页面类路由返回登录页(而非 401,避免依赖浏览器 Basic Auth 弹窗)
        if path == "/" {
            let resp = response(200, "text/html; charset=utf-8", LOGIN_HTML.as_bytes(), None);
            stream.write_all(&resp).await?;
            stream.flush().await?;
            return Ok(());
        }
        let body = "401 Unauthorized: 请先登录 (GET /)".as_bytes();
        let resp = response(401, "text/plain; charset=utf-8", body, None);
        stream.write_all(&resp).await?;
        stream.flush().await?;
        return Ok(());
    }

    let (status, ctype, body) = route(&ctx, &method, &path, &query, authenticated);
    let resp = response(status, ctype, body.as_bytes(), None);
    stream.write_all(&resp).await?;
    stream.flush().await?;
    Ok(())
}

/// 组装 HTTP 响应(header 与 body 字节原样拼接,body 不做编码转换)
fn response(status: u16, ctype: &str, body: &[u8], extra_header: Option<&str>) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n",
        body.len()
    );
    if let Some(h) = extra_header {
        head.push_str(&format!("{h}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut resp = head.into_bytes();
    resp.extend_from_slice(body);
    resp
}

// ─── 路由 ───

fn route(
    ctx: &AppCtx,
    method: &str,
    path: &str,
    query: &str,
    _auth: bool,
) -> (u16, &'static str, String) {
    match (method, path) {
        ("GET", "/") => (200, "text/html; charset=utf-8", DASHBOARD_HTML.to_string()),
        ("GET", "/metrics") => (
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            render_metrics(ctx),
        ),
        ("GET", "/healthz") => (200, "text/plain; charset=utf-8", "ok".to_string()),
        ("GET", "/readyz") => readyz(ctx),
        ("GET", "/api/status") => (200, "application/json", api_status(ctx)),
        ("GET", "/api/connections") => (200, "application/json", api_connections(ctx)),
        ("GET", "/api/sql") => (200, "application/json", api_sql(ctx)),
        ("GET", "/api/slow") => (200, "application/json", api_slow(ctx, query)),
        ("POST", "/api/slow/clear") => api_slow_clear(ctx),
        ("GET", "/api/recent") => (200, "application/json", api_recent(ctx, query)),
        ("POST", "/api/recent/clear") => api_recent_clear(ctx),
        ("GET", "/api/parsefailures") => (200, "application/json", api_parse_failures(ctx)),
        ("GET", "/api/backenderrors") => (200, "application/json", api_backend_errors(ctx)),
        ("GET", "/api/pool") => (200, "application/json", api_pool(ctx)),
        ("GET", "/api/config") => (200, "application/json", api_config(ctx)),
        ("POST", "/api/kill") => api_kill(ctx, query),
        ("POST", "/api/reload") => api_reload(ctx),
        ("GET", "/favicon.ico") => (404, "text/plain", "not found".to_string()),
        _ => (404, "text/plain; charset=utf-8", "not found".to_string()),
    }
}

// ─── 登录与会话 ───

/// 会话是否有效(cookie 中的 token 存在且未过期);顺手清理过期项
fn session_valid(ctx: &AppCtx, sessions: &SessionStore, cookie: Option<&str>) -> bool {
    let Some(token) = cookie.and_then(|c| {
        c.split(';')
            .find_map(|kv| kv.trim().strip_prefix(SESSION_COOKIE_EQ))
    }) else {
        return false;
    };
    let Some(expires) = sessions.get(token).map(|e| *e) else {
        return false;
    };
    let now = std::time::Instant::now();
    if expires <= now {
        sessions.remove(token);
        return false;
    }
    let _ = ctx; // 会话与全局配置解耦,过期清理即可
    true
}

/// 处理 POST /login:校验凭据 → 建会话 → Set-Cookie(302 回首页)
fn do_login(ctx: &AppCtx, sessions: &SessionStore, body: &[u8]) -> Vec<u8> {
    let form = String::from_utf8_lossy(body);
    let field = |name: &str| -> String {
        form.split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
            .map(|v| url_decode(v))
            .unwrap_or_default()
    };
    let user = field("user");
    let pass = field("password");

    let cfg = ctx.load_config();
    let ok = match (cfg.mng_user.as_deref(), cfg.mng_password.as_deref()) {
        // 未配置 mng_user → 免认证,直接发会话
        (None, _) => true,
        (Some(u), p) => {
            ct_eq(user.as_bytes(), u.as_bytes())
                && ct_eq(pass.as_bytes(), p.unwrap_or("").as_bytes())
        }
    };

    if !ok {
        return response(
            401,
            "text/plain; charset=utf-8",
            "用户名或密码错误".as_bytes(),
            None,
        );
    }

    // 生成 32 字节随机 token
    let mut token = [0u8; 32];
    for b in token.iter_mut() {
        *b = rand::random::<u8>();
    }
    let token = hex(&token);
    sessions.insert(
        token.clone(),
        std::time::Instant::now() + std::time::Duration::from_secs(SESSION_TTL_HOURS * 3600),
    );

    // 302 重定向到首页,带 HttpOnly 会话 cookie
    let body = b"redirect";
    let resp = format!(
        "HTTP/1.1 302 Found\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nLocation: /\r\nSet-Cookie: {SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nredirect",
        body.len()
    );
    resp.into_bytes()
}

/// 简单 hex 编码
fn hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(data.len() * 2);
    for &b in data {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// urlencoded 表单值解码(仅处理 %XX 与 +)
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 2;
                } else {
                    out.push(bytes[i]);
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ─── 鉴权(Basic Auth)───

fn authorized(ctx: &AppCtx, auth_header: Option<&str>) -> bool {
    let cfg = ctx.load_config();
    // 配置文件中未配置 mng_user → 关闭鉴权(内网/测试部署);生产必须显式配置。
    let Some(user) = cfg.mng_user.as_deref() else {
        return true;
    };
    let pass = cfg.mng_password.as_deref().unwrap_or("");
    let Some(header) = auth_header else {
        return false;
    };
    let Some(rest) = header.strip_prefix("Basic ") else {
        return false;
    };
    let Some(decoded) = base64_decode(rest) else {
        return false;
    };
    let expected = format!("{user}:{pass}");
    ct_eq(&decoded, expected.as_bytes())
}

/// 手写 base64 解码(标准 alphabet,容忍填充与空白)
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut val: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::new();
    for &c in s.trim().as_bytes() {
        if c == b'=' {
            break;
        }
        let d = TABLE.iter().position(|&t| t == c)? as u32;
        val = (val << 6) | d;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((val >> bits) as u8);
        }
    }
    Some(out)
}

/// 常量时间比较(防时序攻击)
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

// ─── 健康检查 ───

fn readyz(ctx: &AppCtx) -> (u16, &'static str, String) {
    let cfg = ctx.load_config();
    // 主端口 > 0 且配置已加载即视为就绪(主 listener 生命周期由 main 保证)
    if cfg.port == 0 {
        return (
            503,
            "text/plain; charset=utf-8",
            "port not configured".to_string(),
        );
    }
    // 后端拓扑存在性:无任何集群则告警(但仍视为就绪,避免探针误杀空配置部署)
    let clusters = cfg.clusters.len();
    (
        200,
        "text/plain; charset=utf-8",
        format!("ok (clusters={clusters})"),
    )
}

// ─── API 实现 ───

fn api_status(ctx: &AppCtx) -> String {
    let s = ctx.metrics.summary();
    // 阶段平均耗时(µs):累计/查询数,展示请求时间花在哪个环节
    let q = s.queries_total.max(1);
    // 进程负载(扩缩容参考;非 Linux 平台为 null)
    let mut meter = ctx.proc_meter.lock();
    let process = json!({
        "cpu_percent": meter.cpu_percent(),
        "memory_bytes": meter.memory_bytes(),
        "fd_count": meter.fd_count(),
        "connections_active": s.connections_active,
        "uptime_secs": meter.uptime_secs(),
    });
    json!({
        "connections_total": s.connections_total,
        "connections_active": s.connections_active,
        "connections_rejected": s.connections_rejected,
        "queries_total": s.queries_total,
        "queries_errors": s.queries_errors,
        "queries_slow": s.queries_slow,
        "parse_failures": ctx.metrics.parse_failures.get(),
        "backend_errors": ctx.metrics.backend_errors.get(),
        "packets_received": crate::proto::codec::packets_read_total(),
        "packets_sent": crate::proto::codec::packets_written_total(),
        "bytes_received": s.bytes_received,
        "bytes_sent": s.bytes_sent,
        "pool_acquires": s.pool_acquires,
        "pool_acquire_fails": s.pool_acquire_fails,
        "stage_avg_us": {
            "interval": s.stage_interval_us / q,
            "parse": s.stage_parse_us / q,
            "setup": s.stage_setup_us / q,
            "send": s.stage_send_us / q,
            "forward": s.stage_forward_us / q,
        },
        "shards": ctx
            .metrics
            .shard_queries_snapshot()
            .into_iter()
            .map(|(shard, n)| json!({ "shard": shard, "queries": n }))
            .collect::<Vec<_>>(),
        // 分片阶段平均耗时(µs):平均 = 累计/该分片查询数,供面板按分片对比
        "shard_stages": ctx
            .metrics
            .shard_stages_snapshot()
            .into_iter()
            .map(|(shard, queries, a)| {
                let q = queries.max(1);
                json!({
                    "shard": shard,
                    "queries": queries,
                    "avg_us": {
                        "interval": a[0] / q,
                        "parse": a[1] / q,
                        "setup": a[2] / q,
                        "send": a[3] / q,
                        "forward": a[4] / q,
                    }
                })
            })
            .collect::<Vec<_>>(),
        // 按库流量统计(db → [recv_bytes, sent_bytes, recv_pkts, sent_pkts],
        // 按总流量降序;供面板"按库流量"表)
        "db_traffic": ctx
            .metrics
            .db_traffic_snapshot()
            .into_iter()
            .map(|(db, a)| {
                json!({
                    "db": db,
                    "recv_bytes": a[0],
                    "sent_bytes": a[1],
                    "recv_pkts": a[2],
                    "sent_pkts": a[3],
                })
            })
            .collect::<Vec<_>>(),
        // 按后端 MySQL 节点流量(cluster.tablet → recv/sent 字节与包,按总流量降序;
        // 面板"后端节点流量"图数据源)
        "node_traffic": ctx
            .metrics
            .node_traffic_snapshot()
            .into_iter()
            .map(|(node, a)| {
                json!({
                    "node": node,
                    "recv_bytes": a[0],
                    "sent_bytes": a[1],
                    "recv_pkts": a[2],
                    "sent_pkts": a[3],
                })
            })
            .collect::<Vec<_>>(),
        "process": process,
    })
    .to_string()
}

/// 解析过滤参数:?sql= &db= &user= &shard=(子串,不区分大小写,均可不填)
fn parse_filters(query: &str) -> (String, String, String, String) {
    let mut sql = String::new();
    let mut db = String::new();
    let mut user = String::new();
    let mut shard = String::new();
    for kv in query.split('&') {
        if let Some(v) = kv.strip_prefix("sql=") {
            sql = v.to_ascii_lowercase();
        } else if let Some(v) = kv.strip_prefix("db=") {
            db = v.to_ascii_lowercase();
        } else if let Some(v) = kv.strip_prefix("user=") {
            user = v.to_ascii_lowercase();
        } else if let Some(v) = kv.strip_prefix("shard=") {
            shard = v.to_ascii_lowercase();
        }
    }
    (sql, db, user, shard)
}

/// GET /api/parsefailures:最近 SQL 解析失败(命令包解析失败/非 UTF-8 等,≤200 条)
fn api_parse_failures(ctx: &AppCtx) -> String {
    let list: Vec<_> = ctx
        .metrics
        .parse_failures_snapshot()
        .into_iter()
        .map(|f| json!({ "ts": f.ts, "reason": f.reason, "sql": f.sql }))
        .collect();
    json!({ "failures": list, "total": ctx.metrics.parse_failures.get() }).to_string()
}

/// GET /api/backenderrors:后端错误统计(总数 + 按错误码聚合 + 最近明细)。
/// 面板"后端错误统计"数据源:top 表按码聚合(次数/占比/最近),明细表最近 200 条
fn api_backend_errors(ctx: &AppCtx) -> String {
    let stats: Vec<_> = ctx
        .metrics
        .backend_error_stats_snapshot()
        .into_iter()
        .map(|(code, s)| {
            json!({
                "code": code,
                "count": s.count,
                "last_ts": s.last_ts,
                "last_sql": s.last_sql,
            })
        })
        .collect();
    let recent: Vec<_> = ctx
        .metrics
        .backend_errors_snapshot()
        .into_iter()
        .map(|e| json!({ "ts": e.ts, "code": e.code, "sql": e.sql }))
        .collect();
    json!({
        "total": ctx.metrics.backend_errors.get(),
        "stats": stats,
        "recent": recent,
    })
    .to_string()
}

/// GET /api/recent:最近所有查询(含单次 5 阶段耗时,新→旧,最多 500 条)。
/// 支持 `?sql= &db= &user= &shard=` 过滤(子串,不区分大小写),查看单独某条 SQL/某库/某用户/某分片的阶段耗时
fn api_recent(ctx: &AppCtx, query: &str) -> String {
    let (fsql, fdb, fuser, fshard) = parse_filters(query);
    let list: Vec<_> = ctx
        .metrics
        .recent_queries_snapshot()
        .into_iter()
        .filter(|q| {
            (fsql.is_empty() || q.sql.to_ascii_lowercase().contains(&fsql))
                && (fdb.is_empty() || q.db.to_ascii_lowercase().contains(&fdb))
                && (fuser.is_empty() || q.puser.to_ascii_lowercase().contains(&fuser))
                && (fshard.is_empty() || q.shard.to_ascii_lowercase().contains(&fshard))
        })
        .map(|q| {
            json!({
                "ts": q.ts,
                "sql": q.sql,
                "db": q.db,
                "puser": q.puser,
                "shard": q.shard,
                "elapsed_us": q.elapsed_us,
                "slow": q.slow,
                "interval_us": q.interval_us,
                "parse_us": q.parse_us,
                "setup_us": q.setup_us,
                "send_us": q.send_us,
                "forward_us": q.forward_us,
            })
        })
        .collect();
    json!({ "recent": list }).to_string()
}

/// GET /api/slow:最近慢查询(含 5 阶段耗时,新→旧,最多 200 条)。
/// 支持 `?sql= &db= &user= &shard=` 过滤
fn api_slow(ctx: &AppCtx, query: &str) -> String {
    let (fsql, fdb, fuser, fshard) = parse_filters(query);
    let list: Vec<_> = ctx
        .metrics
        .slow_queries_snapshot()
        .into_iter()
        .filter(|q| {
            (fsql.is_empty() || q.sql.to_ascii_lowercase().contains(&fsql))
                && (fdb.is_empty() || q.db.to_ascii_lowercase().contains(&fdb))
                && (fuser.is_empty() || q.puser.to_ascii_lowercase().contains(&fuser))
                && (fshard.is_empty() || q.shard.to_ascii_lowercase().contains(&fshard))
        })
        .map(|q| {
            json!({
                "ts": q.ts,
                "sql": q.sql,
                "db": q.db,
                "puser": q.puser,
                "shard": q.shard,
                "elapsed_us": q.elapsed_us,
                "interval_us": q.interval_us,
                "parse_us": q.parse_us,
                "setup_us": q.setup_us,
                "send_us": q.send_us,
                "forward_us": q.forward_us,
            })
        })
        .collect();
    json!({ "slow": list }).to_string()
}

/// POST /api/slow/clear:清空慢查询缓冲(面板"清除"按钮)
fn api_slow_clear(ctx: &AppCtx) -> (u16, &'static str, String) {
    ctx.metrics.clear_slow_queries();
    (200, "application/json", json!({ "ok": true }).to_string())
}

/// POST /api/recent/clear:清空最近查询缓冲(查询统计面板"清除"按钮)
fn api_recent_clear(ctx: &AppCtx) -> (u16, &'static str, String) {
    ctx.metrics.clear_recent_queries();
    (200, "application/json", json!({ "ok": true }).to_string())
}

fn api_connections(ctx: &AppCtx) -> String {
    let list: Vec<_> = ctx
        .connections
        .iter()
        .map(|e| {
            let h = e.value();
            json!({
                "cid": *e.key(),
                "addr": h.peer_addr.to_string(),
                "user": h.user.read().clone(),
                "db": h.db.read().clone(),
                "state": h.state.read().clone(),
                "backends": h.backends.read().clone(),
                "uptime_secs": h.started_at.elapsed().as_secs(),
            })
        })
        .collect();
    json!({ "connections": list }).to_string()
}

fn api_sql(ctx: &AppCtx) -> String {
    let list: Vec<_> = ctx
        .metrics
        .top_queries(100)
        .into_iter()
        .map(|((tpl, shard), s)| {
            json!({
                "template": tpl,
                "shard": shard,
                "count": s.count,
                "total_time_us": s.total_time_us,
                "max_time_us": s.max_time_us,
                "avg_us": s.total_time_us.checked_div(s.count).unwrap_or(0),
                "rows_sent": s.rows_sent,
            })
        })
        .collect();
    json!({ "top": list }).to_string()
}

fn api_pool(ctx: &AppCtx) -> String {
    let buckets: Vec<_> = ctx
        .srv_pool
        .all_buckets()
        .into_iter()
        .map(|(key, b)| {
            // 键格式 "cluster.tablet.user.db":前三段为集群/分片/用户,其余为库(库名可含 .)
            let mut it = key.splitn(4, '.');
            let cluster = it.next().unwrap_or("").to_string();
            let tablet = it.next().unwrap_or("").to_string();
            let user = it.next().unwrap_or("").to_string();
            let db = it.next().unwrap_or("").to_string();
            json!({
                "cluster": cluster,
                "tablet": tablet,
                "user": user,
                "db": db,
                "shard": format!("{}.{}", cluster, tablet),
                "write_queue": b.write.lock().len(),
                "read_queue": b.read.lock().len(),
                "max_size": b.cfg.max_size,
                "min_idle": b.cfg.min_idle,
                "max_serve_times": b.cfg.max_serve_times,
                "max_idle_secs": b.cfg.max_idle_secs,
                "stale_on_acquire_secs": b.cfg.stale_on_acquire_secs,
            })
        })
        .collect();
    json!({ "buckets": buckets }).to_string()
}

fn api_config(ctx: &AppCtx) -> String {
    let cfg = ctx.load_config();
    // 摘要输出:绝不包含任何密码/凭据
    let tablets_total: usize = cfg.clusters.values().map(|c| c.tablets.len()).sum();
    // 拓扑明细:每个集群的分片及其 master 地址(供一键检测遍历所有分片/集群)
    let topology: Vec<serde_json::Value> = cfg
        .clusters
        .iter()
        .map(|(cid, c)| {
            json!({
                "cluster_id": cid,
                "name": c.name,
                "tablets": c.tablets.iter().map(|t| {
                    let master = t.groups.iter().find_map(|g| g.master.as_ref());
                    json!({
                        "tablet_id": t.tablet_id,
                        "master": master.map(|db| json!({ "host": db.host, "port": db.port })),
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "port": cfg.port,
        "mng_port": cfg.mng_port,
        "log_dir": cfg.log_dir,
        "log_level": cfg.log_level.as_str(),
        "clusters": cfg.clusters.len(),
        "tablets": tablets_total,
        "topology": topology,
        "db_users": cfg.db_users.len(),
        "product_users": cfg.product_users.len(),
        "slow_query_ms": cfg.slow_query_ms,
        "plan_bindings": cfg
            .plan_bindings
            .iter()
            .map(|b| json!({ "sql_pattern": b.sql_pattern, "hint": b.hint }))
            .collect::<Vec<_>>(),
        "server_version": ctx.server_version,
    })
    .to_string()
}

fn api_kill(ctx: &AppCtx, query: &str) -> (u16, &'static str, String) {
    let cid: u32 = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("cid="))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    match ctx.kill_connection(cid) {
        Ok(()) => (
            200,
            "application/json",
            json!({ "ok": true, "cid": cid }).to_string(),
        ),
        Err(msg) => (
            400,
            "application/json",
            json!({ "ok": false, "error": msg }).to_string(),
        ),
    }
}

fn api_reload(ctx: &AppCtx) -> (u16, &'static str, String) {
    match ctx.reload_config() {
        Ok(diff) => (
            200,
            "application/json",
            json!({ "ok": true, "diff": diff }).to_string(),
        ),
        Err(msg) => (
            400,
            "application/json",
            json!({ "ok": false, "error": msg }).to_string(),
        ),
    }
}

// ─── Prometheus 文本渲染(零依赖手写)───

fn render_metrics(ctx: &AppCtx) -> String {
    let m = &ctx.metrics;
    let s = m.summary();
    let mut out = String::with_capacity(2048);
    push_counter(
        &mut out,
        "newproxy_connections_total",
        "Total accepted connections",
        s.connections_total,
    );
    out.push_str("# HELP newproxy_connections_active Active connections\n# TYPE newproxy_connections_active gauge\n");
    out.push_str(&format!(
        "newproxy_connections_active {}\n",
        s.connections_active
    ));
    push_counter(
        &mut out,
        "newproxy_connections_rejected_total",
        "Rejected connections (auth/IP)",
        s.connections_rejected,
    );
    push_counter(
        &mut out,
        "newproxy_queries_total",
        "Total queries",
        s.queries_total,
    );
    push_counter(
        &mut out,
        "newproxy_queries_errors_total",
        "Queries with errors",
        s.queries_errors,
    );
    push_counter(
        &mut out,
        "newproxy_sql_parse_failures_total",
        "SQL parse failures (bad command packet / non-UTF-8)",
        ctx.metrics.parse_failures.get(),
    );
    push_counter(
        &mut out,
        "newproxy_backend_errors_total",
        "Backend ERR responses (syntax/perm/etc)",
        ctx.metrics.backend_errors.get(),
    );
    push_counter(
        &mut out,
        "newproxy_queries_slow_total",
        "Slow queries (elapsed above config slow_query_ms threshold)",
        s.queries_slow,
    );
    push_counter(
        &mut out,
        "newproxy_traffic_received_bytes_total",
        "Bytes received from clients",
        s.bytes_received,
    );
    push_counter(
        &mut out,
        "newproxy_traffic_sent_bytes_total",
        "Bytes sent to clients",
        s.bytes_sent,
    );
    push_counter(
        &mut out,
        "newproxy_pool_acquires_total",
        "Backend pool acquires",
        s.pool_acquires,
    );
    push_counter(
        &mut out,
        "newproxy_pool_acquire_fails_total",
        "Backend pool acquire failures",
        s.pool_acquire_fails,
    );

    // 进程负载(扩缩容参考;非 Linux 平台为 NaN 表示不可用)
    {
        let mut meter = ctx.proc_meter.lock();
        out.push_str(
            "# HELP newproxy_process_cpu_percent Process CPU utilization (%)
# TYPE newproxy_process_cpu_percent gauge
",
        );
        out.push_str(&format!(
            "newproxy_process_cpu_percent {:.2}\n",
            meter.cpu_percent().unwrap_or(f64::NAN)
        ));
        out.push_str("# HELP newproxy_process_memory_bytes Process RSS memory bytes\n# TYPE newproxy_process_memory_bytes gauge\n");
        out.push_str(&format!(
            "newproxy_process_memory_bytes {}\n",
            meter.memory_bytes().map_or(-1i64, |v| v as i64)
        ));
        out.push_str("# HELP newproxy_process_fd_count Open file descriptors\n# TYPE newproxy_process_fd_count gauge\n");
        out.push_str(&format!(
            "newproxy_process_fd_count {}\n",
            meter.fd_count().map_or(-1i64, |v| v as i64)
        ));
        out.push_str("# HELP newproxy_process_uptime_seconds Process uptime seconds\n# TYPE newproxy_process_uptime_seconds gauge\n");
        out.push_str(&format!(
            "newproxy_process_uptime_seconds {}\n",
            meter.uptime_secs()
        ));
    }

    // SQL 模板统计(有界:≥1024 条后不再登记新模板,见 Metrics::record_query)
    out.push_str(
        "# HELP newproxy_sql_total Queries per SQL template\n# TYPE newproxy_sql_total counter\n",
    );
    out.push_str("# HELP newproxy_sql_duration_seconds_total Cumulative duration per template\n# TYPE newproxy_sql_duration_seconds_total counter\n");
    out.push_str("# HELP newproxy_sql_rows_total Rows sent per template\n# TYPE newproxy_sql_rows_total counter\n");
    out.push_str("# HELP newproxy_sql_max_duration_seconds Max duration per template\n# TYPE newproxy_sql_max_duration_seconds gauge\n");
    for ((tpl, shard), st) in m.sql_stats_snapshot() {
        let label = prom_label(&tpl);
        let slabel = prom_label(&shard);
        out.push_str(&format!(
            "newproxy_sql_total{{template=\"{label}\",shard=\"{slabel}\"}} {}\n",
            st.count
        ));
        out.push_str(&format!(
            "newproxy_sql_duration_seconds_total{{template=\"{label}\",shard=\"{slabel}\"}} {:.6}\n",
            st.total_time_us as f64 / 1e6
        ));
        out.push_str(&format!(
            "newproxy_sql_rows_total{{template=\"{label}\",shard=\"{slabel}\"}} {}\n",
            st.rows_sent
        ));
        out.push_str(&format!(
            "newproxy_sql_max_duration_seconds{{template=\"{label}\",shard=\"{slabel}\"}} {:.6}\n",
            st.max_time_us as f64 / 1e6
        ));
    }

    // ── 查询耗时直方图(Prometheus histogram;桶内存为非累计,此处转累计)──
    out.push_str(
        "# HELP newproxy_queries_duration_seconds Query execution latency distribution\n# TYPE newproxy_queries_duration_seconds histogram\n",
    );
    {
        let buckets = m.latency_histogram.snapshot();
        let mut cum: u64 = 0;
        for (i, b) in buckets.iter().enumerate() {
            cum += b;
            let le = match crate::metric::LATENCY_EDGES_US.get(i) {
                Some(&e) if e != u64::MAX => format!("{}", e as f64 / 1e6),
                _ => "+Inf".to_string(),
            };
            out.push_str(&format!(
                "newproxy_queries_duration_seconds_bucket{{le=\"{le}\"}} {cum}\n"
            ));
        }
        out.push_str(&format!(
            "newproxy_queries_duration_seconds_sum {:.6}\nnewproxy_queries_duration_seconds_count {}\n",
            m.queries_elapsed_us.get() as f64 / 1e6,
            s.queries_total,
        ));
    }

    // ── 分片维度:查询数 / 5 阶段耗时合计 / 慢查询数(与 /api/status 同源)──
    out.push_str(
        "# HELP newproxy_shard_queries_total Queries per shard\n# TYPE newproxy_shard_queries_total counter\n",
    );
    out.push_str("# HELP newproxy_shard_duration_seconds_total Cumulative 5-stage duration per shard\n# TYPE newproxy_shard_duration_seconds_total counter\n");
    for (shard, qn, stages) in m.shard_stages_snapshot() {
        let slabel = prom_label(&shard);
        let dur_us: u64 = stages.iter().sum();
        out.push_str(&format!(
            "newproxy_shard_queries_total{{shard=\"{slabel}\"}} {qn}\n"
        ));
        out.push_str(&format!(
            "newproxy_shard_duration_seconds_total{{shard=\"{slabel}\"}} {:.6}\n",
            dur_us as f64 / 1e6
        ));
    }
    out.push_str(
        "# HELP newproxy_shard_slow_queries_total Slow queries per shard\n# TYPE newproxy_shard_slow_queries_total counter\n",
    );
    for (shard, n) in m.shard_slow_snapshot() {
        let slabel = prom_label(&shard);
        out.push_str(&format!(
            "newproxy_shard_slow_queries_total{{shard=\"{slabel}\"}} {n}\n"
        ));
    }

    // ── 按库 / 按后端节点流量(客户端方向;与 /api/status 同源)──
    out.push_str("# HELP newproxy_db_traffic_received_bytes_total Bytes received from clients per database\n# TYPE newproxy_db_traffic_received_bytes_total counter\n");
    out.push_str("# HELP newproxy_db_traffic_sent_bytes_total Bytes sent to clients per database\n# TYPE newproxy_db_traffic_sent_bytes_total counter\n");
    for (db, a) in m.db_traffic_snapshot() {
        let l = prom_label(&db);
        out.push_str(&format!(
            "newproxy_db_traffic_received_bytes_total{{db=\"{l}\"}} {}\n",
            a[0]
        ));
        out.push_str(&format!(
            "newproxy_db_traffic_sent_bytes_total{{db=\"{l}\"}} {}\n",
            a[1]
        ));
    }
    out.push_str("# HELP newproxy_node_traffic_received_bytes_total Bytes received from clients per backend node\n# TYPE newproxy_node_traffic_received_bytes_total counter\n");
    out.push_str("# HELP newproxy_node_traffic_sent_bytes_total Bytes sent to clients per backend node\n# TYPE newproxy_node_traffic_sent_bytes_total counter\n");
    for (node, a) in m.node_traffic_snapshot() {
        let l = prom_label(&node);
        out.push_str(&format!(
            "newproxy_node_traffic_received_bytes_total{{node=\"{l}\"}} {}\n",
            a[0]
        ));
        out.push_str(&format!(
            "newproxy_node_traffic_sent_bytes_total{{node=\"{l}\"}} {}\n",
            a[1]
        ));
    }

    // ── 后端错误按错误码拆分(总计数已在上方输出;此处明细与总计数一致)──
    for (code, st) in m.backend_error_stats_snapshot() {
        let l = prom_label(&code);
        out.push_str(&format!(
            "newproxy_backend_errors_total{{code=\"{l}\"}} {}\n",
            st.count
        ));
    }

    // ── 连接池空闲连接(遍历桶队列;role: master=写队列, slave=读队列)──
    out.push_str("# HELP newproxy_pool_idle_connections Idle backend connections per pool bucket\n# TYPE newproxy_pool_idle_connections gauge\n");
    for (key, b) in ctx.srv_pool.all_buckets() {
        let mut it = key.splitn(4, '.');
        let c = prom_label(it.next().unwrap_or(""));
        let t = prom_label(it.next().unwrap_or(""));
        let u = prom_label(it.next().unwrap_or(""));
        let d = prom_label(it.next().unwrap_or(""));
        let base = format!(
            "newproxy_pool_idle_connections{{cluster=\"{c}\",tablet=\"{t}\",user=\"{u}\",db=\"{d}\""
        );
        out.push_str(&format!("{base},role=\"master\"}} {}\n", b.write.lock().len()));
        out.push_str(&format!("{base},role=\"slave\"}} {}\n", b.read.lock().len()));
    }

    // ── 前端连接按状态(Auth/Command)──
    out.push_str(
        "# HELP newproxy_connections_by_state Active client connections by auth state\n# TYPE newproxy_connections_by_state gauge\n",
    );
    let mut states: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for e in ctx.connections.iter() {
        *states.entry(e.value().state.read().clone()).or_default() += 1;
    }
    for (st, n) in states {
        out.push_str(&format!(
            "newproxy_connections_by_state{{state=\"{}\"}} {n}\n",
            prom_label(&st)
        ));
    }

    out
}

/// Prometheus label 值转义(反斜杠/引号/换行)
fn prom_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// 输出一个 counter 指标(HELP + TYPE + 值)
fn push_counter(out: &mut String, name: &str, help: &str, v: u64) {
    out.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        assert_eq!(base64_decode("YWRtaW46YWRtaW4=").unwrap(), b"admin:admin");
        assert_eq!(base64_decode("dXNlcjpwYXNz").unwrap(), b"user:pass");
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());
        assert!(base64_decode("!!!").is_none());
    }

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
    }

    #[test]
    fn prom_label_escapes() {
        assert_eq!(prom_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(prom_label("plain"), "plain");
    }

    #[test]
    fn response_format() {
        let r = response(200, "text/plain", b"ok", None);
        let rs = String::from_utf8_lossy(&r);
        assert!(rs.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(rs.contains("Content-Length: 2\r\n"));
        assert!(rs.ends_with("ok"));
    }

    #[test]
    fn route_unknown() {
        // 需要 ctx;此处仅验证方法/路径分发逻辑的 404 分支(空 ctx 不可构造,
        // 直接验证 response 构造与 401 头)
        let r = response(
            401,
            "text/plain",
            b"x",
            Some("WWW-Authenticate: Basic realm=\"t\""),
        );
        let rs = String::from_utf8_lossy(&r);
        assert!(rs.contains("WWW-Authenticate: Basic realm=\"t\""));
    }

    // ─── API 处理函数(直接以 AppCtx 单测,无需起服务)───

    fn test_ctx(tag: &str) -> Arc<AppCtx> {
        let conf = format!(
            "[MySQL_Proxy_Layer]\nport=4051\nmng_port=9111\nmng_user=admin\nmng_password=admin\nmax_threads=4\nlog_dir=logs\nlog_level=info\n\
             [Cluster_0]\nname=test_cluster\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n\
             [DB_User_dbu]\ndb_username=root\ndb_password=secret\ncluster_name=test_cluster\n\
             [Product_User_pu]\nusername=u\npassword=p\ndb_username=root\ncluster_name=test_cluster\n"
        );
        let dir = std::env::temp_dir().join(format!("newproxy-http-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.conf");
        std::fs::write(&path, conf).unwrap();
        let ctx = Arc::new(AppCtx::new(
            crate::config::load_config(&path).unwrap(),
            Arc::new(crate::pool::backend::SrvPool::new()),
            path.to_string_lossy().to_string(),
        ));
        ctx
    }

    fn populate(ctx: &Arc<AppCtx>) {
        let m = &ctx.metrics;
        m.record_query("SELECT * FROM users WHERE id = ?", "0.t0", 500, 3, 200_000);
        m.record_query("SELECT SLEEP(?)", "0.t0", 2_000_000, 0, 200_000);
        m.record_query_stages("0.t0", 10, 20, 30, 40, 50);
        m.record_parse_failure("命令包解析失败", "SELEC\u{fffd}T");
        m.record_backend_error("1064", "SELEC * FROM t");
        m.record_recent_query(crate::metric::QueryRecord {
            ts: 1,
            sql: "SELECT 1".into(),
            db: "test".into(),
            puser: "u".into(),
            shard: "0.t0".into(),
            elapsed_us: 100,
            slow: false,
            interval_us: 0,
            parse_us: 0,
            setup_us: 0,
            send_us: 0,
            forward_us: 0,
        });
        m.record_slow_query(crate::metric::SlowQuery {
            ts: 2,
            sql: "SELECT SLEEP(2)".into(),
            db: "test".into(),
            puser: "u".into(),
            shard: "0.t0".into(),
            elapsed_us: 2_000_000,
            interval_us: 0,
            parse_us: 0,
            setup_us: 0,
            send_us: 0,
            forward_us: 0,
        });
    }

    #[test]
    fn parse_filters_all_fields() {
        let (sql, db, user, shard) = parse_filters("sql=SELECT&db=test&user=u&shard=0.t1");
        assert_eq!(sql, "select");
        assert_eq!(db, "test");
        assert_eq!(user, "u");
        assert_eq!(shard, "0.t1");
        let (sql, db, user, shard) = parse_filters("");
        assert_eq!(
            (sql, db, user, shard),
            (String::new(), String::new(), String::new(), String::new())
        );
    }

    #[test]
    fn api_status_and_shards() {
        let ctx = test_ctx("status");
        populate(&ctx);
        let body = api_status(&ctx);
        assert!(body.contains("\"queries_total\":2"), "got: {body}");
        assert!(body.contains("\"shards\""), "got: {body}");
        assert!(body.contains("\"0.t0\""), "got: {body}");
        assert!(body.contains("\"shard_stages\""), "got: {body}");
        assert!(body.contains("\"stage_avg_us\""), "got: {body}");
        assert!(body.contains("\"process\""), "got: {body}");
    }

    #[test]
    fn api_connections_and_kill() {
        let ctx = test_ctx("conn");
        let addr: std::net::SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let cid = ctx.register_connection(100, addr).0;
        // 注意 register_connection 返回 handle;cid 参数由调用方指定
        let _ = cid;
        let body = api_connections(&ctx);
        assert!(body.contains("\"cid\":100"), "got: {body}");
        // kill 有效/无效 cid
        let (code, _, body) = api_kill(&ctx, "cid=100");
        assert_eq!(code, 200);
        assert!(body.contains("\"ok\":true"), "got: {body}");
        let (code, _, body) = api_kill(&ctx, "cid=999");
        assert!(code != 200);
        assert!(body.contains("\"ok\":false"), "got: {body}");
    }

    #[test]
    fn api_sql_pool_failures() {
        let ctx = test_ctx("sql");
        populate(&ctx);
        let body = api_sql(&ctx);
        assert!(body.contains("\"top\""), "got: {body}");
        assert!(body.contains("users"), "got: {body}");
        // 分片与阶段维度
        let body = api_parse_failures(&ctx);
        assert!(body.contains("\"total\":1"), "got: {body}");
        assert!(body.contains("命令包解析失败"), "got: {body}");
        // 后端错误统计(按码聚合 + 最近明细,不再混入解析失败)
        let body = api_backend_errors(&ctx);
        assert!(body.contains("\"total\":1"), "got: {body}");
        assert!(body.contains("\"code\":\"1064\""), "got: {body}");
        assert!(body.contains("\"count\":1"), "got: {body}");
        assert!(body.contains("\"stats\""), "got: {body}");
        assert!(body.contains("\"recent\""), "got: {body}");
        assert!(
            !body.contains("命令包解析失败"),
            "后端错误不应包含解析失败: {body}"
        );
        // 池
        let body = api_pool(&ctx);
        assert!(body.contains("\"buckets\""), "got: {body}");
        // 慢查询 / 最近查询(含过滤)
        let body = api_slow(&ctx, "");
        assert!(body.contains("SLEEP"), "got: {body}");
        let body = api_slow(&ctx, "db=test&shard=0");
        assert!(body.contains("SLEEP"), "过滤后仍应命中: {body}");
        let body = api_slow(&ctx, "db=none");
        // 只断言语义(慢查询数组为空),避免把 JSON 序列化字段顺序当 API 契约。
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            v["slow"].as_array().unwrap().is_empty(),
            "不匹配的过滤应为空: {body}"
        );
        let body = api_recent(&ctx, "user=u");
        assert!(body.contains("SELECT 1"), "got: {body}");
        // 清除
        let (code, _, body) = api_slow_clear(&ctx);
        assert_eq!(code, 200);
        assert!(body.contains("\"ok\":true"));
        let (code, _, _) = api_recent_clear(&ctx);
        assert_eq!(code, 200);
    }

    #[test]
    fn api_config_fields() {
        let ctx = test_ctx("cfg");
        let body = api_config(&ctx);
        assert!(body.contains("\"clusters\":1"), "got: {body}");
        assert!(body.contains("\"tablets\":1"), "got: {body}");
        assert!(body.contains("\"topology\""), "got: {body}");
        assert!(body.contains("\"server_version\""), "got: {body}");
        assert!(body.contains("\"mng_port\":9111"), "got: {body}");
        assert!(!body.contains("password"), "配置摘要不应泄露凭据: {body}");
    }

    #[test]
    fn render_metrics_format() {
        let ctx = test_ctx("metrics");
        populate(&ctx);
        let out = render_metrics(&ctx);
        assert!(out.contains("newproxy_connections_total"), "got: {out}");
        assert!(out.contains("newproxy_queries_total 2"), "got: {out}");
        assert!(out.contains("newproxy_sql_total{template=\"SELECT * FROM users WHERE id = ?\",shard=\"0.t0\"} 1"), "got: {out}");
        assert!(
            out.contains("newproxy_sql_duration_seconds_total"),
            "got: {out}"
        );
        assert!(
            out.contains("newproxy_sql_max_duration_seconds"),
            "got: {out}"
        );
        assert!(out.contains("newproxy_sql_rows_total"), "got: {out}");
        // 进程指标(非 Linux 平台显示 -)
        assert!(out.contains("newproxy_process_cpu_percent"), "got: {out}");
        // 直方图:桶输出 + sum/count(populate 两查询,共 2_000_500µs)
        assert!(
            out.contains("newproxy_queries_duration_seconds_bucket{le=\"0.0001\"} 0"),
            "got: {out}"
        );
        assert!(
            out.contains("newproxy_queries_duration_seconds_sum 2.000500"),
            "got: {out}"
        );
        assert!(
            out.contains("newproxy_queries_duration_seconds_count 2"),
            "got: {out}"
        );
        // 分片维度 / 错误码 / 进程 uptime
        assert!(
            out.contains("newproxy_shard_queries_total{shard=\"0.t0\"} 2"),
            "got: {out}"
        );
        assert!(
            out.contains("newproxy_shard_slow_queries_total{shard=\"0.t0\"} 1"),
            "got: {out}"
        );
        assert!(
            out.contains("newproxy_backend_errors_total{code=\"1064\"} 1"),
            "got: {out}"
        );
        assert!(out.contains("newproxy_process_uptime_seconds"), "got: {out}");
        // 慢查询 HELP 不再写死 1s(阈值来自配置 slow_query_ms)
        assert!(!out.contains("Slow queries (>1s)"), "got: {out}");
    }

    #[test]
    fn update_config_value_variants() {
        let dir = std::env::temp_dir().join(format!("newproxy-cfgmod-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.conf");
        std::fs::write(&path, "[MySQL_Proxy_Layer]\nslow_query_ms=200\n").unwrap();
        // 修改已存在键
        update_config_value(
            path.to_str().unwrap(),
            "MySQL_Proxy_Layer",
            "slow_query_ms",
            "500",
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("slow_query_ms=500"), "got: {text}");
        // 段内追加新键
        update_config_value(path.to_str().unwrap(), "MySQL_Proxy_Layer", "new_key", "1").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("new_key=1"), "got: {text}");
        // 新段
        update_config_value(path.to_str().unwrap(), "New_Section", "k", "v").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[New_Section]"), "got: {text}");
        // 不存在文件 → Err
        assert!(update_config_value("/nonexistent/x.conf", "S", "k", "v").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn api_set_slow_query_ms_validation() {
        let ctx = test_ctx("sqms");
        // 非法值
        let (code, _, body) = api_set_slow_query_ms(&ctx, "value=0");
        assert!(code != 200, "0 应被拒绝: {body}");
        let (code, _, _) = api_set_slow_query_ms(&ctx, "value=9999999999");
        assert!(code != 200, "超上限应被拒绝");
        let (code, _, _) = api_set_slow_query_ms(&ctx, "");
        assert!(code != 200, "缺参数应被拒绝");
        // 合法值:写入配置并 reload
        let (code, _, body) = api_set_slow_query_ms(&ctx, "value=300");
        assert_eq!(code, 200, "got: {body}");
        assert!(body.contains("\"slow_query_ms\":300"), "got: {body}");
    }

    #[test]
    fn authorized_basic_auth_branches() {
        let ctx = test_ctx("authz");
        // 无 header → false(已配置 mng_user)
        assert!(!authorized(&ctx, None));
        // 错误凭据 → false("admin:wrong" base64)
        let bad = "Basic YWRtaW46d3Jvbmc=";
        assert!(!authorized(&ctx, Some(bad)));
        // 非 Basic 头 → false
        assert!(!authorized(&ctx, Some("Bearer xyz")));
        // 正确凭据 → true("admin:admin" base64)
        let good = "Basic YWRtaW46YWRtaW4=";
        assert!(authorized(&ctx, Some(good)));
        // 非法 base64 → false
        assert!(!authorized(&ctx, Some("Basic !!!")));
        // 未配置 mng_user → 免认证
        let mut cfg = crate::config::AppConfig::default();
        cfg.mng_user = None;
        let ctx2 = Arc::new(AppCtx::new(
            cfg,
            Arc::new(crate::pool::backend::SrvPool::new()),
            "/tmp/a".into(),
        ));
        assert!(authorized(&ctx2, None));
    }

    #[test]
    fn do_login_success_and_failure() {
        let ctx = test_ctx("login");
        let sessions: SessionStore = std::sync::Arc::new(dashmap::DashMap::new());
        // 正确凭据 → 302 + 会话 cookie
        let resp = do_login(&ctx, &sessions, b"user=admin&password=admin");
        let s = String::from_utf8_lossy(&resp);
        assert!(s.contains("302"), "got: {s}");
        assert!(s.contains("newproxy_session="), "got: {s}");
        assert_eq!(sessions.len(), 1);
        // 从成功响应提取 token 用于会话校验
        let token = s
            .split("newproxy_session=")
            .nth(1)
            .and_then(|t| t.split(';').next())
            .map(|t| t.trim().to_string())
            .unwrap_or_default();
        let cookie = format!("newproxy_session={token}");
        assert!(session_valid(&ctx, &sessions, Some(&cookie)));
        assert!(!session_valid(
            &ctx,
            &sessions,
            Some("newproxy_session=bogus")
        ));
        assert!(!session_valid(&ctx, &sessions, None));
        // 错误凭据 → 401(且不新增会话)
        let before = sessions.len();
        let resp = do_login(&ctx, &sessions, b"user=admin&password=wrong");
        let s = String::from_utf8_lossy(&resp);
        assert!(s.contains("401"), "got: {s}");
        assert_eq!(sessions.len(), before, "失败登录不应创建会话");
    }
}

// ─── 配置在线修改(面板设置 → 一键 reload 生效)───

/// 修改 INI 配置文件中某段某键的值(找不到则在该段末尾追加;段不存在则新建)
fn update_config_value(path: &str, section: &str, key: &str, value: &str) -> Result<(), String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("读配置失败: {e}"))?;
    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();

    let sec_hdr = format!("[{section}]");
    let mut in_section = false;
    let mut found = false;
    let mut section_tail: Option<usize> = None; // 段内最后一行(插入位置)
    let mut sec_start: Option<usize> = None;

    for i in 0..lines.len() {
        let t = lines[i].trim().to_string();
        if t.starts_with('[') {
            in_section = t.eq_ignore_ascii_case(&sec_hdr);
            if in_section {
                sec_start = Some(i);
            }
            continue;
        }
        if in_section {
            section_tail = Some(i);
            if let Some((k, _)) = t.split_once('=') {
                if k.trim().eq_ignore_ascii_case(key) {
                    lines[i] = format!("{key}={value}");
                    found = true;
                }
            }
        }
    }

    if !found {
        if let Some(tail) = section_tail {
            lines.insert(tail + 1, format!("{key}={value}"));
        } else if let Some(start) = sec_start {
            lines.insert(start + 1, format!("{key}={value}"));
        } else {
            lines.push(sec_hdr);
            lines.push(format!("{key}={value}"));
        }
    }

    let new_content = lines.join("\n");
    std::fs::write(path, new_content).map_err(|e| format!("写配置失败: {e}"))
}

/// POST /api/config/slow_query_ms?value=<ms>:写配置 + reload 一键生效
fn api_set_slow_query_ms(ctx: &AppCtx, query: &str) -> (u16, &'static str, String) {
    let value: u64 = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("value="))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if value == 0 || value > 3_600_000 {
        return (
            400,
            "application/json",
            json!({ "ok": false, "error": "value 需为 1-3600000(ms)" }).to_string(),
        );
    }
    let path = ctx.config_path().to_string();
    let value_str = value.to_string();
    if let Err(e) = update_config_value(&path, "MySQL_Proxy_Layer", "slow_query_ms", &value_str) {
        return (
            500,
            "application/json",
            json!({ "ok": false, "error": e }).to_string(),
        );
    }
    match ctx.reload_config() {
        Ok(diff) => (
            200,
            "application/json",
            json!({ "ok": true, "slow_query_ms": value, "diff": diff }).to_string(),
        ),
        Err(e) => (
            500,
            "application/json",
            json!({ "ok": false, "error": format!("reload 失败: {e}") }).to_string(),
        ),
    }
}

// ─── 一键健康检测 ───

/// GET /api/check:配置 / 后端连通性 / 连接池 一键检测
async fn api_check(ctx: &AppCtx) -> (u16, &'static str, String) {
    use serde_json::Value;
    let mut checks: Vec<Value> = Vec::new();

    // 1) 配置
    let cfg = ctx.load_config();
    let cfg_ok = !cfg.clusters.is_empty() && !cfg.product_users.is_empty();
    let tablets_total: usize = cfg.clusters.values().map(|c| c.tablets.len()).sum();
    checks.push(json!({
        "name": "配置",
        "ok": cfg_ok,
        "detail": format!(
            "clusters={} tablets={} product_users={} slow_query_ms={}ms",
            cfg.clusters.len(),
            tablets_total,
            cfg.product_users.len(),
            cfg.slow_query_ms
        ),
    }));

    // 2) 后端连通性:遍历所有 集群×分片×组 的 master,并发逐一探测 MySQL 端口
    //    (多分片/多集群场景全量覆盖,detail 标注 cluster.tablet)
    let targets: Vec<(String, &crate::config::Database)> = cfg
        .clusters
        .iter()
        .flat_map(|(cid, c)| c.tablets.iter().map(move |t| (cid, t)))
        .flat_map(|(cid, t)| t.groups.iter().map(move |g| (cid, t, g)))
        .filter_map(|(cid, t, g)| {
            g.master
                .as_ref()
                .map(|db| (format!("{}.{}", cid, t.tablet_id), db))
        })
        .collect();
    if targets.is_empty() {
        checks.push(json!({ "name": "后端", "ok": false, "detail": "未配置后端(master)" }));
    } else {
        let probes = targets.iter().map(|(label, db)| {
            let label = label.clone();
            let host = db.host.clone();
            let port = db.port;
            async move {
                let addr = format!("{host}:{port}");
                let probe = async {
                    let mut s = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
                    s.set_nodelay(true)?;
                    let mut buf = [0u8; 16];
                    tokio::io::AsyncReadExt::read_exact(&mut s, &mut buf).await?;
                    // 兼容两种 greeting:标准(首字节 0x0a 协议版本)与 MySQL 8.0.46+
                    // 带 4 字节长度前缀的形态(0x4a 00 00 00 0a ...)——后者 [4]=0x0a
                    Ok::<(u8, u8), std::io::Error>((buf[0], buf[4]))
                };
                match tokio::time::timeout(std::time::Duration::from_secs(2), probe).await {
                    Ok(Ok((b0, b4))) if b0 == 10 || b4 == 10 => (
                        label,
                        true,
                        format!("{addr} 可达(MySQL greeting 0x{b0:02X}/0x{b4:02X})"),
                    ),
                    Ok(Ok((b0, _))) => (
                        label,
                        false,
                        format!("{addr} 返回非 MySQL 协议(首字节 0x{b0:02X})"),
                    ),
                    Ok(Err(e)) => (label, false, format!("{addr} 不可达: {e}")),
                    Err(_) => (label, false, format!("{addr} 连接超时(2s)")),
                }
            }
        });
        for (label, ok, detail) in futures::future::join_all(probes).await {
            checks.push(json!({
                "name": "后端",
                "ok": ok,
                "detail": format!("[{label}] {detail}"),
            }));
        }
    }

    // 3) 连接池
    let buckets = ctx.srv_pool.all_buckets();
    let idle: usize = buckets
        .iter()
        .map(|(_, b)| b.write.lock().len() + b.read.lock().len())
        .sum();
    let fails = ctx.metrics.pool_acquire_fails.get();
    let pool_ok = idle > 0 || fails == 0;
    checks.push(json!({
        "name": "连接池",
        "ok": pool_ok,
        "detail": format!("桶数={} 空闲连接={} 获取失败={}", buckets.len(), idle, fails),
    }));

    let overall = checks.iter().all(|c| c["ok"] == Value::Bool(true));
    (
        200,
        "application/json",
        json!({ "overall": if overall { "ok" } else { "degraded" }, "checks": checks }).to_string(),
    )
}

// 前端连接:FrontConn + conn_task + drive() 状态机
// T2.1 实现:将协议层集成到 tokio task
//
// 对齐 C 侧 tr_conn_t + core_driver_machine + conn_handler_pt
// 按 work-stealing 模型重构:一个连接 = 一个 task

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_rustls::server::TlsStream;
use tracing::{debug, error, info, warn};

use crate::app::{AppCtx, ConnHandle};
use crate::config::AppConfig;
use crate::config::{DbUser, MasterSlave, ProductUser};
use crate::limit::IpChecker;
use crate::mgmt::MgmtCommand;
use crate::pool::backend::{BackConn, BackendGuard, BucketCfg};
use crate::proto::auth::{self, AuthMoreDataResponse, BackendGreeting};
use crate::proto::codec;
use crate::proto::command::{self, Command, CommandPacket};
use crate::proto::error::ProtoError;
use crate::proto::handshake;
use crate::proto::result;

// ─── 连接状态机 ───

/// 前端连接状态(对应 C tr_conn_t 的 state 字段)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontState {
    /// 刚 accept,准备发 handshake
    Accepted,
    /// 已发 server handshake,等待客户端 auth response
    AwaitingAuth,
    /// 认证完成,进入命令循环
    CommandLoop,
    /// 连接关闭
    #[allow(dead_code)]
    Closed,
}

/// 连接流:TCP 明文 或 TLS 加密
///
/// 说明:TLS 变体(TlsStream<PrefixedStream>)体积较大(~1.2KB,rustls 内部状态),
/// 但每个连接只会持有其中一种,且连接结束即释放,内存开销可忽略,
/// 因此不装箱以保持访问简单。clippy 的大枚举变体告警在此豁免。
#[allow(clippy::large_enum_variant)]
enum ConnStream {
    Plain(TcpStream),
    Tls(TlsStream<PrefixedStream>),
    /// 临时占位,用于 swap 时过渡(不会被实际使用)
    Placeholder,
}

/// TLS 升级用的包装流:在读取底层 TcpStream 之前,先把已预读到应用层的
/// 剩余字节(可能包含 TLS ClientHello)读出。
///
/// 背景:`read_packet` 读取 SSLRequest 包时,底层 `read_buf` 可能把紧随其后
/// 的 TLS ClientHello 也一并读进应用层缓冲区。若直接把裸 TcpStream 交给
/// `TlsAcceptor`,这些 ClientHello 字节已不在 socket 中,TLS 握手会因等待
/// ClientHello 而挂起。此结构把这些预读字节作为"前缀"优先喂给 TLS 层。
struct PrefixedStream {
    /// 预读的剩余字节(SSLRequest 之后、属于 TLS 握手的数据)
    prefix: bytes::Bytes,
    /// 底层 TCP 流
    inner: TcpStream,
}

impl PrefixedStream {
    fn new(prefix: bytes::Bytes, inner: TcpStream) -> Self {
        Self { prefix, inner }
    }
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // 优先消费前缀字节
        if !self.prefix.is_empty() {
            let n = std::cmp::min(buf.remaining(), self.prefix.len());
            buf.put_slice(&self.prefix.split_to(n));
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl AsyncRead for ConnStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            ConnStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            ConnStream::Tls(s) => Pin::new(s).poll_read(cx, buf),
            ConnStream::Placeholder => unreachable!(),
        }
    }
}

impl AsyncWrite for ConnStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            ConnStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            ConnStream::Tls(s) => Pin::new(s).poll_write(cx, buf),
            ConnStream::Placeholder => unreachable!(),
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            ConnStream::Plain(s) => Pin::new(s).poll_flush(cx),
            ConnStream::Tls(s) => Pin::new(s).poll_flush(cx),
            ConnStream::Placeholder => unreachable!(),
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            ConnStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            ConnStream::Tls(s) => Pin::new(s).poll_shutdown(cx),
            ConnStream::Placeholder => unreachable!(),
        }
    }
}

/// 会话级后端连接:整个前端会话复用同一条后端连接。
///
/// 关键:必须保证一个客户端会话的所有查询走**同一个**后端连接,
/// 否则 BEGIN/COMMIT 之间的事务语句会落到不同连接上各自自动提交,
/// 破坏事务原子性(例如 sysbench 的 DELETE+INSERT 会触发主键冲突 1062)。
/// 同时也能正确保持会话状态(当前库、会话变量等)。
///
/// 连接来源:优先从全局 `SrvPool` 取,miss 时新建并注册到池。
/// 会话结束时 `Drop` 自动将 TCP 连接归还到池。
struct BackendSession {
    /// 后端 TCP 流(从池中 BackConn 取出,或新建)
    stream: Option<TcpStream>,
    /// 后端读缓冲(复用)
    buf: BytesMut,
    /// RAII guard:drop 时将 stream 归还 BackConn 并释放回池
    guard: Option<BackendGuard>,
    /// 后端连接当前实际选中的库(COM_INIT_DB 后更新;空=未选库)
    selected_db: Option<String>,
    /// 该连接所属池桶的 db 段(取用时的桶键)。若会话中途切换库导致
    /// `selected_db != bucket_db`,归还时丢弃连接而非回池,避免污染
    /// 按 db 分桶的池(否则下个会话复用到错误库的连接)。
    bucket_db: String,
    /// 连接角色(读写分离:Master=leader 连接,Slave=follower 连接)
    role: crate::config::MasterSlave,
}

impl BackendSession {
    /// 标记底层后端连接为损坏:会话结束时该连接将被丢弃而非回池。
    /// 在查询期间出现后端 IO/协议错误时调用——连接可能已死或残留未读数据,
    /// 若原样回池,下一个会话复用到它必然失败(Broken pipe/脏数据雪崩)。
    fn mark_broken(&self) {
        if let Some(guard) = self.guard.as_ref() {
            guard.conn().mark_broken();
        }
    }

    /// 标记底层后端连接不再复用:会话内创建过 prepared statement
    /// (COM_STMT_PREPARE 成功或文本 PREPARE),服务端仍持有未释放的语句,
    /// 归还回池会泄漏给下个会话——直接丢弃连接以释放服务端资源。
    fn mark_no_reuse(&self) {
        if let Some(guard) = self.guard.as_ref() {
            guard.conn().mark_no_reuse();
        }
    }
}

impl Drop for BackendSession {
    fn drop(&mut self) {
        // 会话中途切换过库(selected_db != 桶键 db):连接不再属于原桶,
        // 标记 no_reuse 让 release 直接丢弃,防止污染按 db 分桶的池。
        // 空库(None/"" 等价)视为一致,正常回池。
        let db_same = self.selected_db.as_deref().unwrap_or("") == self.bucket_db.as_str();
        if !db_same {
            if let Some(guard) = self.guard.as_ref() {
                guard.conn().mark_no_reuse();
            }
        }
        // 先归还 stream 到 BackConn,再让 guard 的 drop 将连接归还池
        if let (Some(stream), Some(ref guard)) = (self.stream.take(), &self.guard) {
            guard.return_stream(stream);
        }
        // guard 在此 drop → release 到 AttrBucket
    }
}

/// 前端连接(替代 C tr_conn_t god-struct)
struct FrontConn {
    /// 连接流(TCP 或 TLS)
    stream: ConnStream,
    /// 客户端地址
    peer_addr: SocketAddr,
    /// 连接 ID(全局自增,贯穿日志,便于按连接排查)
    cid: u32,
    /// 读缓冲(复用,避免每次分配)
    read_buf: BytesMut,
    /// 协议状态
    state: FrontState,
    /// 服务端 scramble(认证用)
    server_scramble: [u8; handshake::SCRAMBLE_LEN],
    /// 认证通过后绑定的产品用户
    product_user: Option<Arc<ProductUser>>,
    /// 客户端能力位
    client_caps: u32,
    /// MySQL 包序号
    seq: u8,
    /// 全局上下文
    ctx: Arc<AppCtx>,
    /// 会话级粘滞后端连接(首次查询时懒建立,之后复用)
    backend: Option<BackendSession>,
    /// 显式事务中(BEGIN/START TRANSACTION … COMMIT/ROLLBACK;读分流强制 leader)
    in_txn: bool,
    /// 会话被 pin 到 leader(读分流关闭):SET / @变量 / prepared 等
    read_pinned: bool,
    /// 各分片最近是否执行过写(会话一致水位需要;键 = "cluster.tablet")
    written_shards: std::collections::HashMap<String, bool>,
    /// 本次查询实际使用的后端分片(cluster.tablet;指标分片维度,空=未绑定)
    backend_shard: String,
    /// 客户端登录时指定的数据库名(CLIENT_CONNECT_WITH_DB)
    client_database: Option<String>,
    /// 后端会话当前库(跟踪用,SELECT DATABASE() 本地应答的数据源)。
    /// 会话建立时由 COM_INIT_DB 结果确定,客户端 use(COM_INIT_DB 透传)
    /// 成功后更新;未知/失败时为 None(应答 NULL,与真实 MySQL 一致)。
    current_db: Option<String>,
    /// kill 信号接收端(注册表 cancel 时触发;外层命令循环与内层
    /// 后端转发均监听,保证空闲/挂起查询都能被 `checkproxy kill` 中断)
    kill_rx: watch::Receiver<bool>,
    /// 注册表句柄(同步 user/db/state/backends,供 show connections 展示)
    reg: Option<ConnHandle>,
}

impl FrontConn {
    fn new(stream: TcpStream, peer_addr: SocketAddr, ctx: Arc<AppCtx>) -> Self {
        let (_dummy_tx, kill_rx) = watch::channel(false); // conn_task 中替换为注册表通道
        Self {
            stream: ConnStream::Plain(stream),
            peer_addr,
            cid: 0, // conn_task 预分配(注册表需要)
            read_buf: BytesMut::with_capacity(4096),
            state: FrontState::Accepted,
            server_scramble: [0u8; handshake::SCRAMBLE_LEN],
            product_user: None,
            client_caps: 0,
            seq: 0,
            ctx,
            backend: None,
            in_txn: false,
            read_pinned: false,
            written_shards: std::collections::HashMap::new(),
            backend_shard: String::new(),
            client_database: None,
            current_db: None,
            kill_rx,
            reg: None,
        }
    }

    /// 更新当前库并同步注册表(show connections 的 db 列)
    fn set_current_db(&mut self, db: Option<String>) {
        self.current_db = db.clone();
        if let Some(h) = self.reg.as_ref() {
            *h.db.write() = db;
        }
    }

    /// 发送包到客户端并计入当前库的流量统计(本地应答路径;转发路径见 ForwardCount)
    async fn send_client(&mut self, seq: u8, payload: &[u8]) -> Result<(), ProtoError> {
        codec::send_packet(&mut self.stream, seq, payload).await?;
        let db = self.current_db.clone().unwrap_or_default();
        self.ctx.metrics.record_db_traffic(
            &db,
            0,
            (codec::HEADER_LEN + payload.len()) as u64,
            0,
            1,
        );
        // 按后端节点流量统计(发):本地应答计入当前绑定节点
        let node = self.backend_shard.clone();
        self.ctx
            .metrics
            .record_node_traffic(&node, 0, (codec::HEADER_LEN + payload.len()) as u64, 0, 1);
        Ok(())
    }

    /// 驱动状态机一步。返回 true 表示应继续,false 表示关闭连接
    async fn drive(&mut self) -> Result<bool, ProtoError> {
        match self.state {
            FrontState::Accepted => self.do_handshake().await,
            FrontState::AwaitingAuth => self.do_auth().await,
            FrontState::CommandLoop => self.do_command_loop().await,
            FrontState::Closed => Ok(false),
        }
    }

    // ─── 握手阶段 ───

    async fn do_handshake(&mut self) -> Result<bool, ProtoError> {
        let _cfg = self.ctx.load_config();
        // cid 已在 conn_task 预分配(注册表需要先于握手建立)
        let connection_id = self.cid;
        self.server_scramble = handshake::generate_scramble();

        let greeting = handshake::build_handshake(
            &self.ctx.server_version,
            connection_id,
            &self.server_scramble,
        );

        self.send_client(self.seq, &greeting).await?;
        self.seq = self.seq.wrapping_add(1);
        self.state = FrontState::AwaitingAuth;

        debug!(addr = %self.peer_addr, cid = connection_id, "handshake sent");
        Ok(true)
    }

    // ─── 认证阶段 ───

    async fn do_auth(&mut self) -> Result<bool, ProtoError> {
        // 等待客户端响应(可能是 SSL 请求或 auth 包)
        let (pkt_seq, payload) = codec::read_packet(&mut self.stream, &mut self.read_buf).await?;

        // SSL 请求检测:payload 恰好 32 字节(非 auth 包格式)
        if payload.len() == 32 && pkt_seq == 1 {
            info!(addr = %self.peer_addr, "SSL request detected, upgrading...");
            // 取出 TCP 流,执行 TLS accept
            let tcp = match std::mem::replace(&mut self.stream, ConnStream::Placeholder) {
                ConnStream::Plain(s) => s,
                _ => unreachable!(),
            };
            // 转移已预读到应用层的剩余字节(可能含 TLS ClientHello),
            // 作为前缀优先喂给 TLS 层,避免 ClientHello 丢失导致握手挂起。
            // 同时清空 read_buf,TLS 建立后的读取从空缓冲开始。
            let leftover = std::mem::take(&mut self.read_buf).freeze();
            if !leftover.is_empty() {
                debug!(
                    addr = %self.peer_addr,
                    bytes = leftover.len(),
                    "feeding pre-read bytes to TLS layer"
                );
            }
            let acceptor = crate::proto::tls::make_acceptor()
                .map_err(|e| ProtoError::Protocol(format!("TLS init: {e}")))?;
            let tls = acceptor
                .accept(PrefixedStream::new(leftover, tcp))
                .await
                .map_err(|e| ProtoError::Protocol(format!("TLS accept: {e}")))?;
            self.stream = ConnStream::Tls(tls);
            info!(addr = %self.peer_addr, "TLS upgrade complete");

            // 通过 TLS 重新读取真正的 auth 包
            let (pkt_seq, payload) =
                codec::read_packet(&mut self.stream, &mut self.read_buf).await?;
            return self.verify_auth(pkt_seq, payload).await;
        }

        self.verify_auth(pkt_seq, payload).await
    }

    async fn verify_auth(
        &mut self,
        pkt_seq: u8,
        payload: bytes::Bytes,
    ) -> Result<bool, ProtoError> {
        let auth = match handshake::parse_client_auth(&payload) {
            Ok(a) => a,
            Err(e) => {
                warn!(addr = %self.peer_addr, "parse auth failed: {}", e);
                let err_pkt = crate::proto::error::build_access_denied("Malformed auth packet");
                self.send_client(pkt_seq, &err_pkt).await?;
                return Ok(false);
            }
        };

        self.client_caps = auth.capabilities;
        self.client_database = auth.database;

        // IP 黑白名单检查
        let cfg = self.ctx.load_config();
        let client_ip = match self.peer_addr.ip() {
            std::net::IpAddr::V4(ip) => ip,
            _ => {
                let err = crate::proto::error::build_access_denied("IPv4 only");
                self.send_client(1, &err).await?;
                return Ok(false);
            }
        };

        if !IpChecker::check_access(&cfg, client_ip, &auth.username) {
            warn!(addr = %self.peer_addr, cid = self.cid, user = %auth.username, "IP access denied");
            self.ctx.metrics.connections_rejected.inc();
            let err = crate::proto::error::build_access_denied(&format!(
                "Access denied for user '{}'@'{}' (IP not allowed)",
                auth.username,
                self.peer_addr.ip()
            ));
            self.send_client(1, &err).await?;
            return Ok(false);
        }

        // 查找 product_user 并验证密码
        let cfg = self.ctx.load_config();
        let pu = match cfg.product_users.get(&auth.username) {
            Some(u) => u,
            None => {
                warn!(addr = %self.peer_addr, cid = self.cid, user = %auth.username, "unknown user");
                self.ctx.metrics.connections_rejected.inc();
                let err_pkt = crate::proto::error::build_access_denied(&format!(
                    "Access denied for user '{}'@'{}'",
                    auth.username,
                    self.peer_addr.ip()
                ));
                self.send_client(pkt_seq.wrapping_add(1), &err_pkt).await?;
                return Ok(false);
            }
        };

        // mysql_native_password scramble 验证
        let verified = handshake::verify_native_scramble(
            pu.password.as_bytes(),
            &self.server_scramble,
            &auth.auth_response,
        );

        if !verified {
            warn!(addr = %self.peer_addr, cid = self.cid, user = %auth.username, "auth failed");
            self.ctx.metrics.connections_rejected.inc();
            let err_pkt = crate::proto::error::build_access_denied(&format!(
                "Access denied for user '{}'@'{}' (using password: YES)",
                auth.username,
                self.peer_addr.ip()
            ));
            self.send_client(pkt_seq.wrapping_add(1), &err_pkt).await?;
            return Ok(false);
        }

        // 认证通过 → 发 OK 包
        let ok_pkt =
            crate::proto::error::build_ok(0, 0, handshake::SERVER_STATUS_AUTOCOMMIT, 0, None);
        self.send_client(pkt_seq.wrapping_add(1), &ok_pkt).await?;

        self.seq = 0; // 进入命令循环后 seq 重置
        self.product_user = Some(Arc::new(pu.clone()));
        self.state = FrontState::CommandLoop;
        self.ctx.metrics.connections_active.inc();

        // 同步注册表:用户名/状态/客户端指定库
        if let Some(h) = self.reg.as_ref() {
            *h.user.write() = auth.username.clone();
            *h.state.write() = "Command".to_string();
            *h.db.write() = self.client_database.clone();
        }

        info!(addr = %self.peer_addr, cid = self.cid, user = %auth.username, "auth success");
        Ok(true)
    }

    // ─── 命令循环 ───

    async fn do_command_loop(&mut self) -> Result<bool, ProtoError> {
        // 阶段计时:interval = 命令间隔(距上一条命令的等待,**含客户端空闲**,
        // 非代理耗时;数据到达后的实际读包是 µs 级,无法与等待分离故一并归此);
        // parse = 解析+本地拦截检测。
        // 与 handle_query 内的 setup/send/exec/recv/cli_send 一起构成完整请求阶段分解,
        // 慢请求可定位到:客户端发包间隔长 / 解析慢 / 后端准备慢 / 发送慢 /
        // 后端执行等待慢 / 后端回包慢 / 向客户端发送慢。
        let t_cmd_start = std::time::Instant::now();
        // 读取前端命令
        let (_pkt_seq, payload) = codec::read_packet(&mut self.stream, &mut self.read_buf).await?;
        let t_cmd_read = std::time::Instant::now();
        // 按库流量统计(收):每条客户端命令包计入当前会话库(空库归 "-")
        let db_now = self.current_db.clone().unwrap_or_default();
        self.ctx.metrics.record_db_traffic(
            &db_now,
            (codec::HEADER_LEN + payload.len()) as u64,
            0,
            1,
            0,
        );
        // 按后端节点流量统计(收):命令包计入当前绑定节点
        let node = self.backend_shard.clone();
        self.ctx.metrics.record_node_traffic(
            &node,
            (codec::HEADER_LEN + payload.len()) as u64,
            0,
            1,
            0,
        );

        let cmd = match command::parse_command_packet(&payload) {
            Ok(c) => c,
            Err(e) => {
                warn!(addr = %self.peer_addr, "parse command: {}", e);
                // 命令包解析失败计入监控指标与面板列表(原始字节转可打印文本)
                let raw: String = payload
                    .iter()
                    .take(256)
                    .map(|&b| {
                        if b.is_ascii_graphic() || b == b' ' {
                            b as char
                        } else {
                            '?'
                        }
                    })
                    .collect();
                self.ctx
                    .metrics
                    .record_parse_failure(&format!("命令包解析失败: {e}"), &raw);
                return Ok(false);
            }
        };

        debug!(addr = %self.peer_addr, cmd = %cmd.command, "command");

        // 拦截裸 help 查询:mysql 客户端在多语句批量/部分工具下会把 help 作为
        // COM_QUERY 发给服务器,这里本地返回内置管理命令,不再透传到后端
        // (后端网关对 help 返回 1064 语法错误)。
        // 注意:交互模式下 mysql 客户端的裸 `help` 是客户端本地命令,不会到达
        // 代理,此时请使用 `checkproxy help`(同样展示内置管理命令)。
        // 热路径:help 检测与管理命令解析共用一次小写化,避免每个查询两次分配。
        if cmd.command == Command::Query {
            if let Ok(sql) = std::str::from_utf8(&cmd.payload) {
                let lower = sql.trim().trim_end_matches(';').trim().to_lowercase();
                if lower == "help" || lower.starts_with("help ") {
                    debug!(addr = %self.peer_addr, cid = self.cid, "local answer: help");
                    return self.answer_help().await.map(|_| true);
                }
                if let Some(mgmt) = MgmtCommand::parse_normalized(&lower) {
                    let user = self
                        .product_user
                        .as_ref()
                        .map(|p| p.username.as_str())
                        .unwrap_or("?");
                    info!(
                        addr = %self.peer_addr,
                        cid = self.cid,
                        user = %user,
                        cmd = %sql.trim(),
                        "management command"
                    );
                    // 客户端命令包 seq=0，响应首包 seq 必须从 1 开始
                    // show connections:结果集形式(批量/交互模式均可见)
                    if matches!(mgmt, MgmtCommand::ShowConnections) {
                        return self.answer_show_connections().await.map(|_| true);
                    }
                    // checkproxy help / 裸 checkproxy:与裸 help 相同的命令列表结果集
                    if matches!(mgmt, MgmtCommand::Help) {
                        return self.answer_help().await.map(|_| true);
                    }
                    // stats/pool/sql/config:纯文本命令。OK 包的 info 文本在批量模式
                    // (-e)下 mysql 客户端不显示,统一渲染成单列结果集(一行文本一行数据)
                    if let MgmtCommand::ShowStats
                    | MgmtCommand::ShowPool
                    | MgmtCommand::ShowSqlStats
                    | MgmtCommand::ShowConfig = &mgmt
                    {
                        let column = match &mgmt {
                            MgmtCommand::ShowStats => "Status",
                            MgmtCommand::ShowPool => "Pool",
                            MgmtCommand::ShowSqlStats => "Top Queries",
                            _ => "Configuration",
                        };
                        let metrics = self.ctx.metrics.clone();
                        let cfg = self.ctx.load_config();
                        let text = mgmt.info_text(&metrics, Some(&cfg));
                        return self
                            .answer_text_resultset(column, &text)
                            .await
                            .map(|_| true);
                    }
                    // kill <cid>:按前端连接 id 取消目标任务(级联回收其后端连接)
                    if let MgmtCommand::KillConnection(id) = &mgmt {
                        let resp = match self.ctx.kill_connection(*id) {
                            Ok(()) => crate::proto::error::build_ok(
                                0,
                                0,
                                handshake::SERVER_STATUS_AUTOCOMMIT,
                                0,
                                Some(&format!("KILL CONNECTION {id}: OK")),
                            ),
                            Err(msg) => {
                                warn!(cid = self.cid, "kill {id} failed: {msg}");
                                crate::proto::error::build_error(1094, "HY000", &msg)
                            }
                        };
                        self.send_client(1, &resp).await?;
                        return Ok(true);
                    }

                    let resp = match &mgmt {
                        MgmtCommand::Reload => {
                            // 热加载配置:重解析文件 → 原子替换 → 清空连接池
                            match self.ctx.reload_config() {
                                Ok(diff) => {
                                    info!(cid = self.cid, "config reloaded: {diff}");
                                    crate::proto::error::build_ok(
                                        0,
                                        0,
                                        handshake::SERVER_STATUS_AUTOCOMMIT,
                                        0,
                                        Some(&format!("RELOAD: OK\n{diff}")),
                                    )
                                }
                                Err(e) => {
                                    error!(cid = self.cid, "config reload failed: {e}");
                                    crate::proto::error::build_error(
                                        1064,
                                        "HY000",
                                        &format!("RELOAD failed: {e}"),
                                    )
                                }
                            }
                        }
                        _ => {
                            let metrics = self.ctx.metrics.clone();
                            let cfg = self.ctx.load_config();
                            mgmt.execute(&metrics, Some(&cfg))
                        }
                    };
                    self.send_client(1, &resp).await?;
                    return Ok(true);
                }
            }
        }

        match cmd.command {
            Command::Quit => {
                // COM_QUIT:直接关闭
                debug!(addr = %self.peer_addr, "client quit");
                return Ok(false);
            }
            Command::Ping => {
                // COM_PING:直接回 OK（seq=1，因为客户端 Ping 包 seq=0）
                let ok_pkt = crate::proto::error::build_ok(
                    0,
                    0,
                    handshake::SERVER_STATUS_AUTOCOMMIT,
                    0,
                    None,
                );
                self.send_client(1, &ok_pkt).await?;
            }
            Command::Query => {
                // SELECT DATABASE() 本地应答:网关在未选库连接上对该查询返回
                // 34952,而 mysql 客户端交互式 `use` 前会先执行它,收到错误即
                // 自动重连(表现为 "No connection. Trying to reconnect...")。
                // 真实 MySQL 在未选库时返回 NULL,这里按相同语义本地应答。
                if self.is_select_database(&cmd) {
                    debug!(addr = %self.peer_addr, cid = self.cid, "local answer: SELECT DATABASE()");
                    return self.answer_select_database().await.map(|_| true);
                }
                // COM_QUERY:透传到后端(带 interval/parse 阶段耗时)
                let interval_us = (t_cmd_read - t_cmd_start).as_micros() as u64;
                let parse_us = (std::time::Instant::now() - t_cmd_read).as_micros() as u64;
                self.handle_query(&cmd, interval_us, parse_us).await?;
            }
            Command::InitDb | Command::Statistics => {
                // COM_INIT_DB:切换数据库
                // COM_STATISTICS:服务器统计
                self.handle_pass_through(&cmd).await?;
            }
            Command::FieldList => {
                // COM_FIELD_LIST:客户端交互模式下的表名/列名自动补全
                // 响应为 ColumnDef* + EOF，首包 0x03 不能按 OK 判断，
                // 需专用转发函数 forward_field_list_response
                self.handle_field_list(&cmd).await?;
            }
            Command::StmtPrepare
            | Command::StmtExecute
            | Command::StmtClose
            | Command::StmtReset
            | Command::StmtSendLongData
            | Command::StmtFetch => {
                // 预处理语句:透传到会话级后端连接。
                // COM_STMT_PREPARE 的响应首包是 0x00(COM_STMT_PREPARE_OK),
                // 会被 forward_backend_response 误判为 OK 而提前结束,
                // 故用专用转发函数。
                self.handle_stmt(&cmd).await?;
            }
            _ => {
                warn!(addr = %self.peer_addr, cmd = %cmd.command, "unsupported");
                let err = crate::proto::error::build_unknown_command();
                // 响应 seq = 请求 seq + 1（客户端命令包 seq=0，响应包 seq=1）
                self.send_client(1, &err).await?;
            }
        }

        self.seq = 0;
        Ok(true)
    }

    /// 处理 COM_QUERY:透传到会话级后端连接并回传结果
    ///
    /// 复用 `self.backend`(会话粘滞),保证事务/会话状态正确。
    /// 日志:debug 记录每条 SQL(单行截断)并带**完整阶段耗时分解**:
    /// read=前端收包(do_command_loop), parse=解析+本地拦截检测,
    /// setup=后端准备(池获取/新建握手), send=发命令, forward=后端响应+转发;
    /// >1s 以 WARN 记录慢查询,同样带全部阶段,用于定位慢在哪个环节。
    /// 同时写入全局 Metrics 供 `checkproxy show status / show sql` 排查。
    async fn handle_query(
        &mut self,
        cmd: &CommandPacket,
        interval_us: u64,
        parse_us: u64,
    ) -> Result<(), ProtoError> {
        let t0 = std::time::Instant::now(); // 查询开始(含后端准备)
        let pu = self.product_user.as_ref().unwrap().clone();
        let cfg = self.ctx.load_config();
        let db_user = cfg.db_users.get(&pu.db_username);

        // SQL 文本:单行化 + 截断,用于日志(避免换行/超长 SQL 刷爆日志)
        // 非 UTF-8 的 SQL(二进制/异常编码)视为解析失败,计入监控与列表
        if std::str::from_utf8(&cmd.payload).is_err() {
            let raw: String = cmd
                .payload
                .iter()
                .take(256)
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        '?'
                    }
                })
                .collect();
            self.ctx
                .metrics
                .record_parse_failure("SQL 非 UTF-8 编码", &raw);
        }
        let sql_text = String::from_utf8_lossy(&cmd.payload);
        let sql_log: String = sql_text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(512)
            .collect();

        // 按表尾号路由:解析 SQL 表名 → 目标分片(每条查询动态切换后端)。
        // 无尾号表走默认第一个分片;无表语句(SELECT 1 等)保持当前后端。
        // 热路径只用表名 → token 化轻量提取(非 DML 前缀窥探即返回,无全串拷贝)。
        let tables = crate::parser::classify::table_names_for_routing(&sql_text);
        let target = self.route_tablet_target(&cfg, &tables);

        // 读写分离:受管分片 + 一致性档位非 strong 时,可分流纯读走 follower。
        let qkind = query_kind(&sql_text);
        let read_plan = if qkind == QueryKind::Select {
            self.plan_read_route(
                &cfg,
                target.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
                db_user,
                &sql_text,
            )
        } else {
            None
        };
        let want_follower = matches!(
            read_plan.as_ref(),
            Some((d, _)) if d.role == crate::ha::consistency::RouteRole::Follower
        );
        // 读路由标识:SELECT 发往主库时记录原因(计数 + 单条记录排障用)。
        // plan_read_route 返回 None 表示未进入分流决策(非受管/strong 直通),
        // 这类 SELECT 同样发往主库,原因记 "level-strong";决策进入但被判主库
        // 时用决策器给出的 leader_reason(in-transaction/session-pinned/…)。
        let mut route_reason: Option<String> = None;
        if qkind == QueryKind::Select {
            if want_follower {
                route_reason = Some("follower".to_string());
            } else {
                let reason = read_plan
                    .as_ref()
                    .and_then(|(d, _)| d.leader_reason)
                    .unwrap_or("level-strong");
                route_reason = Some(reason.to_string());
                // 主库读计数:产品用户 × 原因(面板"主库读分布"表/Prometheus label)
                self.ctx.metrics.record_leader_read(&pu.username, reason);
            }
        }

        // 建立后端连接(leader 或 follower;后者按档位可带 GTID 屏障)。
        // 若后端认证失败（如数据库不存在），将错误消息转发给客户端而非直接断开。
        let ensure_res = if want_follower {
            let wait = read_plan.as_ref().and_then(|(_, w)| *w);
            self.ensure_backend_ex(db_user, target.as_ref(), MasterSlave::Slave, wait)
                .await
        } else {
            self.ensure_backend_ex(db_user, target.as_ref(), MasterSlave::Master, None)
                .await
        };
        let effective_role = match ensure_res {
            Ok(r) => r,
            Err(e) => {
                if let ProtoError::AuthFailed(msg) = &e {
                    let err_pkt = crate::proto::error::build_error(1049, "42000", msg);
                    self.send_client(1, &err_pkt).await?;
                    return Ok(());
                }
                let setup_ms = t0.elapsed().as_millis() as u64;
                error!(addr = %self.peer_addr, cid = self.cid, user = %pu.username, interval_us, parse_us, setup_ms, err = %e, "backend connect failed");
                return Err(e);
            }
        };
        // 降级修正:请求 follower 但从库不可用/追不平回落主库时,
        // 路由标识从 "follower" 改标为 "follower-fallback",并计入主库读
        // (原因维度;user 维度已在下方统一由 route_reason 判定,这里只改标签)。
        if want_follower && effective_role == MasterSlave::Master {
            if route_reason.as_deref() == Some("follower") {
                route_reason = Some("follower-fallback".to_string());
                self.ctx
                    .metrics
                    .record_leader_read(&pu.username, "follower-fallback");
            }
        }
        let t1 = std::time::Instant::now(); // 后端已就绪(池获取/新建+握手)

        // 执行计划绑定:SQL 模板命中规则则注入优化器 hint(改写后转发,
        // 后端按固定计划执行);未命中则原样透传。指标/日志仍按原始 SQL 统计。
        let rewritten = crate::parser::rewrite::apply_plan_binding(&sql_text, &cfg.plan_bindings);
        let cmd_payload: &[u8] = match rewritten.as_deref() {
            Some(r) => {
                debug!(addr = %self.peer_addr, cid = self.cid, hint = %r, "plan binding applied");
                r.as_bytes()
            }
            None => &cmd.payload,
        };
        let mut cmd_pkt = vec![cmd.command.as_u8()];
        cmd_pkt.extend_from_slice(cmd_payload);
        // 文本协议 PREPARE(PREPARE stmt FROM ...)同样会在服务端创建 prepared
        // statement,且与二进制 COM_STMT_PREPARE 一样不会随会话结束释放:
        // 检测到即标记连接不再复用(无分配的前缀检查,忽略大小写)。
        if is_text_prepare(&sql_text) {
            debug!(addr = %self.peer_addr, cid = self.cid, "text PREPARE detected, backend connection won't be pooled");
            let backend = self.backend.as_mut().unwrap();
            backend.mark_no_reuse();
            // prepared 语句绑定在后端,会话读不再分流(读写分离关闭)
            self.read_pinned = true;
        }
        debug!(addr = %self.peer_addr, cid = self.cid, user = %pu.username, sql = %sql_log, "query start");

        // 命令阶段每条命令 seq 从 0 开始
        let backend = self.backend.as_mut().unwrap();
        let be_stream = backend
            .stream
            .as_mut()
            .expect("BackendSession: stream missing");
        if let Err(e) = codec::send_packet(be_stream, 0, &cmd_pkt).await {
            backend.mark_broken();
            return Err(e);
        }
        let t2 = std::time::Instant::now(); // 命令已发送到后端
                                            // 转发后端响应时监听 kill 信号:后端挂起(慢查询/锁等待)时
                                            // `checkproxy kill <cid>` 能立即中断;此时后端 socket 可能有未读
                                            // 残留字节,必须 mark_broken,禁止脏回连接池。
                                            // 注:`changed()` future 每查询只注册一次 waker(后续 poll 为
                                            // 廉价版本检查),kill 唤醒与开销均优,保持原实现。
        let mut fc = result::ForwardCount::default();
        let mut fwd_ok = false; // USE 语句切库跟踪用:本次转发是否返回 OK
        tokio::select! {
            biased;
            _ = self.kill_rx.changed() => {
                if *self.kill_rx.borrow() {
                    backend.mark_broken();
                    return Err(ProtoError::ConnectionClosed);
                }
            }
            r = result::forward_backend_response(
                be_stream,
                &mut self.stream,
                &mut backend.buf,
                &mut fc,
            ) => {
                match r {
                    Ok((kind, err_code)) => {
                        // 按库流量统计(发):转发字节/包数计入当前会话库
                        let db = self.current_db.clone().unwrap_or_default();
                        self.ctx
                            .metrics
                            .record_db_traffic(&db, 0, fc.bytes, 0, fc.pkts);
                        let node = self.backend_shard.clone();
                        self.ctx
                            .metrics
                            .record_node_traffic(&node, 0, fc.bytes, 0, fc.pkts);
                        // 后端返回 ERR(语法 1064/权限等)计入"后端错误统计"监控与列表;
                        // 查询级错误计数与后端错误同源(对客户端可见的查询失败)
                        if kind == result::ResponseKind::Err {
                            let code = err_code
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "-".to_string());
                            self.ctx.metrics.record_backend_error(&code, &sql_log);
                            self.ctx.metrics.queries_errors.inc();
                        }
                        fwd_ok = matches!(kind, result::ResponseKind::Ok);
                    }
                    Err(e) => {
                        backend.mark_broken();
                        return Err(e);
                    }
                }
            }
        }
        // USE 语句(COM_QUERY 形式)切库:后端已切换,同步会话库跟踪。
        // 否则 selected_db 滞后 → 会话结束时连接被放回旧 db 桶,污染按 db
        // 分桶的池(下个会话复用到错误库的连接)。
        if let Some(db) = use_db_name(&sql_text) {
            if fwd_ok {
                self.set_current_db(Some(db.clone()));
                if let Some(bs) = self.backend.as_mut() {
                    bs.selected_db = Some(db);
                }
            }
        }
        // 会话状态簿记(读分流状态机):BEGIN/COMMIT/ROLLBACK、SET、写标记。
        // 仅在 fwd_ok(后端 OK)时更新,避免失败语句污染状态。
        if fwd_ok {
            match qkind {
                QueryKind::Begin => self.in_txn = true,
                QueryKind::Commit | QueryKind::Rollback => self.in_txn = false,
                QueryKind::Set => {
                    if !self.read_pinned {
                        self.read_pinned = true;
                        debug!(addr = %self.peer_addr, cid = self.cid, "session read pinned to leader (SET)");
                    }
                }
                // 会话一致水位:记录本会话在本分片写过(读己之写需要屏障)
                QueryKind::Write if !self.backend_shard.is_empty() => {
                    self.written_shards.insert(self.backend_shard.clone(), true);
                }
                _ => {}
            }
        }
        let t3 = std::time::Instant::now(); // 响应已转发完毕

        // 阶段耗时分解(µs/ms 双精度):setup=后端准备(首个查询含建连握手),
        // send=发送命令到后端,
        // exec=后端执行等待(send 完→后端首个响应包到达;判定后端执行慢的关键),
        // recv=接收后端响应包(首包之后全部后端读;回包慢/大结果集传输慢时大),
        // cli_send=向客户端发送响应(批量缓冲写出+逐包;客户端收包慢时大),
        // forward=后端执行+转发合计(exec+recv+cli_send,兼容旧口径),
        // elapsed=总耗时(含 setup,自 t0 起)。
        let elapsed_us = (t3 - t0).as_micros() as u64;
        let elapsed_ms = (t3 - t0).as_millis() as u64;
        let setup_ms = (t1 - t0).as_millis() as u64;
        let send_ms = (t2 - t1).as_millis() as u64;
        let forward_ms = (t3 - t2).as_millis() as u64;
        let setup_us = (t1 - t0).as_micros() as u64;
        let send_us = (t2 - t1).as_micros() as u64;
        let forward_us = (t3 - t2).as_micros() as u64;
        // 子阶段(转发层 fc 计时):后端执行等待 / 收后端包 / 发客户端包
        let exec_us = fc.exec_us;
        let recv_us = fc.recv_us;
        let cli_send_us = fc.cli_send_us;
        // 慢查询阈值来自配置(ms → µs),热加载动态生效
        let slow_threshold_us = cfg.slow_query_ms.saturating_mul(1000);
        // Top SQL 按模板聚合(字面量归一化为 ?),日志/列表仍保留原始 SQL
        let sql_template = crate::parser::rewrite::normalize_sql(&sql_text);
        self.ctx.metrics.record_query(
            &sql_template,
            &self.backend_shard,
            elapsed_us,
            fc.rows, // 结果集返回行数(转发层逐行计数,OK/ERR 为 0)
            slow_threshold_us,
        );
        // 阶段累计(面板展示平均耗时分布;同时按分片累计供面板按分片对比)
        self.ctx.metrics.record_query_stages(
            &self.backend_shard,
            interval_us,
            parse_us,
            setup_us,
            send_us,
            exec_us,
            recv_us,
            cli_send_us,
            forward_us,
        );
        let is_slow = elapsed_us > slow_threshold_us;
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // 维度:会话当前库 + 产品用户(供面板按 db/puser 显示与过滤)
        let q_db = self.current_db.clone().unwrap_or_default();
        let q_puser = pu.username.clone();
        // 慢查询入缓冲(有界,面板 /api/slow 展示各阶段耗时)
        if is_slow {
            self.ctx
                .metrics
                .record_slow_query(crate::metric::SlowQuery {
                    ts: now_ts,
                    sql: sql_log.clone(),
                    db: q_db.clone(),
                    puser: q_puser.clone(),
                    shard: self.backend_shard.clone(),
                    elapsed_us,
                    interval_us,
                    parse_us,
                    setup_us,
                    send_us,
                    exec_us,
                    recv_us,
                    cli_send_us,
                    forward_us,
                    route: route_reason.clone(),
                });
        }
        // 所有查询入"最近查询"缓冲(有界;面板 /api/recent 可按 SQL/db/用户
        // 检索单条执行的 5 阶段耗时,不受日志级别影响)
        let route_tag = route_reason.clone().unwrap_or_else(|| "-".to_string());
        self.ctx
            .metrics
            .record_recent_query(crate::metric::QueryRecord {
                ts: now_ts,
                sql: sql_log.clone(),
                db: q_db,
                puser: q_puser,
                shard: self.backend_shard.clone(),
                elapsed_us,
                slow: is_slow,
                interval_us,
                parse_us,
                setup_us,
                send_us,
                exec_us,
                recv_us,
                cli_send_us,
                forward_us,
                route: route_reason,
            });

        if elapsed_us > 1_000_000 {
            warn!(addr = %self.peer_addr, cid = self.cid, user = %pu.username, elapsed_ms, interval_us, parse_us, setup_ms, send_ms, exec_us, recv_us, cli_send_us, forward_ms, route = %route_tag, sql = %sql_log, "slow query");
        } else {
            debug!(addr = %self.peer_addr, cid = self.cid, user = %pu.username, elapsed_ms, interval_us, parse_us, setup_ms, send_ms, exec_us, recv_us, cli_send_us, forward_ms, route = %route_tag, sql = %sql_log, "query done");
        }
        Ok(())
    }

    /// 判断是否为 `SELECT DATABASE()`(大小写/空白/尾分号不敏感,可选 LIMIT 1)
    fn is_select_database(&self, cmd: &CommandPacket) -> bool {
        if cmd.command != Command::Query {
            return false;
        }
        let Ok(sql) = std::str::from_utf8(&cmd.payload) else {
            return false;
        };
        let normalized: String = sql
            .trim()
            .trim_end_matches(';')
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        normalized == "select database()" || normalized == "select database() limit 1"
    }

    /// 本地应答 `SELECT DATABASE()`(旧式协议结果集):
    /// column_count=1 + ColumnDefinition41("DATABASE()") + EOF + 行 + EOF。
    /// 行值为当前库名(未选库为 lenenc NULL 0xFB),与真实 MySQL 语义一致。
    async fn answer_select_database(&mut self) -> Result<(), ProtoError> {
        // 包 1:列数 = 1
        self.send_client(1, &[0x01]).await?;
        // 包 2:ColumnDefinition41
        let coldef = Self::column_def("DATABASE()");
        self.send_client(2, &coldef).await?;

        // 包 3:EOF(旧式协议:5 字节)
        self.send_client(3, &[0xFE, 0x00, 0x00, 0x02, 0x00]).await?;

        // 包 4:行(当前库名 lenenc 字符串,未选库为 NULL)
        match self.current_db.as_deref() {
            Some(db) => {
                let mut row = Vec::with_capacity(db.len() + 1);
                row.push(db.len() as u8);
                row.extend_from_slice(db.as_bytes());
                self.send_client(4, &row).await?;
            }
            None => {
                self.send_client(4, &[0xFB]).await?;
            }
        }

        // 包 5:EOF
        self.send_client(5, &[0xFE, 0x00, 0x00, 0x02, 0x00]).await?;
        Ok(())
    }

    /// 本地应答 `help`:以**结果集**形式返回内置管理命令(两列 command/description)。
    /// 不用 OK+info——mysql 客户端在批量模式(`-e`)不显示 OK 包的 info 文本,
    /// 结果集则批量/交互模式都能正常展示。
    async fn answer_help(&mut self) -> Result<(), ProtoError> {
        // 包 1:列数 = 2
        self.send_client(1, &[0x02]).await?;
        // 包 2-3:列定义
        let coldef = Self::column_def("command");
        self.send_client(2, &coldef).await?;
        let coldef = Self::column_def("description");
        self.send_client(3, &coldef).await?;
        // 包 4:列 EOF
        self.send_client(4, &[0xFE, 0x00, 0x00, 0x02, 0x00]).await?;
        // 包 5..:数据行
        let mut seq: u8 = 5;
        for (cmd, desc) in crate::mgmt::MGMT_COMMANDS {
            let mut row = Vec::with_capacity(cmd.len() + desc.len() + 2);
            row.push(cmd.len() as u8);
            row.extend_from_slice(cmd.as_bytes());
            row.push(desc.len() as u8);
            row.extend_from_slice(desc.as_bytes());
            self.send_client(seq, &row).await?;
            seq = seq.wrapping_add(1);
        }
        // 行尾 EOF
        self.send_client(seq, &[0xFE, 0x00, 0x00, 0x02, 0x00])
            .await?;
        Ok(())
    }

    /// 本地应答 `checkproxy show connections`:从全局注册表读取活跃前端连接,
    /// 以结果集形式返回(Id/User/Host/db/State/Time/Backends)。
    ///
    /// id 即前端连接 cid,与 `checkproxy kill <id>` 一一对应;
    /// Backends 列展示会话绑定的后端 host:port(多分片场景便于定位)。
    async fn answer_show_connections(&mut self) -> Result<(), ProtoError> {
        let headers = ["Id", "User", "Host", "db", "State", "Time", "Backends"];
        // 收集快照(锁不跨 await;先拷贝再排序)
        let mut rows: Vec<Vec<String>> = self
            .ctx
            .connections
            .iter()
            .map(|e| {
                let (cid, h) = (e.key(), e.value());
                vec![
                    cid.to_string(),
                    h.user.read().clone(),
                    h.peer_addr.to_string(),
                    h.db.read().clone().unwrap_or_default(),
                    h.state.read().clone(),
                    h.started_at.elapsed().as_secs().to_string(),
                    h.backends.read().join(", "),
                ]
            })
            .collect();
        rows.sort_by_key(|r| r[0].parse::<u32>().unwrap_or(0));

        // 包 1:列数
        self.send_client(1, &[headers.len() as u8]).await?;
        // 包 2..:列定义
        let mut seq: u8 = 2;
        for name in headers {
            let coldef = Self::column_def(name);
            self.send_client(seq, &coldef).await?;
            seq = seq.wrapping_add(1);
        }
        // 列 EOF
        self.send_client(seq, &[0xFE, 0x00, 0x00, 0x02, 0x00])
            .await?;
        seq = seq.wrapping_add(1);
        // 数据行
        for row in &rows {
            let mut pkt = Vec::with_capacity(row.len() * 4);
            for v in row {
                pkt.push(v.len() as u8);
                pkt.extend_from_slice(v.as_bytes());
            }
            self.send_client(seq, &pkt).await?;
            seq = seq.wrapping_add(1);
        }
        // 行尾 EOF
        self.send_client(seq, &[0xFE, 0x00, 0x00, 0x02, 0x00])
            .await?;
        Ok(())
    }

    /// 单列文本结果集:一行文本 = 一行数据(跳过空行)。
    ///
    /// stats/config 等纯文本命令走这里——批量模式下 OK 包 info 文本不显示,
    /// 结果集在批量/交互两种模式下都可见。
    async fn answer_text_resultset(&mut self, column: &str, text: &str) -> Result<(), ProtoError> {
        // 包 1:列数
        self.send_client(1, &[1u8]).await?;
        // 包 2:列定义
        self.send_client(2, &Self::column_def(column)).await?;
        // 列 EOF
        self.send_client(3, &[0xFE, 0x00, 0x00, 0x02, 0x00]).await?;
        // 数据行(lenenc 长度,支持超过 255 字节的行)
        let mut seq: u8 = 4;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let mut pkt = Vec::with_capacity(line.len() + 3);
            Self::push_lenenc_str(&mut pkt, line);
            self.send_client(seq, &pkt).await?;
            seq = seq.wrapping_add(1);
        }
        // 行尾 EOF
        self.send_client(seq, &[0xFE, 0x00, 0x00, 0x02, 0x00])
            .await?;
        Ok(())
    }

    /// lenenc 字符串:< 251 时单字节长度;否则 0xFC + u16 小端。
    /// (行内容可能是长 SQL/多分片列表,不能只写一个 u8)
    fn push_lenenc_str(buf: &mut Vec<u8>, s: &str) {
        let len = s.len();
        if len < 251 {
            buf.push(len as u8);
        } else {
            buf.push(0xFC);
            buf.extend_from_slice(&(len as u16).to_le_bytes());
        }
        buf.extend_from_slice(s.as_bytes());
    }

    /// 构建 ColumnDefinition41 包(类型 VAR_STRING, charset 33)
    fn column_def(name: &str) -> Vec<u8> {
        let mut coldef = Vec::with_capacity(64);
        coldef.push(0x03);
        coldef.extend_from_slice(b"def"); // catalog
        coldef.push(0x00); // schema
        coldef.push(0x00); // table
        coldef.push(0x00); // org_table
        coldef.push(name.len() as u8);
        coldef.extend_from_slice(name.as_bytes());
        coldef.push(0x00); // org_name
        coldef.push(0x0C); // 固定字段长度 12
        coldef.extend_from_slice(&33u16.to_le_bytes()); // charset utf8
        coldef.extend_from_slice(&0u32.to_le_bytes()); // column_length
        coldef.push(0xFD); // 类型 VAR_STRING
        coldef.extend_from_slice(&[0x00, 0x00]); // flags
        coldef.push(0x00); // decimals
        coldef.extend_from_slice(&[0x00, 0x00]); // filler
        coldef
    }

    /// 按表尾号解析本次查询的目标分片 (cluster_id, tablet_id)。
    ///
    /// - SQL 含 `_<数字>` 尾号的表(如 sbtest_1)→ 尾号 % 分片数,定位用户所属集群内的分片
    /// - 有表但无尾号(如 users)→ 默认第一个分片
    /// - 无表语句(SELECT 1 / SHOW 等)→ None(保持当前后端,不做无谓切换)
    fn route_tablet_target(&self, cfg: &AppConfig, tables: &[String]) -> Option<(String, String)> {
        let pu = self.product_user.as_ref()?;
        let cluster = cfg
            .clusters
            .values()
            .find(|c| c.name == pu.cluster_name || c.id == pu.cluster_name)?;
        if cluster.tablets.is_empty() {
            return None;
        }
        // 第一个带尾号的表决定路由
        for t in tables {
            if let Some(idx) = crate::parser::route::table_tail_index(t, cluster.tablets.len()) {
                let tablet = &cluster.tablets[idx];
                return Some((tablet.cluster_id.clone(), tablet.tablet_id.clone()));
            }
        }
        // 有表但无尾号 → 默认第一个分片
        if !tables.is_empty() {
            let tablet = &cluster.tablets[0];
            return Some((tablet.cluster_id.clone(), tablet.tablet_id.clone()));
        }
        None
    }

    /// 读分流计划:受管分片且生效档位非 strong 时,为纯 SELECT 计算角色与屏障。
    /// 返回 Some((decision, barrier_wait_ms))——decision.role==Follower 才走从库。
    fn plan_read_route(
        &self,
        cfg: &AppConfig,
        target: Option<(&str, &str)>,
        db_user: Option<&DbUser>,
        sql: &str,
    ) -> Option<(crate::ha::consistency::RouteDecision, Option<u64>)> {
        use crate::config::ReadConsistency;
        use crate::ha::consistency::{Barrier, RouteRole, SessionView};

        let (cid, tid) = target?;
        let x = cfg
            .clusters
            .get(cid)?
            .tablets
            .iter()
            .find(|t| t.tablet_id == tid)?
            .xenon
            .as_ref()?;
        if x.read_consistency == ReadConsistency::Strong
            && x.db_overrides.is_empty()
            && x.user_overrides.is_empty()
        {
            return None; // 默认 strong:零开销
        }
        // 生效档位:用户 > 库 > 分片默认
        let mut lvl = x.read_consistency;
        let db = self
            .client_database
            .clone()
            .or_else(|| db_user.and_then(|u| u.default_db.clone()))
            .unwrap_or_default();
        if let Some(v) = x.db_overrides.get(&db.to_ascii_lowercase()) {
            lvl = *v;
        }
        if let Some(pu) = &self.product_user {
            if let Some(v) = x.user_overrides.get(&pu.username.to_ascii_lowercase()) {
                lvl = *v;
            }
        }
        if lvl == ReadConsistency::Strong {
            return None;
        }
        let read_class = crate::parser::analyze::classify_read_only(sql);
        let sess = SessionView {
            in_txn: self.in_txn,
            pinned: self.read_pinned,
            session_dirty_shard: self
                .written_shards
                .get(&format!("{cid}.{tid}"))
                .copied()
                .unwrap_or(false),
        };
        let d = crate::ha::consistency::decide(lvl, read_class, sess);
        if d.role != RouteRole::Follower {
            return Some((d, None));
        }
        let wait = match d.barrier {
            Barrier::WaitHighWater => Some(x.barrier_wait_ms),
            Barrier::None => None,
        };
        Some((d, wait))
    }

    /// 解析指定分片 (cluster_id, tablet_id) 的 master 后端(池键 + 建连目标)。
    /// 运行时拓扑覆盖层(配置中心下发)优先,未命中回落文件配置。
    fn resolve_master(
        &self,
        cfg: &AppConfig,
        cluster_id: &str,
        tablet_id: &str,
    ) -> Option<(
        String,
        u16,
        String,
        String,
        std::sync::Arc<crate::config::Database>,
    )> {
        let t = cfg
            .clusters
            .get(cluster_id)?
            .tablets
            .iter()
            .find(|t| t.tablet_id == tablet_id)?;
        let g = t.groups.iter().find(|g| g.master.is_some())?;
        let base = g.master.as_ref()?;
        let over = self
            .ctx
            .topology
            .get(cluster_id, tablet_id)
            .and_then(|topo| topo.master.clone());
        match over {
            // 配置中心覆盖:克隆基线 Database,仅替换 host/port
            Some(addr) => {
                let mut db = base.clone();
                db.host = addr.host.clone();
                db.port = addr.port;
                Some((
                    db.host.clone(),
                    db.port,
                    cluster_id.to_string(),
                    tablet_id.to_string(),
                    std::sync::Arc::new(db),
                ))
            }
            None => Some((
                base.host.clone(),
                base.port,
                cluster_id.to_string(),
                tablet_id.to_string(),
                std::sync::Arc::new(base.clone()),
            )),
        }
    }

    /// 解析配置中第一个可用 master 后端(默认路由目标)
    fn resolve_first_master(
        &self,
        cfg: &AppConfig,
    ) -> Option<(
        String,
        u16,
        String,
        String,
        std::sync::Arc<crate::config::Database>,
    )> {
        for c in cfg.clusters.values() {
            for t in &c.tablets {
                if let Some(r) = self.resolve_master(cfg, &c.id, &t.tablet_id) {
                    return Some(r);
                }
            }
        }
        None
    }

    /// 确保会话级后端连接已建立(懒初始化),之后整个会话复用。
    ///
    /// 包装:默认 leader 角色(写与不可分流读都走 raft leader)。
    async fn ensure_backend(
        &mut self,
        db_user: Option<&DbUser>,
        target: Option<&(String, String)>,
    ) -> Result<(), ProtoError> {
        self.ensure_backend_ex(db_user, target, MasterSlave::Master, None)
            .await
            .map(|_| ())
    }

    /// 角色感知的连接保证(读写分离核心)。
    ///
    /// - `role=Master`:解析目标分片 raft leader(拓扑覆盖优先),现有语义;
    /// - `role=Slave`:按候选链(followers)建立 follower 连接,`barrier_wait_ms`
    ///   为 Some 时在建连后先执行 GTID 高水位屏障(WAIT_FOR_EXECUTED_GTID_SET),
    ///   失败(从库追不平/错误)自动回落到 leader;
    /// - leader 建连失败且为 xenon 受管分片时,触发 raft 重探一次后按新 leader
    ///   重试一次(仅 ensure 阶段,查询字节未发出,不会重复执行)。
    ///
    /// 用状态循环实现(Slave → Master 单向降级),避免 async 递归。
    ///
    /// 返回最终生效的后端角色:调用方据此修正读路由标识
    /// (请求 follower 但降级主库时,记录改标为 "follower-fallback")。
    async fn ensure_backend_ex(
        &mut self,
        db_user: Option<&DbUser>,
        target: Option<&(String, String)>,
        role: MasterSlave,
        barrier_wait_ms: Option<u64>,
    ) -> Result<MasterSlave, ProtoError> {
        let mut role = role;
        let mut barrier_wait_ms = barrier_wait_ms;
        loop {
            let cfg = self.ctx.load_config();
            let mut connect_attempt = 0u32;

            // 目标分片与当前绑定对比(角色也要一致):命中直接复用/按需补屏障
            if let Some((cid, tid)) = target {
                let key = format!("{cid}.{tid}");
                if self.backend.is_some() {
                    let same_shard = self.backend_shard == key;
                    let same_role = self.backend.as_ref().map(|b| b.role == role).unwrap_or(false);
                    if same_shard && same_role {
                        if role == MasterSlave::Slave {
                            if let Some(ms) = barrier_wait_ms {
                                match self.run_gtid_barrier(db_user, cid, tid, ms).await {
                                    Ok(true) => return Ok(role),
                                    _ => {
                                        self.backend = None; // WAIT-only,干净归还
                                        self.backend_shard.clear();
                                        self.ctx.metrics.ha_reads_leader_fallback.inc();
                                        role = MasterSlave::Master;
                                        barrier_wait_ms = None;
                                        continue;
                                    }
                                }
                            }
                            return Ok(role);
                        }
                        return Ok(role);
                    }
                    // 分片或角色不一致 → 归还当前连接
                    self.backend = None;
                    self.backend_shard.clear();
                }
            } else if role == MasterSlave::Master && self.backend.is_some() {
                return Ok(role); // 无目标且已绑定 → 保持当前后端
            } else if role == MasterSlave::Slave {
                // follower 只服务于带目标分片的纯读;无目标 → 回退 leader 语义
                role = MasterSlave::Master;
                barrier_wait_ms = None;
                continue;
            }

            // ── 解析目标地址 ──(master_addr 供连接/兜底/屏障采样)
            let (cluster_id, tablet_id, db_arc, master_addr): (
                String,
                String,
                std::sync::Arc<crate::config::Database>,
                (String, u16),
            ) = if role == MasterSlave::Slave {
                let (cid, tid) = target
                    .as_ref()
                    .ok_or_else(|| ProtoError::Protocol("follower needs target".into()))?;
                let (h, p, c, t, d) = self
                    .resolve_master(&cfg, cid, tid)
                    .ok_or_else(|| ProtoError::Protocol(format!("no leader for tablet {cid}.{tid}")))?;
                (c, t, d, (h, p))
            } else {
                match target {
                    Some((cid, tid)) => {
                        let (h, p, c, t, d) = self.resolve_master(&cfg, cid, tid).ok_or_else(|| {
                            ProtoError::Protocol(format!("no backend for tablet {cid}.{tid}"))
                        })?;
                        (c, t, d, (h, p))
                    }
                    None => {
                        let (h, p, c, t, d) = self
                            .resolve_first_master(&cfg)
                            .ok_or_else(|| ProtoError::Protocol("no backend configured".into()))?;
                        (c, t, d, (h, p))
                    }
                }
            };
            self.backend_shard = format!("{cluster_id}.{tablet_id}");

            // follower 候选;无候选 → 降级 leader
            let mut candidates: Vec<(String, u16)> = if role == MasterSlave::Slave {
                let v = self.follower_candidates(&cfg, &cluster_id, &tablet_id);
                if v.is_empty() {
                    role = MasterSlave::Master;
                    barrier_wait_ms = None;
                    continue;
                }
                v
            } else {
                vec![master_addr]
            };

            let ha_managed = cfg
                .clusters
                .get(&cluster_id)
                .and_then(|c| c.tablets.iter().find(|t| t.tablet_id == tablet_id))
                .map(|t| t.xenon.is_some())
                .unwrap_or(false);

            let user_id = db_user.map(|u| u.username.as_str()).unwrap_or("");
            let bucket_cfg = BucketCfg {
                max_serve_times: cfg.conn_pool_socket_max_serve_client_times,
                ..BucketCfg::default()
            };
            let target_db = self
                .client_database
                .clone()
                .or_else(|| db_user.and_then(|u| u.default_db.clone()));
            let db_key = target_db.as_deref().unwrap_or_default();

            // 先试池(按角色队列)
            let mut pool_hit: Option<BackendSession> = None;
            for (host, port) in &candidates {
                if let Some(back_conn) = self.ctx.srv_pool.try_acquire(
                    &cluster_id,
                    &tablet_id,
                    user_id,
                    db_key,
                    role,
                ) {
                    self.ctx.metrics.pool_acquires.inc();
                    let bucket = self.ctx.srv_pool.get_or_create_bucket(
                        &cluster_id,
                        &tablet_id,
                        user_id,
                        db_key,
                        bucket_cfg.clone(),
                    );
                    let guard = BackendGuard::new(back_conn, bucket);
                    let stream = guard.take_stream();
                    info!(%host, %port, "reused backend connection from pool");
                    let buf = BytesMut::with_capacity(4096);
                    if let Some(db) = &target_db {
                        self.set_current_db(Some(db.to_string()));
                    }
                    if let Some(h) = self.reg.as_ref() {
                        *h.backends.write() = vec![format!("{host}:{port}")];
                    }
                    pool_hit = Some(BackendSession {
                        stream: Some(stream),
                        buf,
                        guard: Some(guard),
                        selected_db: target_db.clone(),
                        bucket_db: db_key.to_string(),
                        role,
                    });
                    break;
                }
            }
            if let Some(session) = pool_hit {
                self.backend = Some(session);
                if role == MasterSlave::Slave {
                    if let Some(ms) = barrier_wait_ms {
                        let ok = self
                            .run_gtid_barrier(db_user, &cluster_id, &tablet_id, ms)
                            .await
                            .unwrap_or(false);
                        if !ok {
                            // 追不平 → 归还该连接(仅执行过 WAIT),降级 leader
                            //(外层循环首部会按 Master 角色重新解析)
                            self.backend = None;
                            self.ctx.metrics.ha_reads_leader_fallback.inc();
                            self.ctx.metrics.ha_barrier_timeouts.inc();
                            role = MasterSlave::Master;
                            barrier_wait_ms = None;
                            continue;
                        }
                    }
                    self.ctx.metrics.ha_reads_follower.inc();
                }
                return Ok(role);
            }

            // 池 miss:新建连接
            loop {
                let (host, port) = &candidates[0];
                info!(%host, %port, "connecting to backend (pool miss)");
                let conn_res = tokio::net::TcpStream::connect((host.as_str(), *port)).await;
                let mut stream = match conn_res {
                    Ok(s) => s,
                    Err(e) => {
                        self.ctx.metrics.pool_acquire_fails.inc();
                        if role == MasterSlave::Slave {
                            candidates.remove(0);
                            if !candidates.is_empty() {
                                continue;
                            }
                            self.ctx.metrics.ha_reads_leader_fallback.inc();
                            role = MasterSlave::Master;
                            barrier_wait_ms = None;
                            break; // 回外层循环走 leader 新建路径
                        }
                        // leader:首次失败且受管 → raft 重探一次
                        if connect_attempt == 0 && ha_managed {
                            connect_attempt = 1;
                            warn!(addr = %format!("{host}:{port}"), cluster = %cluster_id,
                                tablet = %tablet_id,
                                "xenon raft: leader connect failed, forcing re-probe");
                            crate::ha::center::HaCenter::force_probe_async(
                                self.ctx.clone(),
                                cluster_id.clone(),
                                tablet_id.clone(),
                            )
                            .await;
                            let cfg = self.ctx.load_config();
                            if let Some((h2, p2, _, _, _)) =
                                self.resolve_master(&cfg, &cluster_id, &tablet_id)
                            {
                                if h2 != *host || p2 != *port {
                                    info!(host = %h2, port = p2, "xenon raft re-probe switched leader; retrying connect once");
                                    candidates[0] = (h2, p2);
                                    continue;
                                }
                            }
                        }
                        return Err(e.into());
                    }
                };
                stream.set_nodelay(true)?;
                let mut buf = BytesMut::with_capacity(4096);
                if let Err(e) = do_backend_handshake(
                    &mut stream,
                    db_user,
                    self.client_database.as_deref(),
                    &mut buf,
                    cfg.default_charset,
                )
                .await
                {
                    self.ctx.metrics.pool_acquire_fails.inc();
                    if role == MasterSlave::Slave {
                        candidates.remove(0);
                        if !candidates.is_empty() {
                            continue;
                        }
                        self.ctx.metrics.ha_reads_leader_fallback.inc();
                        role = MasterSlave::Master;
                        barrier_wait_ms = None;
                        break;
                    }
                    return Err(e);
                }
                let (host, port) = candidates[0].clone();
                info!(%host, %port, "backend handshake complete");

                let addr = stream
                    .peer_addr()
                    .map_err(|e| ProtoError::Protocol(format!("peer_addr: {e}")))?;
                let back_conn = std::sync::Arc::new(BackConn::new(addr, db_arc.clone(), role));
                *back_conn.stream.lock() = Some(stream);
                let bucket = self.ctx.srv_pool.get_or_create_bucket(
                    &cluster_id,
                    &tablet_id,
                    user_id,
                    db_key,
                    bucket_cfg.clone(),
                );
                let guard = BackendGuard::new(back_conn, bucket);
                let mut stream = guard.take_stream();

                let mut selected_db = None;
                if let Some(db) = &target_db {
                    match select_database(&mut stream, db, &mut buf).await {
                        Ok(()) => {
                            debug!(%host, %port, db, "selected database on fresh backend");
                            self.set_current_db(Some(db.clone()));
                            selected_db = Some(db.clone());
                        }
                        Err(e) => {
                            warn!(%host, %port, db, "select database on fresh backend failed: {e}");
                            self.set_current_db(None);
                        }
                    }
                }
                if let Some(h) = self.reg.as_ref() {
                    *h.backends.write() = vec![format!("{host}:{port}")];
                }
                self.backend = Some(BackendSession {
                    stream: Some(stream),
                    buf,
                    guard: Some(guard),
                    selected_db,
                    bucket_db: db_key.to_string(),
                    role,
                });
                if role == MasterSlave::Slave {
                    if let Some(ms) = barrier_wait_ms {
                        let ok = self
                            .run_gtid_barrier(db_user, &cluster_id, &tablet_id, ms)
                            .await
                            .unwrap_or(false);
                        if !ok {
                            // 追不平 → 归还该连接(仅执行过 WAIT),降级 leader:
                            // break 出内层建连循环,由外层按 Master 重新解析
                            self.backend = None;
                            self.ctx.metrics.ha_reads_leader_fallback.inc();
                            self.ctx.metrics.ha_barrier_timeouts.inc();
                            role = MasterSlave::Master;
                            barrier_wait_ms = None;
                            break;
                        }
                    }
                    self.ctx.metrics.ha_reads_follower.inc();
                }
                return Ok(role);
            }
            // 内层循环结束(role 已降为 Master)→ 外层 continue 重新解析
        }
    }

    /// follower 候选:topology.slaves > 配置 slave > xenon members(减 leader)。
    fn follower_candidates(
        &self,
        cfg: &AppConfig,
        cluster_id: &str,
        tablet_id: &str,
    ) -> Vec<(String, u16)> {
        let mut out: Vec<(String, u16)> = Vec::new();
        if let Some(topo) = self.ctx.topology.get(cluster_id, tablet_id) {
            for s in &topo.slaves {
                let k = (s.host.clone(), s.port);
                if !out.contains(&k) {
                    out.push(k);
                }
            }
        }
        if out.is_empty() {
            if let Some(t) = cfg
                .clusters
                .get(cluster_id)
                .and_then(|c| c.tablets.iter().find(|t| t.tablet_id == tablet_id))
            {
                for g in &t.groups {
                    if let Some(sl) = g.slave.as_ref() {
                        let k = (sl.host.clone(), sl.port);
                        if !out.contains(&k) {
                            out.push(k);
                        }
                    }
                }
                if let Some(x) = &t.xenon {
                    let leader_addr = self
                        .ctx
                        .topology
                        .get(cluster_id, tablet_id)
                        .and_then(|tp| tp.master.clone())
                        .map(|m| (m.host, m.port));
                    for m in &x.members {
                        let k = (m.host.clone(), m.mysql_port);
                        if leader_addr.as_ref() == Some(&k) {
                            continue;
                        }
                        if !out.contains(&k) {
                            out.push(k);
                        }
                    }
                }
            }
        }
        out
    }

    /// 在当前后端连接上执行 GTID 高水位屏障:先取 leader `@@GLOBAL.gtid_executed`,
    /// 再在同连接执行 `SELECT WAIT_FOR_EXECUTED_GTID_SET(...)`;返回是否就绪。
    async fn run_gtid_barrier(
        &mut self,
        db_user: Option<&DbUser>,
        cluster_id: &str,
        tablet_id: &str,
        wait_ms: u64,
    ) -> Result<bool, String> {
        let Some(db_user) = db_user else {
            return Ok(false);
        };
        let cfg = self.ctx.load_config();
        let charset = cfg.default_charset;
        let (leader_host, leader_port) = self
            .resolve_master(&cfg, cluster_id, tablet_id)
            .map(|(h, p, _, _, _)| (h, p))
            .ok_or_else(|| "no leader resolved for barrier".to_string())?;

        // 1) 采样 leader 已提交前缀(通过探测客户端;不占用会话连接)
        let res = crate::ha::probe::query_text(
            &leader_host,
            leader_port,
            db_user,
            charset,
            "SELECT @@GLOBAL.gtid_executed",
            wait_ms.saturating_add(1500),
        )
        .await?;
        let set = res
            .rows
            .first()
            .and_then(|r| r.first().cloned())
            .flatten()
            .ok_or_else(|| "leader gtid_executed empty".to_string())?;
        if set.trim().is_empty() {
            return Err("leader gtid_executed empty".into());
        }

        // 2) 在当前(follower)连接上等待该前缀(WAIT 与原 SELECT 同连接)
        let wait_sql = format!(
            "SELECT WAIT_FOR_EXECUTED_GTID_SET('{}', {})",
            set.replace('\'', "''"),
            wait_ms
        );
        let backend = self.backend.as_mut().ok_or("no backend for barrier")?;
        let stream = backend
            .stream
            .as_mut()
            .ok_or_else(|| "backend stream missing".to_string())?;
        let r = crate::ha::probe::query_text_on_stream(stream, &mut backend.buf, &wait_sql)
            .await?;
        let ok = r
            .rows
            .first()
            .and_then(|row| row.first().cloned())
            .flatten()
            .map(|v| v == "1")
            .unwrap_or(false);
        Ok(ok)
    }

    /// 处理简单透传命令(COM_INIT_DB / COM_STATISTICS),复用会话级后端连接
    async fn handle_pass_through(&mut self, cmd: &CommandPacket) -> Result<(), ProtoError> {
        let pu = self.product_user.as_ref().unwrap().clone();
        let cfg = self.ctx.load_config();
        let db_user = cfg.db_users.get(&pu.db_username);

        if let Err(e) = self.ensure_backend(db_user, None).await {
            if let ProtoError::AuthFailed(msg) = &e {
                let err_pkt = crate::proto::error::build_error(1049, "42000", msg);
                self.send_client(1, &err_pkt).await?;
                return Ok(());
            }
            return Err(e);
        }

        let mut cmd_pkt = vec![cmd.command.as_u8()];
        cmd_pkt.extend_from_slice(&cmd.payload);

        let backend = self.backend.as_mut().unwrap();
        let be_stream = backend
            .stream
            .as_mut()
            .expect("BackendSession: stream missing");
        if let Err(e) = codec::send_packet(be_stream, 0, &cmd_pkt).await {
            backend.mark_broken();
            return Err(e);
        }
        let mut fc = result::ForwardCount::default();
        let kind =
            result::forward_simple_response(be_stream, &mut self.stream, &mut backend.buf, &mut fc)
                .await?;
        // 按库流量统计(发):转发字节/包数计入当前会话库
        let db_now = self.current_db.clone().unwrap_or_default();
        self.ctx
            .metrics
            .record_db_traffic(&db_now, 0, fc.bytes, 0, fc.pkts);
        let node = self.backend_shard.clone();
        self.ctx
            .metrics
            .record_node_traffic(&node, 0, fc.bytes, 0, fc.pkts);
        // 跟踪会话当前库:COM_INIT_DB 成功(OK)则记录,失败(1049 等)则置空,
        // 供 SELECT DATABASE() 本地应答使用;同时同步后端连接的 selected_db
        // (会话中途切库后,归还池时因 selected_db != 桶键 db 而被丢弃,
        // 保证按 db 分桶的池不被污染)。
        if cmd.command == Command::InitDb {
            let db = String::from_utf8_lossy(&cmd.payload).into_owned();
            let ok = matches!(kind, result::ResponseKind::Ok);
            if let Some(bs) = self.backend.as_mut() {
                bs.selected_db = if ok { Some(db.clone()) } else { None };
            }
            self.set_current_db(if ok { Some(db) } else { None });
        }
        Ok(())
    }

    /// 处理 COM_FIELD_LIST:透传 ColumnDef* + EOF 响应
    ///
    /// 与 `handle_pass_through` 的区别:使用 `forward_field_list_response` 而非
    /// `forward_simple_response`。后者会因 Column Definition 包首字节 0x03 与
    /// OK 包 affected_rows=3 无法区分而提前退出。
    async fn handle_field_list(&mut self, cmd: &CommandPacket) -> Result<(), ProtoError> {
        let pu = self.product_user.as_ref().unwrap().clone();
        let cfg = self.ctx.load_config();
        let db_user = cfg.db_users.get(&pu.db_username);

        if let Err(e) = self.ensure_backend(db_user, None).await {
            if let ProtoError::AuthFailed(msg) = &e {
                let err_pkt = crate::proto::error::build_error(1049, "42000", msg);
                self.send_client(1, &err_pkt).await?;
                return Ok(());
            }
            return Err(e);
        }

        let mut cmd_pkt = vec![cmd.command.as_u8()];
        cmd_pkt.extend_from_slice(&cmd.payload);

        let backend = self.backend.as_mut().unwrap();
        let be_stream = backend
            .stream
            .as_mut()
            .expect("BackendSession: stream missing");
        if let Err(e) = codec::send_packet(be_stream, 0, &cmd_pkt).await {
            backend.mark_broken();
            return Err(e);
        }
        let mut fc = result::ForwardCount::default();
        if let Err(e) = result::forward_field_list_response(
            be_stream,
            &mut self.stream,
            &mut backend.buf,
            &mut fc,
        )
        .await
        {
            backend.mark_broken();
            return Err(e);
        }
        // 按库流量统计(发):转发字节/包数计入当前会话库
        let db_now = self.current_db.clone().unwrap_or_default();
        self.ctx
            .metrics
            .record_db_traffic(&db_now, 0, fc.bytes, 0, fc.pkts);
        let node = self.backend_shard.clone();
        self.ctx
            .metrics
            .record_node_traffic(&node, 0, fc.bytes, 0, fc.pkts);
        Ok(())
    }

    /// 处理预处理语句命令(COM_STMT_PREPARE/EXECUTE/CLOSE/RESET/SEND_LONG_DATA/FETCH):
    /// 透传到会话级后端连接,按命令类型选择响应转发策略。
    async fn handle_stmt(&mut self, cmd: &CommandPacket) -> Result<(), ProtoError> {
        let pu = self.product_user.as_ref().unwrap().clone();
        let cfg = self.ctx.load_config();
        let db_user = cfg.db_users.get(&pu.db_username);

        if let Err(e) = self.ensure_backend(db_user, None).await {
            if let ProtoError::AuthFailed(msg) = &e {
                let err_pkt = crate::proto::error::build_error(1049, "42000", msg);
                self.send_client(1, &err_pkt).await?;
                return Ok(());
            }
            return Err(e);
        }

        let mut cmd_pkt = vec![cmd.command.as_u8()];
        cmd_pkt.extend_from_slice(&cmd.payload);

        let backend = self.backend.as_mut().unwrap();
        let be_stream = backend
            .stream
            .as_mut()
            .expect("BackendSession: stream missing");
        if let Err(e) = codec::send_packet(be_stream, 0, &cmd_pkt).await {
            backend.mark_broken();
            return Err(e);
        }

        let mut fc = result::ForwardCount::default();
        match cmd.command {
            Command::StmtPrepare => {
                // prepared 语句绑定在后端,本会话读不再分流(读写分离关闭)
                self.read_pinned = true;
                // COM_STMT_PREPARE 响应:COM_STMT_PREPARE_OK + params列定义 + EOF
                // + columns列定义 + EOF。首包 0x00 会被通用转发误判为 OK,故专用。
                match result::forward_stmt_prepare_response(
                    be_stream,
                    &mut self.stream,
                    &mut backend.buf,
                    &mut fc,
                )
                .await
                {
                    Ok(prepare_ok) => {
                        // 后端已创建 prepared statement(服务端持有;代理不做
                        // stmt_id 跟踪,无法保证客户端 COM_STMT_CLOSE 全部到达):
                        // 会话结束时该连接**不再回池**,直接丢弃释放服务端语句。
                        if prepare_ok {
                            backend.mark_no_reuse();
                        }
                    }
                    Err(e) => {
                        backend.mark_broken();
                        return Err(e);
                    }
                }
            }
            Command::StmtExecute | Command::StmtFetch => {
                // COM_STMT_EXECUTE/FETCH 响应与 COM_QUERY 同构(结果集或 OK/ERR)
                if let Err(e) = result::forward_backend_response(
                    be_stream,
                    &mut self.stream,
                    &mut backend.buf,
                    &mut fc,
                )
                .await
                {
                    backend.mark_broken();
                    return Err(e);
                }
            }
            Command::StmtReset => {
                // COM_STMT_RESET 响应:单个 OK
                if let Err(e) = result::forward_simple_response(
                    be_stream,
                    &mut self.stream,
                    &mut backend.buf,
                    &mut fc,
                )
                .await
                {
                    backend.mark_broken();
                    return Err(e);
                }
            }
            Command::StmtClose | Command::StmtSendLongData => {
                // COM_STMT_CLOSE / COM_STMT_SEND_LONG_DATA:后端不返回响应
            }
            _ => {}
        }
        // 按库流量统计(发):本命令转发的字节/包数计入当前会话库
        let db_now = self.current_db.clone().unwrap_or_default();
        self.ctx
            .metrics
            .record_db_traffic(&db_now, 0, fc.bytes, 0, fc.pkts);
        let node = self.backend_shard.clone();
        self.ctx
            .metrics
            .record_node_traffic(&node, 0, fc.bytes, 0, fc.pkts);
        Ok(())
    }
}

// ─── 后端握手辅助 ───

/// 完成后端 MySQL 的完整握手(mysql_native_password)
pub(crate) async fn do_backend_handshake(
    stream: &mut TcpStream,
    db_user: Option<&DbUser>,
    default_db: Option<&str>,
    buf: &mut BytesMut,
    charset: u8,
) -> Result<(BackendGreeting, u8), ProtoError> {
    // 接收后端 Server Greeting
    let (_, greeting_pkt) = codec::read_packet(stream, buf).await?;
    tracing::debug!(
        "backend greeting: {} bytes, first_byte={:02x}",
        greeting_pkt.len(),
        greeting_pkt.first().unwrap_or(&0)
    );
    let gre = auth::parse_backend_greeting(&greeting_pkt)
        .map_err(|e| ProtoError::Protocol(format!("backend greeting: {e}")))?;
    tracing::debug!(
        "backend greeting parsed: version={}, caps={:#010x}, charset={}, plugin={}",
        gre.server_version,
        gre.capabilities,
        gre.charset,
        gre.auth_plugin_name
    );

    // 构建 auth response
    let db_user = db_user.ok_or_else(|| ProtoError::Protocol("no db user".into()))?;
    // 目标库(客户端 auth 指定 或 配置 default_db)——仅用于日志,**不**写入
    // auth 包:实测部分后端/网关对 HandshakeResponse41 携带 CLIENT_CONNECT_WITH_DB
    // (无论带不带库名)的连接在命令阶段直接 RST(客户端表现为 ERROR 2013),
    // 故后端 auth 恒不置该标志、不携带库名;库选择由 ensure_backend 在会话
    // 建立后统一补发 COM_INIT_DB 完成(含池复用路径)。
    let database = default_db.or(db_user.default_db.as_deref());
    let backend_caps = handshake::BACKEND_CAP_FULL;

    let auth_resp = if gre.auth_plugin_name == "caching_sha2_password" {
        // MySQL 8.0: caching_sha2_password
        auth::build_sha256_auth_response(
            &db_user.username,
            &db_user.password,
            &gre.scramble,
            backend_caps,
            charset,
        )
    } else {
        // MySQL 5.7: mysql_native_password
        auth::build_backend_auth_response(&db_user.username, &db_user.password, &gre, charset)
    };

    tracing::debug!(
        "sending auth packet: {} bytes payload (caps={:#010x}, charset={}, db_omitted={:?})",
        auth_resp.len(),
        backend_caps,
        charset,
        database
    );
    codec::send_packet(stream, 1, &auth_resp).await?;

    // 读取后端认证结果
    let (_, result) = codec::read_packet(stream, buf).await?;
    let rprev: String = result
        .iter()
        .take(16)
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(" ");
    tracing::debug!(
        "backend auth result: {} bytes, first_bytes=[{}]",
        result.len(),
        rprev
    );
    if result.is_empty() {
        return Err(ProtoError::Protocol("empty auth result".into()));
    }
    // 握手完成后进入命令阶段，seq 重置为 0（MySQL 协议规定）
    let mut next_seq = 0u8;

    match result[0] {
        crate::proto::error::OK_HEADER => {
            // OK — 认证成功
            debug!("backend auth OK");
        }
        crate::proto::error::ERR_HEADER => {
            let msg = crate::proto::error::parse_error_message(&result);
            return Err(ProtoError::AuthFailed(msg));
        }
        other => {
            // 可能是 AuthMoreData (0x01) 或 AuthSwitch (0xFE)
            if other == auth::AUTH_MORE_DATA {
                let amd = auth::parse_auth_more_data(&result)
                    .map_err(|e| ProtoError::Protocol(format!("auth_more_data: {e}")))?;
                match amd {
                    AuthMoreDataResponse::FastAuthSuccess => {
                        // caching_sha2 fast-auth 直接成功
                        debug!("backend auth: fast-auth success");
                    }
                    AuthMoreDataResponse::PerformFullAuth => {
                        // 需要发送明文密码
                        let clear = auth::build_sha256_clear_password(&db_user.password);
                        codec::send_packet(stream, 2, &clear).await?;
                        // 读取最终结果
                        let (_, final_result) = codec::read_packet(stream, buf).await?;
                        if final_result.is_empty()
                            || final_result[0] != crate::proto::error::OK_HEADER
                        {
                            let msg =
                                if final_result.first() == Some(&crate::proto::error::ERR_HEADER) {
                                    crate::proto::error::parse_error_message(&final_result)
                                } else {
                                    format!(
                                        "backend full auth failed (header=0x{:02X})",
                                        final_result.first().unwrap_or(&0)
                                    )
                                };
                            return Err(ProtoError::AuthFailed(msg));
                        }
                        next_seq = 0;
                    }
                    _ => {
                        return Err(ProtoError::Protocol(format!(
                            "unexpected auth_more_data: {:?}",
                            amd
                        )));
                    }
                }
            } else if other == auth::AUTH_SWITCH_REQUEST {
                let sw = auth::parse_auth_switch_request(&result)
                    .map_err(|e| ProtoError::Protocol(format!("auth_switch: {e}")))?;
                let sw_resp = auth::build_auth_switch_response(
                    &db_user.password,
                    &sw.plugin_name,
                    &sw.auth_plugin_data,
                )
                .map_err(|e| ProtoError::Protocol(format!("auth_switch_resp: {e}")))?;
                codec::send_packet(stream, 2, &sw_resp).await?;
                // 读取最终结果
                let (_, final_result) = codec::read_packet(stream, buf).await?;
                if final_result.is_empty() || final_result[0] != crate::proto::error::OK_HEADER {
                    let msg = if final_result.first() == Some(&crate::proto::error::ERR_HEADER) {
                        crate::proto::error::parse_error_message(&final_result)
                    } else {
                        format!(
                            "backend auth switch failed (header=0x{:02X})",
                            final_result.first().unwrap_or(&0)
                        )
                    };
                    return Err(ProtoError::AuthFailed(msg));
                }
                next_seq = 0;
            } else {
                return Err(ProtoError::Protocol(format!(
                    "unexpected auth result: 0x{:02X}",
                    other
                )));
            }
        }
    }

    // 注意:库选择(COM_INIT_DB)不在这里做,统一由 ensure_backend 在会话
    // 建立后对两条路径(池命中/新建)都执行,避免池复用连接残留旧库状态。

    Ok((gre, next_seq))
}

/// 提取简单 `USE <db>` 语句的库名(忽略大小写/首尾空白/尾分号/前导注释)。
///
/// 基于词法器:首 token 为 USE、次 token 为库名(支持反引号/双引号包裹,
/// 如 `` USE `my db` ``);其后只允许空白/注释/尾分号。
/// 非 USE 查询只扫前 1~2 个 token 即返回 None,热路径零全串拷贝。
fn use_db_name(sql: &str) -> Option<String> {
    use crate::parser::lex::{self, Lexer, TokenKind};

    let mut lexer = Lexer::new(sql);
    // 跳过前导注释,取首个 token
    let first = loop {
        match lexer.next_token() {
            None => return None,
            Some(t) if t.kind == TokenKind::Comment => continue,
            Some(t) => break t,
        }
    };
    if !lex::is_word(sql, first, "use") {
        return None;
    }
    let second = loop {
        match lexer.next_token() {
            None => return None,
            Some(t) if t.kind == TokenKind::Comment => continue,
            Some(t) => break t,
        }
    };
    let db = match second.kind {
        // 反引号/引号包裹或裸标识符都算库名;去掉包裹符并解码双写转义
        TokenKind::Word | TokenKind::Str => {
            let raw = second.text(sql);
            let (inner, wrap) = if raw.len() >= 2 && raw.starts_with('`') && raw.ends_with('`') {
                (&raw[1..raw.len() - 1], '`')
            } else if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
                (&raw[1..raw.len() - 1], '"')
            } else {
                (raw, '\0')
            };
            let decoded = if wrap == '`' {
                inner.replace("``", "`")
            } else if wrap == '"' {
                inner.replace("\"\"", "\"")
            } else if wrap == '\'' {
                inner.replace("''", "'")
            } else {
                inner.to_string()
            };
            if decoded.is_empty() {
                return None;
            }
            decoded
        }
        _ => return None,
    };
    // 之后只允许注释与尾分号
    loop {
        match lexer.next_token() {
            None => return Some(db.to_string()),
            Some(t) if t.kind == TokenKind::Comment => continue,
            Some(t) if t.kind == TokenKind::Punct && t.text(sql) == ";" => {
                // 分号后只允许空白/注释/更多分号
                loop {
                    match lexer.next_token() {
                        None => return Some(db.to_string()),
                        Some(t2) if t2.kind == TokenKind::Comment => continue,
                        Some(t2) if t2.kind == TokenKind::Punct && t2.text(sql) == ";" => continue,
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
    }
}

/// 语句"种类"(读写分离状态机的最小判定;基于词法器首词,忽略大小写/前导注释)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryKind {
    Select,
    Begin,
    Commit,
    Rollback,
    Set,
    /// INSERT/UPDATE/DELETE/REPLACE 等写(执行成功后标记会话写过)
    Write,
    Other,
}

fn query_kind(sql: &str) -> QueryKind {
    use crate::parser::lex::{self, Lexer, TokenKind};
    let mut lexer = Lexer::new(sql);
    let first = loop {
        match lexer.next_token() {
            None => return QueryKind::Other,
            Some(t) if t.kind == TokenKind::Comment => continue,
            Some(t) => break t,
        }
    };
    if first.kind != TokenKind::Word {
        return QueryKind::Other;
    }
    let w = first.text(sql).to_ascii_lowercase();
    match w.as_str() {
        "select" | "with" | "(" => QueryKind::Select,
        "begin" => QueryKind::Begin,
        "start" => {
            // START TRANSACTION 才开事务;START SLAVE 等按 Other
            match lexer.next_token() {
                Some(t)
                    if t.kind == TokenKind::Word
                        && lex::eq_ignore_ascii_case(t.text(sql), "transaction") =>
                {
                    QueryKind::Begin
                }
                _ => QueryKind::Other,
            }
        }
        "commit" => QueryKind::Commit,
        "rollback" => QueryKind::Rollback,
        "set" => QueryKind::Set,
        "insert" | "update" | "delete" | "replace" => QueryKind::Write,
        _ => QueryKind::Other,
    }
}

/// 轻量判断 SQL 是否为文本协议 PREPARE 语句(`PREPARE stmt FROM ...`)。
///
/// 基于词法器:跳过前导注释后首个 token 为 PREPARE 关键字即可
/// (MySQL 语法中 PREPARE 是保留字,以此开头的合法语句只可能是 PREPARE)。
fn is_text_prepare(sql: &str) -> bool {
    use crate::parser::lex::{self, Lexer, TokenKind};
    let mut lexer = Lexer::new(sql);
    loop {
        match lexer.next_token() {
            None => return false,
            Some(t) if t.kind == TokenKind::Comment => continue,
            Some(t) => return lex::is_word(sql, t, "prepare"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_text_prepare;
    use super::use_db_name;

    #[test]
    fn use_db_name_parses() {
        assert_eq!(use_db_name("USE sbtest"), Some("sbtest".to_string()));
        assert_eq!(use_db_name("use test2;"), Some("test2".to_string()));
        assert_eq!(use_db_name("  USE  mydb  ;  "), Some("mydb".to_string()));
        assert_eq!(use_db_name("USE `my db`"), Some("my db".to_string()));
        assert_eq!(use_db_name("USE `a``b`"), Some("a`b".to_string()));
        assert_eq!(use_db_name("-- c\nUSE sbtest"), Some("sbtest".to_string()));
        // 非 USE 或不支持的形式 → None
        assert_eq!(use_db_name("SELECT 1"), None);
        assert_eq!(use_db_name("USE"), None);
        assert_eq!(use_db_name("USE a b"), None);
        assert_eq!(use_db_name("USE a, b"), None);
        assert_eq!(use_db_name("USE 1"), None);
        assert_eq!(use_db_name("USE `my db` SELECT 1"), None);
        assert_eq!(use_db_name(""), None);
    }

    #[test]
    fn text_prepare_detection() {
        // 命中
        assert!(is_text_prepare("PREPARE stmt FROM 'SELECT 1'"));
        assert!(is_text_prepare("prepare s FROM 'SELECT ?'"));
        assert!(is_text_prepare("  Prepare\ns FROM 'SELECT 1'"));
        assert!(is_text_prepare("PREPARE s FROM 'x'"));
        // 前导注释也能识别(token 定位)
        assert!(is_text_prepare("-- comment\nPREPARE s FROM 'x'"));

        // 不命中
        assert!(!is_text_prepare("SELECT * FROM t"));
        assert!(!is_text_prepare("PREPARED_stmt FROM 'x'")); // 非保留字前缀
        assert!(!is_text_prepare(""));
        assert!(!is_text_prepare("EXECUTE s"));
        assert!(!is_text_prepare("DEALLOCATE PREPARE s"));
        assert!(!is_text_prepare("/* hint */ SELECT 1"));
    }
}

/// 向后端补发 COM_INIT_DB 选择目标库(命令阶段 seq 从 0 开始,响应为单个 OK/ERR 包)
async fn select_database(
    stream: &mut TcpStream,
    db: &str,
    buf: &mut BytesMut,
) -> Result<(), ProtoError> {
    let mut pkt = vec![command::Command::InitDb.as_u8()];
    pkt.extend_from_slice(db.as_bytes());
    codec::send_packet(stream, 0, &pkt).await?;
    let (_seq, resp) = codec::read_packet(stream, buf).await?;
    match resp.first() {
        Some(&crate::proto::error::OK_HEADER) => Ok(()),
        Some(&crate::proto::error::ERR_HEADER) => Err(ProtoError::Protocol(format!(
            "COM_INIT_DB '{db}': {}",
            crate::proto::error::parse_error_message(&resp)
        ))),
        other => Err(ProtoError::Protocol(format!(
            "COM_INIT_DB '{db}' unexpected header: 0x{:02X}",
            other.unwrap_or(&0)
        ))),
    }
}

// ─── 连接入口 ───

/// 每个前端连接对应一个 task
pub async fn conn_task(stream: TcpStream, peer_addr: SocketAddr, ctx: Arc<AppCtx>) {
    // 前端 socket 必须禁用 Nagle:send_packet 每包做两次小写入(header+payload),
    // Nagle 会滞留第二次写入直到首个写入收到 ACK——ping-pong 流量下每次往返
    // 额外等待一个 ACK 周期,显著抬高小查询延迟(实测 p50 翻倍以上)。
    let _ = stream.set_nodelay(true);
    ctx.metrics.connections_total.inc();
    let mut conn = FrontConn::new(stream, peer_addr, ctx.clone());
    // 预分配 cid(注册表需要先于握手建立),握手 greeting 使用同一 id
    conn.cid = ctx.next_connection_id();
    let (handle, mut kill_rx) = ctx.register_connection(conn.cid, peer_addr);
    conn.reg = Some(handle);
    conn.kill_rx = kill_rx.clone();
    // 任何退出路径都注销注册表(RAII)
    struct Unregister(Arc<AppCtx>, u32);
    impl Drop for Unregister {
        fn drop(&mut self) {
            self.0.unregister_connection(self.1);
        }
    }
    let _unreg = Unregister(ctx.clone(), conn.cid);

    loop {
        // kill 信号已在上一轮触发(错过 changed() 的情形)→ 直接退出
        if *kill_rx.borrow() {
            info!(addr = %peer_addr, cid = conn.cid, "connection killed by checkproxy kill");
            break;
        }
        tokio::select! {
            biased;
            // 命令循环空闲等待(等客户端下一条命令)时被 kill → 立即干净退出,
            // 后端连接正常归还池
            _ = kill_rx.changed() => {
                info!(addr = %peer_addr, cid = conn.cid, "connection killed by checkproxy kill");
                break;
            }
            result = conn.drive() => {
                match result {
                    Ok(true) => {} // 继续
                    Ok(false) => {
                        debug!(addr = %peer_addr, cid = conn.cid, "connection closed");
                        break;
                    }
                    Err(e) => {
                        match e {
                            ProtoError::ConnectionClosed | ProtoError::Io(_) => {
                                debug!(addr = %peer_addr, cid = conn.cid, "connection reset: {}", e);
                            }
                            _ => {
                                error!(addr = %peer_addr, cid = conn.cid, "drive error: {}", e);
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
    // 认证成功后该连接会计入 active,结束时必须扣减
    if conn.state == FrontState::CommandLoop {
        ctx.metrics.connections_active.dec();
    }
}

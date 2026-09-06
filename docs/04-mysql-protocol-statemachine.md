# MySQL 协议状态机(async 实现)

> 承接 [02-并发模型](./02-concurrency-model.md) §3。本文档定义 Rust 版 MySQL wire protocol 的 async 编解码与状态机。
>
> **实现标准:以 MySQL 官方协议文档为权威规范**,而非从 C 代码逆向。
> - 官方文档:https://dev.mysql.com/doc/dev/mysql-server/latest/PAGE_PROTOCOL.html
> - 四个顶层章节:**Protocol Basics**(帧/数据类型/能力位)、**Connection Lifecycle**、**Connection Phase**(握手/auth/TLS)、**Command Phase**(COM_* 命令与响应)。
> - C 侧源码(`tr_packet.c` 3047、`tr_packet_com.h`、`tr_front_auth.c`、`tr_back_auth.c`)仅作**语义对照与缺口参照**——见 §1.2。
>
> **目标 MySQL 版本**:后端 MySQL 8.x(客户端用 `mysql-connector-j-8.4.0.jar`,已核实)。因此协议层需覆盖 MySQL 8.0+ 默认的 `caching_sha2_password`,这是 C 版未实现的关键缺口。

## 1. 规范来源与实现范围

### 1.1 官方文档章节 → 实现映射

| 官方章节 | 覆盖内容 | 本文实现 | 状态 |
|---|---|---|---|
| **Protocol Basics** | packet 帧(4B header:3B len LE + 1B seq)、lenenc int/string、capability flags | §2 | 必须 |
| **Connection Phase · Handshake** | `Protocol::Handshake`(server greeting)、`Protocol::HandshakeResponse`(client) | §4.1–4.2 | 必须 |
| **Connection Phase · SSLRequest** | 客户端请求 TLS 的 `Protocol::SSLRequest` + SSL 握手序列 | §4.4 | 新增(C 版无) |
| **Connection Phase · Auth Methods** | `mysql_native_password`(SHA1)、`caching_sha2_password`(SHA256+nonce)、`AuthSwitchRequest`、`AuthMoreData`(0x01) | §4.3–4.5 | 新增 caching_sha2/switch(C 版无) |
| **Connection Lifecycle** | 连接建立/认证结果/断开 | §3 状态机 | 必须 |
| **Command Phase** | COM_* 命令分发 | §5 | 必须 |
| **Command Phase · Response** | OK(0x00)/ERR(0xFF)/EOF(0xFE)/Result Set(column def + row + EOF/OK)、`CLIENT_DEPRECATE_EOF` | §6 | 必须,且支持新式 OK 结束 |
| **Command Phase · Prepared** | COM_STMT_PREPARE/EXECUTE/CLOSE/RESET/SEND_LONG_DATA/FETCH、binary protocol 参数 | §7 | 必须 |

### 1.2 C 版实现现状与缺口(对照规范,决定补齐范围)

C 版实现的事实(已逐行核实),及其相对官方规范的**缺口**:

- **包帧**:4B header,3B 长度 LE + 1B seq(`tr_packet_com.h`/`tr_packet.c`)——✅ 符合规范。但 C 版**不做 0xFFFFFF 分包重组**,只在 `tr_conn.c:998` 拒绝超长包;规范要求 payload 恰为 0xFFFFFF 时有后续包,**Rust 版须补齐分包重组**(§2)。
- **认证**:仅 `mysql_native_password`,20B scramble(SHA1 challenge-response,`tr_back_auth.c:334-339`)。
  - ❌ **缺 `caching_sha2_password`**:MySQL 8.0+ 默认插件,Rust 版须实现(SHA256 + 20B nonce,明文/SSL/ RSA 三条路径,§4.5)。
  - ❌ **缺 `AuthSwitchRequest`**:server 要求 client 切换插件时的协议;C 版不处理,故连 MySQL 8.0 默认账号会失败。
  - ❌ **缺 SSL/TLS**:`Protocol::SSLRequest` 未实现,`CLIENT_SSL` 定义但不用。
- **协议风格**:pre-8.0 EOF 风格(总发 EOF 包),`CLIENT_DEPRECATE_EOF` 定义但未启用——**Rust 版须支持新式 OK 结束**(client 宣告 `CLIENT_DEPRECATE_EOF` 时用 OK 包替代 EOF,§6)。
- **能力位**:`CLIENT_PROTOCOL_41`/`SECURE_CONNECTION`/`PLUGIN_AUTH`/`FOUND_ROWS`/`IGNORE_SPACE`/`MULTI_RESULTS` 等已定义(`tr_packet_com.h:37-56`)——✅,但 Rust 版须补 `CLIENT_SSL`、`CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA`、`CLIENT_DEPRECATE_EOF` 的实际处理。
- **命令集**(`is_valid_command`,`tr_packet.c:2593-2599`):基础 + 预编译完整——✅,作为实现清单。
- **前后端分离**:front(客户端→proxy)与 back(proxy→MySQL)各一套握手/auth——✅ 保留,但两侧插件须对齐(caching_sha2 下 proxy 对后端也要走该插件)。

> **结论**:C 版是一个 pre-8.0、无 TLS 的子集。Rust 版以官方规范为标准,首版至少补齐 `caching_sha2_password` + `AuthSwitchRequest` + 新式 OK 结束 + 分包重组——否则连不上 MySQL 8 默认账号。SSL/TLS 可作为首版之后的增量(§4.4)。

## 2. 编解码基础:Packet 帧读写

C 侧用裸 `read()`/`write()` + `tr_byte_array_t` 缓冲手工拼帧。Rust 用 `tokio::AsyncReadExt`/`BytesMut`,帧边界由专门的 `read_packet` 保证。

```rust
use bytes::{BytesMut, Buf, BufMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncRead, AsyncWrite};

const HEADER_LEN: usize = 4;

/// 读取一个完整 MySQL packet(自动跨多次 read 拼帧)
/// 对应 C 侧 tr_conn.c real_read + packet_len/header_read_len 状态
async fn read_packet<R: AsyncRead + Unpin>(r: &mut R, buf: &mut BytesMut) -> Result<Bytes> {
    // 1. 读 4 字节 header
    buf.reserve(HEADER_LEN);
    while buf.len() < HEADER_LEN {
        let n = r.read_buf(buf).await?;
        if n == 0 { return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()); }
    }
    let len = (&buf[0..3]).get_uint_le(3) as usize;
    let seq = buf[3];
    buf.advance(HEADER_LEN);

    // 2. 读 len 字节 payload
    buf.reserve(len);
    while buf.len() < len {
        let n = r.read_buf(buf).await?;
        if n == 0 { return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()); }
    }
    let payload = buf.split_to(len).freeze();

    // 3. 处理 >16MB 的分片包(MySQL 协议:payload 恰好 0xFFFFFF 时有后续包)
    if len == 0xFFFFFF {
        let mut rest = read_packet(r, buf).await?;
        // 合并(实际场景几乎不触发,proxy 侧大结果集走流式,见 §6)
        let mut combined = payload.to_vec();
        combined.extend_from_slice(&rest);
        return Ok(Bytes::from(combined));
    }
    Ok(payload)  // seq 通过返回值或外部状态维护
}
```

**`AsyncReadExt::read_buf` 自动处理 partial read**——替代 C 侧 `header_read_len`/`packet_read_len` 两个偏移变量的手工状态机。帧拼装逻辑从 ~200 行 C 收敛到 ~20 行 Rust。

## 3. 前端状态机(客户端 → proxy)

对应 C 侧 `tr_front_auth.c` + `tr_front_cmd.c` 的 `STATE_FRONTEND_*` 序列。

```rust
enum FrontState {
    Accepted,           // 刚 accept,准备发 handshake
    HandshakeSent,      // 已发 server handshake,等 client auth response
    AuthRead,           // 读到 auth response,校验
    AuthResultSent,     // 已发 auth ok/err
    WaitBackend,        // 等后端就绪
    UpstreamReturned,   // 后端就绪,进入命令循环
    CommandLoop,        // 主循环:读命令 → 路由 → 转发 → 回结果
}

impl FrontConn {
    async fn drive(&mut self) -> Result<DriveOutcome> {
        match self.state {
            FrontState::Accepted => {
                let hs = build_handshake(&self.ctx);   // §4
                self.send_packet(hs).await?;
                self.state = FrontState::HandshakeSent;
            }
            FrontState::HandshakeSent => {
                let pkt = self.read_packet().await?;
                let cap = parse_client_auth(&pkt)?;     // §4:解析能力位、用户名、scramble、db
                if !self.ctx.auth.check(&cap)? {
                    self.send_error(1045, "Access denied").await?;
                    return Ok(DriveOutcome::ShutDown);
                }
                self.state = FrontState::AuthRead;
            }
            FrontState::AuthRead => {
                self.send_ok().await?;
                self.state = FrontState::CommandLoop;
            }
            FrontState::CommandLoop => {
                let cmd = self.read_command().await?;    // §5
                self.dispatch(cmd).await?;
            }
            _ => {}
        }
        Ok(DriveOutcome::Continue)
    }
}
```

## 4. 握手与认证

### 4.1 Server handshake(发往客户端)
对应 C 侧 `sent_handshake`(`tr_front_auth.c:170`)。`handshake_packet` 结构已核实(`tr_conn.h:4-11`):protocol_version、server_version、thread_id、scramble(8+12)、capability、charset、server_status。

```rust
fn build_handshake(ctx: &AppCtx) -> Bytes {
    let mut buf = BytesMut::new();
    buf.put_u8(10);                              // protocol version
    buf.put_slice(b"5.7.0-newproxy\0");           // server version
    buf.put_u32_le(ctx.next_thread_id());        // connection id
    buf.put_slice(&ctx.scramble()[0..8]);        // scramble part 1
    buf.put_u8(0);                               // filler
    buf.put_u16_le(SERVER_CAP_LOWER);            // capability flags lower
    buf.put_u8(33);                              // charset utf8
    buf.put_u16_le(0x0002);                      // server status (AUTOCOMMIT)
    buf.put_u16_le(SERVER_CAP_UPPER);            // capability flags upper
    buf.put_u8(21);                              // scramble length
    buf.put_slice(&[0u8; 10]);                   // reserved
    buf.put_slice(&ctx.scramble()[8..20]);       // scramble part 2
    buf.put_u8(0);                               // mysql_native_password\0
    buf.freeze()
}
```

### 4.2 Client auth response(解析)
对应 C 侧 `auth_read`(`tr_front_auth.c:244`)。

```rust
struct ClientAuth {
    capabilities: u32,
    max_packet: u32,
    charset: u8,
    user: String,
    scramble: [u8; 20],
    db: Option<String>,
}

fn parse_client_auth(pkt: &[u8]) -> Result<ClientAuth> {
    let mut r = pkt;
    let cap_lo = r.get_u16_le() as u32;
    let cap_hi = r.get_u16_le() as u32;
    let caps = cap_lo | (cap_hi << 16);
    let max_packet = r.get_u32_le();
    let charset = r.get_u8();
    r.advance(23);                               // reserved
    let user = read_null_string(&mut r)?;
    let scramble = if caps & CLIENT_SECURE_CONNECTION != 0 {
        let n = r.get_u8() as usize;             // scramble length
        let mut s = [0u8; 20];
        s[..n.min(20)].copy_from_slice(&r[..n.min(20)]);
        r.advance(n);
        s
    } else { /* 旧协议:直接 20 字节 */ unimplemented!() };
    let db = if caps & CLIENT_CONNECT_WITH_DB != 0 { Some(read_null_string(&mut r)?) } else { None };
    Ok(ClientAuth { capabilities: caps, max_packet, charset, user, scramble, db })
}
```

### 4.3 后端认证(proxy → MySQL)
对应 C 侧 `tr_back_auth.c`。proxy 用 dbuser 的密码对后端做 scramble:

```rust
/// mysql_native_password: SHA1(password) XOR SHA1(scramble + SHA1(SHA1(password)))
/// 对应 C 侧 tr_back_auth.c:334 scramble() + tr_password.c/tr_md5.c
fn scramble_password(password: &str, scramble: &[u8; 20]) -> [u8; 20] {
    let sha1_pw = sha1(password.as_bytes());
    let sha1_sha1_pw = sha1(&sha1_pw);
    let mut h = Sha1::new();
    h.update(scramble);
    h.update(&sha1_sha1_pw);
    let stage2 = h.finalize();
    let mut out = [0u8; 20];
    for i in 0..20 { out[i] = sha1_pw[i] ^ stage2[i]; }
    out
}
```

用 `sha1` crate(C 侧是手写 `tr_md5.c`)。后端 auth response 包按 `build_handshake` 的逆过程构造。

### 4.4 caching_sha2_password(MySQL 8.0+ 默认,必须补齐)

官方规范:`caching_sha2_password` 用 SHA256 + 20 字节 nonce,有**三条 fast-auth 路径**(client 能力位决定走哪条)。C 版未实现,故连 MySQL 8 默认账号失败——Rust 版必须实现。

**scramble 算法**(对应官方 `caching_sha2_password` 定义):
```rust
/// caching_sha2_password 的客户端响应(32 字节)
/// XOR(SHA256(password), SHA256(SHA256(SHA256(password)) | nonce))
/// 其中 nonce = server handshake 的 20 字节 scramble
fn sha2_scramble(password: &str, nonce: &[u8; 20]) -> [u8; 32] {
    let sha256_pw = sha256(password.as_bytes());                       // stage1
    let mut h = Sha256::new();
    h.update(&sha256_pw);
    let stage2 = h.finalize_reset();                                   // stage2 = SHA256(stage1)
    h.update(&stage2);
    h.update(nonce);
    let stage3 = h.finalize();                                         // SHA256(stage2 | nonce)
    let mut out = [0u8; 32];
    for i in 0..32 { out[i] = sha256_pw[i] ^ stage3[i]; }
    out
}
```

**fast-auth 三条路径**(server 在 auth 过程中通过 `AuthMoreData` 包首字节指示):
```rust
enum Sha2AuthPath {
    Fast,        // 0x03: server 缓存了该用户的 stage2,scramble 校验通过 → 直接发 OK
    PerformFull, // 0x04: 需完整认证(server 无缓存,如首次登录/密码变更)
    // PerformFull 下再分:
    //   - client 有 CLIENT_SSL/TLS → 明文密码走加密通道
    //   - client 无 SSL 但 server 有 RSA 公钥 → RSA 加密明文密码
    //   - 否则 → 拉取 server RSA 公钥(AuthMoreData 0x01)再加密
}
```

| 路径 | 触发 | Rust 实现 |
|---|---|---|
| Fast | server 缓存命中 | scramble 校验后直接 OK,无需额外交互 |
| TLS 明文 | client 宣告 `CLIENT_SSL` | §4.5 TLS 握手后,发明文密码(无 RSA) |
| RSA 加密 | 无 TLS,server 有公钥 | 用 server 公钥 RSA-OAEP 加密明文密码 |

**Rust crate**:`sha2` crate(SHA256)、`rsa` crate(若需 RSA 路径,首版可只实现 Fast + TLS 明文,RSA 作为增量)。

### 4.5 AuthSwitchRequest(插件协商,必须补齐)

官方规范:当 server 要求的 auth 插件与 client 初始宣告的不一致,server 发 `Protocol::AuthSwitchRequest`(0x01 前缀的 `AuthMoreData`),client 切换插件重新 scramble。

```
client → HandshakeResponse(宣告插件 A)
server → AuthSwitchRequest(要求插件 B + B 的 nonce)
client → 按 B 的算法 scramble(nonce),发 response
server → OK / 继续协商
```

```rust
impl FrontConn {
    /// 处理 server 的 AuthSwitchRequest(对应官方 AuthSwitchRequest 协议)
    async fn handle_auth_switch(&mut self, pkt: &[u8]) -> Result<()> {
        let plugin_name = read_null_string(&mut &pkt[1..])?;   // 如 b"caching_sha2_password"
        let auth_data = read_eop_string(&mut r)?;               // 新 nonce
        let resp = match plugin_name {
            b"mysql_native_password"  => scramble_password(&pw, &nonce20(&auth_data)?),
            b"caching_sha2_password"  => sha2_scramble(&pw, &nonce20(&auth_data)?).to_vec(),
            other => return Err(Err::UnknownPlugin(other)),
        };
        self.send_packet(resp).await
    }
}
```

**意义**:这是连接 MySQL 8 的关键——server 默认账号是 `caching_sha2_password`,若 client 先用 `mysql_native_password` 握手,server 会发 AuthSwitchRequest 切换。C 版不处理该包,直接认证失败。

### 4.6 SSL/TLS(增量,首版可后置)

官方规范 `Protocol::SSLRequest`:client 在收到 server Handshake 后、发 HandshakeResponse 前,可先发 `SSLRequest`(字段同 HandshakeResponse 但无 user/pw)并完成 TLS 握手。

```rust
// 首版策略:proxy 对 client 不开 TLS(内网部署),proxy 对后端 MySQL 视配置决定是否 TLS
// 结构预留:FrontConn/BackConn 的 stream 类型用泛型或 trait object,后续可换 TlsStream<TcpStream>
// 首版 stream = TcpStream;开 TLS 后 stream = TlsStream<TcpStream>,协议层不变
```

> **首版范围决策**:caching_sha2_password + AuthSwitchRequest 必须实现(否则连不上 MySQL 8);SSL/TLS 可后置——因首版若 proxy↔client 走明文、proxy↔backend 走明文,caching_sha2 的 fast 路径仍可用(server 缓存命中)。但若后端账号要求强制加密(`caching_sha2_password` + 无缓存首登),则需 RSA 或 TLS——届时再补。文档与任务卡已标注此为增量项。

## 5. 命令分发

对应 C 侧 `tr_handle_front_command`(`tr_front_cmd.c:42`)的 dispatch。COM_* 用 `enum` 而非 C 的 char 宏:

```rust
#[repr(u8)]
enum Command {
    Sleep = 0x00, Quit = 0x01, InitDb = 0x02, Query = 0x03, FieldList = 0x04,
    // ...
    Ping = 0x0e,
    StmtPrepare = 0x16, StmtExecute = 0x17, StmtSendLongData = 0x18,
    StmtClose = 0x19, StmtReset = 0x1a, SetOption = 0x1b, StmtFetch = 0x1c,
}

impl FrontConn {
    async fn dispatch(&mut self, cmd: Command, payload: &[u8]) -> Result<()> {
        match cmd {
            Command::Query => self.handle_query(payload).await,
            Command::StmtPrepare => self.handle_prepare(payload).await,
            Command::StmtExecute => self.handle_execute(payload).await,
            Command::StmtClose  => self.handle_stmt_close(payload),
            Command::Ping => { self.send_ok().await }
            Command::Quit => return Err(Quit.into()),
            Command::InitDb => self.handle_use_db(payload).await,
            _ => self.forward_raw(cmd, payload).await,   // 不识别的透传
        }
        Ok(())
    }
}
```

**非法命令**:C 侧 `is_valid_command` 返回 false 时拒绝。Rust 里 `Command::from_u8` 返回 `Option`,None 即拒绝——编译期保证不会漏处理新增命令(warns on non-exhaustive match)。

## 6. 结果集构造与流式

官方规范:**响应结束包取决于 client 能力位**。client 宣告 `CLIENT_DEPRECATE_EOF` 时,结果集用 `OK` 包(0x00 前缀)结束列定义与行流;否则用 `EOF` 包(0xFE 前缀)。C 版固定用 EOF 风格——Rust 版须按对端能力位选择。

C 侧参照(`tr_packet.c` 的 packet 构造,行号已勘误):`make_ok_packet`(571)、`fill_error_packet`(541)、`make_eof_packet`(814)、`make_result_set_header_packet`(786)、`make_full_field_packet`(638)。

### 6.1 OK / Error / EOF 包
```rust
/// 结束包类型由对端能力位决定(官方 CLIENT_DEPRECATE_EOF 规范)
enum EndPacket { Eof { warnings: u16, status: u16 }, Ok { affected: u64, last_id: u64, warnings: u16, status: u16 } }

fn build_end(peer_caps: u32, e: EndPacket, seq: u8) -> Bytes {
    match e {
        EndPacket::Eof { .. } if peer_caps & CLIENT_DEPRECATE_EOF == 0 => build_eof(/*...*/),
        // deprecate_eof 模式下,EOF 语义用 OK 包承载
        EndPacket::Eof { warnings, status } | EndPacket::Ok { warnings, status, .. }
            if peer_caps & CLIENT_DEPRECATE_EOF != 0 => build_ok(/*affected=0,last_id=0,*/warnings, status, seq),
        EndPacket::Ok { .. } => build_ok(/*...*/),
    }
}
fn build_ok(affected: u64, last_id: u64, warnings: u16, status: u16, seq: u8) -> Bytes { /* 0x00 + lenenc + ... */ }
fn build_error(code: u16, sql_state: &[u8; 5], msg: &str, seq: u8) -> Bytes { /* 0xFF + ... */ }
fn build_eof(warnings: u16, status: u16, seq: u8) -> Bytes { /* 0xFE + warnings + status */ }
```

### 6.2 流式结果集
C 侧 `stream_transport_enable=1`(配置项)支持流式传输——大结果集边收边发,不缓冲全量。Rust 用 async 天然支持。**结束判定按对端能力位**:`CLIENT_DEPRECATE_EOF` 下行流的结束包是 OK(0x00 前缀,且 payload<0xFFFFFF),不再是 0xFE:

```rust
/// 从 backend 流式读取结果,边读边转发给 client(不缓冲全量)
async fn proxy_result_set(&mut self, backend: &Arc<BackConn>) -> Result<()> {
    let header = backend.read_packet().await?;
    let n_cols = (&header[..]).get_uint_le(...) as usize;   // lenenc 列数
    self.send_packet(header).await?;
    for _ in 0..n_cols {
        self.send_packet(backend.read_packet().await?).await?;   // 列定义透传
    }
    // 列定义结束包(EOF 或 OK,按 backend 能力位)
    self.send_packet(backend.read_packet().await?).await?;

    // 行流式转发,直到结束包
    let peer_eof = self.peer_caps & CLIENT_DEPRECATE_EOF == 0;
    loop {
        let row = backend.read_packet().await?;
        // 旧式:0xFE 是 EOF;新式 deprecate_eof:0x00 + payload<16MB 是 OK 结束(需与普通 OK 区分)
        let is_end = match row.first() {
            Some(&0xFF) => true,                              // ERR
            Some(&0xFE) if peer_eof && row.len() < 9 => true, // 旧式 EOF
            Some(&0x00) if !peer_eof => true,                 // 新式 OK 结束(简化:实际需结合上下文)
            _ => false,
        };
        self.send_packet(row).await?;
        if is_end { break; }
    }
    Ok(())
}
```

**合并查询的流式**:scatter/gather 多分片时需先收齐再合并(原 `tr_result.c` 的 min-heap merge)。这种场景不能流式,需缓冲——但这是业务语义决定的(合并需要全量),与协议无关。

## 7. 预编译语句(Prepare/Execute)

对应 C 侧 `tr_stmt_prepare.c`(1302 行)。newproxy 的 prepare 有特殊语义:**缓存解析后的 AST** 以便 re-exec 跳过解析。

```rust
struct PreparedStmt {
    stmt_id: u32,
    sql: String,
    ast: AstHandle,              // FFI 持有的解析 AST(见 05-parser-pool.md)
    num_params: u16,
    num_columns: u16,
    param_types: Vec<ParamType>,
}

impl FrontConn {
    async fn handle_prepare(&mut self, sql: &[u8]) -> Result<()> {
        let sql_str = std::str::from_utf8(sql)?;
        // 用 parser pool 解析(见 05-parser-pool.md),缓存 AST
        let ast = self.ctx.parser_pool.parse(sql_str).await?;
        let (np, nc, ptypes) = analyze_prepare(&ast);
        let stmt_id = self.next_stmt_id();
        self.prepared.insert(stmt_id, PreparedStmt { stmt_id, sql: sql_str.into(), ast, num_params: np, num_columns: nc, param_types: ptypes });
        // 发 COM_STMT_PREPARE_OK 响应
        self.send_prepare_ok(stmt_id, np, nc, 0, 0).await
    }

    async fn handle_execute(&mut self, payload: &[u8]) -> Result<()> {
        let stmt_id = (&payload[0..4]).get_u32_le();
        let stmt = self.prepared.get(&stmt_id).ok_or(Err::UnknownStmt)?;
        // 解析 binary protocol 参数,绑定到缓存的 AST,路由执行
        let params = parse_execute_params(&payload[10..], &stmt.param_types)?;
        // ... 路由 + 执行 ...
    }
}
```

**binary protocol 参数解析**对应 C 侧 `fill_prepare_param`/`get_prepare_params`(`tr_packet.c:2799,2963`)——null bitmap + type + value 逐字段解。`MYSQL_TYPE_NULL` 的 1 字节对齐正是 ASAN 测试抓到的那个堆溢出(`test/asan/`,仅 Makefile,C 源已缺失),Rust 里用 `Buf` trait 的边界检查从根上杜绝。

## 8. 与 C 侧的对照(规范驱动)

| 官方规范要素 | C 版 | Rust 版 |
|---|---|---|
| packet 帧(4B header) | ✅ | ✅ + 补 0xFFFFFF 分包重组 |
| lenenc int/string | ✅(手写) | ✅(`Buf`/`BufMut`) |
| `mysql_native_password` | ✅(SHA1,`tr_password.c`) | ✅(`sha1` crate) |
| `caching_sha2_password` | ❌ | ✅(§4.4,SHA256 + fast-auth 三路径) |
| `AuthSwitchRequest` | ❌ | ✅(§4.5) |
| `SSLRequest`/TLS | ❌(`CLIENT_SSL` 未用) | 增量(§4.6,结构预留) |
| `CLIENT_DEPRECATE_EOF` | ❌(固定 EOF) | ✅(§6,按对端能力位) |
| COM_* 命令集 | ✅ | ✅(`enum Command`) |
| prepared binary protocol | ✅ | ✅ + 边界检查(消除 NULL 对齐溢出) |
| god-struct + 裸 read 偏移状态机 | — | task 局部 `FrontConn` + `read_packet` |

## 9. 验证

- **官方协议文档为标准**:实现时对照 https://dev.mysql.com/doc/dev/mysql-server/latest/PAGE_PROTOCOL.html 各章节字段定义,而非照抄 C 代码。
- **662 个 MySQL 兼容用例**(`mysql_case/totalTest.sh`,用 `mysqltest` 二进制驱动):覆盖握手、各命令、结果集、预编译、事务——协议层主要黑盒验收。
- **caching_sha2 专项**:用 MySQL 8 默认账号(非改回 `mysql_native_password`)连接 Rust 版代理,验证 fast-auth 三路径(缓存命中/首登 RSA 或 TLS)。
- **AuthSwitch 专项**:构造 server 要求切换插件的场景,验证协商成功。
- **行为等价对照**:Rust 版与 C 版对同组用例输出做字节级 diff(packet_id、payload);但 `DEPRECATE_EOF` 等新能力 C 版无对照,以官方规范为标准。
- **流式压测**:大结果集(>16MB 分包、>100w 行)验证分包重组正确、流式不缓冲全量、内存恒定。
- **ASAN 用例重建**:`test/asan/` 的 `MYSQL_TYPE_NULL` 用例 C 源已缺失,在 Rust 侧自写等价单测验证 binary protocol 边界。

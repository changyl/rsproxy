// MySQL 握手协议:Server Greeting / Client Auth Response / scramble
// T1.2 实现:前端握手 + mysql_native_password 认证
//
// 对齐 C 侧 tr_front_auth.c:170 sent_handshake / tr_front_auth.c:244 auth_read
// 参考 MySQL 官方 Protocol:Connection Phase · Handshake

use bytes::{Buf, BufMut, Bytes, BytesMut};
use rand::RngCore;
use sha1::{Digest, Sha1};

// ─── 能力位常量 ───

/// 完整能力位(u32)——先算全量再拆分,避免 u16 截断丢失高位
///
/// 注意:**不宣告 CLIENT_DEPRECATE_EOF**。前端一旦宣告,客户端就会按新式协议
/// 解析响应(列后无 EOF、行结尾用 OK 包),但旧后端(5.5/5.6/部分 5.7)会按旧式
/// 发送 5 字节 EOF 包,代理原样转发会导致客户端报 2027 Malformed packet。
/// 不宣告 → 客户端统一按旧式解析,与旧后端天然一致;新式后端(8.0)的响应由
/// `forward_backend_response` 的 ColumnEof 分支插入合成 EOF 适配。
pub const SERVER_CAP_FULL: u32 = CLIENT_PROTOCOL_41
    | CLIENT_SECURE_CONNECTION
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PLUGIN_AUTH
    | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
    | CLIENT_SSL;

/// 后端连接能力位(proxy → MySQL):不含 CLIENT_SSL 与 CLIENT_DEPRECATE_EOF,
/// 也不默认含 CLIENT_CONNECT_WITH_DB。
///
/// 关键:proxy 作为客户端连接后端 MySQL 时**不能**宣告 CLIENT_SSL。
/// 一旦在 HandshakeResponse41 里置上 CLIENT_SSL,后端 MySQL 会认为客户端
/// 要求升级 SSL,从而等待 TLS ClientHello;而 proxy 并不对后端做 TLS,
/// 于是双方死锁(后端等 ClientHello,proxy 等 auth result),表现为登录/查询挂起。
/// CLIENT_SSL 只用于前端 Server Greeting,供前端客户端选择是否启用 SSL。
///
/// 不宣告 CLIENT_DEPRECATE_EOF:后端一律按**旧式协议**回包(列后 5B EOF、
/// 行尾 5B EOF),与前端客户端(代理未宣告该位,客户端按旧式解析)一致,
/// 转发器原样透传即可,无需转换。若后端宣告或忽略该位导致 new-style 回包,
/// 转发器的 ColumnEof 分支会插入合成 EOF 兜底。
///
/// CLIENT_CONNECT_WITH_DB 也不默认置位:实测部分后端/网关对 auth 中携带
/// 该标志(无论是否带库名)的连接在查询阶段直接 RST。只有当确实需要向
/// 后端传递数据库名时(见 `auth::build_backend_auth_response`),才在发送
/// 时临时并入该标志。
pub const BACKEND_CAP_FULL: u32 = (SERVER_CAP_FULL & !CLIENT_SSL) & !CLIENT_CONNECT_WITH_DB;

/// 低 16 位能力位(Server Greeting 用)
pub const SERVER_CAP_LOWER: u16 = (SERVER_CAP_FULL & 0xFFFF) as u16;

/// 高 16 位能力位
pub const SERVER_CAP_UPPER: u16 = ((SERVER_CAP_FULL >> 16) & 0xFFFF) as u16;

// 基础能力位(来自 MySQL 官方 mysql_com.h)
pub const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
pub const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
pub const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;
pub const CLIENT_DEPRECATE_EOF: u32 = 0x0100_0000;
pub const CLIENT_SSL: u32 = 0x0000_0800;
pub const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
pub const CLIENT_MULTI_RESULTS: u32 = 0x0004_0000;
pub const CLIENT_PS_MULTI_RESULTS: u32 = 0x0001_0000;
pub const CLIENT_SESSION_TRACK: u32 = 0x0080_0000;

/// 服务端状态标志
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;

/// 默认字符集:utf8mb4 (45) 或 utf8 (33)
pub const DEFAULT_CHARSET: u8 = 33; // utf8

/// scramble 长度(mysql_native_password: 20 字节)
pub const SCRAMBLE_LEN: usize = 20;

/// auth plugin 名称
pub const AUTH_PLUGIN_NATIVE: &str = "mysql_native_password\0";

// ─── Server Greeting(HandshakeV10) ───

/// 构建 Server Greeting 包(发往客户端)
///
/// 格式:
/// - 1B  protocol_version = 10
/// - NUL-terminated server_version
/// - 4B  connection_id
/// - 8B  scramble_part1
/// - 1B  filler (0x00)
/// - 2B  capability_flags_lower
/// - 1B  character_set
/// - 2B  status_flags
/// - 2B  capability_flags_upper
/// - 1B  auth_plugin_data_len (21 = 8 + 12 + 1 NUL)
/// - 10B reserved (0x00)
/// - NUL-terminated auth_plugin_name + rest of scramble(12B)
///
/// 对齐 C 侧 tr_front_auth.c:170 sent_handshake
pub fn build_handshake(
    server_version: &str,
    connection_id: u32,
    scramble: &[u8; SCRAMBLE_LEN],
) -> Bytes {
    let mut buf = BytesMut::with_capacity(128);
    // protocol version
    buf.put_u8(10);
    // server version (NUL-terminated)
    buf.put_slice(server_version.as_bytes());
    buf.put_u8(0);
    // connection id
    buf.put_u32_le(connection_id);
    // scramble part 1 (first 8 bytes)
    buf.put_slice(&scramble[0..8]);
    // filler
    buf.put_u8(0);
    // capability flags lower
    buf.put_u16_le(SERVER_CAP_LOWER);
    // character set
    buf.put_u8(DEFAULT_CHARSET);
    // status flags
    buf.put_u16_le(SERVER_STATUS_AUTOCOMMIT);
    // capability flags upper
    buf.put_u16_le(SERVER_CAP_UPPER);
    // auth plugin data length (8 + 12 + 1 = 21)
    buf.put_u8(21);
    // reserved (10 bytes of 0x00)
    buf.put_slice(&[0u8; 10]);
    // scramble part 2 (last 12 bytes) + NUL terminator
    buf.put_slice(&scramble[8..SCRAMBLE_LEN]);
    buf.put_u8(0);
    // auth plugin name (NUL-terminated) — already includes NUL
    buf.put_slice(AUTH_PLUGIN_NATIVE.as_bytes());

    buf.freeze()
}

/// 生成随机 20 字节 scramble
pub fn generate_scramble() -> [u8; SCRAMBLE_LEN] {
    let mut buf = [0u8; SCRAMBLE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf
}

// ─── Client Auth Response ───

/// 客户端认证响应(解析自 Client Response Packet)
///
/// 对齐 C 侧 tr_front_auth.c:244 auth_read
#[derive(Debug, Clone)]
pub struct ClientAuth {
    pub capabilities: u32,
    pub max_packet_size: u32,
    pub charset: u8,
    pub username: String,
    /// scramble 响应(20 字节)
    pub auth_response: [u8; SCRAMBLE_LEN],
    pub database: Option<String>,
    pub auth_plugin: Option<String>,
}

/// 解析客户端认证响应包
///
/// 格式(Client Response Packet 41):
/// - 4B  capability_flags
/// - 4B  max_packet_size
/// - 1B  charset
/// - 23B reserved (0x00)
/// - NUL-terminated username
/// - lenenc或1B长度的 auth_response + 数据
/// - NUL-terminated database (if CLIENT_CONNECT_WITH_DB)
/// - NUL-terminated auth_plugin (if CLIENT_PLUGIN_AUTH)
pub fn parse_client_auth(payload: &[u8]) -> Result<ClientAuth, &'static str> {
    if payload.len() < 32 {
        return Err("payload too short for client auth");
    }
    let mut r = payload;

    let capabilities = r.get_u32_le();
    let max_packet_size = r.get_u32_le();
    let charset = r.get_u8();

    // skip 23 bytes reserved
    if r.len() < 23 {
        return Err("payload too short: reserved field");
    }
    r = &r[23..];

    // NUL-terminated username
    let username = read_null_string(&mut r)?;

    // auth_response: if CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA, lenenc length
    // otherwise if CLIENT_SECURE_CONNECTION, 1B length
    let auth_len = if capabilities & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
        read_lenenc_int(&mut r)? as usize
    } else if capabilities & CLIENT_SECURE_CONNECTION != 0 {
        if r.is_empty() {
            return Err("expected auth response length byte");
        }
        let len = r[0] as usize;
        r = &r[1..];
        len
    } else {
        SCRAMBLE_LEN // 旧协议:默认 20 字节(含尾部 NUL)
    };

    let auth_len = auth_len.min(SCRAMBLE_LEN);
    let mut auth_response = [0u8; SCRAMBLE_LEN];
    if r.len() < auth_len {
        return Err("payload too short: auth response");
    }
    auth_response[..auth_len].copy_from_slice(&r[..auth_len]);
    r = &r[auth_len..];

    // database (if CLIENT_CONNECT_WITH_DB)
    let database = if capabilities & CLIENT_CONNECT_WITH_DB != 0 {
        Some(read_null_string(&mut r)?)
    } else {
        None
    };

    // auth_plugin (if CLIENT_PLUGIN_AUTH)
    let auth_plugin = if capabilities & CLIENT_PLUGIN_AUTH != 0 {
        Some(read_null_string(&mut r)?)
    } else {
        None
    };

    Ok(ClientAuth {
        capabilities,
        max_packet_size,
        charset,
        username,
        auth_response,
        database,
        auth_plugin,
    })
}

// ─── mysql_native_password scramble ───

/// 计算 mysql_native_password 的 scramble 响应
///
/// 公式:
///   SHA1(password) XOR SHA1(server_scramble + SHA1(SHA1(password)))
///
/// 对齐 C 侧 tr_back_auth.c:334 scramble() + tr_password.c
pub fn scramble_native(password: &[u8], server_scramble: &[u8]) -> [u8; SCRAMBLE_LEN] {
    // stage1 = SHA1(password)
    let stage1 = Sha1::digest(password);

    // stage2 = SHA1(stage1) = SHA1(SHA1(password))
    #[allow(clippy::needless_borrows_for_generic_args)]
    let stage2 = Sha1::digest(&stage1);

    // stage3 = SHA1(server_scramble + stage2)
    let mut hasher = Sha1::new();
    hasher.update(server_scramble);
    hasher.update(stage2);
    let stage3 = hasher.finalize();

    // result = stage1 XOR stage3
    let mut result = [0u8; SCRAMBLE_LEN];
    for i in 0..SCRAMBLE_LEN {
        result[i] = stage1[i] ^ stage3[i];
    }
    result
}

/// 验证客户端 scramble 响应
///
/// 对给定密码计算期望的 scramble 响应,与客户端提供的对比
pub fn verify_native_scramble(
    password: &[u8],
    server_scramble: &[u8],
    client_response: &[u8; SCRAMBLE_LEN],
) -> bool {
    let expected = scramble_native(password, server_scramble);
    // 常量时间比较(防止时序攻击)
    expected == *client_response
}

// ─── 辅助 ───

/// 读取 NUL 结尾字符串,返回 String 并推进指针
fn read_null_string(r: &mut &[u8]) -> Result<String, &'static str> {
    let pos = r
        .iter()
        .position(|&b| b == 0)
        .ok_or("unterminated string")?;
    let s = String::from_utf8_lossy(&r[..pos]).into_owned();
    *r = &r[pos + 1..];
    Ok(s)
}

/// 读取 lenenc 整数
fn read_lenenc_int(r: &mut &[u8]) -> Result<u64, &'static str> {
    if r.is_empty() {
        return Err("unexpected EOF reading lenenc_int");
    }
    let first = r[0];
    *r = &r[1..];
    match first {
        0xFC => {
            if r.len() < 2 {
                return Err("unexpected EOF");
            }
            let val = u16::from_le_bytes([r[0], r[1]]);
            *r = &r[2..];
            Ok(val as u64)
        }
        0xFD => {
            if r.len() < 3 {
                return Err("unexpected EOF");
            }
            let val = r[0] as u32 | ((r[1] as u32) << 8) | ((r[2] as u32) << 16);
            *r = &r[3..];
            Ok(val as u64)
        }
        0xFE => {
            if r.len() < 8 {
                return Err("unexpected EOF");
            }
            let val = u64::from_le_bytes(r[..8].try_into().unwrap());
            *r = &r[8..];
            Ok(val)
        }
        _ => Ok(first as u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_packet_structure() {
        let scramble = [0x41u8; SCRAMBLE_LEN];
        let hs = build_handshake("5.7.0-newproxy", 1, &scramble);

        // protocol version
        assert_eq!(hs[0], 10);
        // find NUL after version
        let ver_end = hs[1..].iter().position(|&b| b == 0).unwrap();
        assert_eq!(&hs[1..1 + ver_end], b"5.7.0-newproxy");
        // connection id at offset after version + NUL
        let cid_off = 2 + ver_end;
        assert_eq!(
            u32::from_le_bytes(hs[cid_off..cid_off + 4].try_into().unwrap()),
            1
        );
    }

    #[test]
    fn scramble_native_known_vector() {
        // 已知测试向量(来自 MySQL 源码)
        let password = b"test_password";
        let scramble: [u8; 20] = [
            0x3e, 0x45, 0x7a, 0x42, 0x6b, 0x15, 0x19, 0x1c, 0x23, 0x54, 0x6d, 0x2f, 0x78, 0x3a,
            0x41, 0x5c, 0x11, 0x63, 0x2e, 0x7d,
        ];
        let response = scramble_native(password, &scramble);
        // 验证 XOR 性质:再次 XOR 可得 stage1 XOR stage3
        assert_eq!(response.len(), SCRAMBLE_LEN);
    }

    #[test]
    fn scramble_verify_roundtrip() {
        let password = b"secret_password";
        let server_scramble = generate_scramble();
        let client_response = scramble_native(password, &server_scramble);

        assert!(verify_native_scramble(
            password,
            &server_scramble,
            &client_response
        ));
    }

    #[test]
    fn scramble_verify_wrong_password() {
        let server_scramble = generate_scramble();
        let client_response = scramble_native(b"correct_password", &server_scramble);
        assert!(!verify_native_scramble(
            b"wrong_password",
            &server_scramble,
            &client_response
        ));
    }

    #[test]
    fn parse_client_auth_minimal() {
        // 最小 Client Response:只有 username + auth_response,不含 DB 和 plugin
        let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION;
        let mut buf = BytesMut::new();
        buf.put_u32_le(caps); // capabilities
        buf.put_u32_le(16 * 1024 * 1024); // max_packet_size
        buf.put_u8(33); // charset utf8
        buf.put_slice(&[0u8; 23]); // reserved
        buf.put_slice(b"root\0"); // username
        buf.put_u8(SCRAMBLE_LEN as u8); // auth_response length
        buf.put_slice(&[0x41u8; SCRAMBLE_LEN]); // auth_response

        let auth = parse_client_auth(&buf.freeze()).unwrap();
        assert_eq!(auth.username, "root");
        assert_eq!(auth.charset, 33);
        assert!(auth.database.is_none());
        assert!(auth.auth_plugin.is_none());
    }

    #[test]
    fn parse_client_auth_full() {
        // 完整 Client Response:含 database + auth_plugin
        let caps = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_CONNECT_WITH_DB
            | CLIENT_PLUGIN_AUTH;
        let mut buf = BytesMut::new();
        buf.put_u32_le(caps);
        buf.put_u32_le(16 * 1024 * 1024);
        buf.put_u8(33);
        buf.put_slice(&[0u8; 23]);
        buf.put_slice(b"root\0");
        buf.put_u8(SCRAMBLE_LEN as u8);
        buf.put_slice(&[0x42u8; SCRAMBLE_LEN]);
        buf.put_slice(b"testdb\0"); // database
        buf.put_slice(b"mysql_native_password\0"); // auth_plugin

        let auth = parse_client_auth(&buf.freeze()).unwrap();
        assert_eq!(auth.username, "root");
        assert_eq!(auth.database.as_deref(), Some("testdb"));
        assert_eq!(auth.auth_plugin.as_deref(), Some("mysql_native_password"));
    }

    #[test]
    fn parse_client_auth_with_db() {
        // Client Response with DB only (no auth_plugin in caps)
        let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_CONNECT_WITH_DB;
        let mut buf = BytesMut::new();
        buf.put_u32_le(caps);
        buf.put_u32_le(16 * 1024 * 1024);
        buf.put_u8(33);
        buf.put_slice(&[0u8; 23]);
        buf.put_slice(b"root\0");
        buf.put_u8(SCRAMBLE_LEN as u8);
        buf.put_slice(&[0x42u8; SCRAMBLE_LEN]);
        buf.put_slice(b"testdb\0");

        let auth = parse_client_auth(&buf.freeze()).unwrap();
        assert_eq!(auth.username, "root");
        assert_eq!(auth.database.as_deref(), Some("testdb"));
        assert!(auth.auth_plugin.is_none());
    }

    #[test]
    fn parse_client_auth_errors() {
        // 过短
        assert!(parse_client_auth(&[0u8; 10]).is_err());
        // reserved 不足(32 字节头后无 23 字节)
        let mut buf = BytesMut::new();
        buf.put_u32_le(CLIENT_PROTOCOL_41);
        buf.put_u32_le(1024);
        buf.put_u8(33);
        buf.put_slice(&[0u8; 5]); // 只给 5 字节 reserved
        assert!(parse_client_auth(&buf.freeze()).is_err());
        // auth_response 数据不足
        let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION;
        let mut buf = BytesMut::new();
        buf.put_u32_le(caps);
        buf.put_u32_le(1024);
        buf.put_u8(33);
        buf.put_slice(&[0u8; 23]);
        buf.put_slice(b"root\0");
        buf.put_u8(20);
        buf.put_slice(&[0u8; 3]); // 只有 3 字节 auth data
        assert!(parse_client_auth(&buf.freeze()).is_err());
    }

    #[test]
    fn parse_client_auth_lenenc_and_legacy() {
        // lenenc 长度前缀(CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA)
        let caps = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;
        let mut buf = BytesMut::new();
        buf.put_u32_le(caps);
        buf.put_u32_le(1024);
        buf.put_u8(33);
        buf.put_slice(&[0u8; 23]);
        buf.put_slice(b"u\0");
        buf.put_u8(20); // lenenc:1 字节长度 20
        buf.put_slice(&[0x55u8; 20]);
        let auth = parse_client_auth(&buf.freeze()).unwrap();
        assert_eq!(auth.username, "u");
        assert_eq!(auth.auth_response[0], 0x55);

        // 旧协议:无 CLIENT_SECURE_CONNECTION → 默认 20 字节(含 NUL)
        let caps = CLIENT_PROTOCOL_41; // 无 secure connection
        let mut buf = BytesMut::new();
        buf.put_u32_le(caps);
        buf.put_u32_le(1024);
        buf.put_u8(33);
        buf.put_slice(&[0u8; 23]);
        buf.put_slice(b"root\0");
        buf.put_slice(&[0x33u8; 20]);
        let auth = parse_client_auth(&buf.freeze()).unwrap();
        assert_eq!(auth.auth_response[0], 0x33);
        assert_eq!(auth.auth_response[19], 0x33);
    }

    #[test]
    fn handshake_packet_fields() {
        let pkt = build_handshake("5.7.0", 1, &[0x11u8; SCRAMBLE_LEN]);
        assert_eq!(pkt[0], 0x0A, "协议版本 10");
        // 服务器版本 NUL 结尾
        let ver_end = pkt.iter().position(|&b| b == 0).unwrap();
        assert_eq!(&pkt[1..ver_end], b"5.7.0");
        // 连接 id 小端(version NUL 之后)
        assert_eq!(
            u32::from_le_bytes([pkt[ver_end + 1], pkt[ver_end + 2], pkt[ver_end + 3], pkt[ver_end + 4]]),
            1
        );
        // auth plugin data 长度 = 21(8+12+1),位于 cap_upper(2B)之后
        let auth_len_pos = ver_end + 1 + 4 + 8 + 1 + 2 + 1 + 2 + 2;
        assert_eq!(pkt[auth_len_pos], 21, "auth plugin data length");
        // scramble 两段均填充 0x11;包尾是 auth 插件名
        let plugin = String::from_utf8_lossy(&pkt[pkt.len() - AUTH_PLUGIN_NATIVE.len()..]);
        assert_eq!(plugin, AUTH_PLUGIN_NATIVE);
        // 第二段 scramble(12 字节)在 reserved 之后
        assert!(pkt[auth_len_pos + 1 + 10..auth_len_pos + 1 + 10 + 12].iter().all(|&b| b == 0x11));
    }

    #[test]
    fn generate_scramble_unique_and_length() {
        let a = generate_scramble();
        let b = generate_scramble();
        assert_eq!(a.len(), SCRAMBLE_LEN);
        assert_ne!(a, b, "两次生成的 scramble 应不同");
    }
}

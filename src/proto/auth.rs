// MySQL 后端认证:proxy → MySQL 的握手与 auth
// T1.3 + T1.7 实现:mysql_native_password + caching_sha2_password + AuthSwitch
//
// 对齐 C 侧 tr_back_auth.c:334 scramble() + tr_back_auth.c 握手状态机
// 参考 MySQL 官方 Protocol:Connection Phase · Auth Methods

use bytes::Buf;
use sha2::{Digest, Sha256};

use crate::proto::handshake::{self, SCRAMBLE_LEN};

/// 解析后端 Server Greeting,提取关键字段
#[derive(Debug, Clone)]
pub struct BackendGreeting {
    pub protocol_version: u8,
    pub server_version: String,
    pub connection_id: u32,
    /// 完整 20 字节 scramble(part1 8B + part2 12B)
    pub scramble: [u8; SCRAMBLE_LEN],
    pub capabilities: u32,
    pub charset: u8,
    pub status_flags: u16,
    pub auth_plugin_name: String,
}

/// 解析后端 MySQL 发来的 Server Greeting 包
pub fn parse_backend_greeting(payload: &[u8]) -> Result<BackendGreeting, &'static str> {
    if payload.is_empty() {
        return Err("empty greeting payload");
    }

    let mut r = payload;
    let protocol_version = r.get_u8();

    if protocol_version == 0xFF {
        return Err("backend sent error packet during handshake");
    }
    if protocol_version != 10 {
        return Err("unsupported protocol version");
    }

    // NUL-terminated server version
    let server_version = read_null_string(&mut r)?;

    // connection_id (4B)
    if r.len() < 4 {
        return Err("greeting too short: connection_id");
    }
    let connection_id = r.get_u32_le();

    // scramble part 1 (8 bytes)
    if r.len() < 8 {
        return Err("greeting too short: scramble part 1");
    }
    let mut scramble = [0u8; SCRAMBLE_LEN];
    scramble[..8].copy_from_slice(&r[..8]);
    r = &r[8..];

    // filler (1 byte)
    if r.is_empty() {
        return Err("greeting too short: filler");
    }
    r = &r[1..];

    // capabilities lower (2B)
    if r.len() < 2 {
        return Err("greeting too short: cap_lower");
    }
    let cap_lower = r.get_u16_le() as u32;

    // charset (1B)
    if r.is_empty() {
        return Err("greeting too short: charset");
    }
    let charset = r.get_u8();

    // status flags (2B)
    if r.len() < 2 {
        return Err("greeting too short: status_flags");
    }
    let status_flags = r.get_u16_le();

    // capabilities upper (2B)
    if r.len() < 2 {
        return Err("greeting too short: cap_upper");
    }
    let cap_upper = r.get_u16_le() as u32;
    let capabilities = cap_lower | (cap_upper << 16);

    // auth_plugin_data_len (1B) — if capabilities & CLIENT_PLUGIN_AUTH
    let auth_data_len = if capabilities & handshake::CLIENT_PLUGIN_AUTH != 0 {
        if r.is_empty() {
            return Err("greeting too short: auth_data_len");
        }
        r.get_u8()
    } else {
        8 // pre-4.1: only 8 bytes
    };

    // reserved (10 bytes)
    if r.len() < 10 {
        return Err("greeting too short: reserved");
    }
    r = &r[10..];

    // scramble part 2: auth_data_len - 8 - 1(NUL) bytes
    let part2_len = (auth_data_len as usize).saturating_sub(8).saturating_sub(1);
    let part2_len = part2_len.min(12); // max 12 bytes
    let part2_len = part2_len.min(r.len().saturating_sub(1)); // at most r.len()-1 (leaving NUL)
    if r.len() < part2_len + 1 {
        return Err("greeting too short: scramble part 2");
    }
    scramble[8..8 + part2_len].copy_from_slice(&r[..part2_len]);
    r = &r[part2_len..];

    // 跳过 auth_plugin_data 的 NUL 终止符
    if r.first() == Some(&0) {
        r = &r[1..];
    }

    // auth_plugin_name (NUL-terminated) — if capabilities & CLIENT_PLUGIN_AUTH
    let auth_plugin_name = if capabilities & handshake::CLIENT_PLUGIN_AUTH != 0 {
        read_null_string(&mut r)?
    } else {
        String::new()
    };

    Ok(BackendGreeting {
        protocol_version,
        server_version,
        connection_id,
        scramble,
        capabilities,
        charset,
        status_flags,
        auth_plugin_name,
    })
}

/// 构建发往后端 MySQL 的 Client Auth Response 包
///
/// 格式(HandshakeResponse41):
/// - 4B  capability_flags
/// - 4B  max_packet_size
/// - 1B  charset
/// - 23B reserved
/// - NUL-terminated username
/// - 1B (or lenenc) auth_response_len + auth_response_data
/// - NUL-terminated database
/// - NUL-terminated auth_plugin_name
pub fn build_backend_auth_response(
    db_user: &str,
    db_password: &str,
    greeting: &BackendGreeting,
    charset: u8,
) -> Vec<u8> {
    // 后端连接不宣告 CLIENT_SSL,否则后端会等待 TLS ClientHello 导致死锁。
    // 也不置位 CLIENT_CONNECT_WITH_DB / 不携带库名:实测部分后端/网关对 auth
    // 中携带该标志(无论带不带库名)的连接在命令阶段直接 RST(ERROR 2013)。
    // 库选择改由认证后补发 COM_INIT_DB 完成(见 conn::front::do_backend_handshake)。
    let caps = handshake::BACKEND_CAP_FULL;
    let auth_response = handshake::scramble_native(db_password.as_bytes(), &greeting.scramble);

    let mut buf = Vec::with_capacity(256);

    // capabilities
    buf.extend_from_slice(&caps.to_le_bytes());
    // max_packet_size
    buf.extend_from_slice(&(16_777_215u32).to_le_bytes());
    // charset(来自配置 default_charset,不再写死)
    buf.push(charset);
    // reserved (23 bytes)
    buf.extend_from_slice(&[0u8; 23]);
    // username (NUL-terminated)
    buf.extend_from_slice(db_user.as_bytes());
    buf.push(0);
    // auth_response: 1B length + data
    buf.push(SCRAMBLE_LEN as u8);
    buf.extend_from_slice(&auth_response);
    // auth_plugin_name
    buf.extend_from_slice(b"mysql_native_password");
    buf.push(0);

    buf
}

// ─── caching_sha2_password (T1.7) ───

/// SHA256 密钥长度
pub const SHA256_SCRAMBLE_LEN: usize = 32;

/// 计算 caching_sha2_password 的 scramble 响应
///
/// 公式:
///   XOR(
///     SHA256(password),
///     SHA256(server_scramble + SHA256(SHA256(password)))
///   )
///
/// 这是 caching_sha2_password "fast-auth" 路径的核心
pub fn scramble_sha256(password: &[u8], server_scramble: &[u8]) -> Vec<u8> {
    // stage1 = SHA256(password)
    let stage1 = Sha256::digest(password);

    // stage2 = SHA256(stage1) = SHA256(SHA256(password))
    #[allow(clippy::needless_borrows_for_generic_args)]
    let stage2 = Sha256::digest(&stage1);

    // stage3 = SHA256(server_scramble + stage2)
    let mut hasher = Sha256::new();
    hasher.update(server_scramble);
    hasher.update(stage2);
    let stage3 = hasher.finalize();

    // result = stage1 XOR stage3
    let mut result = vec![0u8; 32];
    for i in 0..32 {
        result[i] = stage1[i] ^ stage3[i];
    }
    result
}

/// 验证 caching_sha2_password 的 scramble 响应
pub fn verify_sha256_scramble(
    password: &[u8],
    server_scramble: &[u8],
    client_response: &[u8],
) -> bool {
    let expected = scramble_sha256(password, server_scramble);
    expected == client_response
}

// ─── AuthSwitch (T1.7) ───

/// AuthSwitchRequest 包标志
pub const AUTH_SWITCH_REQUEST: u8 = 0xFE;

/// AuthMoreData 包标志
pub const AUTH_MORE_DATA: u8 = 0x01;

/// AuthMoreData 状态码
pub const AUTH_MORE_DATA_FAST_AUTH_SUCCESS: u8 = 0x03;
pub const AUTH_MORE_DATA_PERFORM_FULL_AUTH: u8 = 0x04;

/// AuthSwitchRequest 解析结果
#[derive(Debug, Clone)]
pub struct AuthSwitchRequest {
    pub plugin_name: String,
    pub auth_plugin_data: Vec<u8>,
}

/// 解析 AuthSwitchRequest 包
///
/// 服务器在认证过程中如果收到不支持的认证插件响应,
/// 会发回 0xFE 包要求切换到新的认证插件
///
/// 格式:
/// - 1B  status: 0xFE
/// - NUL-terminated plugin_name
/// - auth_plugin_data (剩余内容)
pub fn parse_auth_switch_request(payload: &[u8]) -> Result<AuthSwitchRequest, &'static str> {
    if payload.is_empty() || payload[0] != AUTH_SWITCH_REQUEST {
        return Err("not an auth switch request");
    }

    let mut r = &payload[1..];
    let plugin_name = read_null_string(&mut r)?;
    let auth_plugin_data = r.to_vec();

    Ok(AuthSwitchRequest {
        plugin_name,
        auth_plugin_data,
    })
}

/// 解析 AuthMoreData 包 (0x01)
///
/// 用于 caching_sha2_password 的后续交互:
/// - 0x03: fast_auth_success — 认证完成
/// - 0x04: perform_full_auth — 需要发送完整密码
#[derive(Debug, Clone)]
pub enum AuthMoreDataResponse {
    FastAuthSuccess,
    PerformFullAuth,
    Unknown(u8),
}

/// 解析 AuthMoreData 包
pub fn parse_auth_more_data(payload: &[u8]) -> Result<AuthMoreDataResponse, &'static str> {
    if payload.is_empty() || payload[0] != AUTH_MORE_DATA {
        return Err("not an auth more data packet");
    }

    if payload.len() < 2 {
        return Err("auth more data packet too short");
    }

    match payload[1] {
        AUTH_MORE_DATA_FAST_AUTH_SUCCESS => Ok(AuthMoreDataResponse::FastAuthSuccess),
        AUTH_MORE_DATA_PERFORM_FULL_AUTH => Ok(AuthMoreDataResponse::PerformFullAuth),
        n => Ok(AuthMoreDataResponse::Unknown(n)),
    }
}

/// 构建 caching_sha2_password 的 Client Auth Response
///
/// 包含完整的 scramble_sha256 响应
pub fn build_sha256_auth_response(
    db_user: &str,
    db_password: &str,
    scramble: &[u8],
    caps: u32,
    charset: u8,
) -> Vec<u8> {
    let auth_response = scramble_sha256(db_password.as_bytes(), scramble);

    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(&caps.to_le_bytes());
    buf.extend_from_slice(&(16_777_215u32).to_le_bytes());
    buf.push(charset);
    buf.extend_from_slice(&[0u8; 23]);
    buf.extend_from_slice(db_user.as_bytes());
    buf.push(0);
    // auth_response: lenenc length (caching_sha2 uses lenenc)
    buf.push(auth_response.len() as u8);
    buf.extend_from_slice(&auth_response);
    buf.extend_from_slice(b"caching_sha2_password");
    buf.push(0);
    buf
}

/// 构建 AuthSwitchResponse:响应服务器要求的插件切换
pub fn build_auth_switch_response(
    password: &str,
    plugin_name: &str,
    auth_data: &[u8],
) -> Result<Vec<u8>, &'static str> {
    match plugin_name {
        "mysql_native_password" => {
            let mut scramble = [0u8; SCRAMBLE_LEN];
            let copy_len = auth_data.len().min(SCRAMBLE_LEN);
            scramble[..copy_len].copy_from_slice(&auth_data[..copy_len]);
            let resp = handshake::scramble_native(password.as_bytes(), &scramble);
            Ok(resp.to_vec())
        }
        "caching_sha2_password" => {
            let resp = scramble_sha256(password.as_bytes(), auth_data);
            Ok(resp)
        }
        _ => Err("unsupported auth plugin for switch"),
    }
}

/// 构建 caching_sha2_password 的明文密码响应(perform_full_auth 路径)
///
/// 当 fast-auth 失败时,发送 NUL 结尾的明文密码
/// 注:这要求安全通道(SSL/TLS),否则不安全
pub fn build_sha256_clear_password(password: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(password.len() + 1);
    buf.extend_from_slice(password.as_bytes());
    buf.push(0);
    buf
}

/// 读取 NUL 结尾字符串
fn read_null_string(r: &mut &[u8]) -> Result<String, &'static str> {
    let pos = r
        .iter()
        .position(|&b| b == 0)
        .ok_or("unterminated string")?;
    let s = String::from_utf8_lossy(&r[..pos]).into_owned();
    *r = &r[pos + 1..];
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构建一个典型的 MySQL 8.0 Server Greeting 包
    fn make_greeting(scramble: &[u8; 20]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(10); // protocol version
        buf.extend_from_slice(b"8.0.33-newproxy\0"); // server version
        buf.extend_from_slice(&42u32.to_le_bytes()); // connection_id
        buf.extend_from_slice(&scramble[..8]); // scramble part 1
        buf.push(0); // filler
        buf.extend_from_slice(&(handshake::SERVER_CAP_LOWER).to_le_bytes());
        buf.push(handshake::DEFAULT_CHARSET);
        buf.extend_from_slice(&handshake::SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
        buf.extend_from_slice(&(handshake::SERVER_CAP_UPPER).to_le_bytes());
        buf.push(21); // auth_data_len
        buf.extend_from_slice(&[0u8; 10]); // reserved
        buf.extend_from_slice(&scramble[8..20]); // scramble part 2
        buf.push(0); // NUL before plugin name
        buf.extend_from_slice(b"mysql_native_password\0");
        buf
    }

    #[test]
    fn parse_greeting_basic() {
        let scramble = [0x41u8; 20];
        let raw = make_greeting(&scramble);
        let g = parse_backend_greeting(&raw).unwrap();

        assert_eq!(g.protocol_version, 10);
        assert_eq!(g.server_version, "8.0.33-newproxy");
        assert_eq!(g.connection_id, 42);
        assert_eq!(g.scramble, scramble);
        assert_eq!(g.auth_plugin_name, "mysql_native_password");
    }

    #[test]
    fn build_auth_response_structure() {
        let scramble = [0x42u8; 20];
        let raw = make_greeting(&scramble);
        let g = parse_backend_greeting(&raw).unwrap();

        let resp = build_backend_auth_response("root", "secret", &g, 33);

        // 验证结构:至少包含 capabilities + max_packet + charset + reserved + username + auth_response
        assert!(resp.len() > 64);
        // username "root\0" should be present
        let user_pos = resp.windows(5).position(|w| w == b"root\0");
        assert!(
            user_pos.is_some(),
            "username 'root\\0' not found in auth response"
        );
        // auth_plugin "mysql_native_password\0" should be at end
        assert!(resp.ends_with(b"mysql_native_password\0"));
        // 回归:不得置位 CLIENT_CONNECT_WITH_DB —— 部分后端/网关对该标志
        // 的连接在命令阶段直接 RST,库选择改由认证后 COM_INIT_DB 完成
        let caps = u32::from_le_bytes(resp[0..4].try_into().unwrap());
        assert_eq!(
            caps & handshake::CLIENT_CONNECT_WITH_DB,
            0,
            "backend auth must not carry CLIENT_CONNECT_WITH_DB"
        );
    }

    // ─── caching_sha2 tests ───

    #[test]
    fn scramble_sha256_roundtrip() {
        let password = b"test_password";
        let scramble = [0x41u8; 20];
        let response = scramble_sha256(password, &scramble);
        assert_eq!(response.len(), 32);

        assert!(verify_sha256_scramble(password, &scramble, &response));
    }

    #[test]
    fn scramble_sha256_wrong_password() {
        let scramble = [0x42u8; 20];
        let response = scramble_sha256(b"correct", &scramble);
        assert!(!verify_sha256_scramble(b"wrong", &scramble, &response));
    }

    #[test]
    fn parse_auth_switch_request_basic() {
        // 构造 AuthSwitchRequest: 0xFE + "caching_sha2_password\0" + auth_data
        let mut pkt = vec![0xFE];
        pkt.extend_from_slice(b"caching_sha2_password\0");
        pkt.extend_from_slice(&[0x01, 0x02, 0x03]); // auth_data

        let req = parse_auth_switch_request(&pkt).unwrap();
        assert_eq!(req.plugin_name, "caching_sha2_password");
        assert_eq!(req.auth_plugin_data, vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn parse_auth_more_data_fast_auth() {
        let pkt = [0x01, 0x03];
        let resp = parse_auth_more_data(&pkt).unwrap();
        assert!(matches!(resp, AuthMoreDataResponse::FastAuthSuccess));
    }

    #[test]
    fn parse_auth_more_data_full_auth() {
        let pkt = [0x01, 0x04];
        let resp = parse_auth_more_data(&pkt).unwrap();
        assert!(matches!(resp, AuthMoreDataResponse::PerformFullAuth));
    }

    #[test]
    fn build_sha256_auth_response_structure() {
        let scramble = [0x43u8; 20];
        let resp = build_sha256_auth_response("root", "secret", &scramble, 0, 45);

        assert!(resp.len() > 64);
        let user_pos = resp.windows(5).position(|w| w == b"root\0");
        assert!(user_pos.is_some());
        assert!(resp.ends_with(b"caching_sha2_password\0"));
    }

    #[test]
    fn build_auth_switch_response_native() {
        let auth_data = [0x41u8; 20];
        let resp =
            build_auth_switch_response("password", "mysql_native_password", &auth_data).unwrap();
        assert_eq!(resp.len(), 20);
    }

    #[test]
    fn build_auth_switch_response_sha256() {
        let auth_data = [0x42u8; 20];
        let resp =
            build_auth_switch_response("password", "caching_sha2_password", &auth_data).unwrap();
        assert_eq!(resp.len(), 32);
    }

    #[test]
    fn build_sha256_clear_password_smoke() {
        let resp = super::build_sha256_clear_password("secret123");
        assert_eq!(&resp[..9], b"secret123");
        assert_eq!(resp[9], 0); // NUL terminator
        assert_eq!(resp.len(), 10);
    }
}

// 协议层错误类型与 OK/ERR/EOF 包构造
// T1.6 实现 OK/ERR/EOF 包
//
// 对齐 C 侧 tr_packet_com.h 的 OK/ERR/EOF 编码
// 支持 pre-8.0 EOF(0xFE) 和新式 OK 结束(CLIENT_DEPRECATE_EOF)

use bytes::{BufMut, Bytes, BytesMut};
use std::io;

/// MySQL 协议层错误
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Connection closed by peer")]
    ConnectionClosed,

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    #[error("Unsupported feature: {0}")]
    Unsupported(String),

    #[error("Packet too large: {0} bytes (max {1})")]
    PacketTooLarge(usize, usize),
}

// ─── 包类型常量 ───

/// OK 包头标识
pub const OK_HEADER: u8 = 0x00;
/// EOF 包头标识(pre-8.0)
pub const EOF_HEADER: u8 = 0xFE;
/// ERR 包头标识
pub const ERR_HEADER: u8 = 0xFF;

// ─── OK 包 ───

/// 构建 MySQL OK 包(对应 C 侧 build_ok)
///
/// 格式(MySQL Protocol::OK_Packet):
/// - 1B  header: 0x00 (或 0xFE 在 CLIENT_DEPRECATE_EOF 场景)
/// - lenenc affected_rows
/// - lenenc last_insert_id
/// - 2B  status_flags
/// - 2B  warnings
/// - strlen+1 info (可选)
/// - strlen+1 session_state_changes (可选,CLIENT_SESSION_TRACK)
pub fn build_ok(
    affected_rows: u64,
    last_insert_id: u64,
    status_flags: u16,
    warnings: u16,
    info: Option<&str>,
) -> Bytes {
    let mut buf = BytesMut::new();
    buf.put_u8(OK_HEADER);
    put_lenenc_int(&mut buf, affected_rows);
    put_lenenc_int(&mut buf, last_insert_id);
    buf.put_u16_le(status_flags);
    buf.put_u16_le(warnings);
    if let Some(info_str) = info {
        put_lenenc_str(&mut buf, info_str);
    }
    buf.freeze()
}

// ─── ERR 包 ───

/// 构建 MySQL ERR 包(对应 C 侧 build_error)
///
/// 格式(MySQL Protocol::ERR_Packet):
/// - 1B  header: 0xFF
/// - 2B  error_code
/// - 1B  sql_state_marker '#'
/// - 5B  sql_state (如 "HY000")
/// - EOF message (带长度前缀)
pub fn build_error(error_code: u16, sql_state: &str, message: &str) -> Bytes {
    let mut buf = BytesMut::new();
    buf.put_u8(ERR_HEADER);
    buf.put_u16_le(error_code);
    buf.put_u8(b'#');
    // SQL state 固定 5 字节
    let state_bytes = sql_state.as_bytes();
    if state_bytes.len() >= 5 {
        buf.put_slice(&state_bytes[..5]);
    } else {
        buf.put_slice(state_bytes);
        buf.put_bytes(0, 5 - state_bytes.len());
    }
    buf.put_slice(message.as_bytes());
    buf.freeze()
}

/// 快捷:构建 access denied 错误(MySQL error 1045)
pub fn build_access_denied(message: &str) -> Bytes {
    build_error(1045, "28000", message)
}

/// 快捷:构建 unknown command 错误(MySQL error 1047)
pub fn build_unknown_command() -> Bytes {
    build_error(1047, "08S01", "Unknown command")
}

/// 解析后端 ERR 包，提取人类可读的错误消息
///
/// 格式(MySQL Protocol::ERR_Packet):
/// - 1B  header: 0xFF
/// - 2B  error_code
/// - 1B  sql_state_marker '#'
/// - 5B  sql_state
/// - message (剩余字节)
pub fn parse_error_message(payload: &[u8]) -> String {
    if payload.len() < 9 || payload[0] != ERR_HEADER {
        return "unknown error".to_string();
    }
    let error_code = u16::from_le_bytes([payload[1], payload[2]]);
    let sql_state = String::from_utf8_lossy(&payload[4..9]).to_string();
    let message = if payload.len() > 9 {
        String::from_utf8_lossy(&payload[9..]).to_string()
    } else {
        String::new()
    };
    format!("ERROR {} ({}): {}", error_code, sql_state, message)
}

// ─── EOF 包 ───

/// 构建 MySQL EOF 包(pre-8.0,对应 C 侧 build_eof)
///
/// 格式(MySQL Protocol::EOF_Packet):
/// - 1B  header: 0xFE
/// - 2B  warnings
/// - 2B  status_flags
pub fn build_eof(warnings: u16, status_flags: u16) -> Bytes {
    let mut buf = BytesMut::new();
    buf.put_u8(EOF_HEADER);
    buf.put_u16_le(warnings);
    buf.put_u16_le(status_flags);
    buf.freeze()
}

/// 构建新式 EOF(CLIENT_DEPRECATE_EOF 场景)——即 OK 包 header 0xFE
///
/// MySQL 8.0+:当客户端宣告 CLIENT_DEPRECATE_EOF 时,
/// Result Set 用 OK 包(header 0xFE)替代 EOF 包
pub fn build_eof_deprecated(
    affected_rows: u64,
    last_insert_id: u64,
    status_flags: u16,
    warnings: u16,
) -> Bytes {
    let mut buf = BytesMut::new();
    buf.put_u8(EOF_HEADER); // header 0xFE 而非 0x00
    put_lenenc_int(&mut buf, affected_rows);
    put_lenenc_int(&mut buf, last_insert_id);
    buf.put_u16_le(status_flags);
    buf.put_u16_le(warnings);
    buf.freeze()
}

// ─── 辅助编码函数 ───

/// 写长度编码整数(lenenc_int)
fn put_lenenc_int(buf: &mut BytesMut, val: u64) {
    if val < 251 {
        buf.put_u8(val as u8);
    } else if val < 65536 {
        buf.put_u8(0xFC);
        buf.put_u16_le(val as u16);
    } else if val < 16_777_216 {
        buf.put_u8(0xFD);
        // 写 3 字节 LE(无 put_u24_le 方法)
        let v = val as u32;
        buf.put_slice(&[
            (v & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            ((v >> 16) & 0xFF) as u8,
        ]);
    } else {
        buf.put_u8(0xFE);
        buf.put_u64_le(val);
    }
}

/// 写长度编码字符串(lenenc_str):长度前缀(lenenc_int) + 内容
fn put_lenenc_str(buf: &mut BytesMut, s: &str) {
    let bytes = s.as_bytes();
    put_lenenc_int(buf, bytes.len() as u64);
    buf.put_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_packet_minimal() {
        let pkt = build_ok(0, 0, 0x0002, 0, None);
        // [OK_HEADER=0x00] [affected_rows=0] [last_insert_id=0] [status=0x02,0x00] [warnings=0x00,0x00]
        assert_eq!(pkt.len(), 7);
        assert_eq!(pkt[0], OK_HEADER);
        assert_eq!(pkt[1], 0); // affected_rows < 251 → 1B
        assert_eq!(pkt[2], 0); // last_insert_id < 251 → 1B
        assert_eq!(u16::from_le_bytes([pkt[3], pkt[4]]), 0x0002);
        assert_eq!(u16::from_le_bytes([pkt[5], pkt[6]]), 0x0000);
    }

    #[test]
    fn ok_packet_with_info() {
        let pkt = build_ok(1, 42, 0x0002, 0, Some("Records: 1"));
        // 1 + 1 + 1 + 2 + 2 = 7 + lenenc "Records: 1"(1B len + 10B)
        assert_eq!(pkt.len(), 18);
        assert_eq!(&pkt[7..18], b"\x0ARecords: 1");
    }

    #[test]
    fn error_packet_access_denied() {
        let pkt = build_access_denied("Access denied for user 'root'@'localhost'");
        assert_eq!(pkt[0], ERR_HEADER);
        assert_eq!(u16::from_le_bytes([pkt[1], pkt[2]]), 1045);
        assert_eq!(pkt[3], b'#');
        assert_eq!(&pkt[4..9], b"28000");
        assert!(pkt[9..].starts_with(b"Access denied"));
    }

    #[test]
    fn eof_packet() {
        let pkt = build_eof(0, 0x0002);
        assert_eq!(pkt.len(), 5);
        assert_eq!(pkt[0], EOF_HEADER);
        assert_eq!(u16::from_le_bytes([pkt[1], pkt[2]]), 0);
        assert_eq!(u16::from_le_bytes([pkt[3], pkt[4]]), 0x0002);
    }

    #[test]
    fn eof_deprecated_packet() {
        let pkt = build_eof_deprecated(0, 0, 0x0002, 0);
        assert_eq!(pkt[0], EOF_HEADER);
        // [0xFE] [0x00] [0x00] [0x02,0x00] [0x00,0x00]
        assert_eq!(pkt.len(), 7);
    }

    #[test]
    fn lenenc_int_encoding() {
        // < 251: 1 byte
        let mut buf = BytesMut::new();
        put_lenenc_int(&mut buf, 250);
        assert_eq!(&buf[..], &[250]);

        // 251..65535: 0xFC + 2 bytes LE
        buf.clear();
        put_lenenc_int(&mut buf, 1000);
        assert_eq!(buf[0], 0xFC);
        assert_eq!(u16::from_le_bytes([buf[1], buf[2]]), 1000);

        // 65536..16777215: 0xFD + 3 bytes LE
        buf.clear();
        put_lenenc_int(&mut buf, 100_000);
        assert_eq!(buf[0], 0xFD);
        assert_eq!(&buf[1..4], &(100_000u32).to_le_bytes()[..3]);

        // >= 16777216: 0xFE + 8 bytes LE
        buf.clear();
        put_lenenc_int(&mut buf, 20_000_000);
        assert_eq!(buf[0], 0xFE);
        assert_eq!(
            u64::from_le_bytes(buf[1..9].try_into().unwrap()),
            20_000_000
        );
    }

    #[test]
    fn parse_error_message_branches() {
        // 标准 ERR 包 → 完整解析
        let mut pkt = vec![ERR_HEADER, 0x28, 0x04, b'#', b'2', b'8', b'0', b'0', b'0'];
        pkt.extend_from_slice(b"Access denied");
        let msg = parse_error_message(&pkt);
        assert_eq!(msg, "ERROR 1064 (28000): Access denied");
        // 恰好 9 字节(无消息)→ 空消息
        let pkt9 = vec![ERR_HEADER, 0x01, 0x00, b'#', b'0', b'8', b'S', b'0', b'1'];
        let msg = parse_error_message(&pkt9);
        assert!(msg.ends_with("): "), "got: {msg}");
        // 过短 → unknown error
        assert_eq!(parse_error_message(&[ERR_HEADER, 0x01]), "unknown error");
        assert_eq!(parse_error_message(&[]), "unknown error");
        // 非 ERR header → unknown error
        assert_eq!(parse_error_message(&[0x00, 0x01, 0x00, b'#', b'0', b'8', b'S', b'0', b'1', b'x']), "unknown error");
    }

    #[test]
    fn build_error_edge_cases() {
        // 空消息
        let pkt = build_error(1047, "08S01", "");
        assert_eq!(pkt[0], ERR_HEADER);
        // 非 ASCII 消息(UTF-8)
        let pkt = build_error(1000, "HY000", "库不存在: 测试");
        let tail = String::from_utf8_lossy(&pkt[9..]);
        assert!(tail.contains("测试"));
        // 快捷构造
        assert_eq!(build_access_denied("no").len(), build_error(1045, "28000", "no").len());
        assert_eq!(build_unknown_command()[0], ERR_HEADER);
        // 非标准 5 字节 sql_state 也容忍(长度不足时截断解析)
        let pkt = build_error(1, "XY", "m");
        let msg = parse_error_message(&pkt);
        assert!(msg.contains("ERROR 1"), "got: {msg}");
    }

    #[test]
    fn ok_packet_affected_rows_encoding() {
        // 大 affected_rows 走 lenenc 多字节编码
        let pkt = build_ok(0xFFFFFF + 5, 0, 0x0002, 0, None);
        assert!(pkt.len() > 5, "大数应扩展 lenenc 编码");
        let pkt2 = build_ok(0, 0, 0x0002, 0, None);
        assert_eq!(pkt2.len(), 7);
    }
}

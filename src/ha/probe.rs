// HA 探测最小 MySQL 客户端
//
// 用途:代理以普通账号执行只读 SQL(raft 状态表 / SHOW STATUS / @@gtid_executed),
// 用于 raft leader 发现、半同步监控与 GTID 采样。连接不进后端连接池,用完即关。

use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::config::DbUser;
use crate::ha::model::ProbeRow;
use crate::ha::result::{decode_text_packets, is_err_packet, is_terminator, read_lenenc_int, TextResult};
use crate::proto::codec;

/// raft 状态表探测 SQL(按真实部署结构;列名解析,顺序无关)
pub fn raft_status_sql() -> &'static str {
    "SELECT id, leader, view_id, epoch_id, updated_at FROM mysql.xenon_raft_status LIMIT 16"
}

/// 对单成员执行任意只读查询,返回文本结果集。
pub async fn query_text(
    host: &str,
    port: u16,
    user: &DbUser,
    charset: u8,
    sql: &str,
    timeout_ms: u64,
) -> Result<TextResult, String> {
    let fut = async {
        let mut stream = TcpStream::connect((host, port))
            .await
            .map_err(|e| format!("connect {host}:{port}: {e}"))?;
        let _ = stream.set_nodelay(true);
        let mut buf = BytesMut::with_capacity(4096);
        crate::conn::front::do_backend_handshake(&mut stream, Some(user), None, &mut buf, charset)
            .await
            .map_err(|e| format!("handshake {host}:{port}: {e}"))?;

        let r = query_text_on_stream(&mut stream, &mut buf, sql).await;
        let _ = codec::send_packet(&mut stream, 0, &[0x01]).await; // COM_QUIT(尽力)
        r
    };
    tokio::time::timeout(Duration::from_millis(timeout_ms), fut)
        .await
        .map_err(|_| format!("probe timeout after {timeout_ms}ms against {host}:{port}"))?
}


/// 在已握手连接上执行一条查询并读取文本结果(不负责握手/QUIT)。
/// 供:探测客户端(query_text)与"会话级屏障/GTID 采样"(读写分离)复用。
pub async fn query_text_on_stream<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    sql: &str,
) -> Result<TextResult, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut pkt = vec![0x03u8]; // COM_QUERY
    pkt.extend_from_slice(sql.as_bytes());
    codec::send_packet(stream, 0, &pkt)
        .await
        .map_err(|e| format!("send query: {e}"))?;

    let mut packets: Vec<Vec<u8>> = Vec::new();
    // 列数(第一包)先读出来,用于识别"列后经典 EOF"(非结果结束)
    let mut ncols: Option<usize> = None;
    loop {
        let (_seq, payload) = codec::read_packet(stream, buf)
            .await
            .map_err(|e| format!("read packet: {e}"))?;
        let bytes: Vec<u8> = payload.to_vec();
        if is_err_packet(&bytes) {
            let msg = crate::proto::error::parse_error_message(&bytes);
            return Err(format!("server error: {msg}"));
        }
        if packets.is_empty() {
            let mut p = 0usize;
            ncols = read_lenenc_int(&bytes, &mut p).map(|v| v as usize);
        }
        let term = is_terminator(&bytes);
        // 经典协议:恰好 ncols+1 个包(列数+全部列定义)之后的 5B EOF 是"列后
        // EOF",不是结果结束——继续收行(代理未协商 CLIENT_DEPRECATE_EOF,
        // 真实 MySQL 8.0 即此形态)。
        let mid_col_eof = term
            && bytes.len() < 9
            && bytes[0] == 0xFE
            && ncols.map(|k| packets.len() == k + 1).unwrap_or(false);
        packets.push(bytes);
        if term && !mid_col_eof {
            break;
        }
    }
    decode_text_packets(&packets).map_err(|e| format!("decode result: {e}"))
}
/// 探测单成员 raft 状态行;失败返回 Err(调用方按"该成员不可达"处理)。
pub async fn probe_raft_status(
    host: &str,
    port: u16,
    user: &DbUser,
    charset: u8,
    timeout_ms: u64,
) -> Result<ProbeRow, String> {
    let res = query_text(host, port, user, charset, raft_status_sql(), timeout_ms).await?;
    if res.rows.is_empty() {
        return Err(format!("empty raft status result on {host}:{port}"));
    }
    Ok(ProbeRow {
        from_host: host.to_string(),
        row: res.row_map(0),
    })
}

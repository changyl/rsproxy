// 结果集流式转发:proxy_result_set / OK/ERR/EOF 终止 / 包类型识别
// T1.5 实现:后端→前端的流式包转发
//
// 对齐 C 侧 tr_result.c 的结果转发逻辑
// 参考 MySQL 官方 Protocol:Command Phase · Text Resultset

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing;

use crate::proto::codec;
use crate::proto::error::{self, ProtoError};

// ─── 响应包类型 ───

/// 后端响应的包类型(由第一个字节判定)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    /// OK 包 (header 0x00 或 0xFE with CLIENT_DEPRECATE_EOF)
    Ok,
    /// ERR 包 (header 0xFF)
    Err,
    /// EOF 包 (header 0xFE, pre-8.0)
    Eof,
    /// 结果集的列数(lenenc 整数)
    ColumnCount(u64),
}

impl ResponseKind {
    /// 从 payload 的首字节判定包类型
    ///
    /// - 0x00 → OK
    /// - 0xFF → ERR
    /// - 0xFE → 如果 payload 长度 > 5 则为 EOF(或 CLIENT_DEPRECATE_EOF OK)
    /// - < 251 → lenenc column_count
    /// - >= 251 → lenenc column_count(多字节)
    pub fn from_payload(payload: &[u8], expect_resultset: bool) -> Self {
        if payload.is_empty() {
            return ResponseKind::Ok; // 空 OK(不太可能,防御性)
        }
        match payload[0] {
            error::OK_HEADER => ResponseKind::Ok,
            error::ERR_HEADER => ResponseKind::Err,
            error::EOF_HEADER => {
                // 0xFE 可能是 EOF(5 字节)或 CLIENT_DEPRECATE_EOF OK(≥7 字节)
                // 也可能是在结果集上下文中的列数(lenenc 0xFE)
                // 启发式:若 payload_len < 7 且字节 1-2 可解释为 warnings → EOF
                if payload.len() >= 5 && payload.len() < 7 {
                    ResponseKind::Eof
                } else if expect_resultset {
                    // 在结果集上下文中,0xFE 开头的长包 → lenenc column_count
                    ResponseKind::ColumnCount(read_lenenc_column_count(payload))
                } else {
                    ResponseKind::Eof
                }
            }
            n if n < 0xFB => {
                if expect_resultset {
                    ResponseKind::ColumnCount(n as u64)
                } else {
                    ResponseKind::Ok // < 251 也可能是 OK 的 affected_rows
                }
            }
            _ => {
                // ≥ 0xFB:多字节 lenenc column_count(仅在结果集上下文中)
                if expect_resultset {
                    ResponseKind::ColumnCount(read_lenenc_column_count(payload))
                } else {
                    ResponseKind::Ok // 非结果集上下文:当作 OK 处理
                }
            }
        }
    }

    /// 是否为终止类型的包(OK/ERR/EOF)
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ResponseKind::Ok | ResponseKind::Err | ResponseKind::Eof
        )
    }
}

/// 从 payload 读取 lenenc 编码的列数
fn read_lenenc_column_count(payload: &[u8]) -> u64 {
    if payload.is_empty() {
        return 0;
    }
    match payload[0] {
        0xFC => {
            if payload.len() >= 3 {
                u16::from_le_bytes([payload[1], payload[2]]) as u64
            } else {
                0
            }
        }
        0xFD => {
            if payload.len() >= 4 {
                (payload[1] as u64) | ((payload[2] as u64) << 8) | ((payload[3] as u64) << 16)
            } else {
                0
            }
        }
        0xFE => {
            if payload.len() >= 9 {
                u64::from_le_bytes(payload[1..9].try_into().unwrap_or_default())
            } else {
                0
            }
        }
        _ => payload[0] as u64,
    }
}

// ─── 流式转发 ───

/// 结果集转发状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardState {
    /// 等待第一个响应包(判断 OK/ERR/结果集)
    AwaitFirst,
    /// 正在读取列定义
    ReadingColumns { remaining: usize },
    /// 列定义后的 EOF
    ColumnEof,
    /// 正在读取行数据
    ReadingRows,
    /// 结果集结束(读到 EOF/OK)
    Done,
}

/// 把后端终止包适配为前端(旧式)期望的格式。
///
/// 前端客户端按旧式协议解析(代理前端不宣告 CLIENT_DEPRECATE_EOF):
/// 行结束必须是 5 字节 EOF 包。若后端发来 new-style 的 OK 收尾包
/// (0xFE, ≥7B,布局 [FE][affected][last_id][status 2][warnings 2]),
/// 需要转换成旧式 EOF 包([FE][warnings 2][status 2])。
/// 旧式 EOF(0xFE, 5B)与 ERR(0xFF)原样透传。
fn adapt_terminal(payload: &[u8]) -> Bytes {
    if payload.len() >= 7 && payload[0] == error::EOF_HEADER {
        let warnings = u16::from_le_bytes([payload[5], payload[6]]);
        let status = u16::from_le_bytes([payload[3], payload[4]]);
        error::build_eof(warnings, status)
    } else {
        Bytes::copy_from_slice(payload)
    }
}

/// 转发计数:统计透传到客户端的字节数与包数(供按库流量统计)
#[derive(Debug, Default, Clone, Copy)]
pub struct ForwardCount {
    /// 已转发字节(含 4B 包头)
    pub bytes: u64,
    /// 已转发包数
    pub pkts: u64,
    /// 结果集行数(文本协议:ReadingRows 阶段每个非终止包 = 一行;
    /// OK/ERR/0 列结果集恒 0)
    pub rows: u64,
}

impl ForwardCount {
    /// 记录一个待转发包(按 payload 长度计入,含包头)
    #[inline]
    pub fn add_packet(&mut self, payload: &[u8]) {
        self.bytes += (codec::HEADER_LEN + payload.len()) as u64;
        self.pkts += 1;
    }
}

/// 将后端响应流式转发到前端
///
/// 根据 MySQL 协议,后端的 COM_QUERY 响应可能是:
/// - OK 包(非 SELECT 语句)
/// - ERR 包(出错)
/// - 结果集:column_count → N×ColumnDef → EOF → M×Row → EOF/OK
///
/// 透传模式:逐 packet 读取后端 → 逐 packet 写入前端
/// 内存占用 = 当前一个 packet 的大小(≤16MB)
pub async fn forward_backend_response<R, W>(
    backend: &mut R,
    frontend: &mut W,
    back_buf: &mut BytesMut,
    count: &mut ForwardCount,
) -> Result<(ResponseKind, Option<u16>), ProtoError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // 返回 (首个响应包类型, ERR 包的错误码);供调用方识别后端语法/执行错误
    // 客户端查询包 seq=0，响应首包 seq 必须从 1 开始（MySQL 协议规定）
    let mut state = ForwardState::AwaitFirst;
    let mut seq: u8 = 1;
    // 批量写缓冲:连续小包(列定义/行数据)攒批后一次写出,减少逐包 syscall
    // (原实现每包一次 write_vectored,大结果集下 syscall 数 = 包数);
    // 达到阈值或需要返回时冲刷。字节/包数在 push_packet 时计入全局统计。
    let mut out: Vec<u8> = Vec::with_capacity(8192);
    const FLUSH_THRESHOLD: usize = 32 * 1024;

    macro_rules! flush_out {
        () => {
            if !out.is_empty() {
                frontend.write_all(&out).await?;
                out.clear();
            }
        };
    }
    // 拼一个包(header+payload)进批量缓冲,并推进 seq;缓冲超阈值立即冲刷
    macro_rules! push_packet {
        ($payload:expr) => {{
            let pl = $payload;
            count.add_packet(pl);
            codec::record_forwarded(1, (codec::HEADER_LEN + pl.len()) as u64);
            out.extend_from_slice(&[
                (pl.len() & 0xFF) as u8,
                ((pl.len() >> 8) & 0xFF) as u8,
                ((pl.len() >> 16) & 0xFF) as u8,
                seq,
            ]);
            out.extend_from_slice(pl);
            seq = seq.wrapping_add(1);
            if out.len() >= FLUSH_THRESHOLD {
                frontend.write_all(&out).await?;
                out.clear();
            }
        }};
    }

    loop {
        let (pkt_seq, payload) = codec::read_packet(backend, back_buf).await?;
        // 预览 hex 串仅用于 trace 日志:必须在 TRACE 级别开启时才构建,
        // 否则每个转发包都会白做 ~10 次 format! + 一次 join 分配
        // (实测为转发热路径的显著 CPU 开销)。
        if tracing::level_enabled!(tracing::Level::TRACE) {
            let preview: String = payload
                .iter()
                .take(10)
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join(" ");
            tracing::trace!(seq = pkt_seq, len = payload.len(), state = ?state, "backend packet [{}]", preview);
        }

        match state {
            ForwardState::AwaitFirst => {
                let kind = ResponseKind::from_payload(&payload, true);

                match kind {
                    ResponseKind::Ok => {
                        // 非结果集响应(OK):直接转发,结束
                        push_packet!(&payload);
                        flush_out!();
                        return Ok((ResponseKind::Ok, None));
                    }
                    ResponseKind::Err => {
                        // 后端错误:转发并返回错误码(语法 1064/权限等),供上层记录
                        let code = if payload.len() >= 3 {
                            Some(u16::from_le_bytes([payload[1], payload[2]]))
                        } else {
                            None
                        };
                        push_packet!(&payload);
                        flush_out!();
                        return Ok((ResponseKind::Err, code));
                    }
                    ResponseKind::Eof => {
                        // 不应该在第一个包出现 EOF
                        push_packet!(&payload);
                        flush_out!();
                        return Ok((ResponseKind::Eof, None));
                    }
                    ResponseKind::ColumnCount(n) => {
                        // 结果集开始:转发 column_count
                        push_packet!(&payload);
                        if n == 0 {
                            // 0 列:直接跳到行结束的 EOF
                            state = ForwardState::Done;
                        } else {
                            state = ForwardState::ReadingColumns {
                                remaining: n as usize,
                            };
                        }
                    }
                }
            }

            ForwardState::ReadingColumns { remaining } => {
                // 检查是否到达列定义后的 EOF
                // 同时接受旧式 EOF(5B 0xFE) 和新式 OK(≥7B 0xFE)
                let is_eof =
                    payload[0] == error::EOF_HEADER && (payload.len() == 5 || payload.len() >= 7);

                if is_eof {
                    push_packet!(&payload);
                    state = ForwardState::ReadingRows;
                } else {
                    // 列定义:直接转发
                    push_packet!(&payload);
                    let new_remaining = remaining.saturating_sub(1);
                    if new_remaining == 0 {
                        state = ForwardState::ColumnEof;
                    } else {
                        state = ForwardState::ReadingColumns {
                            remaining: new_remaining,
                        };
                    }
                }
            }

            ForwardState::ColumnEof => {
                // 检查是否到达列定义后的 EOF
                // 同时接受旧式 EOF(5B 0xFE) 和新式 OK(≥7B 0xFE)
                let is_eof =
                    payload[0] == error::EOF_HEADER && (payload.len() == 5 || payload.len() >= 7);

                if is_eof {
                    push_packet!(&payload);
                    state = ForwardState::ReadingRows;
                } else if payload[0] != error::ERR_HEADER {
                    // 后端 new-style:列定义后无 EOF,直接发来行数据。
                    // 前端客户端按旧式解析,必须看到列后 EOF → 插入合成 EOF。
                    let synthetic_eof = error::build_eof(0, 0x0002);
                    push_packet!(&synthetic_eof);
                    push_packet!(&payload);
                    state = ForwardState::ReadingRows;
                } else {
                    let preview: String = payload
                        .iter()
                        .take(20)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");
                    return Err(ProtoError::Protocol(format!(
                        "expected EOF after column definitions, got len={} first_bytes=[{}]",
                        payload.len(),
                        preview
                    )));
                }
            }

            ForwardState::ReadingRows => {
                // 检查是否为行结束的 EOF/OK,同时接受新旧格式 + ERR
                let is_terminal = payload[0] == error::ERR_HEADER
                    || (payload[0] == error::EOF_HEADER
                        && (payload.len() == 5 || payload.len() >= 7));

                if is_terminal {
                    // 后端 new-style 的 OK 收尾(≥7B)适配为旧式 5B EOF
                    let adapted = adapt_terminal(&payload);
                    push_packet!(&adapted);
                    flush_out!();
                    return Ok((ResponseKind::Ok, None));
                } else {
                    // 行数据:直接转发(每包 = 一行,供 Top SQL rows_sent / sql_rows_total 统计)
                    count.rows += 1;
                    push_packet!(&payload);
                }
            }

            ForwardState::Done => {
                // 0 列结果集的终止包
                let kind = ResponseKind::from_payload(&payload, false);
                if kind.is_terminal() {
                    let adapted = adapt_terminal(&payload);
                    push_packet!(&adapted);
                    flush_out!();
                    return Ok((kind, None));
                } else {
                    push_packet!(&payload);
                }
            }
        }
    }
}

/// COM_FIELD_LIST 专用转发:透传 ColumnDef* + EOF 响应
///
/// COM_FIELD_LIST 响应的 Column Definition 包首字节 0x03（"def" 长度前缀），
/// 会被 `forward_simple_response` 的 `ResponseKind::from_payload` 误判为 OK
/// （因为 affected_rows=3 的 OK 包首字节也是 0x03），导致提前退出。
/// 此函数改为按 EOF 包头（0xFE）判断终止。
pub async fn forward_field_list_response<R, W>(
    backend: &mut R,
    frontend: &mut W,
    back_buf: &mut BytesMut,
    count: &mut ForwardCount,
) -> Result<(), ProtoError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut seq: u8 = 1;
    loop {
        let (_, payload) = codec::read_packet(backend, back_buf).await?;

        // ERR 包 → 转发并结束
        if payload.first() == Some(&error::ERR_HEADER) {
            count.add_packet(&payload);
            codec::send_packet(frontend, seq, &payload).await?;
            return Ok(());
        }

        // EOF 包检测:0xFE 开头 + 旧式(5B)或新式(≥7B, CLIENT_DEPRECATE_EOF)
        let is_eof = payload.first() == Some(&error::EOF_HEADER)
            && (payload.len() == 5 || payload.len() >= 7);

        count.add_packet(&payload);
        codec::send_packet(frontend, seq, &payload).await?;

        if is_eof {
            return Ok(());
        }
        seq = seq.wrapping_add(1);
    }
}

/// COM_STMT_PREPARE 响应专用转发。
///
/// COM_STMT_PREPARE_OK 响应结构:
///   1. COM_STMT_PREPARE_OK 包(0x00 + stmt_id(4) + num_columns(2LE)
///      + num_params(2LE) + reserved(1) + warning_count(2))
///   2. 若 num_params>0:num_params × ColumnDef + 组尾 EOF 终止包
///   3. 若 num_columns>0:num_columns × ColumnDef + 组尾 EOF 终止包
///
/// 首包以 0x00 开头,会被 `forward_backend_response` 的 ResponseKind 误判为 OK
/// 而提前结束,故需此专用函数按 num_params/num_columns 计数转发列定义。
///
/// 能力协商:后端连接**不**宣告 CLIENT_DEPRECATE_EOF(见 BACKEND_CAP_FULL),
/// 每组非空列定义后必然跟随旧式 5B EOF 终止包。终止包必须**消费**:
/// 残留字节轻则被下一组误读为终止符(列定义丢失),重则滞留 back_buf
/// 错位下一条命令的响应(后端流整体失步)。终止包原样回传前端(新式
/// 0xFE OK 由 adapt_terminal 转换为旧式),保留 warnings/status 标志。
///
/// 返回 `true` 表示后端实际创建了 prepared statement(PREPARE_OK):
/// 调用方应据此将连接标记为不再复用(见 `BackConn::mark_no_reuse`)。
pub async fn forward_stmt_prepare_response<R, W>(
    backend: &mut R,
    frontend: &mut W,
    back_buf: &mut BytesMut,
    count: &mut ForwardCount,
) -> Result<bool, ProtoError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut seq: u8 = 1;

    // 1. 读首包(COM_STMT_PREPARE_OK 或 ERR)
    let (_, first) = codec::read_packet(backend, back_buf).await?;
    if first.is_empty() {
        return Err(ProtoError::Protocol("empty STMT_PREPARE response".into()));
    }

    // 出错:转发 ERR 并结束(prepare 失败,后端未创建语句 → false)
    if first[0] == error::ERR_HEADER {
        count.add_packet(&first);
        codec::send_packet(frontend, seq, &first).await?;
        return Ok(false);
    }

    // COM_STMT_PREPARE_OK:解析 num_columns / num_params
    let (num_columns, num_params) = if first.len() >= 9 && first[0] == error::OK_HEADER {
        let nc = u16::from_le_bytes([first[5], first[6]]) as usize;
        let np = u16::from_le_bytes([first[7], first[8]]) as usize;
        (nc, np)
    } else {
        // 非 OK/ERR 的异常首包:原样转发并结束(防御性)
        tracing::warn!(len = first.len(), "unexpected STMT_PREPARE first packet");
        count.add_packet(&first);
        codec::send_packet(frontend, seq, &first).await?;
        return Ok(false);
    };

    count.add_packet(&first);
    codec::send_packet(frontend, seq, &first).await?;
    seq = seq.wrapping_add(1);

    // 2. 转发参数列定义组(含组尾 EOF 终止包)
    if num_params > 0 {
        seq = forward_column_defs(backend, frontend, back_buf, seq, num_params, count).await?;
    }

    // 3. 转发字段列定义组(含组尾 EOF 终止包)
    if num_columns > 0 {
        seq = forward_column_defs(backend, frontend, back_buf, seq, num_columns, count).await?;
    }
    let _ = seq;

    // PREPARE_OK:后端已创建语句,连接不应再被复用
    Ok(true)
}

/// 转发一组 count 个 ColumnDef,并消费组尾的 EOF 终止包。
///
/// 后端连接不宣告 CLIENT_DEPRECATE_EOF(BACKEND_CAP_FULL),按旧式协议
/// 每组非空列定义后必然跟随 5B EOF。终止包消费后回传前端(新式 0xFE
/// OK 经 adapt_terminal 转为旧式),保留 warnings/status;若读到非终止包,
/// 说明后端流不符合协商协议——显式报错废弃该连接,避免残留字节静默
/// 错位后续命令的响应。
///
/// 返回下一个包的 seq。
async fn forward_column_defs<R, W>(
    backend: &mut R,
    frontend: &mut W,
    back_buf: &mut BytesMut,
    mut seq: u8,
    num_defs: usize,
    fc: &mut ForwardCount,
) -> Result<u8, ProtoError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    for _ in 0..num_defs {
        let (_, payload) = codec::read_packet(backend, back_buf).await?;
        // 组尾 EOF 提前到达(实际列数少于声明,防御路径):回传并结束
        if is_group_terminator(&payload) {
            let out = adapt_terminal(&payload);
            fc.add_packet(&out);
            codec::send_packet(frontend, seq, &out).await?;
            return Ok(seq.wrapping_add(1));
        }
        fc.add_packet(&payload);
        codec::send_packet(frontend, seq, &payload).await?;
        seq = seq.wrapping_add(1);
    }
    // num_defs 个定义已转发:按协商的旧式协议,组尾必跟 EOF 终止包,读取并回传
    let (_, tail) = codec::read_packet(backend, back_buf).await?;
    if !is_group_terminator(&tail) {
        return Err(ProtoError::Protocol(format!(
            "expected EOF after {} column definitions, got {} bytes (first 0x{:02X})",
            num_defs,
            tail.len(),
            tail.first().unwrap_or(&0)
        )));
    }
    let out = adapt_terminal(&tail);
    fc.add_packet(&out);
    codec::send_packet(frontend, seq, &out).await?;
    Ok(seq.wrapping_add(1))
}

/// 列定义组的终止包:旧式 5B EOF,或新式(CLIENT_DEPRECATE_EOF)0xFE OK(≥7B)。
/// ColumnDef 以 lenenc 目录名(通常 0x03 "def")开头,不会与之混淆。
fn is_group_terminator(payload: &[u8]) -> bool {
    payload.first() == Some(&error::EOF_HEADER) && (payload.len() == 5 || payload.len() >= 7)
}

pub async fn forward_simple_response<R, W>(
    backend: &mut R,
    frontend: &mut W,
    back_buf: &mut BytesMut,
    count: &mut ForwardCount,
) -> Result<ResponseKind, ProtoError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (_, payload) = codec::read_packet(backend, back_buf).await?;
    // 客户端命令包 seq=0，响应首包 seq 必须从 1 开始
    count.add_packet(&payload);
    codec::send_packet(frontend, 1, &payload).await?;

    // 简单命令通常只返回一个包,但处理多包场景(multi-result)
    // 检查是否为 OK/ERR(终止包);返回终止包的 kind,供调用方判断成败
    let kind = ResponseKind::from_payload(&payload, false);
    if kind.is_terminal() {
        return Ok(kind);
    }

    // 有后续包,继续读取直到 OK/ERR（首包已用 seq=1，从 2 开始）
    let mut seq: u8 = 2;
    loop {
        let (_, payload) = codec::read_packet(backend, back_buf).await?;
        let kind = ResponseKind::from_payload(&payload, false);
        count.add_packet(&payload);
        codec::send_packet(frontend, seq, &payload).await?;
        if kind.is_terminal() {
            return Ok(kind);
        }
        seq = seq.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_kind_ok() {
        let pkt = error::build_ok(0, 0, 0x0002, 0, None);
        assert_eq!(ResponseKind::from_payload(&pkt, true), ResponseKind::Ok);
        assert_eq!(ResponseKind::from_payload(&pkt, false), ResponseKind::Ok);
    }

    #[test]
    fn response_kind_err() {
        let pkt = error::build_error(1045, "28000", "Access denied");
        assert_eq!(ResponseKind::from_payload(&pkt, true), ResponseKind::Err);
    }

    #[test]
    fn response_kind_eof() {
        let pkt = error::build_eof(0, 0x0002);
        assert_eq!(ResponseKind::from_payload(&pkt, true), ResponseKind::Eof);
        // 在结果集上下文中,EOF(5 字节)也是 EOF
        assert_eq!(ResponseKind::from_payload(&pkt, false), ResponseKind::Eof);
    }

    #[test]
    fn response_kind_column_count_small() {
        // 模拟 column_count=3 的包(第一个 payload 包)
        let mut pkt = vec![0x03u8]; // lenenc:3
        pkt.extend_from_slice(b"def"); // column definition 前缀

        assert_eq!(
            ResponseKind::from_payload(&pkt, true),
            ResponseKind::ColumnCount(3)
        );
        // 非结果集上下文:当作 OK
        assert_eq!(ResponseKind::from_payload(&pkt, false), ResponseKind::Ok);
    }

    #[test]
    fn response_kind_column_count_large() {
        // lenenc 0xFC + 2B LE: 300 columns
        let mut pkt = vec![0xFC, 0x2C, 0x01]; // 300 in LE
        pkt.extend_from_slice(b"def");
        assert_eq!(
            ResponseKind::from_payload(&pkt, true),
            ResponseKind::ColumnCount(300)
        );
    }

    #[test]
    fn read_lenenc_column_count_values() {
        // 1-byte: 250
        assert_eq!(read_lenenc_column_count(&[250]), 250);
        // 2-byte: 0xFC prefix
        assert_eq!(read_lenenc_column_count(&[0xFC, 0xE8, 0x03]), 1000);
        // 3-byte: 0xFD prefix
        assert_eq!(
            read_lenenc_column_count(&[0xFD, 0x40, 0x42, 0x0F]),
            1_000_000
        );
    }

    #[test]
    fn forward_state_terminal_detection() {
        assert!(ResponseKind::Ok.is_terminal());
        assert!(ResponseKind::Err.is_terminal());
        assert!(ResponseKind::Eof.is_terminal());
        assert!(!ResponseKind::ColumnCount(5).is_terminal());
    }

    // ─── COM_STMT_PREPARE 响应转发 ───

    use crate::proto::error::{build_error, build_ok, ERR_HEADER};
    use bytes::BytesMut;
    use tokio_test::io::Builder;

    /// 旧式协议 5B EOF 终止包(warnings=0, status=AUTOCOMMIT)
    const OLD_EOF: [u8; 5] = [0xFE, 0x00, 0x00, 0x02, 0x00];

    /// 构造 COM_STMT_PREPARE_OK payload:
    /// [0x00][stmt_id u32][num_columns u16][num_params u16][reserved u8][warnings u16]
    fn prepare_ok(stmt_id: u32, nc: u16, np: u16) -> Vec<u8> {
        let mut p = vec![0x00u8];
        p.extend_from_slice(&stmt_id.to_le_bytes());
        p.extend_from_slice(&nc.to_le_bytes());
        p.extend_from_slice(&np.to_le_bytes());
        p.push(0x00); // reserved
        p.extend_from_slice(&0u16.to_le_bytes()); // warning_count
        p
    }

    /// 最小 ColumnDef:以 lenenc "def" 目录前缀(0x03)开头,不会被误判为 EOF
    fn coldef(name: &str) -> Vec<u8> {
        let mut p = vec![0x03u8];
        p.extend_from_slice(b"def");
        p.push(0x00);
        p.extend_from_slice(name.as_bytes());
        p
    }

    /// 解析代理发给客户端的完整字节流为 (seq, payload) 列表
    fn parse_client_packets(out: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut packets = Vec::new();
        let mut i = 0usize;
        while i < out.len() {
            assert!(i + 4 <= out.len(), "truncated header at byte {i}");
            let len =
                (out[i] as usize) | ((out[i + 1] as usize) << 8) | ((out[i + 2] as usize) << 16);
            let seq = out[i + 3];
            let (start, end) = (i + 4, i + 4 + len);
            assert!(end <= out.len(), "truncated payload at byte {i}");
            packets.push((seq, out[start..end].to_vec()));
            i = end;
        }
        packets
    }

    /// 断言后端流已被完整消费:back_buf 无残留半包、底层流读尽。
    /// (残留字节会让下一条命令的响应错位——后端流整体失步)
    async fn assert_backend_drained(
        backend: &mut (impl tokio::io::AsyncRead + Unpin),
        buf: &mut BytesMut,
    ) {
        assert!(
            buf.is_empty(),
            "leftover bytes in backend buffer: {:?}",
            &buf[..]
        );
        match codec::read_packet(backend, buf).await {
            Err(ProtoError::ConnectionClosed) => {}
            other => panic!("backend stream has leftover bytes: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stmt_prepare_response_params_and_columns() {
        // SELECT a FROM t WHERE b=? AND c=? → np=2, nc=1
        // 旧式后端(BACKEND_CAP_FULL 不含 CLIENT_DEPRECATE_EOF)必然发:
        // PREPARE_OK + 2×参数定义 + EOF + 1×列定义 + EOF
        let script = [
            codec::encode_packet(0, &prepare_ok(1, 1, 2)),
            codec::encode_packet(1, &coldef("b")),
            codec::encode_packet(2, &coldef("c")),
            codec::encode_packet(3, &OLD_EOF),
            codec::encode_packet(4, &coldef("a")),
            codec::encode_packet(5, &OLD_EOF),
        ]
        .concat();
        let mut backend = Builder::new().read(&script).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();

        forward_stmt_prepare_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();

        let pkts = parse_client_packets(&client);
        // 客户端(旧式)必须看到:OK + pdef×2 + EOF + cdef + EOF,seq 从 1 连续递增
        assert_eq!(pkts.len(), 6, "client packets: {pkts:?}");
        assert_eq!(pkts[0].1, prepare_ok(1, 1, 2));
        assert_eq!(pkts[1].1, coldef("b"));
        assert_eq!(pkts[2].1, coldef("c"));
        assert_eq!(&pkts[3].1[..], &OLD_EOF);
        assert_eq!(
            pkts[4].1,
            coldef("a"),
            "column definition must reach client"
        );
        assert_eq!(&pkts[5].1[..], &OLD_EOF);
        for (i, (seq, _)) in pkts.iter().enumerate() {
            assert_eq!(*seq, (i + 1) as u8, "seq must be contiguous");
        }
        assert_backend_drained(&mut backend, &mut buf).await;
    }

    #[tokio::test]
    async fn stmt_prepare_response_columns_only() {
        // SELECT a FROM t → np=0, nc=1:PREPARE_OK + 列定义 + EOF
        let script = [
            codec::encode_packet(0, &prepare_ok(1, 1, 0)),
            codec::encode_packet(1, &coldef("a")),
            codec::encode_packet(2, &OLD_EOF),
        ]
        .concat();
        let mut backend = Builder::new().read(&script).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();

        forward_stmt_prepare_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();

        let pkts = parse_client_packets(&client);
        assert_eq!(pkts.len(), 3, "client packets: {pkts:?}");
        assert_eq!(pkts[0].1, prepare_ok(1, 1, 0));
        assert_eq!(pkts[1].1, coldef("a"));
        assert_eq!(&pkts[2].1[..], &OLD_EOF);
        assert_backend_drained(&mut backend, &mut buf).await;
    }

    #[tokio::test]
    async fn stmt_prepare_response_params_only() {
        // INSERT INTO t VALUES(?) → np=1, nc=0:PREPARE_OK + 参数定义 + EOF
        let script = [
            codec::encode_packet(0, &prepare_ok(1, 0, 1)),
            codec::encode_packet(1, &coldef("?")),
            codec::encode_packet(2, &OLD_EOF),
        ]
        .concat();
        let mut backend = Builder::new().read(&script).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();

        forward_stmt_prepare_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();

        let pkts = parse_client_packets(&client);
        assert_eq!(pkts.len(), 3, "client packets: {pkts:?}");
        assert_eq!(pkts[0].1, prepare_ok(1, 0, 1));
        assert_eq!(pkts[1].1, coldef("?"));
        assert_eq!(&pkts[2].1[..], &OLD_EOF);
        assert_backend_drained(&mut backend, &mut buf).await;
    }

    #[tokio::test]
    async fn stmt_prepare_response_err() {
        // PREPARE 出错:后端只回一个 ERR 包
        let err = error::build_error(1064, "42000", "You have an error in your SQL syntax");
        let script = codec::encode_packet(0, &err);
        let mut backend = Builder::new().read(&script).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();

        forward_stmt_prepare_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();

        let pkts = parse_client_packets(&client);
        assert_eq!(pkts.len(), 1, "client packets: {pkts:?}");
        assert_eq!(pkts[0].1, err);
        assert_backend_drained(&mut backend, &mut buf).await;
    }

    #[tokio::test]
    async fn stmt_prepare_response_missing_group_eof_rejected() {
        // 不合规后端:参数组后没有 EOF 终止包,紧跟着是下一条命令的响应包。
        // 必须显式报错(mark_broken),否则残留字节会静默错位后续命令的响应。
        let script = [
            codec::encode_packet(0, &prepare_ok(1, 0, 1)),
            codec::encode_packet(1, &coldef("?")),
            codec::encode_packet(2, &error::build_ok(0, 0, 0x0002, 0, None)),
        ]
        .concat();
        let mut backend = Builder::new().read(&script).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();

        let e = forward_stmt_prepare_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(e, ProtoError::Protocol(_)),
            "expected Protocol error, got {e:?}"
        );
    }

    #[test]
    fn response_kind_from_payload_all_branches() {
        // 空 payload → Ok(防御)
        assert_eq!(ResponseKind::from_payload(&[], true), ResponseKind::Ok);
        // 0x00 → Ok;0xFF → Err
        assert_eq!(
            ResponseKind::from_payload(&[0x00, 0x01], false),
            ResponseKind::Ok
        );
        assert_eq!(
            ResponseKind::from_payload(&[0xFF, 0x01], false),
            ResponseKind::Err
        );
        // 0xFE 长度 5-6 → Eof(无论是否结果集上下文)
        assert_eq!(
            ResponseKind::from_payload(&[0xFE, 0, 0, 2, 0], true),
            ResponseKind::Eof
        );
        assert_eq!(
            ResponseKind::from_payload(&[0xFE, 0, 0, 2, 0, 0], false),
            ResponseKind::Eof
        );
        // 0xFE 长包:结果集上下文 → ColumnCount(lenenc);否则 → Eof
        assert_eq!(
            ResponseKind::from_payload(&[0xFE, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], true),
            ResponseKind::ColumnCount(16)
        );
        assert_eq!(
            ResponseKind::from_payload(&[0xFE, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], false),
            ResponseKind::Eof
        );
        // < 0xFB:结果集 → ColumnCount;非结果集 → Ok(affected_rows)
        assert_eq!(
            ResponseKind::from_payload(&[0x03], true),
            ResponseKind::ColumnCount(3)
        );
        assert_eq!(ResponseKind::from_payload(&[0x03], false), ResponseKind::Ok);
        // >= 0xFB:结果集 → lenenc ColumnCount;非结果集 → Ok
        assert_eq!(
            ResponseKind::from_payload(&[0xFC, 0x10, 0], true),
            ResponseKind::ColumnCount(16)
        );
        assert_eq!(
            ResponseKind::from_payload(&[0xFC, 0x10, 0], false),
            ResponseKind::Ok
        );
        // 终止性判断
        assert!(ResponseKind::Ok.is_terminal());
        assert!(ResponseKind::Err.is_terminal());
        assert!(ResponseKind::Eof.is_terminal());
        assert!(!ResponseKind::ColumnCount(2).is_terminal());
    }

    #[test]
    fn lenenc_column_count_truncated() {
        // 0xFC 但长度不足 → 0
        assert_eq!(read_lenenc_column_count(&[0xFC, 0x10]), 0);
        // 0xFD 长度不足 → 0
        assert_eq!(read_lenenc_column_count(&[0xFD, 0x01, 0x02]), 0);
        // 0xFE 长度不足 → 0
        assert_eq!(read_lenenc_column_count(&[0xFE, 1, 2, 3, 4, 5, 6, 7]), 0);
        // 空 payload → 0
        assert_eq!(read_lenenc_column_count(&[]), 0);
        // 0xFD 正常
        assert_eq!(
            read_lenenc_column_count(&[0xFD, 0x01, 0x02, 0x03]),
            0x030201
        );
        // 0xFE 正常
        assert_eq!(read_lenenc_column_count(&[0xFE, 1, 0, 0, 0, 0, 0, 0, 0]), 1);
        // 普通字节
        assert_eq!(read_lenenc_column_count(&[0x05]), 5);
    }

    // ─── forward_backend_response 状态机边界 ───

    #[tokio::test]
    async fn forward_ok_first_packet() {
        // 非结果集响应:OK 包直接转发并结束
        let ok = codec::encode_packet(1, &build_ok(1, 0, 0x0002, 0, None));
        let mut backend = Builder::new().read(&ok).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();
        let (kind, code) = forward_backend_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();
        assert_eq!(kind, ResponseKind::Ok);
        assert_eq!(code, None);
        let pkts = parse_client_packets(&client);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].0, 1, "响应首包 seq 从 1 开始");
    }

    #[tokio::test]
    async fn forward_err_short_payload() {
        // ERR 包但 payload < 3 → 错误码为 None
        let err = codec::encode_packet(1, &[ERR_HEADER, 0x28]);
        let mut backend = Builder::new().read(&err).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();
        let (kind, code) = forward_backend_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();
        assert_eq!(kind, ResponseKind::Err);
        assert_eq!(code, None);
    }

    #[tokio::test]
    async fn forward_synthetic_eof_inserted() {
        // 新式后端:列定义后直接发行数据(无列后 EOF)→ 代理插入合成 EOF
        let script = [
            codec::encode_packet(1, &[1u8]), // column_count=1
            codec::encode_packet(2, &coldef("a")),
            codec::encode_packet(3, &[1u8, b'1']), // 行数据(本应列后 EOF)
            codec::encode_packet(4, &OLD_EOF),     // 行结束
        ]
        .concat();
        let mut backend = Builder::new().read(&script).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();
        forward_backend_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();
        let pkts = parse_client_packets(&client);
        assert_eq!(pkts.len(), 5, "列后应有合成 EOF: {pkts:?}");
        assert_eq!(pkts[2].1[0], 0xFE, "合成 EOF 在列定义后: {pkts:?}");
    }

    #[tokio::test]
    async fn forward_column_eof_err_rejected() {
        // 列定义阶段收到 ERR(非 EOF 非行数据)→ 协议错误
        let script = [
            codec::encode_packet(1, &[1u8]),
            codec::encode_packet(2, &coldef("a")),
            codec::encode_packet(3, &build_error(1064, "42000", "boom")),
        ]
        .concat();
        let mut backend = Builder::new().read(&script).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();
        let e = forward_backend_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(e, ProtoError::Protocol(_)), "got {e:?}");
    }

    #[tokio::test]
    async fn forward_rows_end_newstyle_ok_adapted() {
        // 行结束用新式 OK(0xFE 头,≥7B)→ 适配为旧式 5B EOF 转发
        let mut ok7 = vec![0xFEu8, 0x01, 0x02, 0x00, 0x00];
        ok7.extend_from_slice(&[0x02, 0x00]);
        let script = [
            codec::encode_packet(1, &[1u8]), // column_count=1
            codec::encode_packet(2, &coldef("a")),
            codec::encode_packet(3, &OLD_EOF), // 列后 EOF
            codec::encode_packet(4, &[1u8, b'1']),
            codec::encode_packet(5, &ok7), // 新式 OK 收尾
        ]
        .concat();
        let mut backend = Builder::new().read(&script).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();
        forward_backend_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();
        let pkts = parse_client_packets(&client);
        assert_eq!(pkts.len(), 5);
        assert_eq!(
            pkts[4].1.len(),
            5,
            "新式 OK 应适配为 5B EOF: {:?}",
            pkts[4].1
        );
    }

    // ─── forward_field_list_response 边界 ───

    #[tokio::test]
    async fn field_list_newstyle_ok_and_err() {
        // 列定义 + 新式 OK(0xFE 头,≥7B)终止
        let mut ok7 = vec![0xFEu8, 0x01, 0x02, 0x00, 0x00];
        ok7.extend_from_slice(&[0x02, 0x00]);
        let script = [
            codec::encode_packet(1, &coldef("a")),
            codec::encode_packet(2, &ok7),
        ]
        .concat();
        let mut backend = Builder::new().read(&script).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();
        forward_field_list_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();
        assert_eq!(parse_client_packets(&client).len(), 2);

        // 直接 ERR → 转发并结束
        let err = codec::encode_packet(1, &build_error(1146, "42S02", "no such table"));
        let mut backend = Builder::new().read(&err).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();
        forward_field_list_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();
        let pkts = parse_client_packets(&client);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].1[0], ERR_HEADER);
    }

    // ─── forward_simple_response 边界 ───

    #[tokio::test]
    async fn simple_response_ok_and_err() {
        // OK
        let ok = codec::encode_packet(1, &build_ok(0, 0, 0x0002, 0, None));
        let mut backend = Builder::new().read(&ok).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();
        let kind = forward_simple_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();
        assert_eq!(kind, ResponseKind::Ok);
        // ERR
        let err = codec::encode_packet(1, &build_error(1049, "42000", "unknown db"));
        let mut backend = Builder::new().read(&err).build();
        let mut client = Vec::new();
        let mut buf = BytesMut::new();
        let kind = forward_simple_response(
            &mut backend,
            &mut client,
            &mut buf,
            &mut ForwardCount::default(),
        )
        .await
        .unwrap();
        assert_eq!(kind, ResponseKind::Err);
    }
}

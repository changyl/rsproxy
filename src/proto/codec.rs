// MySQL packet 帧编解码:read_packet / send_packet / 分包重组
// T1.1 实现
//
// 对应 C 侧 tr_packet_com.h / tr_packet.c real_read → packet_len/header_read_len 状态
// 对齐 MySQL 官方 Protocol Basics:4B header(3B len LE + 1B seq) + payload
// 补齐 C 版缺失的 0xFFFFFF 分包重组

use bytes::{Buf, Bytes, BytesMut};
use std::io::IoSlice;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing;

// 全局 MySQL 包收发计数(前后端所有连接;供面板流量图展示包速率)
static PACKETS_READ_TOTAL: AtomicU64 = AtomicU64::new(0);
static PACKETS_WRITTEN_TOTAL: AtomicU64 = AtomicU64::new(0);
// 全局字节收发统计(前后端所有连接;供面板流量图展示 B/s)
static BYTES_READ_TOTAL: AtomicU64 = AtomicU64::new(0);
static BYTES_WRITTEN_TOTAL: AtomicU64 = AtomicU64::new(0);

/// 已接收的 MySQL 包总数
pub fn packets_read_total() -> u64 {
    PACKETS_READ_TOTAL.load(Ordering::Relaxed)
}

/// 已发送的 MySQL 包总数
pub fn packets_written_total() -> u64 {
    PACKETS_WRITTEN_TOTAL.load(Ordering::Relaxed)
}

/// 已接收的字节总数(header + payload)
pub fn bytes_read_total() -> u64 {
    BYTES_READ_TOTAL.load(Ordering::Relaxed)
}

/// 已发送的字节总数(header + payload)
pub fn bytes_written_total() -> u64 {
    BYTES_WRITTEN_TOTAL.load(Ordering::Relaxed)
}

use crate::proto::error::ProtoError;

/// 包头部长度(字节):3 长度 + 1 序号
pub const HEADER_LEN: usize = 4;

/// MySQL 最大包长度(不含 header):2^24 - 1 = 16_777_215
pub const MAX_PAYLOAD_LEN: usize = 0xFF_FF_FF; // 16MB

/// 读取一个完整 MySQL packet
///
/// - 读 4 字节 header → 解析 payload 长度 + seq id
/// - 读 payload(自动跨多次 partial read 拼帧)
/// - 若 payload 恰好 0xFFFFFF,递归读后续包并合并(MySQL 分包协议)
///
/// 返回 (seq_id, payload)。seq 由协议上层管理,不作为状态维护。
///
/// 对应 C 侧:tr_conn.c real_read + packet_len/header_read_len 状态机
/// 替代 ~200 行 C 手工状态机 → ~40 行 Rust
pub async fn read_packet<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut BytesMut,
) -> Result<(u8, Bytes), ProtoError> {
    tracing::trace!("read_packet: buf.len()={}", buf.len());
    // 1. 读 header(4 字节)
    ensure_bytes(r, buf, HEADER_LEN).await?;
    tracing::trace!("read_packet: got header, buf.len()={}", buf.len());
    let payload_len = (&buf[0..3]).get_uint_le(3) as usize;
    let seq_id = buf[3];
    buf.advance(HEADER_LEN);

    // 2. 读 payload
    ensure_bytes(r, buf, payload_len).await?;
    let payload = buf.split_to(payload_len).freeze();
    tracing::trace!(
        "read_packet: got payload {} bytes, seq={}",
        payload.len(),
        seq_id
    );

    // 3. 处理 >16MB 的分片包
    // MySQL 协议:payload 恰好 0xFFFFFF 表示后面还有续包
    if payload_len == MAX_PAYLOAD_LEN {
        let (_, rest) = Box::pin(read_packet(r, buf)).await?;
        let mut combined = payload.to_vec();
        combined.extend_from_slice(&rest);
        PACKETS_READ_TOTAL.fetch_add(1, Ordering::Relaxed);
        BYTES_READ_TOTAL.fetch_add((HEADER_LEN + payload_len) as u64, Ordering::Relaxed);
        return Ok((seq_id, Bytes::from(combined)));
    }

    PACKETS_READ_TOTAL.fetch_add(1, Ordering::Relaxed);
    BYTES_READ_TOTAL.fetch_add((HEADER_LEN + payload_len) as u64, Ordering::Relaxed);
    Ok((seq_id, payload))
}

/// 发送一个 MySQL packet
///
/// - 写入 4 字节 header(3B len LE + 1B seq)
/// - 写入 payload
///
/// 对应 C 侧 tr_packet_com.h 的 send_packet
pub async fn send_packet<W: AsyncWrite + Unpin>(
    w: &mut W,
    seq_id: u8,
    payload: &[u8],
) -> Result<(), ProtoError> {
    let len = payload.len();
    assert!(
        len <= MAX_PAYLOAD_LEN,
        "payload too large for single packet"
    );

    let mut header = [0u8; HEADER_LEN];
    header[0] = (len & 0xFF) as u8;
    header[1] = ((len >> 8) & 0xFF) as u8;
    header[2] = ((len >> 16) & 0xFF) as u8;
    header[3] = seq_id;

    tracing::trace!(
        "send_packet: seq={}, payload_len={}, header=[{:02x} {:02x} {:02x} {:02x}], first_payload={:02x}",
        seq_id, len, header[0], header[1], header[2], header[3],
        payload.first().unwrap_or(&0)
    );

    // header + payload 一次 vectored write 发出:每包通常只做一次 writev 系统调用,
    // 避免两次小写入(header/payload 各自一个 write)带来的额外 syscall 与
    // Nagle 滞留(第二次小写入需等首个写入的 ACK)。
    // 非 vectored writer(Vec<u8> 等)走 AsyncWrite::write_vectored 的默认实现。
    let total = header.len() + payload.len();
    let mut written = 0usize;
    while written < total {
        let bufs = remaining_slices(&header, payload, written);
        let n = w.write_vectored(&bufs).await?;
        if n == 0 {
            return Err(ProtoError::Io(std::io::Error::from(
                std::io::ErrorKind::WriteZero,
            )));
        }
        written += n;
    }
    // 去掉每包 flush():tokio TcpStream 的 flush 是立即 no-op,且连接已
    // 禁用 Nagle(TCP_NODELAY),多包连续写出无需在包间强制刷出。少一次
    // 异步 poll 调用,转发大结果集时每包省一次(实测热路径显著收益)。

    PACKETS_WRITTEN_TOTAL.fetch_add(1, Ordering::Relaxed);
    BYTES_WRITTEN_TOTAL.fetch_add((HEADER_LEN + payload.len()) as u64, Ordering::Relaxed);
    Ok(())
}

/// 转发批量化用的累积计数:把已由上层批量写出的包计入全局收发统计
/// (批量化路径不经过 send_packet,此处补齐 PACKETS/BYTES 全局计数,
/// 保证面板流量图与 /metrics 不失真)。
pub fn record_forwarded(pkts: u64, bytes: u64) {
    PACKETS_WRITTEN_TOTAL.fetch_add(pkts, Ordering::Relaxed);
    BYTES_WRITTEN_TOTAL.fetch_add(bytes, Ordering::Relaxed);
}

/// 构造 [header 剩余部分, payload 剩余部分] 的 vectored slice(跳过已写 `skip` 字节)
fn remaining_slices<'a>(header: &'a [u8], payload: &'a [u8], skip: usize) -> [IoSlice<'a>; 2] {
    if skip < header.len() {
        [IoSlice::new(&header[skip..]), IoSlice::new(payload)]
    } else {
        [
            IoSlice::new(&payload[skip - header.len()..]),
            IoSlice::new(&[]),
        ]
    }
}

/// 编码一个 MySQL packet 为字节序列(同步版本,用于需要在写入前做其他操作的场景)
///
/// 返回完整的 header + payload 字节序列
pub fn encode_packet(seq_id: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    assert!(
        len <= MAX_PAYLOAD_LEN,
        "payload too large for single packet"
    );
    let mut data = Vec::with_capacity(HEADER_LEN + len);
    data.push((len & 0xFF) as u8);
    data.push(((len >> 8) & 0xFF) as u8);
    data.push(((len >> 16) & 0xFF) as u8);
    data.push(seq_id);
    data.extend_from_slice(payload);
    data
}

/// 确保 buf 中有至少 `need` 字节(必要时跨多次 read)
async fn ensure_bytes<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut BytesMut,
    need: usize,
) -> Result<(), ProtoError> {
    buf.reserve(need);
    while buf.len() < need {
        let n = r.read_buf(buf).await?;
        if n == 0 {
            if buf.is_empty() {
                // 对端在包边界上干净关闭(FIN,无残留半包):客户端未发 COM_QUIT
                // 直接关 socket(驱动/健康检查/进程退出/LB 空闲回收)均为该形态,
                // 属正常连接结束路径——conn_task 已以 debug 记录 "connection reset"。
                // 若在此刷 ERROR 会把常规断连误报成故障,淹没真实错误。
                tracing::debug!(
                    "ensure_bytes: peer closed at packet boundary (waiting for {} bytes)",
                    need
                );
            } else {
                // 包中途 EOF:已缓冲部分字节但帧不完整,属协议层异常
                // (对端在包发送途中断开,或字节流被截断),保留 ERROR 级别。
                tracing::error!(
                    "ensure_bytes: EOF mid-packet: got {} of {} bytes",
                    buf.len(),
                    need
                );
            }
            return Err(ProtoError::ConnectionClosed);
        }
        tracing::trace!("ensure_bytes: read {} bytes, buf.len()={}", n, buf.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::io::Builder;

    /// 构建一个完整 MySQL packet 的字节序列
    fn make_packet(seq: u8, payload: &[u8]) -> Vec<u8> {
        let len = payload.len();
        let mut data = Vec::with_capacity(HEADER_LEN + len);
        data.push((len & 0xFF) as u8);
        data.push(((len >> 8) & 0xFF) as u8);
        data.push(((len >> 16) & 0xFF) as u8);
        data.push(seq);
        data.extend_from_slice(payload);
        data
    }

    #[tokio::test]
    async fn read_simple_packet() {
        let payload = b"SELECT 1";
        let data = make_packet(0, payload);
        let mut stream = Builder::new().read(&data).build();
        let mut buf = BytesMut::new();

        let (seq, pkt) = read_packet(&mut stream, &mut buf).await.unwrap();
        assert_eq!(seq, 0);
        assert_eq!(&pkt[..], payload);
    }

    #[tokio::test]
    async fn read_partial_header() {
        // header 分两次到达
        let payload = b"hello";
        let full = make_packet(1, payload);
        let mut stream = Builder::new()
            .read(&full[..2]) // 只给前 2 字节 header
            .read(&full[2..]) // 其余
            .build();
        let mut buf = BytesMut::new();

        let (seq, pkt) = read_packet(&mut stream, &mut buf).await.unwrap();
        assert_eq!(seq, 1);
        assert_eq!(&pkt[..], payload);
    }

    #[tokio::test]
    async fn read_partial_payload() {
        let payload: Vec<u8> = (0..100u8).collect();
        let full = make_packet(2, &payload);
        let (first, second) = full.split_at(HEADER_LEN + 30);
        let mut stream = Builder::new().read(first).read(second).build();
        let mut buf = BytesMut::new();

        let (seq, pkt) = read_packet(&mut stream, &mut buf).await.unwrap();
        assert_eq!(seq, 2);
        assert_eq!(&pkt[..], &payload);
    }

    #[tokio::test]
    async fn read_multiple_packets() {
        let p1 = make_packet(0, b"pkt1");
        let p2 = make_packet(1, b"pkt2");
        let all = [p1, p2].concat();
        let mut stream = Builder::new().read(&all).build();
        let mut buf = BytesMut::new();

        let (seq1, pkt1) = read_packet(&mut stream, &mut buf).await.unwrap();
        assert_eq!(seq1, 0);
        assert_eq!(&pkt1[..], b"pkt1");

        let (seq2, pkt2) = read_packet(&mut stream, &mut buf).await.unwrap();
        assert_eq!(seq2, 1);
        assert_eq!(&pkt2[..], b"pkt2");
    }

    #[tokio::test]
    async fn send_packet_roundtrip() {
        let mut output = vec![];
        let payload = b"SELECT * FROM t WHERE id = 42";
        send_packet(&mut output, 3, payload).await.unwrap();

        // 验证 send 输出的 header
        assert_eq!(output.len(), HEADER_LEN + payload.len());
        assert_eq!(&output[0..3], &(payload.len() as u32).to_le_bytes()[0..3]);
        assert_eq!(output[3], 3);

        // read 回来验证 roundtrip
        let mut stream = Builder::new().read(&output).build();
        let mut buf = BytesMut::new();
        let (seq, pkt) = read_packet(&mut stream, &mut buf).await.unwrap();
        assert_eq!(seq, 3);
        assert_eq!(&pkt[..], payload);
    }

    #[tokio::test]
    async fn eof_during_header() {
        let mut stream = Builder::new()
            .read(&[0x01, 0x00]) // 不完整的 header
            .build();
        let mut buf = BytesMut::new();
        let err = read_packet(&mut stream, &mut buf).await.unwrap_err();
        match err {
            ProtoError::ConnectionClosed => {}
            _ => panic!("expected ConnectionClosed"),
        }
    }

    #[tokio::test]
    async fn eof_at_packet_boundary() {
        // 对端在包边界干净关闭:0 字节可读即 EOF(客户端未发 COM_QUIT 直接断开)。
        // 这是正常断连路径,同样返回 ConnectionClosed,由上层按常规退出处理。
        let mut stream = Builder::new().build();
        let mut buf = BytesMut::new();
        let err = read_packet(&mut stream, &mut buf).await.unwrap_err();
        match err {
            ProtoError::ConnectionClosed => {}
            _ => panic!("expected ConnectionClosed"),
        }
        assert!(buf.is_empty(), "no partial bytes should remain");
    }

    #[tokio::test]
    async fn zero_length_payload() {
        let data = make_packet(5, b"");
        let mut stream = Builder::new().read(&data).build();
        let mut buf = BytesMut::new();

        let (seq, pkt) = read_packet(&mut stream, &mut buf).await.unwrap();
        assert_eq!(seq, 5);
        assert!(pkt.is_empty());
    }

    #[tokio::test]
    async fn max_seq_id() {
        // MySQL 序号在 0-255 循环
        let data = make_packet(255, b"max seq");
        let mut stream = Builder::new().read(&data).build();
        let mut buf = BytesMut::new();

        let (seq, pkt) = read_packet(&mut stream, &mut buf).await.unwrap();
        assert_eq!(seq, 255);
        assert_eq!(&pkt[..], b"max seq");
    }

    #[tokio::test]
    async fn read_multipart_packet() {
        // payload 恰好 0xFFFFFF(16MB)→ 续包合并
        let mut first = vec![0u8; 0xFFFFFF];
        first[0] = b'a';
        first[1] = b'b';
        first[2] = b'c';
        let mut data = Vec::with_capacity(0xFFFFFF + 10);
        // 第一包:header(len=0xFFFFFF, seq=0) + 完整 16MB payload
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0x00]);
        data.extend_from_slice(&first);
        // 第二包:header(len=2, seq=1) + 2 字节 payload
        data.extend_from_slice(&[0x02, 0x00, 0x00, 0x01, b'd', b'e']);
        let mut stream = Builder::new().read(&data).build();
        let mut buf = BytesMut::new();

        let (seq, pkt) = read_packet(&mut stream, &mut buf).await.unwrap();
        assert_eq!(seq, 0);
        assert_eq!(pkt.len(), 0xFFFFFF + 2, "续包应拼接为完整 payload");
        assert_eq!(&pkt[..3], b"abc");
        assert_eq!(&pkt[pkt.len() - 2..], b"de");
    }

    #[test]
    fn remaining_slices_both_branches() {
        let header = [1u8, 2, 3, 4];
        let payload = [9u8, 8, 7];
        // skip < header.len():剩余 header + 全部 payload
        let s = remaining_slices(&header, &payload, 1);
        assert_eq!(&s[0][..], &[2u8, 3, 4]);
        assert_eq!(&s[1][..], &payload);
        // skip >= header.len():进入 payload
        let s = remaining_slices(&header, &payload, 4);
        assert_eq!(&s[0][..], &payload);
        assert!(s[1].is_empty());
    }

    /// 每次只写 1 字节的 writer(触发 send_packet 部分写循环与 vectored 分支)
    struct OneByteWriter {
        out: Vec<u8>,
    }
    impl tokio::io::AsyncWrite for OneByteWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<Result<usize, std::io::Error>> {
            if buf.is_empty() {
                return std::task::Poll::Ready(Ok(0));
            }
            self.get_mut().out.push(buf[0]);
            std::task::Poll::Ready(Ok(1))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn send_packet_partial_writes() {
        let mut w = OneByteWriter { out: Vec::new() };
        send_packet(&mut w, 7, b"hello").await.unwrap();
        assert_eq!(w.out, encode_packet(7, b"hello"), "逐字节写应拼出完整包");
        // 全局计数器与其他并行测试共享,只做下限断言
        assert!(packets_written_total() >= 1);
        assert!(bytes_written_total() >= 9);
    }

    /// 恒返回 0 的 writer → WriteZero 错误
    struct ZeroWriter;
    impl tokio::io::AsyncWrite for ZeroWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<Result<usize, std::io::Error>> {
            std::task::Poll::Ready(Ok(0))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn send_packet_write_zero() {
        let mut w = ZeroWriter;
        let err = send_packet(&mut w, 0, b"x").await.unwrap_err();
        assert!(matches!(err, ProtoError::Io(e) if e.kind() == std::io::ErrorKind::WriteZero));
    }
}

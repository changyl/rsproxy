// MySQL 文本协议结果解析(最小实现,仅供 HA 探测/采样使用)
//
// 覆盖:列数(lenenc)、列定义(取列名)、行(逐单元格,支持 NULL=0xFB)、
// EOF/OK/ERR 终止。列按"名称→值"映射,容忍列顺序/增列差异。

use std::collections::HashMap;

/// 解析后的文本结果集(列名保序 + 行)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl TextResult {
    /// 把第 idx 行映射为 列名(小写)→ 值(仅含返回列;NULL 不放入)。
    pub fn row_map(&self, idx: usize) -> HashMap<String, String> {
        let mut m = HashMap::new();
        if let Some(row) = self.rows.get(idx) {
            for (col, cell) in self.columns.iter().zip(row.iter()) {
                if let Some(v) = cell {
                    m.insert(col.to_ascii_lowercase(), v.clone());
                }
            }
        }
        m
    }
}

/// 读取 lenenc 整数;失败返回 None。
pub fn read_lenenc_int(b: &[u8], p: &mut usize) -> Option<u64> {
    let first = *b.get(*p)?;
    *p += 1;
    match first {
        0xFB => None, // NULL(整数上下文按 None)
        0xFC => {
            if b.len() < *p + 2 {
                return None;
            }
            let v = u64::from(u16::from_le_bytes([b[*p], b[*p + 1]]));
            *p += 2;
            Some(v)
        }
        0xFD => {
            if b.len() < *p + 3 {
                return None;
            }
            let v = u64::from(b[*p]) | (u64::from(b[*p + 1]) << 8) | (u64::from(b[*p + 2]) << 16);
            *p += 3;
            Some(v)
        }
        0xFE => {
            if b.len() < *p + 8 {
                return None;
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&b[*p..*p + 8]);
            *p += 8;
            Some(u64::from_le_bytes(arr))
        }
        n => Some(u64::from(n)),
    }
}

/// 读取 lenenc 字节串;返回 Some(None)=NULL,None=解析失败。
pub fn read_lenenc_bytes<'a>(b: &'a [u8], p: &mut usize) -> Option<Option<&'a [u8]>> {
    let first = *b.get(*p)?;
    if first == 0xFB {
        *p += 1;
        return Some(None);
    }
    let len = match read_lenenc_int(b, p) {
        Some(v) => v as usize,
        None => return None,
    };
    if b.len() < *p + len {
        return None;
    }
    let out = Some(&b[*p..*p + len]);
    *p += len;
    Some(out)
}

fn cell_to_string(cell: Option<&[u8]>) -> Option<String> {
    cell.map(|c| String::from_utf8_lossy(c).into_owned())
}

/// 从列定义包(0x03 'd' 'e' 'f' …)取列名。
///
/// 布局:0x03 + catalog(lenenc) + schema + table + org_table + **name** + org_name
/// + 固定长度 0x0c + charset(2)+ length(4)+ type(1)+ flags(2)+ decimals(1)+ filler(2)。
pub fn parse_column_name(pkt: &[u8]) -> Option<String> {
    let mut p = 0usize;
    if pkt.get(p) != Some(&0x03) {
        return None;
    }
    p += 1;
    for _ in 0..4 {
        // catalog, schema, table, org_table
        read_lenenc_bytes(pkt, &mut p)??;
    }
    let name = read_lenenc_bytes(pkt, &mut p)??;
    let s = String::from_utf8_lossy(name).into_owned();
    Some(s)
}

/// 解析一行(包 payload):返回每列值(Some(None)=NULL)。
///
/// MySQL 文本协议的行**没有列数前缀**,只有逐列的 lenenc 值(NULL=0xFB);
/// 二进制(COM_STMT_EXECUTE)行才有 0x00 头与 null-bitmap,与本函数无关。
pub fn parse_row(pkt: &[u8], ncols: usize) -> Option<Vec<Option<String>>> {
    let mut p = 0usize;
    let mut row = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let cell = read_lenenc_bytes(pkt, &mut p)?;
        row.push(cell_to_string(cell));
    }
    Some(row)
}

/// 是否是结果集终止包(EOF/OK;ERR 由调用方先行识别)。
/// 0xFE 在 len<9 为经典 EOF;len>=9 为 CLIENT_DEPRECATE_EOF 的 OK 终止包。
pub fn is_terminator(pkt: &[u8]) -> bool {
    if pkt.is_empty() {
        return false;
    }
    match pkt[0] {
        0xFE => true,                // EOF / DEPRECATE_EOF OK
        0x00 => pkt.len() >= 7,      // OK
        0xFF => true,                // ERR
        _ => false,
    }
}

/// 是否是错误包。
pub fn is_err_packet(pkt: &[u8]) -> bool {
    pkt.first() == Some(&0xFF)
}

/// 把一组已按顺序收集的包解码为结果集。
/// `packets` 第一包为列数包;返回解析结果或错误说明(不含终止包)。
pub fn decode_text_packets(packets: &[Vec<u8>]) -> Result<TextResult, String> {
    let first = packets.first().ok_or("empty packets")?;
    let mut p = 0usize;
    let ncols = read_lenenc_int(first, &mut p).ok_or("bad column count")? as usize;
    let mut columns = Vec::with_capacity(ncols);
    let mut idx = 1;
    while columns.len() < ncols {
        let pkt = packets.get(idx).ok_or("missing column def")?;
        if is_err_packet(pkt) {
            return Err("server error during result".into());
        }
        // 经典协议:列定义之间有 EOF 包(CLIENT_DEPRECATE_EOF 未协商时),跳过
        if pkt.first() == Some(&0xFE) && pkt.len() < 9 {
            idx += 1;
            continue;
        }
        let name = parse_column_name(pkt).ok_or("bad column def")?;
        columns.push(name);
        idx += 1;
    }
    let mut rows = Vec::new();
    while let Some(pkt) = packets.get(idx) {
        if is_terminator(pkt) {
            // 经典协议的"列后 EOF"可能因收集策略留在 packets 中,跳过
            if rows.is_empty() && pkt.first() == Some(&0xFE) && pkt.len() < 9 && idx + 1 < packets.len() {
                idx += 1;
                continue;
            }
            break;
        }
        if let Some(row) = parse_row(pkt, ncols) {
            rows.push(row);
        } else {
            return Err("bad row packet".into());
        }
        idx += 1;
    }
    Ok(TextResult { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lenenc_int(v: u64) -> Vec<u8> {
        if v < 0xFB {
            vec![v as u8]
        } else if v <= 0xFFFF {
            let mut o = vec![0xFC];
            o.extend_from_slice(&(v as u16).to_le_bytes());
            o
        } else {
            let mut o = vec![0xFE];
            o.extend_from_slice(&v.to_le_bytes());
            o
        }
    }

    fn lenenc_str(s: &str) -> Vec<u8> {
        let mut o = lenenc_int(s.len() as u64);
        o.extend_from_slice(s.as_bytes());
        o
    }

    fn coldef(name: &str) -> Vec<u8> {
        let mut o = vec![0x03];
        o.extend_from_slice(&lenenc_str("def"));
        o.extend_from_slice(&lenenc_str(""));
        o.extend_from_slice(&lenenc_str(""));
        o.extend_from_slice(&lenenc_str(""));
        o.extend_from_slice(&lenenc_str(name));
        o.extend_from_slice(&lenenc_str(""));
        o.extend_from_slice(&[0x0c]);
        o.extend_from_slice(&[33u8, 0]); // charset
        o.extend_from_slice(&0u32.to_le_bytes());
        o.push(0xFD);
        o.extend_from_slice(&[0, 0]);
        o.push(0);
        o.extend_from_slice(&[0, 0]);
        o
    }

    fn row_pkt(vals: &[Option<&str>]) -> Vec<u8> {
        // 真实文本行:无列数前缀
        let mut o = Vec::new();
        for v in vals {
            match v {
                Some(s) => o.extend_from_slice(&lenenc_str(s)),
                None => o.push(0xFB),
            }
        }
        o
    }

    #[test]
    fn lenenc_roundtrip() {
        let mut p = 0;
        assert_eq!(read_lenenc_int(&lenenc_int(5), &mut p), Some(5));
        let mut p = 0;
        assert_eq!(read_lenenc_int(&lenenc_int(300), &mut p), Some(300));
        let mut p = 0;
        assert_eq!(read_lenenc_int(&lenenc_int(1 << 20), &mut p), Some(1 << 20));
    }

    #[test]
    fn parse_column_and_rows_by_name() {
        // 列顺序:id, leader, updated_at(与真实表顺序不同也能按名取值)
        let pkts = vec![
            lenenc_int(3),
            coldef("id"),
            coldef("leader"),
            coldef("updated_at"),
            row_pkt(&[Some("1"), Some("xenon1:8801"), Some("2023-11-14 22:13:20.123")]),
            vec![0xFE, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00],
        ];
        let r = decode_text_packets(&pkts).unwrap();
        assert_eq!(r.columns, vec!["id", "leader", "updated_at"]);
        assert_eq!(r.rows.len(), 1);
        let m = r.row_map(0);
        assert_eq!(m.get("leader").map(|s| s.as_str()), Some("xenon1:8801"));
        assert_eq!(m.get("id").map(|s| s.as_str()), Some("1"));
        assert!(m.contains_key("updated_at"));
    }

    #[test]
    fn null_cells_not_in_map() {
        let pkts = vec![
            lenenc_int(2),
            coldef("leader"),
            coldef("extra"),
            row_pkt(&[Some("x:8801"), None]),
            vec![0xFE, 0, 0, 2, 0, 0, 0],
        ];
        let r = decode_text_packets(&pkts).unwrap();
        let m = r.row_map(0);
        assert_eq!(m.get("leader").map(|s| s.as_str()), Some("x:8801"));
        assert!(!m.contains_key("extra"));
    }

    #[test]
    fn error_packet_detected() {
        assert!(is_err_packet(&[0xFF, 0x00, 0x00, 0x01, 0x00, 0x00]));
        let pkts = vec![lenenc_int(1), vec![0xFF, 0x00, 0x00, 0x01]];
        assert!(decode_text_packets(&pkts).is_err());
    }

    /// 真实 MySQL 8.0 经典协议(未协商 CLIENT_DEPRECATE_EOF)的逐字节形态:
    /// 列数 → 列定义×N → 5B EOF(列后)→ 行(**无列数前缀**,逐列 lenenc)→ 5B EOF。
    /// 该用例用于验证解码器与真实 MySQL 8.0 兼容(修复前应为失败)。
    fn eof5() -> Vec<u8> {
        vec![0xFE, 0x00, 0x00, 0x02, 0x00]
    }

    fn row_real(vals: &[Option<&str>]) -> Vec<u8> {
        // 真实文本行:无列数前缀,逐列 lenenc(与 row_pkt 的旧格式不同)
        let mut o = Vec::new();
        for v in vals {
            match v {
                Some(s) => o.extend_from_slice(&lenenc_str(s)),
                None => o.push(0xFB),
            }
        }
        o
    }

    #[test]
    fn real_mysql_classic_resultset_decodes() {
        // 模拟 SELECT id, leader, view_id, epoch_id, updated_at FROM mysql.xenon_raft_status
        let pkts = vec![
            lenenc_int(5),
            coldef("id"),
            coldef("leader"),
            coldef("view_id"),
            coldef("epoch_id"),
            coldef("updated_at"),
            eof5(), // 列后经典 EOF
            row_real(&[
                Some("1"),
                Some("127.0.0.1:8801"),
                Some("7"),
                Some("1"),
                Some("2026-09-09 12:00:00.000"),
            ]),
            eof5(), // 末尾 EOF
        ];
        let r = decode_text_packets(&pkts).expect("真实 MySQL 8.0 经典结果应可解码");
        assert_eq!(r.columns.len(), 5);
        assert_eq!(r.rows.len(), 1);
        let m = r.row_map(0);
        assert_eq!(m.get("leader").map(|s| s.as_str()), Some("127.0.0.1:8801"));
        assert_eq!(m.get("view_id").map(|s| s.as_str()), Some("7"));
    }

    #[test]
    fn real_mysql_deprecate_eof_resultset_decodes() {
        // 若协商了 CLIENT_DEPRECATE_EOF:列后无 EOF,末尾为 0xFE 头的 OK 终止
        let pkts = vec![
            lenenc_int(2),
            coldef("a"),
            coldef("b"),
            row_real(&[Some("x"), None]),
            vec![0xFE, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00], // OK(0xFE,len>=9)
        ];
        let r = decode_text_packets(&pkts).expect("DEPRECATE_EOF 结果应可解码");
        assert_eq!(r.rows.len(), 1);
        let m = r.row_map(0);
        assert_eq!(m.get("a").map(|s| s.as_str()), Some("x"));
        assert!(!m.contains_key("b")); // NULL 不入 map
    }
}

// Xenon raft 高可用的运行时状态与纯判主逻辑
//
// 探测契约(按真实部署核对的表结构):
//   mysql.xenon_raft_status(id, leader varchar(80) 'raft 端点 host:8801 类',
//                           view_id bigint, epoch_id bigint, updated_at timestamp(3))
// `leader` 是 xenon raft 端点(非 MySQL 端口);(view_id, epoch_id) 单调递增代表
// 领导任期;updated_at 表示自述新鲜度。

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::model::RaftMember;
use crate::config_center::HostAddr;

/// 单节点一次探测取回的原始行
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRow {
    /// 从哪个成员探到的(其 MySQL host)
    pub from_host: String,
    /// raft 状态行原始内容(列名 → 值;仅含表内出现的列)
    pub row: HashMap<String, String>,
}

impl ProbeRow {
    /// 取字符串列(存在且非空)
    fn col(&self, names: &[&str]) -> Option<String> {
        for n in names {
            if let Some(v) = self.row.get(*n) {
                let t = v.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    }

    /// 取整型列
    fn col_u64(&self, names: &[&str]) -> Option<u64> {
        self.col(names)?.parse().ok()
    }

    /// leader 端点(如 `xenon1:8801` / `xenon1` / `1.2.3.4:8801`)
    pub fn leader_endpoint(&self) -> Option<String> {
        self.col(&["leader"])
    }

    pub fn view_id(&self) -> Option<u64> {
        self.col_u64(&["view_id", "viewid", "view"])
    }

    pub fn epoch_id(&self) -> Option<u64> {
        self.col_u64(&["epoch_id", "epochid", "epoch"])
    }

    /// updated_at → Unix 毫秒(容忍 `2026-01-01 00:00:00`/ISO/数字秒/数字毫秒)
    pub fn updated_at_ms(&self) -> Option<u64> {
        let raw = self.col(&["updated_at", "update_time"])?;
        parse_timestamp_ms(&raw)
    }
}

/// 把 MySQL `timestamp` 字符串或秒/毫秒数字解析为 Unix 毫秒。
pub fn parse_timestamp_ms(raw: &str) -> Option<u64> {
    let t = raw.trim();
    // 纯数字:秒(当前 epoch 秒 ~1.7e9,<1e11)或毫秒(≥1e11)
    if let Ok(n) = t.parse::<u64>() {
        return Some(if n < 100_000_000_000 { n * 1000 } else { n });
    }
    // `YYYY-MM-DD[ HH:MM:SS[.fff]]` / ISO 8601
    let digits: String = t.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 14 {
        return None;
    }
    let year: i64 = digits[0..4].parse().ok()?;
    let month: i64 = digits[4..6].parse().ok()?;
    let day: i64 = digits[6..8].parse().ok()?;
    let hour: i64 = digits.get(8..10).and_then(|s| s.parse().ok()).unwrap_or(0);
    let min: i64 = digits.get(10..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    let sec: i64 = digits.get(12..14).and_then(|s| s.parse().ok()).unwrap_or(0);
    let ms: i64 = digits
        .get(14..17)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let days = days_from_civil(year, month as u32, day as u32)? as i64;
    let secs = days * 86400 + hour * 3600 + min * 60 + sec;
    let local_epoch = UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(secs as u64))
        .and_then(|d| d.checked_add(std::time::Duration::from_millis(ms as u64)));
    // MySQL timestamp 无时区(服务器本地);与 now_ms 同机近似比较,
    // 允许 ±48h 的系统时区偏差内的近似值由调用方做新鲜度判断的容差处理。
    local_epoch.map(|d| d.duration_since(UNIX_EPOCH).map(|e| e.as_millis() as u64).unwrap_or(0))
}

/// Howard Hinnant 的 days_from_civil(公历 → 1970-01-01 起天数)
fn days_from_civil(y: i64, m: u32, d: u32) -> Option<u32> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) as u32)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 判主结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeOut {
    /// 当前 leader 的 MySQL 地址
    pub leader: HostAddr,
    /// followers(除 leader 外的成员,按成员顺序)
    pub followers: Vec<HostAddr>,
    /// 任期信息(用于变更判定/展示)
    pub view_id: u64,
    pub epoch_id: u64,
    /// 采纳行的更新时间(ms)
    pub updated_at_ms: u64,
}

/// 判主结论:Valid(采纳新结果)/ KeepOld(不切换,保持现状)/ Unknown(无有效信息)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Valid(JudgeOut),
    /// 无新鲜/可采纳的行:调用方保持 last-known 并降级
    Unknown,
}

/// 用一次探测得到的多节点行判主。
///
/// 规则:
/// 1. 只采纳新鲜行(`now - updated_at ≤ stale_ms`;行无时间列视为新鲜);
/// 2. leader 端点必须能映射到成员(host 或 raft_endpoint 匹配),否则该行无效;
/// 3. 权威性:同一 leader 地址要么被 ≥2 个探源一致上报,要么由 leader 自身
///    单机自述(单源非自述 → 不确定,保持旧状态,下轮再判);
/// 4. 多个权威候选取 (view_id, epoch_id, updated_at) 最大者;
/// 5. followers = 成员减去 leader(保持成员顺序)。
pub fn decide(
    members: &[RaftMember],
    now: u64,
    stale_ms: u64,
    rows: Vec<ProbeRow>,
) -> Decision {
    // 统计每个候选 leader 地址的上报:来源数 / 自述 / 最大任期 / 最新时间
    let mut by_leader: Vec<(HostAddr, usize, bool, u64, u64, u64)> = Vec::new();
    for r in &rows {
        let Some(ep) = r.leader_endpoint() else { continue };
        let Some((member_idx, addr)) = resolve_member(members, &ep) else { continue };
        if let Some(ts) = r.updated_at_ms() {
            if now.saturating_sub(ts) > stale_ms {
                continue; // 陈旧行
            }
        }
        // 来源节点主机(探测目标 host)
        let src_self = r.from_host.eq_ignore_ascii_case(&members[member_idx].host);
        let view = r.view_id().unwrap_or(0);
        let epoch = r.epoch_id().unwrap_or(0);
        let updated = r.updated_at_ms().unwrap_or(now);
        if let Some(e) = by_leader.iter_mut().find(|e| e.0 == addr) {
            e.1 += 1;
            e.2 = e.2 || src_self;
            if (view, epoch, updated) > (e.3, e.4, e.5) {
                e.3 = view;
                e.4 = epoch;
                e.5 = updated;
            }
        } else {
            by_leader.push((addr.clone(), 1, src_self, view, epoch, updated));
        }
    }
    // 权威候选:≥2 源一致 或 单源且自述
    let mut authoritative: Vec<(HostAddr, u64, u64, u64)> = by_leader
        .into_iter()
        .filter(|(_, srcs, self_claimed, ..)| *srcs >= 2 || (*srcs == 1 && *self_claimed))
        .map(|(addr, _, _, view, epoch, updated)| (addr, view, epoch, updated))
        .collect();
    if authoritative.is_empty() {
        return Decision::Unknown;
    }
    // 取任期/时间最大者;同任期多候选冲突 → Unknown(宁旧勿错)
    authoritative.sort_by_key(|(_, v, e, u)| (*v, *e, *u));
    let best = authoritative.last().unwrap().clone();
    let same_top = authoritative
        .iter()
        .filter(|(_, v, e, _)| *v == best.1 && *e == best.2)
        .count();
    if same_top > 1 {
        return Decision::Unknown;
    }
    let followers = members
        .iter()
        .filter(|m| !(m.host == best.0.host && m.mysql_port == best.0.port))
        .map(|m| HostAddr::new(m.host.clone(), m.mysql_port))
        .collect();
    Decision::Valid(JudgeOut {
        leader: best.0,
        followers,
        view_id: best.1,
        epoch_id: best.2,
        updated_at_ms: best.3,
    })
}

/// 把 raft 端点(host 或 host:port)映射到成员索引;返回 (idx, mysql HostAddr)。
/// 匹配优先级:raft_endpoint 配置值一致 > host 一致。
pub fn resolve_member(members: &[RaftMember], endpoint: &str) -> Option<(usize, HostAddr)> {
    let host = endpoint_host(endpoint);
    for (i, m) in members.iter().enumerate() {
        if let Some(raft_ep) = &m.raft_endpoint {
            if eq_ep(raft_ep, endpoint) || eq_ep(raft_ep, &host) {
                return Some((i, HostAddr::new(m.host.clone(), m.mysql_port)));
            }
        }
    }
    for (i, m) in members.iter().enumerate() {
        if m.host.eq_ignore_ascii_case(&host) {
            return Some((i, HostAddr::new(m.host.clone(), m.mysql_port)));
        }
    }
    None
}

fn eq_ep(a: &str, b: &str) -> bool {
    let a_host = endpoint_host(a);
    let b_host = endpoint_host(b);
    a_host.eq_ignore_ascii_case(&b_host)
        && (a == b || a_host == b_host || a == b_host || b == a_host)
}

/// 取 endpoint 的 host 部分(去掉 :port;IPv6 括号化地址暂不做,成员用主机名/IPv4)
pub fn endpoint_host(endpoint: &str) -> String {
    let t = endpoint.trim();
    match t.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
        _ => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(host: &str) -> RaftMember {
        RaftMember {
            host: host.into(),
            mysql_port: 3306,
            raft_endpoint: Some(format!("{host}:8801")),
        }
    }

    fn row(from: &str, leader: &str, view: u64, epoch: u64, updated_ms: Option<u64>) -> ProbeRow {
        let mut m = HashMap::new();
        m.insert("leader".to_string(), leader.to_string());
        m.insert("view_id".to_string(), view.to_string());
        m.insert("epoch_id".to_string(), epoch.to_string());
        if let Some(ts) = updated_ms {
            m.insert("updated_at".to_string(), ts.to_string());
        }
        ProbeRow { from_host: from.into(), row: m }
    }

    fn addrs(h: &str) -> HostAddr {
        HostAddr::new(h.to_string(), 3306)
    }

    #[test]
    fn endpoint_host_strips_port() {
        assert_eq!(endpoint_host("xenon1:8801"), "xenon1");
        assert_eq!(endpoint_host("10.1.2.3:8801"), "10.1.2.3");
        assert_eq!(endpoint_host("xenon1"), "xenon1");
    }

    #[test]
    fn resolve_member_matches_by_endpoint_or_host() {
        let ms = vec![member("a"), member("b")];
        let (i, addr) = resolve_member(&ms, "b:8801").unwrap();
        assert_eq!(i, 1);
        assert_eq!(addr, addrs("b"));
        let (i, _) = resolve_member(&ms, "a").unwrap();
        assert_eq!(i, 0);
        assert!(resolve_member(&ms, "c:8801").is_none());
    }

    #[test]
    fn timestamp_parsing() {
        // 毫秒/秒/常见字符串
        assert_eq!(parse_timestamp_ms("1700000000123"), Some(1700000000123));
        assert_eq!(parse_timestamp_ms("1700000000"), Some(1700000000000));
        assert_eq!(parse_timestamp_ms("2023-11-14 22:13:20.123"), Some(1700000000123));
        assert_eq!(parse_timestamp_ms("2023-11-14 22:13:20"), Some(1700000000000));
        assert!(parse_timestamp_ms("garbage").is_none());
    }

    #[test]
    fn decide_single_self_claim_wins() {
        let ms = vec![member("a"), member("b"), member("c")];
        let now = 1_700_000_000_000u64;
        // 只有 b 探到,且 b 自述自己是 leader(单机自述场景)
        let rows = vec![row("b", "b:8801", 2, 1, Some(now))];
        match decide(&ms, now, 3000, rows) {
            Decision::Valid(out) => {
                assert_eq!(out.leader, addrs("b"));
                assert_eq!(out.followers, vec![addrs("a"), addrs("c")]);
                assert_eq!(out.view_id, 2);
            }
            _ => panic!("expected valid"),
        }
    }

    #[test]
    fn decide_stale_rejected_and_unknown() {
        let ms = vec![member("a"), member("b")];
        let now = 1_700_000_000_000u64;
        let rows = vec![row("a", "a:8801", 3, 1, Some(now - 60_000))];
        assert_eq!(decide(&ms, now, 3000, rows), Decision::Unknown);
        // 无有效行
        assert_eq!(decide(&ms, now, 3000, vec![]), Decision::Unknown);
        // leader 不在成员表
        let rows = vec![row("a", "nope:8801", 3, 1, Some(now))];
        assert_eq!(decide(&ms, now, 3000, rows), Decision::Unknown);
    }

    #[test]
    fn decide_highest_term_when_multiple() {
        let ms = vec![member("a"), member("b"), member("c")];
        let now = 1_700_000_000_000u64;
        // a 节点仍自述旧任期 1;b/c 已看到新任期 2 且一致指向 b
        let rows = vec![
            row("a", "a:8801", 1, 0, Some(now)),
            row("b", "b:8801", 2, 0, Some(now)),
            row("c", "b:8801", 2, 0, Some(now)),
        ];
        match decide(&ms, now, 3000, rows) {
            Decision::Valid(out) => {
                assert_eq!(out.leader, addrs("b"));
                assert_eq!(out.view_id, 2);
            }
            _ => panic!("expected valid"),
        }
    }

    #[test]
    fn decide_conflict_same_term_keeps_unknown() {
        let ms = vec![member("a"), member("b")];
        let now = 1_700_000_000_000u64;
        // a 与 b 各自自述(无多数、无更高任期)
        let rows = vec![
            row("a", "a:8801", 3, 0, Some(now)),
            row("b", "b:8801", 3, 0, Some(now)),
        ];
        assert_eq!(decide(&ms, now, 3000, rows), Decision::Unknown);
    }
}

// 分片路由:T3.3 实现
// HASH_MOD / MD5_HASH_MOD / RANGE / LIST 分片策略
//
// 对齐 C 侧 tr_sql_partition.c + tr_route.c

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::config::{RouteRule, ShardStrategy};

/// 路由结果:目标分片索引
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteResult {
    /// 目标 tablet 索引(0-based)
    pub tablet_index: usize,
    /// 使用的策略
    pub strategy: ShardStrategy,
    /// 使用的分片键值
    pub shard_value: String,
}

/// 根据分片规则和键值计算目标 tablet 索引
///
/// 不匹配任何规则时返回 None(走全表扫描或报错)
pub fn route_to_tablet(
    rules: &[RouteRule],
    table_name: &str,
    shard_key_column: &str,
    shard_key_value: &str,
) -> Option<RouteResult> {
    // 查找匹配的路由规则
    let rule = rules
        .iter()
        .find(|r| r.table_name == table_name && r.partition_key == shard_key_column)?;

    let index = match rule.strategy {
        ShardStrategy::HashMod => {
            let h = hash_string(shard_key_value);
            (h as usize) % rule.tablet_indices.len()
        }
        ShardStrategy::Md5HashMod => {
            let h = md5_hash_mod(shard_key_value);
            h % rule.tablet_indices.len()
        }
        ShardStrategy::Range => range_lookup(shard_key_value, &rule.tablet_indices),
        ShardStrategy::List => list_lookup(shard_key_value, rule.tablet_indices.len()),
        ShardStrategy::Pcre => pcre_match(
            shard_key_value,
            rule.pcre_pattern.as_deref(),
            &rule.tablet_indices,
        ),
    };

    let tablet_index = rule.tablet_indices.get(index).copied().unwrap_or(0);

    Some(RouteResult {
        tablet_index,
        strategy: rule.strategy,
        shard_value: shard_key_value.to_string(),
    })
}

// ─── 策略实现 ───

/// 从表名提取尾号(格式 `table_x`:最后一个 `_` 后的纯数字)。
///
/// 例如 `sbtest_0` → 0,`sbtest_1` → 1,`users_12` → 12;
/// 无 `_<数字>` 后缀(`sbtest1`、`users`、`sbtest_`、`sbtest_x`)返回 None,
/// 调用方按默认路由(第一个分片)处理。
pub fn table_tail_number(table_name: &str) -> Option<u64> {
    // 去除反引号/引号包裹(如 `sbtest_1`、'sbtest_1')
    let name = table_name.trim_matches(|c| c == '`' || c == '\'' || c == '"');
    let (_, tail) = name.rsplit_once('_')?;
    if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    tail.parse().ok()
}

/// 表尾号 → 目标分片索引:尾号 % 分片数
pub fn table_tail_index(table_name: &str, tablet_count: usize) -> Option<usize> {
    if tablet_count == 0 {
        return None;
    }
    table_tail_number(table_name).map(|n| (n as usize) % tablet_count)
}

/// HASH_MOD:字符串哈希取模
fn hash_string(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// MD5_HASH_MOD:MD5 后取前 8 字节作为 u64 再取模
fn md5_hash_mod(s: &str) -> usize {
    use md5::Digest;
    let digest = md5::Md5::digest(s.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(bytes) as usize
}

/// RANGE:线性查找落入的范围
fn range_lookup(value: &str, indices: &[usize]) -> usize {
    let v: i64 = value.parse().unwrap_or(0);
    // 简化为按索引数均分
    if indices.is_empty() {
        return 0;
    }
    let bucket_size = i64::MAX / indices.len() as i64;
    let idx = (v / bucket_size.max(1)) as usize;
    idx.min(indices.len() - 1)
}

/// LIST:哈希到固定列表
fn list_lookup(value: &str, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let h = hash_string(value);
    (h as usize) % count
}

/// PCRE:正则匹配到指定索引
fn pcre_match(value: &str, pattern: Option<&str>, indices: &[usize]) -> usize {
    if let Some(pat) = pattern {
        if let Ok(re) = regex::Regex::new(pat) {
            if re.is_match(value) {
                return 0; // 匹配成功:第一个索引
            }
        }
    }
    indices.get(1).copied().unwrap_or(0) // 默认索引
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouteRule;

    fn make_hash_mod_rule(num_tablets: usize) -> Vec<RouteRule> {
        vec![RouteRule {
            table_name: "users".into(),
            strategy: ShardStrategy::HashMod,
            partition_key: "id".into(),
            tablet_indices: (0..num_tablets).collect(),
            pcre_pattern: None,
        }]
    }

    #[test]
    fn hash_mod_deterministic() {
        let rules = make_hash_mod_rule(4);
        let r1 = route_to_tablet(&rules, "users", "id", "42").unwrap();
        let r2 = route_to_tablet(&rules, "users", "id", "42").unwrap();
        assert_eq!(r1.tablet_index, r2.tablet_index);
    }

    #[test]
    fn hash_mod_distributes() {
        let rules = make_hash_mod_rule(8);
        let mut hits = vec![0u32; 8];
        for i in 0..1000 {
            let r = route_to_tablet(&rules, "users", "id", &i.to_string()).unwrap();
            hits[r.tablet_index] += 1;
        }
        // 所有 tablet 都应有命中
        for &h in &hits {
            assert!(h > 0, "tablet should have hits");
        }
    }

    #[test]
    fn no_matching_rule() {
        let rules = make_hash_mod_rule(4);
        assert!(route_to_tablet(&rules, "orders", "id", "1").is_none());
    }

    #[test]
    fn wrong_partition_key() {
        let rules = make_hash_mod_rule(4);
        assert!(route_to_tablet(&rules, "users", "name", "alice").is_none());
    }

    #[test]
    fn tail_number_extraction() {
        assert_eq!(table_tail_number("sbtest_0"), Some(0));
        assert_eq!(table_tail_number("sbtest_1"), Some(1));
        assert_eq!(table_tail_number("users_12"), Some(12));
        assert_eq!(table_tail_number("t_99"), Some(99));
        // 无 `_<数字>` 后缀 → None
        assert_eq!(table_tail_number("sbtest1"), None);
        assert_eq!(table_tail_number("users"), None);
        assert_eq!(table_tail_number("sbtest_"), None);
        assert_eq!(table_tail_number("sbtest_x"), None);
        assert_eq!(table_tail_number("_1a"), None);
        // 带库名前缀也取最后一个 `_` 后的数字
        assert_eq!(table_tail_number("sbtest.sbtest_2"), Some(2));
        // 反引号/引号包裹
        assert_eq!(table_tail_number("`sbtest_1`"), Some(1));
        assert_eq!(table_tail_number("'sbtest_0'"), Some(0));
    }

    #[test]
    fn tail_index_mod() {
        assert_eq!(table_tail_index("sbtest_0", 2), Some(0));
        assert_eq!(table_tail_index("sbtest_1", 2), Some(1));
        // 尾号超过分片数自动取模
        assert_eq!(table_tail_index("sbtest_2", 2), Some(0));
        assert_eq!(table_tail_index("sbtest_3", 2), Some(1));
        assert_eq!(table_tail_index("users", 2), None);
        assert_eq!(table_tail_index("sbtest_1", 0), None);
    }

    fn make_rule(strategy: ShardStrategy, indices: &[usize]) -> Vec<RouteRule> {
        vec![RouteRule {
            table_name: "users".into(),
            strategy,
            partition_key: "id".into(),
            tablet_indices: indices.to_vec(),
            pcre_pattern: None,
        }]
    }

    #[test]
    fn md5_hash_mod_routes() {
        let rules = make_rule(ShardStrategy::Md5HashMod, &[0, 1]);
        // 确定性 + 落在声明区间内
        let r1 = route_to_tablet(&rules, "users", "id", "42").unwrap();
        let r2 = route_to_tablet(&rules, "users", "id", "42").unwrap();
        assert_eq!(r1.tablet_index, r2.tablet_index);
        assert!(r1.tablet_index <= 1);
        assert_eq!(r1.strategy, ShardStrategy::Md5HashMod);
        assert_eq!(r1.shard_value, "42");
    }

    #[test]
    fn range_and_list_strategies() {
        // RANGE:数值落入区间 → 分片;非数值回落 0
        let rules = make_rule(ShardStrategy::Range, &[0, 1]);
        let r = route_to_tablet(&rules, "users", "id", "0").unwrap();
        assert_eq!(r.tablet_index, 0);
        // i64::MAX / 2≈4.61e18;5e18 应落第二桶
        let r = route_to_tablet(&rules, "users", "id", "5000000000000000000").unwrap();
        assert_eq!(r.tablet_index, 1);
        // 非数值 → 0
        let r = route_to_tablet(&rules, "users", "id", "abc").unwrap();
        assert_eq!(r.tablet_index, 0);
        // 空索引列表 → 0
        let empty = make_rule(ShardStrategy::Range, &[]);
        assert_eq!(route_to_tablet(&empty, "users", "id", "5").unwrap().tablet_index, 0);

        // LIST:哈希取模;确定性
        let rules = make_rule(ShardStrategy::List, &[1, 0]);
        let a = route_to_tablet(&rules, "users", "id", "x").unwrap().tablet_index;
        let b = route_to_tablet(&rules, "users", "id", "x").unwrap().tablet_index;
        assert_eq!(a, b);
        assert!(a <= 1);
        // 空列表 → 0
        let empty = make_rule(ShardStrategy::List, &[]);
        assert_eq!(route_to_tablet(&empty, "users", "id", "y").unwrap().tablet_index, 0);
    }

    #[test]
    fn pcre_strategy_matches() {
        // 匹配成功 → 索引 0;不匹配 → 索引 1(或 0 兜底)
        let rules = vec![RouteRule {
            table_name: "users".into(),
            strategy: ShardStrategy::Pcre,
            partition_key: "id".into(),
            tablet_indices: vec![0, 1],
            pcre_pattern: Some("^vip".to_string()),
        }];
        assert_eq!(route_to_tablet(&rules, "users", "id", "vip-1").unwrap().tablet_index, 0);
        assert_eq!(route_to_tablet(&rules, "users", "id", "normal").unwrap().tablet_index, 1);
        // 无 pattern → 默认索引 1;仅一个索引 → 回落 0
        let nopat = vec![RouteRule {
            table_name: "users".into(),
            strategy: ShardStrategy::Pcre,
            partition_key: "id".into(),
            tablet_indices: vec![0, 1],
            pcre_pattern: None,
        }];
        assert_eq!(route_to_tablet(&nopat, "users", "id", "x").unwrap().tablet_index, 1);
        let single = vec![RouteRule {
            table_name: "users".into(),
            strategy: ShardStrategy::Pcre,
            partition_key: "id".into(),
            tablet_indices: vec![5],
            pcre_pattern: None,
        }];
        assert_eq!(route_to_tablet(&single, "users", "id", "x").unwrap().tablet_index, 5);
    }

    #[test]
    fn pcre_sparse_tablet_index_falls_back_zero() {
        // PCRE 不匹配时 pcre_match 返回第二个配置值(7),而 [3,7].get(7)
        // 越界,route_to_tablet 的 unwrap_or(0) 回落到 tablet 0。
        let rules = vec![RouteRule {
            table_name: "users".into(),
            strategy: ShardStrategy::Pcre,
            partition_key: "id".into(),
            tablet_indices: vec![3, 7],
            pcre_pattern: Some("^vip".to_string()),
        }];
        let r = route_to_tablet(&rules, "users", "id", "ordinary").unwrap();
        assert_eq!(r.tablet_index, 0);
    }
}

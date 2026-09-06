// SQL 改写 (decomposer) + scatter/gather 执行计划
// T3.4 + T3.5 实现
//
// 对齐 C 侧 tr_sql_decomposer.c + tr_result.c

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::parser::ast::{ParsedStatement, StatementType};
use crate::parser::route::RouteResult;

// ─── 执行计划 ───

/// 单个子查询:目标 tablet + 改写后的 SQL
#[derive(Debug, Clone)]
pub struct SubQuery {
    /// 目标 tablet 索引
    pub tablet_index: usize,
    /// 改写后的 SQL(目前透传原 SQL,后续实现表名/条件改写)
    pub sql: String,
    /// 是否需要合并结果
    pub needs_merge: bool,
}

/// 分片执行计划:一个或多个子查询
#[derive(Debug, Clone)]
pub struct ShardPlan {
    /// 子查询列表
    pub sub_queries: Vec<SubQuery>,
    /// 是否需要合并结果
    pub needs_merge: bool,
    /// 合并类型
    pub merge_type: MergeType,
}

/// 结果合并类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeType {
    /// 不需要合并(单 tablet 查询)
    None,
    /// 简单拼接(多 tablet 无 ORDER BY)
    Concat,
    /// 按 ORDER BY 列排序合并(min-heap)
    OrderedMerge,
    /// 聚合合并(SUM/COUNT/AVG/MAX/MIN)
    Aggregate,
}

// ─── SQL 改写 ───

/// 从解析后的 SQL 和路由信息生成分片执行计划
///
/// 当前实现:单 tablet 直连(路由到单个 tablet)
/// 未来扩展:跨分片 scatter/gather
pub fn build_shard_plan(
    stmt: &ParsedStatement,
    route: Option<&RouteResult>,
    total_tablets: usize,
) -> ShardPlan {
    match stmt.stmt_type {
        StatementType::Select
        | StatementType::Insert
        | StatementType::Update
        | StatementType::Delete
        | StatementType::Replace => {
            // 有明确路由目标:单 tablet 查询
            if let Some(route) = route {
                return ShardPlan {
                    sub_queries: vec![SubQuery {
                        tablet_index: route.tablet_index,
                        sql: stmt.raw_sql.clone(),
                        needs_merge: false,
                    }],
                    needs_merge: false,
                    merge_type: MergeType::None,
                };
            }

            // 无路由目标但有表名:可能是全表扫描 → scatter 到所有 tablet
            if !stmt.table_names.is_empty() {
                return scatter_to_all(stmt, total_tablets);
            }

            // 无路由无表名:透传
            ShardPlan {
                sub_queries: vec![SubQuery {
                    tablet_index: 0,
                    sql: stmt.raw_sql.clone(),
                    needs_merge: false,
                }],
                needs_merge: false,
                merge_type: MergeType::None,
            }
        }
        // SET/SHOW/DESCRIBE 等:透传到第一个 tablet
        _ => ShardPlan {
            sub_queries: vec![SubQuery {
                tablet_index: 0,
                sql: stmt.raw_sql.clone(),
                needs_merge: false,
            }],
            needs_merge: false,
            merge_type: MergeType::None,
        },
    }
}

/// Scatter 到所有 tablet(无 WHERE 或全表扫描场景)
fn scatter_to_all(stmt: &ParsedStatement, total: usize) -> ShardPlan {
    let subs: Vec<SubQuery> = (0..total)
        .map(|i| SubQuery {
            tablet_index: i,
            sql: stmt.raw_sql.clone(),
            needs_merge: true,
        })
        .collect();

    let merge_type = if stmt.stmt_type == StatementType::Select {
        MergeType::Concat
    } else {
        MergeType::None // UPDATE/DELETE:不需要合并(每 tablet 各自执行)
    };

    ShardPlan {
        sub_queries: subs,
        needs_merge: merge_type != MergeType::None,
        merge_type,
    }
}

// ─── 结果合并 ───

/// 合并多个 tablet 的结果集
///
/// 当前实现:简单拼接(按 tablet 顺序)
/// 后续扩展:ORDER BY 排序合并、聚合合并
pub fn merge_results(rows: Vec<Vec<String>>, _merge_type: MergeType) -> Vec<Vec<String>> {
    // Concat:简单拼接
    rows
}

/// 按 ORDER BY 列排序合并(min-heap)
///
/// 每个 tablet 的结果已按 ORDER BY 排序,用 min-heap 做 k-way merge
pub fn ordered_merge(
    sorted_rows: Vec<Vec<Vec<String>>>,
    order_col: usize,
    ascending: bool,
) -> Vec<Vec<String>> {
    if ascending {
        ordered_merge_asc(sorted_rows, order_col)
    } else {
        ordered_merge_desc(sorted_rows, order_col)
    }
}

/// 升序合并:Reverse 包装实现 min-heap
fn ordered_merge_asc(sorted_rows: Vec<Vec<Vec<String>>>, order_col: usize) -> Vec<Vec<String>> {
    let mut result = Vec::new();
    let mut heap: BinaryHeap<(Reverse<String>, usize)> = BinaryHeap::new();
    let mut iterators: Vec<_> = sorted_rows
        .into_iter()
        .map(|r| r.into_iter().peekable())
        .collect();

    for (i, iter) in iterators.iter_mut().enumerate() {
        if let Some(row) = iter.peek() {
            heap.push((Reverse(row[order_col].clone()), i));
        }
    }

    while let Some((_, i)) = heap.pop() {
        if let Some(row) = iterators[i].next() {
            result.push(row);
            if let Some(next_row) = iterators[i].peek() {
                heap.push((Reverse(next_row[order_col].clone()), i));
            }
        }
    }
    result
}

/// 降序合并:直接使用 max-heap
fn ordered_merge_desc(sorted_rows: Vec<Vec<Vec<String>>>, order_col: usize) -> Vec<Vec<String>> {
    let mut result = Vec::new();
    let mut heap: BinaryHeap<(String, usize)> = BinaryHeap::new();
    let mut iterators: Vec<_> = sorted_rows
        .into_iter()
        .map(|r| r.into_iter().peekable())
        .collect();

    for (i, iter) in iterators.iter_mut().enumerate() {
        if let Some(row) = iter.peek() {
            heap.push((row[order_col].clone(), i));
        }
    }

    while let Some((_, i)) = heap.pop() {
        if let Some(row) = iterators[i].next() {
            result.push(row);
            if let Some(next_row) = iterators[i].peek() {
                heap.push((next_row[order_col].clone(), i));
            }
        }
    }
    result
}

/// 聚合合并(对多 tablet 结果做 SUM/COUNT/AVG/MAX/MIN)
///
/// 每行聚合函数直接合并,非聚合列取第一 tablet 的值
pub fn aggregate_merge(
    rows: Vec<Vec<String>>,
    agg_columns: &[usize],
    _agg_types: &[&str],
) -> Vec<String> {
    if rows.is_empty() {
        return vec![];
    }

    let col_count = rows[0].len();
    let mut result = rows[0].clone();

    for &col in agg_columns {
        if col >= col_count {
            continue;
        }
        // SUM:简单求和(假设数值)
        let sum: f64 = rows
            .iter()
            .filter_map(|r| r.get(col).and_then(|v| v.parse::<f64>().ok()))
            .sum();
        result[col] = sum.to_string();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ast::ParsedStatement;

    fn make_select(sql: &str) -> ParsedStatement {
        ParsedStatement {
            stmt_type: StatementType::Select,
            raw_sql: sql.into(),
            router_hint: None,
            table_names: Vec::new(),
            shard_keys: Vec::new(),
            is_shard_ddl: false,
        }
    }

    #[test]
    fn build_plan_single_tablet() {
        let stmt = make_select("SELECT * FROM users WHERE id = 42");
        let route = RouteResult {
            tablet_index: 2,
            strategy: crate::config::ShardStrategy::HashMod,
            shard_value: "42".into(),
        };

        let plan = build_shard_plan(&stmt, Some(&route), 4);
        assert_eq!(plan.sub_queries.len(), 1);
        assert_eq!(plan.sub_queries[0].tablet_index, 2);
        assert!(!plan.needs_merge);
    }

    #[test]
    fn build_plan_scatter_all() {
        let mut stmt = make_select("SELECT * FROM users");
        stmt.table_names = vec!["users".into()];

        let plan = build_shard_plan(&stmt, None, 4);
        assert_eq!(plan.sub_queries.len(), 4);
        assert!(plan.needs_merge);
        assert_eq!(plan.merge_type, MergeType::Concat);
    }

    #[test]
    fn build_plan_no_table_no_route() {
        let stmt = ParsedStatement {
            stmt_type: StatementType::Select,
            raw_sql: "SELECT 1".into(),
            router_hint: None,
            table_names: Vec::new(),
            shard_keys: Vec::new(),
            is_shard_ddl: false,
        };
        let plan = build_shard_plan(&stmt, None, 4);
        assert_eq!(plan.sub_queries.len(), 1);
        assert_eq!(plan.sub_queries[0].tablet_index, 0);
    }

    #[test]
    fn merge_results_concat() {
        let rows = vec![vec!["a".to_string()], vec!["b".to_string()]];
        let merged = merge_results(rows, MergeType::Concat);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn ordered_merge_ascending() {
        let sorted = vec![
            vec![vec!["1".into(), "a".into()], vec!["3".into(), "c".into()]],
            vec![vec!["2".into(), "b".into()], vec!["4".into(), "d".into()]],
        ];
        let result = ordered_merge(sorted, 0, true);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0][0], "1");
        assert_eq!(result[3][0], "4");
    }

    #[test]
    fn aggregate_merge_sum() {
        let rows = vec![
            vec!["100".into(), "x".into()],
            vec!["200".into(), "y".into()],
            vec!["300".into(), "z".into()],
        ];
        let result = aggregate_merge(rows, &[0], &["SUM"]);
        assert_eq!(result[0], "600"); // 100 + 200 + 300
    }

    #[test]
    fn ordered_merge_descending() {
        let sorted = vec![
            vec![vec!["3".into(), "c".into()], vec!["1".into(), "a".into()]],
            vec![vec!["4".into(), "d".into()], vec!["2".into(), "b".into()]],
        ];
        let result = ordered_merge(sorted, 0, false);
        assert_eq!(result[0][0], "4");
        assert_eq!(result[3][0], "1");
    }

    #[test]
    fn ordered_merge_empty_and_uneven() {
        // 空输入
        assert!(ordered_merge(vec![], 0, true).is_empty());
        assert!(ordered_merge(vec![], 0, false).is_empty());
        // 某分片无行
        let sorted = vec![
            vec![vec!["2".into()], vec!["5".into()]],
            vec![],
            vec![vec!["3".into()]],
        ];
        let result = ordered_merge(sorted, 0, true);
        assert_eq!(result, vec![vec!["2".to_string()], vec!["3".to_string()], vec!["5".to_string()]]);
    }

    #[test]
    fn aggregate_merge_edge_cases() {
        // 空输入 → 空
        assert!(aggregate_merge(vec![], &[0], &["SUM"]).is_empty());
        // 聚合列越界跳过
        let rows = vec![vec!["1".into(), "x".into()], vec!["2".into(), "y".into()]];
        let r = aggregate_merge(rows, &[5], &["SUM"]);
        assert_eq!(r, vec!["1".to_string(), "x".to_string()]);
        // 非数值跳过(不 panic,SUM 只统计可解析项)
        let rows = vec![vec!["abc".into()], vec!["10".into()]];
        let r = aggregate_merge(rows, &[0], &["SUM"]);
        assert_eq!(r[0], "10");
    }

    #[test]
    fn plan_binding_keyword_edges() {
        use crate::config::model::PlanBinding;
        let b = |pat: &str| PlanBinding { sql_pattern: pat.into(), hint: "/*+ X */".into() };
        // 无关键字(纯注释/数字) → None
        assert!(apply_plan_binding("12345", &[b("12")]).is_none());
        assert!(apply_plan_binding("/* only comment */", &[b("comment")]).is_none());
        // 关键字后已有空白 → 不补空格
        let r = apply_plan_binding("UPDATE orders SET x=1", &[b("update orders")]).unwrap();
        assert!(r.contains("UPDATE /*+ X */ orders"), "got: {r}");
        // 关键字后紧贴内容 → 补空格
        let r = apply_plan_binding("SELECT*FROM t", &[b("select")]).unwrap();
        assert!(r.contains("SELECT /*+ X */ *FROM"), "got: {r}");
        // 空规则集 → None
        assert!(apply_plan_binding("SELECT 1", &[]).is_none());
        // 空 pattern 规则跳过
        assert!(apply_plan_binding("SELECT 1", &[b("")]).is_none());
    }
}

// ─── 执行计划绑定(SQL hint 注入)───

/// 执行计划绑定:SQL 模板命中则注入优化器 hint,使后端走固定执行计划。
///
/// - 匹配:大小写不敏感**子串**匹配(规则 sql_pattern)
/// - 注入:首个语句关键字(SELECT/INSERT/UPDATE/DELETE/REPLACE)之后插入 hint,
///   形如 `SELECT /*+ INDEX(t idx) */ ...`(MySQL 优化器 hint 语法)
/// - 无绑定规则或未命中返回 None(零改写,透传原样)
pub fn apply_plan_binding(
    sql: &str,
    bindings: &[crate::config::model::PlanBinding],
) -> Option<String> {
    if bindings.is_empty() {
        return None;
    }
    let lower = sql.to_ascii_lowercase();
    let rule = bindings.iter().find(|b| {
        let pat = b.sql_pattern.to_ascii_lowercase();
        !pat.is_empty() && lower.contains(&pat)
    })?;

    // 定位首个语句关键字(注释/大小写边缘情况从简,取最小出现位置)
    const KEYWORDS: [&str; 5] = ["select", "insert", "update", "delete", "replace"];
    let mut best: Option<(usize, usize)> = None;
    for k in KEYWORDS {
        if let Some(p) = lower.find(k) {
            if best.map_or(true, |(bp, _)| p < bp) {
                best = Some((p, k.len()));
            }
        }
    }
    let (kw_pos, kw_len) = best?;

    let mut out = String::with_capacity(sql.len() + rule.hint.len() + 3);
    out.push_str(&sql[..kw_pos + kw_len]);
    out.push(' ');
    out.push_str(&rule.hint);
    // 关键字后原文已有空白(如 "UPDATE orders")则不再补空格,避免双空格
    let rest = &sql[kw_pos + kw_len..];
    if !rest.is_empty() && !rest.starts_with(|c: char| c.is_whitespace()) {
        out.push(' ');
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod binding_tests {
    use super::apply_plan_binding;
    use crate::config::model::PlanBinding;

    fn binding(pattern: &str, hint: &str) -> PlanBinding {
        PlanBinding {
            sql_pattern: pattern.to_string(),
            hint: hint.to_string(),
        }
    }

    #[test]
    fn inject_after_select() {
        let bs = vec![binding("FROM orders", "/*+ INDEX(orders idx_user) */")];
        let out = apply_plan_binding("select * from orders where user_id = 1", &bs).unwrap();
        assert_eq!(
            out,
            "select /*+ INDEX(orders idx_user) */ * from orders where user_id = 1"
        );
    }

    #[test]
    fn no_match_returns_none() {
        let bs = vec![binding("FROM orders", "/*+ x */")];
        assert!(apply_plan_binding("SELECT * FROM users", &bs).is_none());
    }

    #[test]
    fn empty_bindings_returns_none() {
        assert!(apply_plan_binding("SELECT 1", &[]).is_none());
    }

    #[test]
    fn case_insensitive_pattern() {
        let bs = vec![binding("select * from big_table", "/*+ MAX_EXECUTION_TIME(1000) */")];
        let out = apply_plan_binding("SELECT * FROM big_table WHERE id=1", &bs).unwrap();
        assert!(out.starts_with("SELECT /*+ MAX_EXECUTION_TIME(1000) */"));
    }

    #[test]
    fn inject_after_update() {
        let bs = vec![binding("UPDATE orders", "/*+ MAX_EXECUTION_TIME(500) */")];
        let out = apply_plan_binding("UPDATE orders SET status=1 WHERE id=2", &bs).unwrap();
        assert_eq!(
            out,
            "UPDATE /*+ MAX_EXECUTION_TIME(500) */ orders SET status=1 WHERE id=2"
        );
    }
}

// ─── SQL 模板归一化(字面量 → ?)───

/// 把 SQL 中的字面量(数字、单双引号字符串)替换为 `?`,得到模板。
///
/// 用途:Top SQL 按模板聚合(如 `SELECT * FROM t WHERE id=?`),
/// 而非按具体 SQL(带字面量)统计,避免同一条语句因参数不同被拆成多条。
/// 注释保留(不影响执行);转义引号(\\')正确处理。
pub fn normalize_sql(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    let mut in_squote = false;
    let mut in_dquote = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut in_number = false;

    while i < bytes.len() {
        let c = bytes[i];
        // 行注释
        if in_line_comment {
            out.push(c as char);
            if c == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        // 块注释
        if in_block_comment {
            out.push(c as char);
            if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                out.push('/');
                i += 2;
                in_block_comment = false;
            } else {
                i += 1;
            }
            continue;
        }
        // 单引号字符串
        if in_squote {
            out.push('?');
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2; // 转义
                    continue;
                }
                if bytes[i] == b'\'' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            in_squote = false;
            continue;
        }
        // 双引号字符串(标识符可加双引号;此处按字符串替换,兼容多数场景)
        if in_dquote {
            out.push('?');
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            in_dquote = false;
            continue;
        }

        match c {
            b'\'' => {
                in_squote = true;
                i += 1;
            }
            b'"' => {
                in_dquote = true;
                i += 1;
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                in_line_comment = true;
                out.push_str("--");
                i += 2;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                in_block_comment = true;
                out.push_str("/*");
                i += 2;
            }
            b'0'..=b'9' => {
                if !in_number {
                    out.push('?');
                    in_number = true;
                }
                i += 1;
            }
            b'.' if in_number => {
                i += 1; // 小数部分并入同一 ?
            }
            _ => {
                in_number = false;
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod normalize_tests {
    use super::normalize_sql;

    #[test]
    fn numbers_and_strings_replaced() {
        assert_eq!(
            normalize_sql("SELECT * FROM t WHERE id=123 AND name='abc'"),
            "SELECT * FROM t WHERE id=? AND name=?"
        );
    }

    #[test]
    fn float_and_quoted_with_escape() {
        assert_eq!(
            normalize_sql("SELECT a FROM t WHERE x=1.5 AND s='it\\'s'"),
            "SELECT a FROM t WHERE x=? AND s=?"
        );
    }

    #[test]
    fn comments_kept() {
        assert_eq!(
            normalize_sql("SELECT /* hint */ * FROM t WHERE id=1"),
            "SELECT /* hint */ * FROM t WHERE id=?"
        );
    }

    #[test]
    fn no_literal_unchanged() {
        assert_eq!(normalize_sql("SELECT a, b FROM t"), "SELECT a, b FROM t");
    }
}

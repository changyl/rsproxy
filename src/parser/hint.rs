// 路由 hint 解析——从 SQL 注释中提取路由指令
//
// 格式（newproxy 专用）:
//   /*{router}*/                    — 空 hint, 表示从默认路由
//   /*{router:cl=0,idx=1}*/         — 指定 cluster 和 tablet 索引
//   /*{router:force_tbl=t0}*/       — 强制指定表
//   /*{router:cl=1,force_tbl=t2}*/  — 组合
//
// 对应 C 侧 tr_sql.c 中的 hint 解析

use crate::parser::ast::RouterHint;

/// 从 SQL 文本中提取路由 hint
pub fn parse_router_hint(sql: &str) -> Option<RouterHint> {
    let hint_text = extract_hint_text(sql)?;
    let mut hint = RouterHint {
        raw: hint_text.to_string(),
        ..Default::default()
    };

    // 解析 hint 内容: "cl=0,idx=1,force_tbl=t0"
    let content = hint_text
        .strip_prefix("{router:")
        .or_else(|| hint_text.strip_prefix("{ROUTER:"))
        .unwrap_or(hint_text)
        .trim_end_matches('}');

    for part in content.split(',') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("cl=") {
            hint.cluster_id = Some(val.to_string());
        } else if let Some(val) = part.strip_prefix("idx=") {
            hint.tablet_index = val.parse().ok();
        } else if let Some(val) = part.strip_prefix("force_tbl=") {
            hint.force_table = Some(val.to_string());
        }
    }

    Some(hint)
}

/// 提取 `/*{router:...}*/` 注释块中的提示文本
fn extract_hint_text(sql: &str) -> Option<&str> {
    let start = sql.find("/*{")?;
    let end = sql[start..].find("}*/")?;
    Some(&sql[start + 2..start + end + 1]) // +2 跳过 "/*", +1 包含 "}"
}

/// 快速检查 SQL 是否包含路由 hint
pub fn has_router_hint(sql: &str) -> bool {
    sql.contains("/*{router") || sql.contains("/*{ROUTER")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_hint() {
        let hint = parse_router_hint("/*{router}*/ SELECT 1").unwrap();
        assert!(hint.cluster_id.is_none());
        assert!(hint.tablet_index.is_none());
        assert!(hint.force_table.is_none());
    }

    #[test]
    fn parse_cluster_and_index() {
        let hint = parse_router_hint("/*{router:cl=0,idx=2}*/ SELECT * FROM t").unwrap();
        assert_eq!(hint.cluster_id.as_deref(), Some("0"));
        assert_eq!(hint.tablet_index, Some(2));
        assert!(hint.force_table.is_none());
    }

    #[test]
    fn parse_force_table() {
        let hint = parse_router_hint("/*{router:force_tbl=t0}*/ SELECT 1").unwrap();
        assert_eq!(hint.force_table.as_deref(), Some("t0"));
        assert!(hint.cluster_id.is_none());
    }

    #[test]
    fn parse_combined() {
        let hint = parse_router_hint(
            "/*{router:cl=1,idx=0,force_tbl=users}*/ INSERT INTO users VALUES(1)",
        )
        .unwrap();
        assert_eq!(hint.cluster_id.as_deref(), Some("1"));
        assert_eq!(hint.tablet_index, Some(0));
        assert_eq!(hint.force_table.as_deref(), Some("users"));
    }

    #[test]
    fn no_hint_returns_none() {
        assert!(parse_router_hint("SELECT 1").is_none());
        assert!(parse_router_hint("/* normal comment */ SELECT 1").is_none());
    }

    #[test]
    fn case_insensitive_router() {
        let hint = parse_router_hint("/*{ROUTER:cl=3}*/ SELECT 1").unwrap();
        assert_eq!(hint.cluster_id.as_deref(), Some("3"));
    }

    #[test]
    fn has_router_hint_check() {
        assert!(has_router_hint("/*{router}*/ SELECT 1"));
        assert!(has_router_hint("/*{router:cl=0}*/ SELECT 1"));
        assert!(!has_router_hint("SELECT 1"));
        assert!(!has_router_hint("/* comment */ SELECT 1"));
    }
}

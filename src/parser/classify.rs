// SQL 语句分类 + 表名提取 + 分片键提取
//
// T3.2 实现:薄壳 —— 全部结构化解析下沉到 analyze.rs(token 流之上),
// 本模块只保留对外 API 形态与既有语义:
//   - classify():完整分析(类型 + 表名 + 分片键 + hint + shard DDL 识别)
//   - table_names_for_routing():热路径轻量表名提取(与 classify().table_names 一致)
//
// 历史:旧实现基于字符串遍历与 `find(" FROM ")` 子串搜索,复杂 SQL
// (子查询/CTE/UNION/派生表/字符串含关键字)会错取或漏取表名 → 已废弃。

use crate::parser::ast::ParsedStatement;
use crate::parser::analyze;

/// 分类 SQL 语句并提取表名和分片键(完整分析)
pub fn classify(sql: &str) -> ParsedStatement {
    let a = analyze::analyze_full(sql);
    ParsedStatement {
        stmt_type: a.stmt_type,
        raw_sql: sql.to_string(),
        router_hint: crate::parser::hint::parse_router_hint(sql),
        table_names: a.table_names,
        shard_keys: a.shard_keys,
        is_shard_ddl: is_shard_ddl_statement(sql),
    }
}

/// 轻量路由专用:仅提取表名(供按表尾号路由),跳过分片键/hint/DDL 等无关工作。
///
/// 热路径瘦身:非 DML(SHOW/SET/USE/BEGIN/COMMIT…)/无表语句只做前缀窥探即返回空;
/// DML 走一次词法 + 结构化收集。结果与 `classify().table_names` 一致
/// (对 SELECT/INSERT/UPDATE/DELETE/REPLACE/EXPLAIN/WITH)。
pub fn table_names_for_routing(sql: &str) -> Vec<String> {
    if !analyze::maybe_routable(sql) {
        return Vec::new();
    }
    analyze::analyze_lite(sql).1
}

/// 判断是否为 newproxy 分片 DDL
fn is_shard_ddl_statement(sql: &str) -> bool {
    let upper = sql.to_uppercase();
    upper.contains("SHARD") || upper.contains("TRIBBLE")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ast::StatementType;

    #[test]
    fn classify_basic_statements() {
        assert_eq!(classify("SELECT 1").stmt_type, StatementType::Select);
        assert_eq!(
            classify("INSERT INTO t VALUES(1)").stmt_type,
            StatementType::Insert
        );
        assert_eq!(
            classify("UPDATE t SET a=1").stmt_type,
            StatementType::Update
        );
        assert_eq!(classify("DELETE FROM t").stmt_type, StatementType::Delete);
        assert_eq!(classify("SHOW DATABASES").stmt_type, StatementType::Show);
        assert_eq!(classify("SET NAMES utf8").stmt_type, StatementType::Set);
        assert_eq!(classify("BEGIN").stmt_type, StatementType::Begin);
        assert_eq!(classify("COMMIT").stmt_type, StatementType::Commit);
        assert_eq!(classify("ROLLBACK").stmt_type, StatementType::Rollback);
    }

    #[test]
    fn routing_tables_match_classify() {
        // 轻量路由版提取的表名必须与 classify().table_names 完全一致
        let cases = [
            "SELECT * FROM sbtest_1 WHERE id=1",
            "SELECT c FROM perf_2, perf_1 WHERE perf_1.id = perf_2.id",
            "SELECT * FROM t1 JOIN t2 ON t1.id=t2.id LEFT JOIN t3 ON t3.id=t1.id",
            "INSERT INTO sbtest_1 (id, k) VALUES (1, 2)",
            "UPDATE sbtest_2 SET k=k+1 WHERE id=5",
            "DELETE FROM perf_1 WHERE id=1",
            "REPLACE INTO t1 (id) VALUES (1)",
            "SELECT (SELECT MAX(x) FROM t2) FROM t1",
            "INSERT INTO t1 SELECT * FROM t2",
            "WITH c AS (SELECT id FROM t2) SELECT * FROM sbtest_1 JOIN c ON 1=1",
            "SELECT * FROM (SELECT id FROM t2) d JOIN t3 ON d.id=t3.id",
            "SELECT 1",
            "SHOW TABLES",
            "BEGIN",
            "COMMIT",
            "/* hint */ SELECT * FROM sbtest_1",
        ];
        for sql in cases {
            assert_eq!(
                table_names_for_routing(sql),
                classify(sql).table_names,
                "mismatch for: {sql}"
            );
        }
    }

    #[test]
    fn classify_with_comments() {
        assert_eq!(
            classify("/* hint */ SELECT 1").stmt_type,
            StatementType::Select
        );
        assert_eq!(
            classify("-- comment\nSELECT 1").stmt_type,
            StatementType::Select
        );
        assert_eq!(
            classify("# comment\nSELECT 1").stmt_type,
            StatementType::Select
        );
    }

    #[test]
    fn classify_case_insensitive() {
        assert_eq!(classify("select 1").stmt_type, StatementType::Select);
        assert_eq!(classify("Insert into t").stmt_type, StatementType::Insert);
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(classify("FOOBAR").stmt_type, StatementType::Unknown);
        assert_eq!(classify("").stmt_type, StatementType::Unknown);
    }

    #[test]
    fn extract_table_names() {
        let stmt = classify("SELECT * FROM users, orders WHERE users.id = orders.uid");
        assert!(stmt.table_names.contains(&"users".to_string()));
        assert!(stmt.table_names.contains(&"orders".to_string()));

        let stmt = classify("SELECT * FROM users JOIN orders ON users.id = orders.uid");
        assert!(stmt.table_names.contains(&"users".to_string()));
        assert!(stmt.table_names.contains(&"orders".to_string()));

        let stmt = classify("INSERT INTO users (id, name) VALUES (1, 'test')");
        assert_eq!(stmt.table_names, vec!["users"]);

        let stmt = classify("UPDATE users SET name='a' WHERE id=1");
        assert_eq!(stmt.table_names, vec!["users"]);

        let stmt = classify("DELETE FROM users WHERE id = 1");
        assert_eq!(stmt.table_names, vec!["users"]);
    }

    #[test]
    fn extract_shard_keys_basic() {
        let stmt = classify("SELECT * FROM t WHERE id = 42 AND name = 'test'");
        assert_eq!(stmt.shard_keys.len(), 2);
        assert_eq!(stmt.shard_keys[0].column, "id");
        assert_eq!(stmt.shard_keys[0].value, "42");
        assert_eq!(stmt.shard_keys[1].column, "name");
        assert_eq!(stmt.shard_keys[1].value, "test");
    }

    #[test]
    fn is_shard_ddl() {
        assert!(is_shard_ddl_statement("CREATE SHARD TABLE t"));
        assert!(!is_shard_ddl_statement("SELECT 1"));
    }

    #[test]
    fn unknown_keeps_table_names_empty() {
        assert!(classify("WITH x AS (SELECT 1)").table_names.is_empty());
        assert!(classify("SELECT 1").table_names.is_empty());
    }
}

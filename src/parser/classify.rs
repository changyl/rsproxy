// SQL 语句分类 + 表名提取 + 分片键提取
// T3.2 实现: 基于关键字匹配的 SQL 分析
//
// 自研 Rust parser,替代原 C sqlparser FFI

use crate::parser::ast::{ParsedStatement, ShardKeyValue, StatementType};

/// 分类 SQL 语句并提取表名和分片键
pub fn classify(sql: &str) -> ParsedStatement {
    let cleaned = strip_comments(sql);
    let upper = cleaned.to_uppercase();
    let first_word = upper.split_whitespace().next().unwrap_or("");

    let stmt_type = classify_by_keyword(first_word);
    let table_names = extract_table_names(&cleaned, stmt_type);
    let shard_keys = extract_shard_keys(&cleaned);

    ParsedStatement {
        stmt_type,
        raw_sql: sql.to_string(),
        router_hint: crate::parser::hint::parse_router_hint(sql),
        table_names,
        shard_keys,
        is_shard_ddl: is_shard_ddl_statement(sql),
    }
}

/// 轻量路由专用:仅提取表名(供按表尾号路由),跳过分片键/hint/DDL 等无关工作。
///
/// 热路径瘦身:`classify` 全量会对清理后的 SQL 做**两次** `to_uppercase` 全串拷贝
/// 并构造 shard_keys/raw_sql/router_hint/is_shard_ddl;而按表尾号路由只需要
/// `table_names`。本函数只做一次大写上卷并复用同一份大写字串提取表名,
/// 结果与 `classify().table_names` 一致(对 SELECT/INSERT/UPDATE/DELETE/REPLACE)。
pub fn table_names_for_routing(sql: &str) -> Vec<String> {
    let cleaned = strip_comments(sql);
    let upper = cleaned.to_uppercase();
    let first_word = upper.split_whitespace().next().unwrap_or("");
    let stmt_type = classify_by_keyword(first_word);

    let mut tables = Vec::new();
    match stmt_type {
        StatementType::Select | StatementType::Delete => {
            extract_from_tables(&upper, "FROM", &mut tables);
            extract_join_tables(&upper, &mut tables);
        }
        StatementType::Insert | StatementType::Replace => {
            extract_from_tables(&upper, "INTO", &mut tables);
        }
        StatementType::Update => {
            extract_update_tables(&upper, &mut tables);
        }
        _ => {}
    }
    tables
}

/// 根据首关键字确定语句类型
fn classify_by_keyword(keyword: &str) -> StatementType {
    match keyword {
        "SELECT" => StatementType::Select,
        "INSERT" => StatementType::Insert,
        "UPDATE" => StatementType::Update,
        "DELETE" => StatementType::Delete,
        "REPLACE" => StatementType::Replace,
        "SET" => StatementType::Set,
        "SHOW" => StatementType::Show,
        "DESCRIBE" | "DESC" => StatementType::Describe,
        "EXPLAIN" => StatementType::Explain,
        "BEGIN" | "START" => StatementType::Begin,
        "COMMIT" => StatementType::Commit,
        "ROLLBACK" => StatementType::Rollback,
        "USE" => StatementType::Use,
        "CREATE" => StatementType::Create,
        "DROP" => StatementType::Drop,
        "ALTER" => StatementType::Alter,
        "TRUNCATE" => StatementType::Truncate,
        "CALL" => StatementType::Call,
        "PREPARE" => StatementType::Prepare,
        "EXECUTE" => StatementType::Execute,
        "DEALLOCATE" => StatementType::Deallocate,
        "KILL" => StatementType::Kill,
        _ => StatementType::Unknown,
    }
}

// ─── 表名提取 ───

/// 提取 SQL 中涉及的表名
///
/// 支持:
/// - SELECT ... FROM t1, t2 JOIN t3 ON ...
/// - INSERT INTO t ...
/// - UPDATE t SET ...
/// - DELETE FROM t ...
/// - REPLACE INTO t ...
fn extract_table_names(sql: &str, stmt_type: StatementType) -> Vec<String> {
    let upper = sql.to_uppercase();
    let mut tables = Vec::new();

    match stmt_type {
        StatementType::Select | StatementType::Delete => {
            extract_from_tables(&upper, "FROM", &mut tables);
            extract_join_tables(&upper, &mut tables);
        }
        StatementType::Insert | StatementType::Replace => {
            extract_from_tables(&upper, "INTO", &mut tables);
        }
        StatementType::Update => {
            // UPDATE t1, t2 SET ...
            extract_update_tables(&upper, &mut tables);
        }
        _ => {}
    }

    tables
}

/// 提取 FROM/INTO 后的表名
fn extract_from_tables(upper: &str, keyword: &str, tables: &mut Vec<String>) {
    // 处理 SQL 以 keyword 开头或无前导空格的情况
    let kw_with_space = format!(" {} ", keyword);
    let rest = if let Some(pos) = upper.find(&kw_with_space) {
        &upper[pos + kw_with_space.len()..]
    } else if upper.starts_with(&format!("{} ", keyword)) {
        &upper[keyword.len() + 1..]
    } else {
        return;
    };

    // 截断到第一个 '(' — 之后的是列定义,不是表名
    let rest = if let Some(paren_pos) = rest.find('(') {
        &rest[..paren_pos]
    } else {
        rest
    };

    let table_list = rest
        .split([' ', ',', '\n', '\t'])
        .filter(|s| !s.is_empty())
        .take_while(|s| !is_sql_keyword(s))
        .collect::<Vec<_>>()
        .join(",");

    for t in table_list.split(',') {
        let t = t.trim();
        if !t.is_empty() && !is_sql_keyword(t) {
            tables.push(t.to_lowercase());
        }
    }
}

/// 提取 JOIN 后的表名
fn extract_join_tables(upper: &str, tables: &mut Vec<String>) {
    for kw in &[
        "JOIN",
        "LEFT JOIN",
        "RIGHT JOIN",
        "INNER JOIN",
        "CROSS JOIN",
    ] {
        let search = format!(" {} ", kw);
        for (i, _) in upper.match_indices(&search) {
            let rest = &upper[i + search.len()..];
            if let Some(t) = rest
                .split_whitespace()
                .next()
                .filter(|s| !is_sql_keyword(s))
            {
                tables.push(t.to_lowercase());
            }
        }
    }
}

/// 提取 UPDATE 后的表名
fn extract_update_tables(upper: &str, tables: &mut Vec<String>) {
    // SQL 可能以 UPDATE 开头或中间有 UPDATE
    let rest = if let Some(stripped) = upper.strip_prefix("UPDATE ") {
        stripped // 跳过 "UPDATE "
    } else if let Some(pos) = upper.find(" UPDATE ") {
        &upper[pos + 8..]
    } else {
        return;
    };

    // 找到 SET 之前的部分
    if let Some(set_pos) = rest.find(" SET ") {
        let table_part = &rest[..set_pos];
        for t in table_part.split(',') {
            let t = t.trim();
            if !t.is_empty() && !is_sql_keyword(t) {
                tables.push(t.to_lowercase());
            }
        }
    }
}

/// 判断 token 是否为 SQL 关键字(截断表名列表)
fn is_sql_keyword(s: &str) -> bool {
    matches!(
        s,
        "WHERE"
            | "SET"
            | "ORDER"
            | "GROUP"
            | "HAVING"
            | "LIMIT"
            | "ON"
            | "USING"
            | "FOR"
            | "AS"
            | "AND"
            | "OR"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "CROSS"
            | "NATURAL"
            | "VALUES"
    )
}

// ─── 分片键提取 ───

/// 从 WHERE 条件中提取等值条件作为候选分片键
///
/// 例如: `WHERE id = 42 AND name = 'test'`
/// → [ShardKeyValue { column: "id", value: "42" }, ...]
fn extract_shard_keys(sql: &str) -> Vec<ShardKeyValue> {
    let upper = sql.to_uppercase();

    let where_pos = match upper.find(" WHERE ") {
        Some(p) => p + 7,
        None => return vec![],
    };

    let where_clause = &upper[where_pos..];
    // 截断到 ORDER BY / GROUP BY / LIMIT / HAVING
    let end = find_first_keyword(where_clause, &["ORDER BY", "GROUP BY", "LIMIT", "HAVING"]);
    let where_clause = &where_clause[..end];

    let mut keys = Vec::new();

    // 简单匹配: column = value (只处理 AND 连接)
    for cond in where_clause.split(" AND ") {
        let cond = cond.trim();
        if let Some((col, val)) = parse_equality(cond) {
            keys.push(ShardKeyValue {
                column: col.to_lowercase(),
                value: val,
            });
        }
    }

    keys
}

/// 解析 column = value 形式的等值条件
fn parse_equality(cond: &str) -> Option<(String, String)> {
    // 处理 column = value 或 column IN (value)
    if let Some(pos) = cond.find('=') {
        let col = cond[..pos].trim().to_lowercase();
        let val = cond[pos + 1..]
            .trim()
            .trim_matches('\'')
            .trim_matches('"')
            .to_lowercase();
        if !col.is_empty() && !val.is_empty() {
            return Some((col, val));
        }
    }

    // 处理 column IN (value)
    if let Some(pos) = cond.find(" IN (") {
        let col = cond[..pos].trim().to_lowercase();
        let val = cond[pos + 4..]
            .trim()
            .trim_matches(')')
            .trim_matches('\'')
            .to_lowercase();
        if !col.is_empty() && !val.is_empty() {
            return Some((col, val));
        }
    }

    None
}

/// 在 SQL 片段中找到第一个关键字的位置
fn find_first_keyword(sql: &str, keywords: &[&str]) -> usize {
    keywords
        .iter()
        .filter_map(|kw| sql.find(kw))
        .min()
        .unwrap_or(sql.len())
}

// ─── 注释剥离 ───

fn strip_comments(sql: &str) -> String {
    let mut result = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if i + 2 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' && bytes[i + 2] == b' ' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        result.push(bytes[i] as char);
        i += 1;
    }

    result
}

/// 判断是否为 newproxy 分片 DDL
fn is_shard_ddl_statement(sql: &str) -> bool {
    let upper = sql.to_uppercase();
    upper.contains("SHARD") || upper.contains("TRIBBLE")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn extract_select_from() {
        let stmt = classify("SELECT * FROM users WHERE id = 1");
        assert_eq!(stmt.table_names, vec!["users"]);
    }

    #[test]
    fn extract_multi_table_from() {
        let stmt = classify("SELECT * FROM users, orders WHERE users.id = orders.uid");
        assert!(stmt.table_names.contains(&"users".to_string()));
        assert!(stmt.table_names.contains(&"orders".to_string()));
    }

    #[test]
    fn extract_join_table() {
        let stmt = classify("SELECT * FROM users JOIN orders ON users.id = orders.uid");
        assert!(stmt.table_names.contains(&"users".to_string()));
        assert!(stmt.table_names.contains(&"orders".to_string()));
    }

    #[test]
    fn extract_insert_into() {
        let stmt = classify("INSERT INTO users (id, name) VALUES (1, 'test')");
        assert_eq!(stmt.table_names, vec!["users"]);
    }

    #[test]
    fn extract_update_table() {
        let stmt = classify("UPDATE users SET name='a' WHERE id=1");
        assert_eq!(stmt.table_names, vec!["users"]);
    }

    #[test]
    fn extract_delete_from() {
        let stmt = classify("DELETE FROM users WHERE id = 1");
        assert_eq!(stmt.table_names, vec!["users"]);
    }

    #[test]
    fn extract_shard_keys_basic() {
        let keys = extract_shard_keys("SELECT * FROM t WHERE id = 42 AND name = 'test'");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].column, "id");
        assert_eq!(keys[0].value, "42");
        assert_eq!(keys[1].column, "name");
        assert_eq!(keys[1].value, "test");
    }

    #[test]
    fn extract_shard_keys_no_where() {
        let keys = extract_shard_keys("SELECT * FROM t");
        assert!(keys.is_empty());
    }

    #[test]
    fn extract_shard_keys_with_limit() {
        let keys = extract_shard_keys("SELECT * FROM t WHERE id = 1 ORDER BY id LIMIT 10");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].column, "id");
        assert_eq!(keys[0].value, "1");
    }

    #[test]
    fn is_shard_ddl() {
        assert!(is_shard_ddl_statement("CREATE SHARD TABLE t"));
        assert!(!is_shard_ddl_statement("SELECT 1"));
    }

    #[test]
    fn strip_comments_basic() {
        assert_eq!(strip_comments("SELECT 1"), "SELECT 1");
        assert_eq!(strip_comments("SELECT /* inline */ 1"), "SELECT  1");
        assert_eq!(strip_comments("-- line\nSELECT 1"), "\nSELECT 1");
        assert_eq!(strip_comments("# line\nSELECT 1"), "\nSELECT 1");
    }
}

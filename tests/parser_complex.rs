// parser 模块复杂 SQL 回归语料
//
// 目标:确保 token 化 + 结构化分析对复杂 SQL 的表名提取/语句分类正确,
// 且热路径 `table_names_for_routing` 与 `classify().table_names` 始终一致。
// 排序契约:own(语句主查询体,UNION 各支)先 → nested(括号内子查询/派生表/CTE 体)后,
// 均文本序、去重保序;表名小写,反引号/库前缀按原文保留。

use newproxy::parser::ast::StatementType;
use newproxy::parser::classify::{classify, table_names_for_routing};

struct Case<'a> {
    sql: &'a str,
    stmt: StatementType,
    tables: &'a [&'a str],
}

fn run(cases: &[Case]) {
    for c in cases {
        let p = classify(c.sql);
        assert_eq!(p.stmt_type, c.stmt, "type mismatch for: {}", c.sql);
        let want: Vec<String> = c.tables.iter().map(|s| s.to_string()).collect();
        assert_eq!(p.table_names, want, "tables mismatch for: {}", c.sql);
        assert_eq!(
            table_names_for_routing(c.sql),
            p.table_names,
            "routing vs classify mismatch for: {}",
            c.sql
        );
    }
}

#[test]
fn complex_sql_corpus() {
    let cases = [
        // ── 简单 DML(行为基线)──
        Case { sql: "SELECT * FROM sbtest_1 WHERE id=1", stmt: StatementType::Select, tables: &["sbtest_1"] },
        Case { sql: "INSERT INTO sbtest_2 (k,c,pad) VALUES (1,'a','b')", stmt: StatementType::Insert, tables: &["sbtest_2"] },
        Case { sql: "UPDATE sbtest_3 SET c='x' WHERE id=2", stmt: StatementType::Update, tables: &["sbtest_3"] },
        Case { sql: "DELETE FROM sbtest_4 WHERE id=3", stmt: StatementType::Delete, tables: &["sbtest_4"] },
        Case { sql: "REPLACE INTO sbtest_5 (id) VALUES (1)", stmt: StatementType::Replace, tables: &["sbtest_5"] },

        // ── 字符串/注释含关键字的污染用例 ──
        Case { sql: "SELECT 'a FROM b' AS s, x FROM t1", stmt: StatementType::Select, tables: &["t1"] },
        Case { sql: "SELECT * FROM t WHERE name='from join where and select' AND id=5", stmt: StatementType::Select, tables: &["t"] },
        Case { sql: "SELECT /* FROM x JOIN y */ * FROM real_t WHERE c='-- and more'", stmt: StatementType::Select, tables: &["real_t"] },
        Case { sql: "INSERT INTO t VALUES ('from x', 'join y', 'set z')", stmt: StatementType::Insert, tables: &["t"] },
        Case { sql: "UPDATE t SET note='set where from' WHERE id=1", stmt: StatementType::Update, tables: &["t"] },

        // ── 子查询(select-list / WHERE / EXISTS / ON)──
        Case { sql: "SELECT (SELECT MAX(x) FROM t2) FROM t1", stmt: StatementType::Select, tables: &["t1", "t2"] },
        Case { sql: "SELECT * FROM t1 WHERE id IN (SELECT id FROM t2)", stmt: StatementType::Select, tables: &["t1", "t2"] },
        Case { sql: "SELECT * FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.k=t1.k)", stmt: StatementType::Select, tables: &["t1", "t2"] },
        Case { sql: "SELECT a.x FROM a JOIN b ON a.id = (SELECT MAX(id) FROM c WHERE c.a=a.id)", stmt: StatementType::Select, tables: &["a", "b", "c"] },
        Case { sql: "SELECT * FROM t1 WHERE (a=1 AND b IN (SELECT b FROM t2)) OR c IS NULL", stmt: StatementType::Select, tables: &["t1", "t2"] },

        // ── 派生表 / JOIN 家族 ──
        Case { sql: "SELECT * FROM (SELECT id FROM t2) d JOIN t3 ON d.id=t3.id", stmt: StatementType::Select, tables: &["t3", "t2"] },
        Case { sql: "SELECT * FROM a LEFT JOIN b ON a.x=b.x RIGHT JOIN c ON b.y=c.y", stmt: StatementType::Select, tables: &["a", "b", "c"] },
        Case { sql: "SELECT * FROM t1 INNER JOIN t2 USING (id) NATURAL JOIN t3", stmt: StatementType::Select, tables: &["t1", "t2", "t3"] },
        Case { sql: "SELECT * FROM t1 CROSS JOIN t2 WHERE t1.a=1", stmt: StatementType::Select, tables: &["t1", "t2"] },
        Case { sql: "SELECT * FROM t1 STRAIGHT_JOIN t2 ON t1.x=t2.x", stmt: StatementType::Select, tables: &["t1", "t2"] },
        Case { sql: "SELECT * FROM t1, t2, t3 WHERE t1.a=t2.a AND t2.b=t3.b", stmt: StatementType::Select, tables: &["t1", "t2", "t3"] },
        Case { sql: "SELECT * FROM t1 USE INDEX (a) JOIN t2 FORCE INDEX (b) ON t1.id=t2.id IGNORE INDEX (c)", stmt: StatementType::Select, tables: &["t1", "t2"] },

        // ── UNION ──
        Case { sql: "SELECT * FROM t1 UNION SELECT * FROM t2 UNION ALL SELECT 1", stmt: StatementType::Select, tables: &["t1", "t2"] },
        Case { sql: "(SELECT a FROM t1) UNION (SELECT b FROM t2)", stmt: StatementType::Select, tables: &["t1", "t2"] },

        // ── CTE / WITH ──
        Case { sql: "WITH c AS (SELECT id FROM t2) SELECT * FROM sbtest_1 JOIN c ON c.id=sbtest_1.id", stmt: StatementType::Select, tables: &["sbtest_1", "t2"] },
        Case { sql: "WITH a AS (SELECT 1), b AS (SELECT id FROM t3) SELECT * FROM t1 JOIN b ON b.id=t1.id", stmt: StatementType::Select, tables: &["t1", "t3"] },
        Case { sql: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<5) SELECT * FROM t1 JOIN c ON c.x=t1.id", stmt: StatementType::Select, tables: &["t1"] },
        Case { sql: "WITH c AS (SELECT id FROM t2) DELETE FROM t1 USING t1 JOIN c ON t1.id=c.id", stmt: StatementType::Delete, tables: &["t1", "t2"] },

        // ── 多表 UPDATE / DELETE / INSERT…SELECT ──
        Case { sql: "UPDATE t1 JOIN t2 ON t1.id=t2.id SET t1.a=1 WHERE t2.b=2", stmt: StatementType::Update, tables: &["t1", "t2"] },
        Case { sql: "UPDATE IGNORE t1, t2 SET t1.a=1, t2.b=2 WHERE t1.x=t2.x", stmt: StatementType::Update, tables: &["t1", "t2"] },
        Case { sql: "DELETE t1 FROM t1 JOIN t2 ON t1.id=t2.id WHERE t2.x=1", stmt: StatementType::Delete, tables: &["t1", "t2"] },
        Case { sql: "DELETE FROM t1 USING t1 JOIN t2 ON t1.id=t2.id WHERE t2.x=1", stmt: StatementType::Delete, tables: &["t1", "t2"] },
        Case { sql: "INSERT INTO t1 (a) SELECT b FROM t2 WHERE c=1", stmt: StatementType::Insert, tables: &["t1", "t2"] },
        Case { sql: "REPLACE INTO t1 SELECT * FROM t2 LEFT JOIN t3 ON t2.id=t3.id", stmt: StatementType::Replace, tables: &["t1", "t2", "t3"] },
        Case { sql: "INSERT INTO t1 VALUES (1,2),(3,4) ON DUPLICATE KEY UPDATE a=VALUES(a)", stmt: StatementType::Insert, tables: &["t1"] },
        Case { sql: "INSERT INTO t1 SET a=1, b=(SELECT MAX(x) FROM t2)", stmt: StatementType::Insert, tables: &["t1", "t2"] },

        // ── 引号标识符 / 库前缀 / 别名 ──
        Case { sql: "SELECT * FROM `order` WHERE `order`.id=1", stmt: StatementType::Select, tables: &["`order`"] },
        Case { sql: "SELECT * FROM sbtest.sbtest_1 WHERE sbtest.sbtest_1.id=1", stmt: StatementType::Select, tables: &["sbtest.sbtest_1"] },
        Case { sql: "SELECT * FROM users AS u JOIN orders o ON u.id=o.uid WHERE u.x=1", stmt: StatementType::Select, tables: &["users", "orders"] },
        Case { sql: "SELECT * FROM db1.t1 AS a, db2.t2 b WHERE a.id=b.id", stmt: StatementType::Select, tables: &["db1.t1", "db2.t2"] },

        // ── EXPLAIN / 前导注释 / 括号查询 ──
        Case { sql: "EXPLAIN SELECT * FROM sbtest_1 WHERE id=1", stmt: StatementType::Explain, tables: &["sbtest_1"] },
        Case { sql: "EXPLAIN FORMAT=JSON UPDATE sbtest_2 SET a=1 WHERE id=2", stmt: StatementType::Explain, tables: &["sbtest_2"] },
        Case { sql: "/*{router:cl=0,idx=1}*/ SELECT * FROM sbtest_3", stmt: StatementType::Select, tables: &["sbtest_3"] },
        Case { sql: "-- c1\n/* c2 */ SELECT * FROM sbtest_4", stmt: StatementType::Select, tables: &["sbtest_4"] },
        Case { sql: "SELECT 1", stmt: StatementType::Select, tables: &[] },
        Case { sql: "SHOW TABLES", stmt: StatementType::Show, tables: &[] },
        Case { sql: "BEGIN", stmt: StatementType::Begin, tables: &[] },
        Case { sql: "SET NAMES utf8mb4", stmt: StatementType::Set, tables: &[] },
        Case { sql: "USE sbtest", stmt: StatementType::Use, tables: &[] },
        Case { sql: "CREATE TABLE t1_2 (id INT)", stmt: StatementType::Create, tables: &[] },

        // ── 换行/制表/大小写混合 ──
        Case { sql: "select\n\t*\nfrom\n\tsbtest_1\nwhere\n\tid=1", stmt: StatementType::Select, tables: &["sbtest_1"] },
        Case { sql: "SeLeCt * FrOm T1 WhErE a=1", stmt: StatementType::Select, tables: &["t1"] },
    ];
    run(&cases);
}

#[test]
fn shard_key_corpus() {
    use newproxy::parser::classify::classify;
    let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
        // 顶层 AND 等值
        ("SELECT * FROM t WHERE id = 42 AND name = 'test'", vec![("id", "42"), ("name", "test")]),
        ("SELECT * FROM t WHERE user_id=100 AND status='active' AND ts > 5", vec![("user_id", "100"), ("status", "active")]),
        ("UPDATE t SET a=1 WHERE id=7", vec![("id", "7")]),
        ("DELETE FROM t WHERE id='abc'", vec![("id", "abc")]),
        ("SELECT * FROM t WHERE t.id=5", vec![("id", "5")]),
        ("SELECT * FROM t WHERE `id`=5", vec![("id", "5")]),
        ("SELECT * FROM t WHERE id IN (42)", vec![("id", "42")]),
        // 保守拒绝:OR / 多值 IN / 子查询 / 函数列 / 范围 / IS
        ("SELECT * FROM t WHERE a=1 OR b=2", vec![]),
        ("SELECT * FROM t WHERE id IN (1,2,3)", vec![]),
        ("SELECT * FROM t WHERE id IN (SELECT id FROM t2)", vec![]),
        ("SELECT * FROM t WHERE LOWER(id)=1", vec![]),
        ("SELECT * FROM t WHERE id > 5", vec![]),
        ("SELECT * FROM t WHERE id BETWEEN 1 AND 2", vec![]),
        ("SELECT * FROM t WHERE id IS NULL", vec![]),
        // 只取最外层(子查询 WHERE 不影响)
        ("SELECT * FROM (SELECT * FROM t2 WHERE b=1) d WHERE a=2", vec![("a", "2")]),
        ("SELECT * FROM t WHERE id=1 ORDER BY id LIMIT 10", vec![("id", "1")]),
    ];
    for (sql, want) in cases {
        let p = classify(sql);
        let got: Vec<(String, String)> = p
            .shard_keys
            .into_iter()
            .map(|k| (k.column, k.value))
            .collect();
        let want: Vec<(String, String)> = want
            .iter()
            .map(|(c, v)| (c.to_string(), v.to_string()))
            .collect();
        assert_eq!(got, want, "shard keys mismatch for: {sql}");
    }
}

#[test]
fn complex_routing_decisions_on_tail_tables() {
    // 尾号路由语义:表名集合被正确带出,供 front.rs 按首个带尾号表定位
    use newproxy::parser::route::table_tail_index;
    // 简单:主表 sbtest_2 → tablet 2%4 = 2
    let p = classify("SELECT * FROM sbtest_2 WHERE id=1");
    let t = p.table_names[0].as_str();
    assert_eq!(table_tail_index(t, 4), Some(2));
    // 复杂:字符串/注释污染修复后,主表仍被正确取出
    let p = classify("SELECT 'a FROM sbtest_3' AS s FROM sbtest_2");
    assert_eq!(p.table_names, vec!["sbtest_2".to_string()]);
    assert_eq!(table_tail_index(&p.table_names[0], 4), Some(2));
    // 多表:首表决定(目标表在前)
    let p = classify("INSERT INTO sbtest_1 SELECT * FROM sbtest_2");
    assert_eq!(table_tail_index(&p.table_names[0], 4), Some(1));
}

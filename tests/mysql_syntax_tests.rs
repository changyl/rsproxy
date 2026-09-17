// MySQL 9.7 语法分类测试
// 基于 https://dev.mysql.com/doc/refman/9.7/en/sql-statements.html
// 覆盖所有支持的 SQL 语句类型

use newproxy::parser::ast::StatementType;
use newproxy::parser::classify::classify;

// ─── DML: Data Manipulation ───

#[test]
fn select_basic() {
    assert_eq!(classify("SELECT 1").stmt_type, StatementType::Select);
    assert_eq!(classify("SELECT * FROM t").stmt_type, StatementType::Select);
    assert_eq!(
        classify("SELECT a, b FROM t WHERE c = 1").stmt_type,
        StatementType::Select
    );
    assert_eq!(
        classify("SELECT DISTINCT name FROM users").stmt_type,
        StatementType::Select
    );
    assert_eq!(
        classify("SELECT SQL_CALC_FOUND_ROWS * FROM t").stmt_type,
        StatementType::Select
    );
}

#[test]
fn select_subquery() {
    assert_eq!(
        classify("SELECT * FROM t WHERE id IN (SELECT id FROM t2)").stmt_type,
        StatementType::Select
    );
    assert_eq!(
        classify("SELECT (SELECT MAX(x) FROM t2) FROM t1").stmt_type,
        StatementType::Select
    );
}

#[test]
fn select_joins() {
    assert_eq!(
        classify("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id").stmt_type,
        StatementType::Select
    );
    assert_eq!(
        classify("SELECT * FROM t1 LEFT JOIN t2 USING(id)").stmt_type,
        StatementType::Select
    );
    assert_eq!(
        classify("SELECT * FROM t1 NATURAL JOIN t2").stmt_type,
        StatementType::Select
    );
    assert_eq!(
        classify("SELECT * FROM t1 CROSS JOIN t2").stmt_type,
        StatementType::Select
    );
}

#[test]
fn select_union() {
    assert_eq!(
        classify("SELECT 1 UNION SELECT 2").stmt_type,
        StatementType::Select
    );
    assert_eq!(
        classify("SELECT 1 UNION ALL SELECT 2").stmt_type,
        StatementType::Select
    );
}

#[test]
fn insert_variants() {
    assert_eq!(
        classify("INSERT INTO t VALUES(1)").stmt_type,
        StatementType::Insert
    );
    assert_eq!(
        classify("INSERT INTO t (a,b) VALUES (1,2)").stmt_type,
        StatementType::Insert
    );
    assert_eq!(
        classify("INSERT INTO t SET a=1, b=2").stmt_type,
        StatementType::Insert
    );
    assert_eq!(
        classify("INSERT INTO t SELECT * FROM t2").stmt_type,
        StatementType::Insert
    );
    assert_eq!(
        classify("INSERT IGNORE INTO t VALUES(1)").stmt_type,
        StatementType::Insert
    );
    assert_eq!(
        classify("INSERT INTO t VALUES(1) ON DUPLICATE KEY UPDATE a=2").stmt_type,
        StatementType::Insert
    );
}

#[test]
fn update_variants() {
    assert_eq!(
        classify("UPDATE t SET a=1").stmt_type,
        StatementType::Update
    );
    assert_eq!(
        classify("UPDATE t SET a=1 WHERE id=2").stmt_type,
        StatementType::Update
    );
    assert_eq!(
        classify("UPDATE t1 JOIN t2 ON t1.id=t2.id SET t1.a=1").stmt_type,
        StatementType::Update
    );
    assert_eq!(
        classify("UPDATE IGNORE t SET a=1").stmt_type,
        StatementType::Update
    );
}

#[test]
fn delete_variants() {
    assert_eq!(classify("DELETE FROM t").stmt_type, StatementType::Delete);
    assert_eq!(
        classify("DELETE FROM t WHERE id=1").stmt_type,
        StatementType::Delete
    );
    assert_eq!(
        classify("DELETE t1 FROM t1 JOIN t2 ON t1.id=t2.id").stmt_type,
        StatementType::Delete
    );
    assert_eq!(
        classify("DELETE FROM t ORDER BY id LIMIT 10").stmt_type,
        StatementType::Delete
    );
}

#[test]
fn replace_variants() {
    assert_eq!(
        classify("REPLACE INTO t VALUES(1)").stmt_type,
        StatementType::Replace
    );
    assert_eq!(
        classify("REPLACE INTO t SET a=1").stmt_type,
        StatementType::Replace
    );
    assert_eq!(
        classify("REPLACE INTO t SELECT * FROM t2").stmt_type,
        StatementType::Replace
    );
}

// ─── DDL: Data Definition ───

#[test]
fn create_variants() {
    assert_eq!(
        classify("CREATE TABLE t (id INT)").stmt_type,
        StatementType::Create
    );
    assert_eq!(
        classify("CREATE TEMPORARY TABLE t (id INT)").stmt_type,
        StatementType::Create
    );
    assert_eq!(
        classify("CREATE TABLE IF NOT EXISTS t (id INT)").stmt_type,
        StatementType::Create
    );
    assert_eq!(
        classify("CREATE INDEX idx ON t(col)").stmt_type,
        StatementType::Create
    );
    assert_eq!(
        classify("CREATE UNIQUE INDEX idx ON t(col)").stmt_type,
        StatementType::Create
    );
    assert_eq!(
        classify("CREATE DATABASE mydb").stmt_type,
        StatementType::Create
    );
    assert_eq!(
        classify("CREATE VIEW v AS SELECT 1").stmt_type,
        StatementType::Create
    );
    assert_eq!(
        classify("CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW SET @x=1").stmt_type,
        StatementType::Create
    );
    assert_eq!(
        classify("CREATE PROCEDURE p() BEGIN SELECT 1; END").stmt_type,
        StatementType::Create
    );
    assert_eq!(
        classify("CREATE FUNCTION f() RETURNS INT RETURN 1").stmt_type,
        StatementType::Create
    );
}

#[test]
fn alter_variants() {
    assert_eq!(
        classify("ALTER TABLE t ADD COLUMN c INT").stmt_type,
        StatementType::Alter
    );
    assert_eq!(
        classify("ALTER TABLE t DROP COLUMN c").stmt_type,
        StatementType::Alter
    );
    assert_eq!(
        classify("ALTER TABLE t MODIFY c VARCHAR(100)").stmt_type,
        StatementType::Alter
    );
    assert_eq!(
        classify("ALTER TABLE t RENAME TO t2").stmt_type,
        StatementType::Alter
    );
    assert_eq!(
        classify("ALTER DATABASE mydb READ ONLY = 1").stmt_type,
        StatementType::Alter
    );
    assert_eq!(
        classify("ALTER VIEW v AS SELECT 2").stmt_type,
        StatementType::Alter
    );
}

#[test]
fn drop_variants() {
    assert_eq!(classify("DROP TABLE t").stmt_type, StatementType::Drop);
    assert_eq!(
        classify("DROP TABLE IF EXISTS t").stmt_type,
        StatementType::Drop
    );
    assert_eq!(
        classify("DROP DATABASE mydb").stmt_type,
        StatementType::Drop
    );
    assert_eq!(
        classify("DROP INDEX idx ON t").stmt_type,
        StatementType::Drop
    );
    assert_eq!(classify("DROP VIEW v").stmt_type, StatementType::Drop);
    assert_eq!(classify("DROP PROCEDURE p").stmt_type, StatementType::Drop);
}

#[test]
fn truncate_variants() {
    assert_eq!(
        classify("TRUNCATE TABLE t").stmt_type,
        StatementType::Truncate
    );
    assert_eq!(classify("TRUNCATE t").stmt_type, StatementType::Truncate);
}

// ─── Transaction / Lock ───

#[test]
fn transaction_statements() {
    assert_eq!(classify("BEGIN").stmt_type, StatementType::Begin);
    assert_eq!(classify("BEGIN WORK").stmt_type, StatementType::Begin);
    assert_eq!(
        classify("START TRANSACTION").stmt_type,
        StatementType::Begin
    );
    assert_eq!(classify("COMMIT").stmt_type, StatementType::Commit);
    assert_eq!(classify("COMMIT WORK").stmt_type, StatementType::Commit);
    assert_eq!(classify("ROLLBACK").stmt_type, StatementType::Rollback);
    assert_eq!(
        classify("ROLLBACK TO SAVEPOINT sp1").stmt_type,
        StatementType::Rollback
    );
}

// ─── Administration ───

#[test]
fn set_variants() {
    assert_eq!(classify("SET @x = 1").stmt_type, StatementType::Set);
    assert_eq!(classify("SET NAMES utf8mb4").stmt_type, StatementType::Set);
    assert_eq!(
        classify("SET GLOBAL max_connections = 100").stmt_type,
        StatementType::Set
    );
    assert_eq!(
        classify("SET SESSION sql_mode = 'TRADITIONAL'").stmt_type,
        StatementType::Set
    );
    assert_eq!(classify("SET autocommit = 0").stmt_type, StatementType::Set);
}

#[test]
fn show_variants() {
    assert_eq!(classify("SHOW DATABASES").stmt_type, StatementType::Show);
    assert_eq!(classify("SHOW TABLES").stmt_type, StatementType::Show);
    assert_eq!(
        classify("SHOW CREATE TABLE t").stmt_type,
        StatementType::Show
    );
    assert_eq!(classify("SHOW PROCESSLIST").stmt_type, StatementType::Show);
    assert_eq!(classify("SHOW STATUS").stmt_type, StatementType::Show);
    assert_eq!(
        classify("SHOW VARIABLES LIKE '%char%'").stmt_type,
        StatementType::Show
    );
    assert_eq!(classify("SHOW WARNINGS").stmt_type, StatementType::Show);
    assert_eq!(
        classify("SHOW ENGINE INNODB STATUS").stmt_type,
        StatementType::Show
    );
}

#[test]
fn describe_variants() {
    assert_eq!(classify("DESCRIBE t").stmt_type, StatementType::Describe);
    assert_eq!(classify("DESC t").stmt_type, StatementType::Describe);
    assert_eq!(
        classify("DESCRIBE t col").stmt_type,
        StatementType::Describe
    );
    assert_eq!(
        classify("EXPLAIN SELECT 1").stmt_type,
        StatementType::Explain
    );
    assert_eq!(
        classify("EXPLAIN FORMAT=JSON SELECT 1").stmt_type,
        StatementType::Explain
    );
}

#[test]
fn use_statement() {
    assert_eq!(classify("USE mydb").stmt_type, StatementType::Use);
}

#[test]
fn call_statement() {
    assert_eq!(classify("CALL myproc(1, 2)").stmt_type, StatementType::Call);
}

#[test]
fn kill_statement() {
    assert_eq!(classify("KILL 42").stmt_type, StatementType::Kill);
    assert_eq!(
        classify("KILL CONNECTION 42").stmt_type,
        StatementType::Kill
    );
    assert_eq!(classify("KILL QUERY 42").stmt_type, StatementType::Kill);
}

// ─── Prepared Statements ───

#[test]
fn prepared_statements() {
    assert_eq!(
        classify("PREPARE stmt FROM 'SELECT 1'").stmt_type,
        StatementType::Prepare
    );
    assert_eq!(classify("EXECUTE stmt").stmt_type, StatementType::Execute);
    assert_eq!(
        classify("EXECUTE stmt USING @a, @b").stmt_type,
        StatementType::Execute
    );
    assert_eq!(
        classify("DEALLOCATE PREPARE stmt").stmt_type,
        StatementType::Deallocate
    );
}

// ─── Comments / Whitespace ───

#[test]
fn leading_comments() {
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
    assert_eq!(classify("   SELECT 1").stmt_type, StatementType::Select);
    assert_eq!(classify("\n\tSELECT 1").stmt_type, StatementType::Select);
}

#[test]
fn inline_comments() {
    assert_eq!(
        classify("SELECT /*inline*/ 1").stmt_type,
        StatementType::Select
    );
    assert_eq!(
        classify("INSERT /* hint */ INTO t VALUES(1)").stmt_type,
        StatementType::Insert
    );
}

// ─── Case Sensitivity ───

#[test]
fn case_insensitive() {
    assert_eq!(classify("select 1").stmt_type, StatementType::Select);
    assert_eq!(classify("SELECT 1").stmt_type, StatementType::Select);
    assert_eq!(classify("Select 1").stmt_type, StatementType::Select);
    assert_eq!(
        classify("insert into t values(1)").stmt_type,
        StatementType::Insert
    );
    assert_eq!(
        classify("INSERT INTO T VALUES(1)").stmt_type,
        StatementType::Insert
    );
    assert_eq!(
        classify("update t set a=1").stmt_type,
        StatementType::Update
    );
    assert_eq!(classify("delete from t").stmt_type, StatementType::Delete);
    assert_eq!(classify("show databases").stmt_type, StatementType::Show);
}

// ─── Unknown / Edge ───

#[test]
fn unknown_statements() {
    assert_eq!(classify("FOOBAR").stmt_type, StatementType::Unknown);
    assert_eq!(classify("").stmt_type, StatementType::Unknown);
    assert_eq!(classify("   \n\t  ").stmt_type, StatementType::Unknown);
}

#[test]
fn empty_after_comments() {
    // 纯注释后无语句
    assert_eq!(classify("/* comment */").stmt_type, StatementType::Unknown);
    assert_eq!(classify("-- comment\n").stmt_type, StatementType::Unknown);
}

// ─── Table name extraction ───

#[test]
fn table_extraction() {
    let s = classify("SELECT * FROM users");
    assert!(s.table_names.contains(&"users".to_string()));

    let s = classify("INSERT INTO orders (id) VALUES(1)");
    assert!(s.table_names.contains(&"orders".to_string()));

    let s = classify("UPDATE products SET price=9.99 WHERE id=1");
    assert!(s.table_names.contains(&"products".to_string()));

    let s = classify("DELETE FROM sessions WHERE expired=1");
    assert!(s.table_names.contains(&"sessions".to_string()));

    let s = classify("REPLACE INTO cache VALUES(1,2)");
    assert!(s.table_names.contains(&"cache".to_string()));
}

#[test]
fn join_table_extraction() {
    let s = classify("SELECT * FROM users JOIN orders ON users.id = orders.user_id");
    assert!(s.table_names.contains(&"users".to_string()));
    assert!(s.table_names.contains(&"orders".to_string()));

    let s = classify("SELECT * FROM a LEFT JOIN b ON a.x=b.x RIGHT JOIN c ON b.y=c.y");
    assert!(s.table_names.contains(&"a".to_string()));
    assert!(s.table_names.contains(&"b".to_string()));
    assert!(s.table_names.contains(&"c".to_string()));
}

// ─── Shard key extraction ───

#[test]
fn shard_key_extraction() {
    let s = classify("SELECT * FROM users WHERE id = 42");
    assert_eq!(s.shard_keys.len(), 1);
    assert_eq!(s.shard_keys[0].column, "id");
    assert_eq!(s.shard_keys[0].value, "42");

    let s = classify("SELECT * FROM t WHERE user_id = 100 AND status = 'active'");
    assert_eq!(s.shard_keys.len(), 2);

    let s = classify("SELECT * FROM t");
    assert!(s.shard_keys.is_empty());
}

// ─── Router hint ───

#[test]
fn router_hint_in_sql() {
    let s = classify("/*{router:cl=0,idx=2}*/ SELECT * FROM t");
    assert!(s.router_hint.is_some());
    assert_eq!(
        s.router_hint.as_ref().unwrap().cluster_id.as_deref(),
        Some("0")
    );
    assert_eq!(s.router_hint.as_ref().unwrap().tablet_index, Some(2));
}

// ─── 复杂 SQL:语句类型 + 表名(无污染)───

#[test]
fn cte_union_and_subquery_classification() {
    // CTE:WITH 后按主体 DML 分类
    let s = classify("WITH c AS (SELECT id FROM t2) SELECT * FROM sbtest_1 JOIN c ON 1=1");
    assert_eq!(s.stmt_type, StatementType::Select);
    assert!(s.table_names.contains(&"sbtest_1".to_string()));
    assert!(s.table_names.contains(&"t2".to_string()));
    assert!(!s.table_names.contains(&"c".to_string())); // CTE 名不是物理表

    let s = classify("SELECT * FROM t1 UNION SELECT * FROM t2 UNION ALL SELECT 1");
    assert_eq!(s.stmt_type, StatementType::Select);
    assert!(s.table_names.contains(&"t1".to_string()));
    assert!(s.table_names.contains(&"t2".to_string()));

    let s = classify("SELECT (SELECT MAX(x) FROM t2) FROM t1");
    assert_eq!(s.stmt_type, StatementType::Select);
    assert!(s.table_names.contains(&"t1".to_string()));
    assert!(s.table_names.contains(&"t2".to_string()));
}

#[test]
fn complex_tables_no_keyword_junk() {
    // INSERT...SELECT:目标 + 源,不含 select/from/* 垃圾词
    let s = classify("INSERT INTO t1 SELECT * FROM t2");
    assert_eq!(s.table_names, vec!["t1".to_string(), "t2".to_string()]);

    // 字符串含 FROM 不再污染表名
    let s = classify("SELECT 'a FROM b' AS s, x FROM real_t");
    assert_eq!(s.table_names, vec!["real_t".to_string()]);

    // 多表 UPDATE / DELETE
    let s = classify("UPDATE t1 JOIN t2 ON t1.id=t2.id SET t1.a=1");
    assert!(s.table_names.contains(&"t1".to_string()));
    assert!(s.table_names.contains(&"t2".to_string()));

    let s = classify("DELETE t1 FROM t1 JOIN t2 ON t1.id=t2.id WHERE t2.x=1");
    assert!(s.table_names.contains(&"t1".to_string()));
    assert!(s.table_names.contains(&"t2".to_string()));
}

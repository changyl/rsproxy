// SQL AST 节点类型——最小可用子集
// TP1: 语句分类 + 路由 hint 解析
// T3.1+: 扩展为完整 MySQL 方言语法树

use std::fmt;

/// SQL 语句类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementType {
    Select,
    Insert,
    Update,
    Delete,
    Replace,
    Set,
    Show,
    Describe,
    Explain,
    Begin,
    Commit,
    Rollback,
    Use,
    Create,
    Drop,
    Alter,
    Truncate,
    Call,
    Prepare,
    Execute,
    Deallocate,
    Kill,
    Ping,
    Quit,
    InitDb,
    Statistics,
    Unknown,
}

impl fmt::Display for StatementType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            StatementType::Select => "SELECT",
            StatementType::Insert => "INSERT",
            StatementType::Update => "UPDATE",
            StatementType::Delete => "DELETE",
            StatementType::Replace => "REPLACE",
            StatementType::Set => "SET",
            StatementType::Show => "SHOW",
            StatementType::Describe => "DESCRIBE",
            StatementType::Explain => "EXPLAIN",
            StatementType::Begin => "BEGIN",
            StatementType::Commit => "COMMIT",
            StatementType::Rollback => "ROLLBACK",
            StatementType::Use => "USE",
            StatementType::Create => "CREATE",
            StatementType::Drop => "DROP",
            StatementType::Alter => "ALTER",
            StatementType::Truncate => "TRUNCATE",
            StatementType::Call => "CALL",
            StatementType::Prepare => "PREPARE",
            StatementType::Execute => "EXECUTE",
            StatementType::Deallocate => "DEALLOCATE",
            StatementType::Kill => "KILL",
            StatementType::Ping => "PING",
            StatementType::Quit => "QUIT",
            StatementType::InitDb => "INIT_DB",
            StatementType::Statistics => "STATISTICS",
            StatementType::Unknown => "UNKNOWN",
        };
        write!(f, "{s}")
    }
}

/// 解析后的 SQL 语句
#[derive(Debug, Clone)]
pub struct ParsedStatement {
    /// 语句类型
    pub stmt_type: StatementType,
    /// 原始 SQL 文本
    pub raw_sql: String,
    /// 路由 hint（如 `/*{router:0}*/`）
    pub router_hint: Option<RouterHint>,
    /// 涉及的表名（粗提取）
    pub table_names: Vec<String>,
    /// WHERE 条件中的等值条件(候选分片键)
    pub shard_keys: Vec<ShardKeyValue>,
    /// 是否为分片 DDL（newproxy 扩展语法）
    pub is_shard_ddl: bool,
}

/// 分片键值(从 WHERE 条件中提取)
#[derive(Debug, Clone)]
pub struct ShardKeyValue {
    pub column: String,
    pub value: String,
}

/// 路由 hint——从 SQL 注释中提取
///
/// 格式: `/*{router:force_tbl=t0}*/` 或 `/*{router:cl=0,idx=1}*/`
#[derive(Debug, Clone, Default)]
pub struct RouterHint {
    /// 强制指定 cluster
    pub cluster_id: Option<String>,
    /// 强制指定 tablet 索引
    pub tablet_index: Option<usize>,
    /// 强制指定表名
    pub force_table: Option<String>,
    /// 原始 hint 文本
    pub raw: String,
}

/// 表引用——提取的表名 + 别名
#[derive(Debug, Clone)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
    pub schema: Option<String>,
}

impl ParsedStatement {
    /// 创建未分类的语句
    pub fn unknown(raw_sql: impl Into<String>) -> Self {
        Self {
            stmt_type: StatementType::Unknown,
            raw_sql: raw_sql.into(),
            router_hint: None,
            table_names: Vec::new(),
            shard_keys: Vec::new(),
            is_shard_ddl: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_type_display() {
        assert_eq!(StatementType::Select.to_string(), "SELECT");
        assert_eq!(StatementType::Insert.to_string(), "INSERT");
        assert_eq!(StatementType::Quit.to_string(), "QUIT");
    }

    #[test]
    fn statement_type_display_all_variants() {
        // 覆盖全部 Display 分支
        let cases = [
            (StatementType::Select, "SELECT"),
            (StatementType::Insert, "INSERT"),
            (StatementType::Update, "UPDATE"),
            (StatementType::Delete, "DELETE"),
            (StatementType::Replace, "REPLACE"),
            (StatementType::Set, "SET"),
            (StatementType::Show, "SHOW"),
            (StatementType::Describe, "DESCRIBE"),
            (StatementType::Explain, "EXPLAIN"),
            (StatementType::Begin, "BEGIN"),
            (StatementType::Commit, "COMMIT"),
            (StatementType::Rollback, "ROLLBACK"),
            (StatementType::Use, "USE"),
            (StatementType::Create, "CREATE"),
            (StatementType::Drop, "DROP"),
            (StatementType::Alter, "ALTER"),
            (StatementType::Truncate, "TRUNCATE"),
            (StatementType::Call, "CALL"),
            (StatementType::Prepare, "PREPARE"),
            (StatementType::Execute, "EXECUTE"),
            (StatementType::Deallocate, "DEALLOCATE"),
            (StatementType::Kill, "KILL"),
            (StatementType::Ping, "PING"),
            (StatementType::Quit, "QUIT"),
            (StatementType::InitDb, "INIT_DB"),
            (StatementType::Statistics, "STATISTICS"),
            (StatementType::Unknown, "UNKNOWN"),
        ];
        for (t, expected) in cases {
            assert_eq!(t.to_string(), expected, "{t:?}");
        }
    }

    #[test]
    fn parsed_statement_unknown() {
        let stmt = ParsedStatement::unknown("BOGUS SQL");
        assert_eq!(stmt.stmt_type, StatementType::Unknown);
        assert_eq!(stmt.raw_sql, "BOGUS SQL");
        assert!(stmt.router_hint.is_none());
        assert!(stmt.table_names.is_empty());
        assert!(stmt.shard_keys.is_empty());
        assert!(!stmt.is_shard_ddl);
    }
}

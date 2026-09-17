// Rust 原生 SQL 解析器
// 语句分类 + 路由 hint 解析 + 分片键提取 + SQL 改写
//
// 架构:lex.rs 单遍词法器(token 流,零拷贝)→ analyze.rs 结构化分析
// (查询块/CTE/UNION/子查询/多表写 → 表名与顶层 WHERE 条件)→
// classify/hint/rewrite/route 在其上提供对外 API。
// 自研替代原 C sqlparser FFI 方案;复杂 SQL(嵌套子查询/CTE/UNION/字符串
// 含关键字)不再依赖字符串子串扫描,结构上正确解析。

pub mod ast;
pub mod analyze;
pub mod lex;
pub mod classify;
pub mod hint;
pub mod rewrite;
pub mod route;

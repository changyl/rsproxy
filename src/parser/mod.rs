// Rust 原生 SQL 解析器
// 语句分类 + 路由 hint 解析 + 分片键提取 + SQL 改写
//
// 自研替代原 C sqlparser FFI 方案

pub mod ast;
pub mod classify;
pub mod hint;
pub mod rewrite;
pub mod route;

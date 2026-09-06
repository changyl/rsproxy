// sqlparser FFI 存根(未启用 sqlparser-ffi feature 时使用)
// 允许代码在不链接 sqlparser 静态库的情况下编译通过

#![allow(non_camel_case_types, non_snake_case)]

use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
pub struct sql_command_t {
    _private: [u8; 0],
}

pub type mem_pool_t = c_void;
pub type sql_parser_t = c_void;

extern "C" {
    pub fn mp_init(size: u32) -> *mut mem_pool_t;
    pub fn mp_clear(pool: *mut mem_pool_t);
    pub fn mp_free(pool: *mut mem_pool_t);
    pub fn sql_parser_init(pool: *mut mem_pool_t) -> *mut sql_parser_t;
    pub fn sql_parser_clear(p: *mut sql_parser_t);
    pub fn parse_sql(sql: *mut c_char, p: *mut sql_parser_t) -> c_int;
    pub fn sql_parser_free(p: *mut sql_parser_t);
}

// SQL 解析器 FFI 绑定(bindgen 生成 + 安全封装)
// T0.2 实现:bindgen 绑定 → ParserPool/ParserGuard/AstHandle

// sqlparser FFI 仅在 feature = "sqlparser-ffi" 时启用
#[cfg(feature = "sqlparser-ffi")]
pub mod raw; // bindgen 生成的原始绑定

#[cfg(feature = "sqlparser-ffi")]
pub mod pool; // ParserPool / ParserGuard / AstHandle

// 预留:离 FFI 边界后的安全 API
#[cfg(not(feature = "sqlparser-ffi"))]
pub mod stub;

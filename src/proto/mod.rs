// MySQL 协议层:packet 帧编解码、握手/auth、命令分发、结果集构造
pub mod auth;
pub mod codec;
pub mod command;
pub mod error;
pub mod handshake;
pub mod result;
pub mod tls;

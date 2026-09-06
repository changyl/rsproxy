// MySQL 命令分发:Command enum + COM_* 分发 + 透传
// T1.4 实现:命令枚举与基本路由
//
// 对齐 C 侧 tr_packet.c:2593 is_valid_command + tr_front_cmd.c 命令分发
// 参考 MySQL 官方 Protocol:Command Phase

use bytes::Bytes;
use std::fmt;

// ─── 命令枚举 ───

/// MySQL COM_* 命令码
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    Sleep = 0x00,
    Quit = 0x01,
    InitDb = 0x02,
    Query = 0x03,
    FieldList = 0x04,
    CreateDb = 0x05,
    DropDb = 0x06,
    Refresh = 0x07,
    Shutdown = 0x08,
    Statistics = 0x09,
    ProcessInfo = 0x0A,
    Connect = 0x0B,
    ProcessKill = 0x0C,
    Debug = 0x0D,
    Ping = 0x0E,
    Time = 0x0F,
    DelayedInsert = 0x10,
    ChangeUser = 0x11,
    BinlogDump = 0x12,
    TableDump = 0x13,
    ConnectOut = 0x14,
    RegisterSlave = 0x15,
    StmtPrepare = 0x16,
    StmtExecute = 0x17,
    StmtSendLongData = 0x18,
    StmtClose = 0x19,
    StmtReset = 0x1A,
    SetOption = 0x1B,
    StmtFetch = 0x1C,
    /// 未知命令(0x1D+ 或无定义)
    Unknown(u8),
}

impl Command {
    /// 从命令码解析
    pub fn from_u8(code: u8) -> Self {
        match code {
            0x00 => Command::Sleep,
            0x01 => Command::Quit,
            0x02 => Command::InitDb,
            0x03 => Command::Query,
            0x04 => Command::FieldList,
            0x05 => Command::CreateDb,
            0x06 => Command::DropDb,
            0x07 => Command::Refresh,
            0x08 => Command::Shutdown,
            0x09 => Command::Statistics,
            0x0A => Command::ProcessInfo,
            0x0B => Command::Connect,
            0x0C => Command::ProcessKill,
            0x0D => Command::Debug,
            0x0E => Command::Ping,
            0x0F => Command::Time,
            0x10 => Command::DelayedInsert,
            0x11 => Command::ChangeUser,
            0x12 => Command::BinlogDump,
            0x13 => Command::TableDump,
            0x14 => Command::ConnectOut,
            0x15 => Command::RegisterSlave,
            0x16 => Command::StmtPrepare,
            0x17 => Command::StmtExecute,
            0x18 => Command::StmtSendLongData,
            0x19 => Command::StmtClose,
            0x1A => Command::StmtReset,
            0x1B => Command::SetOption,
            0x1C => Command::StmtFetch,
            n => Command::Unknown(n),
        }
    }

    /// 命令码
    pub fn as_u8(self) -> u8 {
        match self {
            Command::Unknown(n) => n,
            _ => unsafe { *(<*const _>::from(&self) as *const u8) },
        }
    }

    /// 是否为"简单透传"命令(不需要 SQL 解析)
    pub fn is_pass_through(self) -> bool {
        matches!(
            self,
            Command::Query | Command::Ping | Command::Quit | Command::InitDb
        )
    }

    /// 是否为预处理语句命令
    pub fn is_prepared(self) -> bool {
        matches!(
            self,
            Command::StmtPrepare
                | Command::StmtExecute
                | Command::StmtClose
                | Command::StmtReset
                | Command::StmtSendLongData
                | Command::StmtFetch
        )
    }

    /// 是否为不修改状态的查询命令
    pub fn is_read_only(self) -> bool {
        matches!(
            self,
            Command::Query | Command::Statistics | Command::FieldList
        )
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Command::Sleep => "Sleep",
            Command::Quit => "Quit",
            Command::InitDb => "Init DB",
            Command::Query => "Query",
            Command::FieldList => "Field List",
            Command::CreateDb => "Create DB",
            Command::DropDb => "Drop DB",
            Command::Refresh => "Refresh",
            Command::Shutdown => "Shutdown",
            Command::Statistics => "Statistics",
            Command::ProcessInfo => "Process Info",
            Command::Connect => "Connect",
            Command::ProcessKill => "Process Kill",
            Command::Debug => "Debug",
            Command::Ping => "Ping",
            Command::Time => "Time",
            Command::DelayedInsert => "Delayed Insert",
            Command::ChangeUser => "Change User",
            Command::BinlogDump => "Binlog Dump",
            Command::TableDump => "Table Dump",
            Command::ConnectOut => "Connect Out",
            Command::RegisterSlave => "Register Slave",
            Command::StmtPrepare => "Stmt Prepare",
            Command::StmtExecute => "Stmt Execute",
            Command::StmtSendLongData => "Stmt Send Long Data",
            Command::StmtClose => "Stmt Close",
            Command::StmtReset => "Stmt Reset",
            Command::SetOption => "Set Option",
            Command::StmtFetch => "Stmt Fetch",
            Command::Unknown(n) => return write!(f, "Unknown(0x{:02X})", n),
        };
        write!(f, "{}", name)
    }
}

// ─── 命令包解析 ───

/// 解析后的命令包
#[derive(Debug, Clone)]
pub struct CommandPacket {
    pub command: Command,
    /// 命令载荷(不含 1B 命令码)
    pub payload: Bytes,
}

/// 从 MySQL 协议包解析命令
///
/// 格式:
/// - 1B  command code
/// - remainder: command-specific payload
pub fn parse_command_packet(pkt: &[u8]) -> Result<CommandPacket, &'static str> {
    if pkt.is_empty() {
        return Err("empty command packet");
    }
    let code = pkt[0];
    let command = Command::from_u8(code);
    let payload = Bytes::copy_from_slice(&pkt[1..]);

    Ok(CommandPacket { command, payload })
}

/// 提取 COM_QUERY 中的 SQL 文本
pub fn extract_query(payload: &[u8]) -> Result<&str, &'static str> {
    std::str::from_utf8(payload).map_err(|_| "invalid UTF-8 in query")
}

// ─── 命令路由决策 ───

/// 命令路由结果:决定如何处理一个前端命令
#[derive(Debug, Clone)]
pub enum RouteDecision {
    /// 透传到后端(不分片)
    PassThrough,
    /// 需要 SQL 解析和分片路由
    Shard,
    /// 内部处理(如 COM_PING/COM_QUIT)
    Internal,
    /// 不支持的命令
    Unsupported,
}

/// 根据命令类型和配置决定路由策略
///
/// 初期:P3 未就绪时所有 QUERY 走 PassThrough
pub fn decide_route(cmd: &CommandPacket, _shard_enabled: bool) -> RouteDecision {
    match cmd.command {
        Command::Query => {
            // P3 就绪后:解析 SQL 判断是否分片表
            // 目前全部透传
            RouteDecision::PassThrough
        }
        Command::Ping | Command::Quit => RouteDecision::Internal,
        Command::InitDb | Command::Statistics => RouteDecision::PassThrough,
        Command::StmtPrepare
        | Command::StmtExecute
        | Command::StmtClose
        | Command::StmtReset
        | Command::StmtFetch
        | Command::StmtSendLongData => {
            // 预处理语句暂不支持,透传
            RouteDecision::PassThrough
        }
        Command::FieldList | Command::CreateDb | Command::DropDb => RouteDecision::PassThrough,
        Command::Unknown(_) => RouteDecision::Unsupported,
        _ => RouteDecision::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_from_u8_all_valid() {
        for code in 0x00u8..=0x1C {
            let cmd = Command::from_u8(code);
            assert!(
                !matches!(cmd, Command::Unknown(_)),
                "code {:#04X} should be valid",
                code
            );
        }
    }

    #[test]
    fn command_unknown() {
        let cmd = Command::from_u8(0xFF);
        assert_eq!(cmd, Command::Unknown(0xFF));
    }

    #[test]
    fn parse_query_packet() {
        let sql = b"SELECT * FROM t WHERE id = 1";
        let mut pkt = vec![0x03]; // COM_QUERY
        pkt.extend_from_slice(sql);

        let cmd = parse_command_packet(&pkt).unwrap();
        assert_eq!(cmd.command, Command::Query);
        assert_eq!(
            extract_query(&cmd.payload).unwrap(),
            "SELECT * FROM t WHERE id = 1"
        );
    }

    #[test]
    fn parse_ping_packet() {
        let pkt = [0x0Eu8]; // COM_PING, no payload
        let cmd = parse_command_packet(&pkt).unwrap();
        assert_eq!(cmd.command, Command::Ping);
        assert!(cmd.payload.is_empty());
    }

    #[test]
    fn route_query_passthrough() {
        let pkt = parse_command_packet(b"\x03SELECT 1").unwrap();
        let decision = decide_route(&pkt, false);
        assert!(matches!(decision, RouteDecision::PassThrough));
    }

    #[test]
    fn route_ping_internal() {
        let pkt = parse_command_packet(b"\x0E").unwrap();
        let decision = decide_route(&pkt, false);
        assert!(matches!(decision, RouteDecision::Internal));
    }

    #[test]
    fn route_unknown_unsupported() {
        let pkt = CommandPacket {
            command: Command::Unknown(0xFE),
            payload: Bytes::new(),
        };
        let decision = decide_route(&pkt, false);
        assert!(matches!(decision, RouteDecision::Unsupported));
    }

    #[test]
    fn is_pass_through() {
        assert!(Command::Query.is_pass_through());
        assert!(Command::Ping.is_pass_through());
        assert!(Command::Quit.is_pass_through());
        assert!(!Command::StmtPrepare.is_pass_through());
    }

    #[test]
    fn extract_query_invalid_utf8() {
        assert!(extract_query(b"SELECT \xff\xfe").is_err());
        assert!(extract_query(b"\xc3\x28").is_err(), "非法 UTF-8 序列");
        assert_eq!(extract_query(b"SELECT 1").unwrap(), "SELECT 1");
    }

    #[test]
    fn route_all_command_kinds() {
        // 每个命令码的路由决策
        let route = |code: u8| {
            let pkt = parse_command_packet(&[code]).unwrap();
            decide_route(&pkt, false)
        };
        assert!(matches!(route(0x03), RouteDecision::PassThrough), "Query");
        assert!(matches!(route(0x0E), RouteDecision::Internal), "Ping");
        assert!(matches!(route(0x01), RouteDecision::Internal), "Quit");
        assert!(matches!(route(0x02), RouteDecision::PassThrough), "InitDb");
        assert!(matches!(route(0x09), RouteDecision::PassThrough), "Statistics");
        assert!(matches!(route(0x16), RouteDecision::PassThrough), "StmtPrepare");
        assert!(matches!(route(0x17), RouteDecision::PassThrough), "StmtExecute");
        assert!(matches!(route(0x18), RouteDecision::PassThrough), "SendLongData");
        assert!(matches!(route(0x19), RouteDecision::PassThrough), "StmtClose");
        assert!(matches!(route(0x1A), RouteDecision::PassThrough), "StmtReset");
        assert!(matches!(route(0x1C), RouteDecision::PassThrough), "StmtFetch");
        assert!(matches!(route(0x04), RouteDecision::PassThrough), "FieldList");
        assert!(matches!(route(0x05), RouteDecision::PassThrough), "CreateDb");
        assert!(matches!(route(0x06), RouteDecision::PassThrough), "DropDb");
        assert!(matches!(route(0x1D), RouteDecision::Unsupported), "0x1D 以上未知");
        assert!(matches!(route(0xFF), RouteDecision::Unsupported), "0xFF 未知");
    }

    #[test]
    fn command_from_u8_unknown_range() {
        // 0x1D..=0xFF 未定义 → Unknown
        for code in 0x1Du8..=0xFF {
            assert!(
                matches!(Command::from_u8(code), Command::Unknown(_)),
                "0x{code:02X} 应为 Unknown"
            );
        }
    }

    #[test]
    fn parse_command_packet_short_payload() {
        // 空包 → 错误
        assert!(parse_command_packet(&[]).is_err());
    }

    #[test]
    fn command_display() {
        assert_eq!(format!("{}", Command::Query), "Query");
        assert_eq!(format!("{}", Command::Ping), "Ping");
        assert_eq!(format!("{}", Command::Unknown(0xAB)), "Unknown(0xAB)");
    }
}

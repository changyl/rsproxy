// 管理命令:checkproxy SQL 格式的运维命令
// T4.3 实现:SHOW/STATS/KILL/RELOAD 等管理原语
//
// 对齐 C 侧:管理命令走业务端口 4051,以 checkproxy 前缀 SQL 形式接入

/// 管理 HTTP 服务(Web 面板 / JSON API / Prometheus 指标 / 健康检查,
/// 挂在 mng_port;Basic Auth 鉴权)——见 docs/13-observability-design.md
pub mod http;
/// 进程资源采样(CPU/内存/FD),面板与 Prometheus 负载指标数据源
pub mod proc;

use bytes::Bytes;

use crate::config::AppConfig;
use crate::metric::Metrics;
use crate::proto::error;
use crate::proto::handshake;

/// 管理命令解析结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MgmtCommand {
    /// SHOW STATUS / STATS
    ShowStats,
    /// SHOW CONNECTIONS
    ShowConnections,
    /// SHOW POOL
    ShowPool,
    /// SHOW SQL STATS
    ShowSqlStats,
    /// SHOW CONFIG
    ShowConfig,
    /// KILL CONNECTION <id>
    KillConnection(u32),
    /// RELOAD CONFIG (热加载,推迟)
    Reload,
    /// HELP — 列出所有内部管理命令
    Help,
    /// 未知管理命令
    Unknown(String),
}

impl MgmtCommand {
    /// 从 SQL 文本解析管理命令
    ///
    /// 格式: checkproxy <subcommand>
    pub fn parse(sql: &str) -> Option<Self> {
        Self::parse_normalized(&sql.trim().to_lowercase())
    }

    /// 从已 trim + 小写化的 SQL 文本解析管理命令。
    ///
    /// 热路径用:前端对每个 COM_QUERY 只做一次小写化,同时服务 help 检测
    /// 与本解析,避免每个查询重复分配小写字符串。
    pub fn parse_normalized(lower: &str) -> Option<Self> {
        if !lower.starts_with("checkproxy") {
            return None; // 非管理命令
        }

        let rest = lower["checkproxy".len()..].trim();
        let cmd = match rest {
            s if s.starts_with("show status") || s.starts_with("stats") => MgmtCommand::ShowStats,
            s if s.starts_with("show connections") || s.starts_with("show processlist") => {
                MgmtCommand::ShowConnections
            }
            s if s.starts_with("show pool") => MgmtCommand::ShowPool,
            s if s.starts_with("show sql") || s.starts_with("show query") => {
                MgmtCommand::ShowSqlStats
            }
            s if s.starts_with("show config") || s == "config" => MgmtCommand::ShowConfig,
            s if s.starts_with("kill") || s.starts_with("kill connection") => {
                let id_str = s.split_whitespace().last().unwrap_or("0");
                let id = id_str.parse().unwrap_or(0);
                MgmtCommand::KillConnection(id)
            }
            s if s.starts_with("reload") => MgmtCommand::Reload,
            // help / ? / 裸 checkproxy → 显示帮助
            s if s.is_empty() || s == "?" || s.starts_with("help") => MgmtCommand::Help,
            _ => MgmtCommand::Unknown(rest.to_string()),
        };

        Some(cmd)
    }

    /// 执行管理命令,返回响应包
    ///
    /// `config` 仅在 ShowConfig 时需要,其余命令传 None
    pub fn execute(&self, metrics: &Metrics, config: Option<&AppConfig>) -> Bytes {
        if let MgmtCommand::Unknown(cmd) = self {
            return error::build_error(1064, "42000", &format!("Unknown checkproxy command: {cmd}"));
        }
        // OK-info 兜底路径:front.rs 会拦截这些命令并渲染成结果集
        // (批量模式下 OK 包 info 文本不可见),这里仅作非拦截路径的回退。
        let info = self.info_text(metrics, config);
        error::build_ok(0, 0, handshake::SERVER_STATUS_AUTOCOMMIT, 0, Some(&info))
    }

    /// 管理命令的可读多行文本。
    ///
    /// front.rs 把它按行渲染成单列结果集;execute() 的 OK-info 兜底也复用。
    pub fn info_text(&self, metrics: &Metrics, config: Option<&AppConfig>) -> String {
        match self {
            MgmtCommand::ShowStats => {
                let summary = metrics.summary();
                format!(
                    "Uptime: N/A\n\
                     Connections: total={} active={} rejected={}\n\
                     Queries: total={} errors={} slow={}\n\
                     Traffic: recv={} sent={}\n\
                     Pool: acquires={} fails={}",
                    summary.connections_total,
                    summary.connections_active,
                    summary.connections_rejected,
                    summary.queries_total,
                    summary.queries_errors,
                    summary.queries_slow,
                    summary.bytes_received,
                    summary.bytes_sent,
                    summary.pool_acquires,
                    summary.pool_acquire_fails,
                )
            }
            MgmtCommand::ShowConnections => {
                "Connections: (not yet tracked per-connection)\n".into()
            }
            MgmtCommand::ShowPool => "Pool: (not yet populated)\n".into(),
            MgmtCommand::ShowSqlStats => {
                let top = metrics.top_queries(10);
                let mut info = String::from("Top Queries:\n");
                for (i, ((sql, shard), stat)) in top.iter().enumerate() {
                    info.push_str(&format!(
                        "{}. [{}] {} (count={}, avg={}us, max={}us)\n",
                        i + 1,
                        shard,
                        sql,
                        stat.count,
                        stat.total_time_us.checked_div(stat.count).unwrap_or(0),
                        stat.max_time_us,
                    ));
                }
                info
            }
            MgmtCommand::ShowConfig => match config {
                Some(cfg) => format_config(cfg),
                None => "Config: (not available)\n".to_string(),
            },
            MgmtCommand::KillConnection(id) => {
                format!("KILL CONNECTION {id}: not implemented\n")
            }
            MgmtCommand::Reload => "RELOAD: not implemented (requires restart)\n".into(),
            MgmtCommand::Help => help_text(),
            MgmtCommand::Unknown(cmd) => format!("Unknown checkproxy command: {cmd}\n"),
        }
    }
}

/// 将 AppConfig 格式化为可读的多行字符串
fn format_config(cfg: &AppConfig) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "[MySQL_Proxy_Layer]\n\
         port            = {}\n\
         mng_port        = {}\n\
         max_threads     = {}\n\
         log_dir         = {}\n\
         log_level       = {:?}\n\
         client_timeout  = {}s\n\
         server_timeout  = {}s\n\
         max_serve_times = {}\n\
         max_sql_size    = {}\n\
         max_query_size  = {}\n\
         default_charset = {} (collation {})\n\
         stream_on       = {}\n",
        cfg.port,
        cfg.mng_port,
        cfg.max_threads,
        cfg.log_dir,
        cfg.log_level,
        cfg.front_idle_timeout,
        cfg.backend_idle_timeout,
        cfg.conn_pool_socket_max_serve_client_times,
        cfg.max_sql_size,
        cfg.max_query_size,
        crate::config::model::charset_id_to_name(cfg.default_charset),
        cfg.default_charset,
        cfg.stream_on,
    ));

    s.push_str(&format!("\nClients: {} total\n", cfg.product_users.len()));
    for (name, pu) in &cfg.product_users {
        s.push_str(&format!(
            "  user={} -> db_user={} cluster={} max_conn={}\n",
            name, pu.db_username, pu.cluster_name, pu.max_connections,
        ));
    }

    s.push_str(&format!("\nBackends: {} total\n", cfg.db_users.len()));
    for (name, db) in &cfg.db_users {
        s.push_str(&format!(
            "  db_user={} default_db='{}' cluster={}\n",
            name,
            db.default_db.as_deref().unwrap_or(""),
            db.cluster_name,
        ));
    }

    s.push_str(&format!("\nClusters: {}\n", cfg.clusters.len()));
    for (cid, cluster) in &cfg.clusters {
        s.push_str(&format!(
            "  [{}] name={} tablets={}\n",
            cid,
            cluster.name,
            cluster.tablets.len(),
        ));
        for tablet in &cluster.tablets {
            let master = tablet
                .groups
                .iter()
                .filter_map(|g| g.master.as_ref())
                .collect::<Vec<_>>();
            s.push_str(&format!(
                "    tablet={} masters={} slaves={}\n",
                tablet.tablet_id,
                master.len(),
                tablet.groups.iter().filter(|g| g.slave.is_some()).count(),
            ));
            for m in master {
                s.push_str(&format!(
                    "      master {}:{} pool={} conn={} weight={}\n",
                    m.host, m.port, m.max_pool_size, m.max_connections, m.weight,
                ));
            }
        }
    }

    s.push_str(&format!("\nAuth IPs: {} rules\n", cfg.auth_ips.len()));
    s.push_str(&format!("Ignore IPs: {} rules\n", cfg.ignore_ips.len()));

    s
}

/// 判断是否为 `help` 查询(大小写/空白/尾分号不敏感)。
///
/// mysql 客户端在交互模式把裸 `help` 当客户端命令本地处理,不会发给服务器;
/// 但在多语句批量(`-e 'select 1; help'`)或部分客户端工具下,`help` 会作为
/// COM_QUERY 到达代理。此处统一识别,由代理本地返回内置管理命令。
pub fn is_help_query(sql: &str) -> bool {
    let lower = sql.trim().trim_end_matches(';').trim().to_lowercase();
    lower == "help" || lower.starts_with("help ")
}

/// 内置管理命令清单(完整调用形式, 说明)——help 文本与 help 结果集共用
pub const MGMT_COMMANDS: &[(&str, &str)] = &[
    (
        "checkproxy show status | stats",
        "运行状态与统计 (连接/查询/流量/连接池)",
    ),
    ("checkproxy show connections", "当前连接列表 (processlist)"),
    ("checkproxy show pool", "后端连接池状态"),
    ("checkproxy show sql | show query", "Top SQL 统计"),
    ("checkproxy show config | config", "当前运行配置"),
    ("checkproxy kill <connection_id>", "关闭指定连接"),
    ("checkproxy reload", "热加载配置 (暂未实现)"),
    ("checkproxy help | help | ?", "显示本帮助"),
];

/// 生成 `checkproxy help` / `help` 的帮助文本,列出所有内部管理命令
pub(crate) fn help_text() -> String {
    let mut s = String::from(
        "NewProxy newproxy 内置管理命令\n\
         用法: checkproxy <command> 或 help\n\n",
    );
    for (cmd, desc) in MGMT_COMMANDS {
        s.push_str(&format!("  {:<38}{}\n", cmd, desc));
    }
    s.push_str("\n示例:\n  mysql> checkproxy show status;\n  mysql> checkproxy kill 42;\n  mysql> help;\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_show_stats() {
        let cmd = MgmtCommand::parse("checkproxy show status").unwrap();
        assert_eq!(cmd, MgmtCommand::ShowStats);
    }

    #[test]
    fn parse_show_connections() {
        let cmd = MgmtCommand::parse("checkproxy show connections").unwrap();
        assert_eq!(cmd, MgmtCommand::ShowConnections);
    }

    #[test]
    fn parse_show_config() {
        let cmd = MgmtCommand::parse("checkproxy show config").unwrap();
        assert_eq!(cmd, MgmtCommand::ShowConfig);
        let cmd2 = MgmtCommand::parse("checkproxy config").unwrap();
        assert_eq!(cmd2, MgmtCommand::ShowConfig);
    }

    #[test]
    fn parse_kill() {
        let cmd = MgmtCommand::parse("checkproxy kill 42").unwrap();
        assert_eq!(cmd, MgmtCommand::KillConnection(42));
    }

    #[test]
    fn parse_help() {
        // help / ? / 裸 checkproxy 均解析为 Help
        assert_eq!(
            MgmtCommand::parse("checkproxy help").unwrap(),
            MgmtCommand::Help
        );
        assert_eq!(
            MgmtCommand::parse("checkproxy ?").unwrap(),
            MgmtCommand::Help
        );
        assert_eq!(MgmtCommand::parse("checkproxy").unwrap(), MgmtCommand::Help);
        assert_eq!(
            MgmtCommand::parse("CHECKPROXY HELP").unwrap(),
            MgmtCommand::Help
        );
    }

    #[test]
    fn execute_help() {
        let metrics = Metrics::new();
        let resp = MgmtCommand::Help.execute(&metrics, None);
        assert_eq!(resp[0], 0x00); // OK 包
        let text = String::from_utf8_lossy(&resp[7..]);
        // 帮助文本应列出关键管理命令
        assert!(text.contains("checkproxy"));
        assert!(text.contains("show status"));
        assert!(text.contains("show config"));
        assert!(text.contains("kill"));
        assert!(text.contains("help"));
    }

    #[test]
    fn parse_not_mgmt() {
        assert!(MgmtCommand::parse("SELECT 1").is_none());
        assert!(MgmtCommand::parse("SHOW STATUS").is_none());
    }

    #[test]
    fn is_help_query_variants() {
        // 裸 help / 带参数 / 大小写 / 尾分号 均识别;非 help 查询不识别
        assert!(is_help_query("help"));
        assert!(is_help_query("help;"));
        assert!(is_help_query("HELP"));
        assert!(is_help_query("help contents"));
        assert!(is_help_query(" help "));
        assert!(!is_help_query("helpful"));
        assert!(!is_help_query("SELECT 1"));
        assert!(!is_help_query("checkproxy help"));
    }

    #[test]
    fn parse_aliases_and_pool_reload() {
        // 别名:stats / processlist / query / config
        assert_eq!(MgmtCommand::parse("checkproxy stats").unwrap(), MgmtCommand::ShowStats);
        assert_eq!(
            MgmtCommand::parse("checkproxy show processlist").unwrap(),
            MgmtCommand::ShowConnections
        );
        assert_eq!(MgmtCommand::parse("checkproxy show pool").unwrap(), MgmtCommand::ShowPool);
        assert_eq!(
            MgmtCommand::parse("checkproxy show query").unwrap(),
            MgmtCommand::ShowSqlStats
        );
        assert_eq!(
            MgmtCommand::parse("checkproxy reload").unwrap(),
            MgmtCommand::Reload
        );
        // kill 非数字 id → 0
        assert_eq!(
            MgmtCommand::parse("checkproxy kill abc").unwrap(),
            MgmtCommand::KillConnection(0)
        );
        assert_eq!(
            MgmtCommand::parse("checkproxy kill connection 7").unwrap(),
            MgmtCommand::KillConnection(7)
        );
        // 未知命令
        assert_eq!(
            MgmtCommand::parse("checkproxy bogus x").unwrap(),
            MgmtCommand::Unknown("bogus x".to_string())
        );
        // 大小写不敏感
        assert_eq!(MgmtCommand::parse("CHECKPROXY SHOW POOL").unwrap(), MgmtCommand::ShowPool);
    }

    #[test]
    fn execute_unknown_returns_1064() {
        let metrics = Metrics::new();
        let resp = MgmtCommand::Unknown("bogus".to_string()).execute(&metrics, None);
        assert_eq!(resp[0], 0xFF, "ERR 包首字节");
        let text = String::from_utf8_lossy(&resp[9..]);
        assert!(text.contains("bogus"), "got: {text}");
    }

    #[test]
    fn info_text_all_variants() {
        let metrics = Metrics::new();
        // ShowStats 输出汇总字段
        let s = MgmtCommand::ShowStats.info_text(&metrics, None);
        assert!(s.contains("Connections: total="));
        assert!(s.contains("Queries: total="));
        assert!(s.contains("Traffic: recv="));
        assert!(s.contains("Pool: acquires="));
        // 连接/池/SQL 统计
        assert!(MgmtCommand::ShowConnections.info_text(&metrics, None).contains("Connections:"));
        assert!(MgmtCommand::ShowPool.info_text(&metrics, None).contains("Pool:"));
        let q = MgmtCommand::ShowSqlStats.info_text(&metrics, None);
        assert!(q.contains("Queries"));
        // ShowConfig:有配置与无配置
        let c = MgmtCommand::ShowConfig.info_text(&metrics, Some(&AppConfig::default()));
        assert!(c.contains("port") || c.contains("Port"), "got: {c}");
        assert!(MgmtCommand::ShowConfig.info_text(&metrics, None).contains("not available"));
        // kill / reload / help
        assert!(MgmtCommand::KillConnection(3).info_text(&metrics, None).contains("KILL CONNECTION"));
        assert!(MgmtCommand::Reload.info_text(&metrics, None).contains("RELOAD"));
        assert!(MgmtCommand::Help.info_text(&metrics, None).contains("用法"));
    }

    #[test]
    fn execute_show_stats() {
        let metrics = Metrics::new();
        let cmd = MgmtCommand::ShowStats;
        let resp = cmd.execute(&metrics, None);
        // 验证是 OK 包格式
        assert_eq!(resp[0], 0x00);
    }

    #[test]
    fn execute_show_config() {
        let metrics = Metrics::new();
        let cfg = AppConfig::default();
        let cmd = MgmtCommand::ShowConfig;
        let resp = cmd.execute(&metrics, Some(&cfg));
        assert_eq!(resp[0], 0x00); // OK 包
        let text = String::from_utf8_lossy(&resp[7..]); // 跳过头
        assert!(text.contains("port"));
        assert!(text.contains("4051"));
        assert!(text.contains("max_threads"));
    }
}

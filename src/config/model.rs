// 配置数据模型:集群/分片/后端/用户等结构化定义
// T0.3 最小集 + T4.1 全量扩展
// 对齐 C 侧 tr_config.h:276-364 struct tr_config_s

use std::collections::HashMap;
use std::fmt;
use std::net::Ipv4Addr;

// ─── 基础类型 ───

/// 集群标识
pub type ClusterId = String;
/// 分片标识
pub type TabletId = String;
/// 用户名
pub type UserId = String;
/// 组标识(如 group_0)
pub type GroupId = String;

/// 主从标识
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MasterSlave {
    Master = 0,
    Slave = 1,
}

impl fmt::Display for MasterSlave {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MasterSlave::Master => write!(f, "master"),
            MasterSlave::Slave => write!(f, "slave"),
        }
    }
}

/// 分片策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardStrategy {
    HashMod,
    Md5HashMod,
    Range,
    List,
    Pcre,
}

/// 路由规则(简版)
#[derive(Debug, Clone)]
pub struct RouteRule {
    pub table_name: String,
    pub strategy: ShardStrategy,
    pub partition_key: String,
    pub tablet_indices: Vec<usize>,
    /// PCRE 正则(仅 strategy=Pcre 时使用)
    pub pcre_pattern: Option<String>,
}

// ─── 执行计划绑定(SQL hint 注入)───

/// 执行计划绑定规则:SQL 模板命中时,代理在语句关键字后注入优化器 hint,
/// 使后端按固定执行计划执行(等价于数据库 plan binding/outline 的代理层实现)。
#[derive(Debug, Clone)]
pub struct PlanBinding {
    /// SQL 匹配模板(大小写不敏感子串匹配)
    pub sql_pattern: String,
    /// 注入的优化器 hint 原文(含 `/*+ ... */` 注释)
    pub hint: String,
}

// ─── 后端数据库 ───

/// 后端 MySQL 数据库实例
#[derive(Debug, Clone)]
pub struct Database {
    /// 主机: IP 地址或主机名(Docker 服务名 / DNS 域名)
    /// 运行时由 tokio TcpStream::connect 解析,支持 IPv4/主机名
    pub host: String,
    pub port: u16,
    /// 最大连接池大小(max_conn_pool_size)
    pub max_pool_size: u32,
    /// 最大连接数(max_connections)
    pub max_connections: u32,
    /// 连接超时(秒)
    pub connect_timeout: u32,
    /// 权重(负载均衡用)
    pub weight: u32,
    /// 所属分片名
    pub tablet_name: Option<String>,
}

/// 数据库组:一组主从(相同集群+分片+用户的 M/S pair)
#[derive(Debug, Clone)]
pub struct DatabaseGroup {
    pub group_id: GroupId,
    pub master: Option<Database>,
    pub slave: Option<Database>,
}

// ─── Xenon Raft 高可用(XenonRaft 段)───

/// 读一致性档位(只作用于"可分流纯读";详见 ha/consistency 与文档)。
///
/// 语义承诺由强到弱:
/// - `Strong`(内置默认):全走 raft leader(线性化),零额外成本;
/// - `Causal`:高水位 GTID 屏障读,满足跨客户端因果(读 = 主库某提交前缀);
/// - `Session`:会话水位屏障,读己之写 + 本会话单调读(不约束其它客户端);
/// - `Eventual`:免屏障直读 follower,允许任意滞后/乱序(业务自担)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ReadConsistency {
    #[default]
    Strong,
    Causal,
    Session,
    Eventual,
}

impl ReadConsistency {
    /// 大小写不敏感解析;未知返回 None。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "strong" => Some(ReadConsistency::Strong),
            "causal" => Some(ReadConsistency::Causal),
            "session" => Some(ReadConsistency::Session),
            "eventual" => Some(ReadConsistency::Eventual),
            _ => None,
        }
    }
}

impl fmt::Display for ReadConsistency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ReadConsistency::Strong => "strong",
            ReadConsistency::Causal => "causal",
            ReadConsistency::Session => "session",
            ReadConsistency::Eventual => "eventual",
        };
        write!(f, "{s}")
    }
}

/// Xenon raft 成员(一个成员 = 一台部署了 xenon 且其 MySQL 为同一数据集的节点)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaftMember {
    /// MySQL 主机(xenon 与 MySQL 同机部署时的 raft 端点主机)
    pub host: String,
    /// 该成员 MySQL 端口
    pub mysql_port: u16,
    /// xenon raft 端点(形如 `xenon1:8801`),用于对齐状态表 `leader` 列
    pub raft_endpoint: Option<String>,
}

impl RaftMember {
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.mysql_port)
    }
}

/// 分片级 Xenon raft 管理配置(`[XenonRaft_<cluster>_<tablet>]` 段)。
///
/// 提供:成员列表(探测目标 + follower 来源)、探测节奏、一致性档位默认与
/// 库/用户覆盖、GTID 屏障参数。
#[derive(Debug, Clone)]
pub struct XenonRaft {
    /// raft 全量成员(探测与 follower 池的来源)
    pub members: Vec<RaftMember>,
    /// 状态表探测周期(ms)
    pub probe_interval_ms: u64,
    /// 单成员探测超时(ms)
    pub probe_timeout_ms: u64,
    /// 状态行允许的最长新鲜度(ms);超过视为无效
    pub leader_stale_ms: u64,
    /// 分片默认读一致性档位
    pub read_consistency: ReadConsistency,
    /// 库级覆盖:db 名(小写)→ 档位
    pub db_overrides: HashMap<String, ReadConsistency>,
    /// 产品用户级覆盖:用户名 → 档位
    pub user_overrides: HashMap<String, ReadConsistency>,
    /// WAIT_FOR_EXECUTED_GTID_SET 预算(ms)
    pub barrier_wait_ms: u64,
    /// leader @@GLOBAL.gtid_executed 采样缓存(ms)
    pub gtid_sample_cache_ms: u64,
}

impl Default for XenonRaft {
    fn default() -> Self {
        Self {
            members: Vec::new(),
            probe_interval_ms: 1000,
            probe_timeout_ms: 500,
            leader_stale_ms: 3000,
            read_consistency: ReadConsistency::Strong,
            db_overrides: HashMap::new(),
            user_overrides: HashMap::new(),
            barrier_wait_ms: 200,
            gtid_sample_cache_ms: 30,
        }
    }
}

// ─── 分片(ClusterTablet) ───

/// 分片(对应 C tr_cluster_tablet_t)
#[derive(Debug, Clone)]
pub struct ClusterTablet {
    pub cluster_id: ClusterId,
    pub tablet_id: TabletId,
    /// tablet 在集群内的索引(0-based)
    pub index: usize,
    /// 数据库组(一组 M/S),按 group_id 索引
    pub groups: Vec<DatabaseGroup>,
    /// 路由规则
    pub routes: Vec<RouteRule>,
    /// Xenon raft 管理配置(无 = 静态主从,现状行为)
    pub xenon: Option<XenonRaft>,
}

// ─── 集群 ───

/// 集群配置(对应 C tr_cluster_t)
#[derive(Debug, Clone)]
pub struct Cluster {
    pub id: ClusterId,
    pub name: String,
    /// 此集群下的分片列表(按 index 排序)
    pub tablets: Vec<ClusterTablet>,
}

// ─── 用户 ───

/// 数据库用户(后端认证)
#[derive(Debug, Clone)]
pub struct DbUser {
    pub username: String,
    pub password: String,
    /// 默认数据库
    pub default_db: Option<String>,
    /// 所属集群
    pub cluster_name: String,
}

/// 产品用户(前端认证)
#[derive(Debug, Clone)]
pub struct ProductUser {
    pub username: String,
    pub password: String,
    /// 映射到的后端数据库用户
    pub db_username: String,
    /// 最大连接数(同一产品用户并发上限)
    pub max_connections: u32,
    /// 所属集群
    pub cluster_name: String,
    /// 预计算的 scramble_password
    pub scramble_password: Option<Vec<u8>>,
}

impl ProductUser {
    /// 默认数据库(从 db_username 对应的 DbUser 获取)
    /// 实际由调用方通过 DbUser 提供,此处为占位
    pub fn default_db(&self) -> Option<&str> {
        None
    }
}

// ─── IP 黑白名单 ───

/// IP 认证规则
#[derive(Debug, Clone)]
pub struct AuthIp {
    pub ip: Ipv4Addr,
    pub mask: u32,
    /// 允许的用户列表(空=所有人)
    pub users: Vec<String>,
}

// ─── 顶级配置 ───

/// 应用完整配置(对应 C tr_config_t)
#[derive(Debug, Clone)]
pub struct AppConfig {
    // [MySQL_Proxy_Layer]
    pub port: u16,
    pub mng_port: u16,
    pub max_threads: usize,
    pub log_dir: String,
    pub log_level: LogLevel,
    pub front_idle_timeout: u32,
    pub backend_idle_timeout: u32,
    pub conn_pool_socket_max_serve_client_times: u64,
    pub max_sql_size: usize,
    pub max_query_size: usize,
    pub default_charset: u8,
    pub stream_on: u8,
    /// 配置文件变更自动热加载的轮询间隔(秒);0 = 关闭自动热加载
    pub reload_interval_secs: u64,

    /// 慢查询阈值(ms):超过即计入慢查询(日志 WARN + /api/slow + 面板),
    /// 配置热加载动态生效(改配置后 checkproxy reload 或面板 RELOAD)
    pub slow_query_ms: u64,

    /// 执行计划绑定规则列表(SQL 模板 → hint 注入),热加载动态生效
    pub plan_bindings: Vec<PlanBinding>,

    /// 管理 HTTP 面板/API 的 Basic Auth 用户名(空 = 不鉴权;生产必须配置)
    pub mng_user: Option<String>,
    /// 管理 HTTP 面板/API 的 Basic Auth 密码
    pub mng_password: Option<String>,

    /// 配置中心(拓扑热更新的外部来源;kind=none 时仅用文件配置)
    pub config_center: ConfigCenterCfg,

    // 集群
    pub clusters: HashMap<ClusterId, Cluster>,
    /// cluster.tablet_name → ClusterTablet 快速映射
    pub cluster_tablets: HashMap<String, ClusterTablet>,

    // 认证
    pub db_users: HashMap<UserId, DbUser>,
    pub product_users: HashMap<UserId, ProductUser>,
    pub auth_ips: Vec<AuthIp>,
    pub ignore_ips: Vec<(Ipv4Addr, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// 映射为 tracing EnvFilter 的级别指令字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

/// 配置中心配置(`[ConfigCenter]` 段)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigCenterCfg {
    /// 类型:`none`(默认,仅文件配置) | `etcd` | `zookeeper`
    pub kind: String,
    /// 端点:etcd 形如 `http://127.0.0.1:2379`;zk 形如 `127.0.0.1:2181`(逗号分隔多个)
    pub endpoints: Vec<String>,
    /// 根路径/前缀,默认 `/newproxy`
    pub root: String,
}

impl Default for ConfigCenterCfg {
    fn default() -> Self {
        Self {
            kind: "none".to_string(),
            endpoints: Vec::new(),
            root: "/newproxy".to_string(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: 4051,
            mng_port: 9111,
            max_threads: 4,
            log_dir: "logs".into(),
            log_level: LogLevel::Info,
            front_idle_timeout: 28800,
            backend_idle_timeout: 28800,
            conn_pool_socket_max_serve_client_times: 10000,
            max_sql_size: 16 * 1024 * 1024,
            max_query_size: 16 * 1024 * 1024,
            default_charset: 33,
            stream_on: 0,
            reload_interval_secs: 5,
            // 慢查询阈值默认 200ms(可配置,动态生效)
            slow_query_ms: 200,
            plan_bindings: Vec::new(),
            // 管理面板鉴权:默认不鉴权(配置文件中显式配置 mng_user/mng_password
            // 才启用);生产环境务必显式配置,否则面板/API 无保护。
            mng_user: None,
            mng_password: None,
            config_center: ConfigCenterCfg::default(),
            clusters: HashMap::new(),
            cluster_tablets: HashMap::new(),
            db_users: HashMap::new(),
            product_users: HashMap::new(),
            auth_ips: Vec::new(),
            ignore_ips: Vec::new(),
        }
    }
}

/// 人类可读字符集名称 → MySQL collation id。
///
/// 握手包里的 charset 字节实际是 collation id(不是字符集编号),这里只收录
/// 最常用的几个默认 collation;配置文件可写名称也可写数字 id(向后兼容)。
pub fn charset_name_to_id(name: &str) -> Option<u8> {
    match name.trim().to_ascii_lowercase().as_str() {
        "big5" => Some(1),                        // big5_chinese_ci
        "latin1" => Some(8),                      // latin1_swedish_ci
        "gb2312" => Some(24),                     // gb2312_chinese_ci
        "gbk" => Some(28),                        // gbk_chinese_ci
        "utf8" | "utf-8" | "utf8mb3" => Some(33), // utf8_general_ci
        "utf8mb4" => Some(45),                    // utf8mb4_general_ci
        "binary" => Some(63),                     // binary
        _ => None,
    }
}

/// collation id → 人类可读名称(用于 `checkproxy show status` 展示)。
/// 未收录的 id 原样返回数字字符串。
pub fn charset_id_to_name(id: u8) -> String {
    match id {
        1 => "big5".into(),
        8 => "latin1".into(),
        24 => "gb2312".into(),
        28 => "gbk".into(),
        33 => "utf8".into(),
        45 => "utf8mb4".into(),
        63 => "binary".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_slave_display() {
        assert_eq!(MasterSlave::Master.to_string(), "master");
        assert_eq!(MasterSlave::Slave.to_string(), "slave");
    }

    #[test]
    fn log_level_as_str_all() {
        assert_eq!(LogLevel::Debug.as_str(), "debug");
        assert_eq!(LogLevel::Info.as_str(), "info");
        assert_eq!(LogLevel::Warn.as_str(), "warn");
        assert_eq!(LogLevel::Error.as_str(), "error");
    }

    #[test]
    fn product_user_default_db_placeholder() {
        let pu = ProductUser {
            username: "u".into(),
            password: "p".into(),
            db_username: "d".into(),
            max_connections: 1,
            cluster_name: "c".into(),
            scramble_password: None,
        };
        assert!(pu.default_db().is_none());
    }

    #[test]
    fn config_center_and_app_defaults() {
        let cc = ConfigCenterCfg::default();
        assert_eq!(cc.kind, "none");
        assert!(cc.endpoints.is_empty());
        assert_eq!(cc.root, "/newproxy");
        let cfg = AppConfig::default();
        assert_eq!(cfg.port, 4051);
        assert_eq!(cfg.mng_port, 9111);
        assert!(cfg.clusters.is_empty());
        assert!(cfg.plan_bindings.is_empty());
        assert_eq!(cfg.slow_query_ms, 200);
    }

    #[test]
    fn read_consistency_parse_display() {
        assert_eq!(ReadConsistency::default(), ReadConsistency::Strong);
        assert_eq!(ReadConsistency::parse("strong"), Some(ReadConsistency::Strong));
        assert_eq!(ReadConsistency::parse("STRONG"), Some(ReadConsistency::Strong));
        assert_eq!(ReadConsistency::parse("causal"), Some(ReadConsistency::Causal));
        assert_eq!(ReadConsistency::parse("Session"), Some(ReadConsistency::Session));
        assert_eq!(ReadConsistency::parse("eventual"), Some(ReadConsistency::Eventual));
        assert_eq!(ReadConsistency::parse(" bogus "), None);
        assert_eq!(ReadConsistency::Causal.to_string(), "causal");
        assert_eq!(ReadConsistency::Eventual.to_string(), "eventual");
    }

    #[test]
    fn xenon_raft_defaults_sane() {
        let x = XenonRaft::default();
        assert!(x.members.is_empty());
        assert_eq!(x.probe_interval_ms, 1000);
        assert_eq!(x.read_consistency, ReadConsistency::Strong);
        assert!(x.db_overrides.is_empty() && x.user_overrides.is_empty());
        assert!(x.members.is_empty());
        let m = RaftMember {
            host: "xenon1".into(),
            mysql_port: 3306,
            raft_endpoint: Some("xenon1:8801".into()),
        };
        assert_eq!(m.endpoint(), "xenon1:3306");
    }
}

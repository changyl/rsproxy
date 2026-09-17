// 配置解析:newproxy.conf INI 格式(GKeyFile: [section] key=value, # 注释)
// T0.3 实现最小集,T4.1 全量补齐
//
// C 侧入口:tr_config.c:2284 tr_read_config → tr_config.c:2300 g_key_file_load_from_file
// Section 分发:tr_config.c:2313-2371 前缀匹配

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::Path;
use std::str::FromStr;

use ini::{Ini, Properties};

use crate::config::model::*;

// ─── 错误类型 ───

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("INI parse error: {0}")]
    Ini(#[from] ini::Error),
    #[error("Missing required field in section [{section}]: {field}")]
    MissingField { section: String, field: String },
    #[error("Invalid value in section [{section}]: {field} = {value} ({reason})")]
    InvalidValue {
        section: String,
        field: String,
        value: String,
        reason: String,
    },
    #[error("Cluster not found: {0}")]
    ClusterNotFound(String),
    #[error("Tablet not found: {0}")]
    TabletNotFound(String),
}

// ─── 解析入口 ───

/// 从 INI 文件加载完整配置
pub fn load_config(path: impl AsRef<Path>) -> Result<AppConfig, ConfigError> {
    let ini = Ini::load_from_file(path)?;
    let mut cfg = AppConfig::default();

    for (section, props) in ini.iter() {
        let section_name = section.unwrap_or("").to_string();
        match &section_name {
            s if s.starts_with("MySQL_Proxy_Layer") => {
                parse_proxy_layer(&mut cfg, props)?;
            }
            s if s.starts_with("Cluster") && !s.starts_with("CTablet") => {
                parse_cluster(&mut cfg, s, props)?;
            }
            s if s.starts_with("CTablet") => {
                parse_tablet(&mut cfg, s, props)?;
            }
            s if s.starts_with("Master_Host") => {
                parse_host(&mut cfg, s, props, MasterSlave::Master)?;
            }
            s if s.starts_with("Slave_Host") => {
                parse_host(&mut cfg, s, props, MasterSlave::Slave)?;
            }
            s if s.starts_with("XenonRaft") => {
                parse_xenon_raft(&mut cfg, s, props)?;
            }
            s if s.starts_with("DB_User") => {
                parse_db_user(&mut cfg, s, props)?;
            }
            s if s.starts_with("Product_User") => {
                parse_product_user(&mut cfg, s, props)?;
            }
            s if s.starts_with("Auth_IP") => {
                parse_auth_ip(&mut cfg, s, props)?;
            }
            s if s.starts_with("Ignore_IP") => {
                parse_ignore_ip(&mut cfg, s, props)?;
            }
            s if s.starts_with("ConfigCenter") => {
                parse_config_center(&mut cfg, props)?;
            }
            s if s.starts_with("PlanBinding") => {
                parse_plan_binding(&mut cfg, props)?;
            }
            _ => {}
        }
    }

    rebuild_tablet_map(&mut cfg);
    Ok(cfg)
}

// ─── Section 解析 ───

fn parse_proxy_layer(cfg: &mut AppConfig, props: &Properties) -> Result<(), ConfigError> {
    for (key, val) in props.iter() {
        match key {
            "port" => {
                let p = parse_u16("MySQL_Proxy_Layer", "port", val)?;
                if p == 0 {
                    return Err(ConfigError::InvalidValue {
                        section: "MySQL_Proxy_Layer".into(),
                        field: "port".into(),
                        value: val.to_string(),
                        reason: "port must be 1-65535".into(),
                    });
                }
                cfg.port = p;
            }
            "mng_port" => cfg.mng_port = parse_u16("MySQL_Proxy_Layer", "mng_port", val)?,
            "slow_query_ms" => cfg.slow_query_ms = parse_u64("MySQL_Proxy_Layer", "slow_query_ms", val)?,
            "mng_user" => cfg.mng_user = Some(val.to_string()),
            "mng_password" => cfg.mng_password = Some(val.to_string()),
            "max_threads" => {
                let n = parse_usize("MySQL_Proxy_Layer", "max_threads", val)?;
                if n == 0 || n > 128 {
                    return Err(ConfigError::InvalidValue {
                        section: "MySQL_Proxy_Layer".into(),
                        field: "max_threads".into(),
                        value: val.to_string(),
                        reason: "must be 1-128".into(),
                    });
                }
                cfg.max_threads = n;
            }
            "log_dir" => cfg.log_dir = val.to_string(),
            "log_level" => {
                cfg.log_level = match val.to_lowercase().as_str() {
                    "debug" => LogLevel::Debug,
                    "info" => LogLevel::Info,
                    "warn" | "warning" => LogLevel::Warn,
                    "error" => LogLevel::Error,
                    _ => LogLevel::Info,
                };
            }
            "client_timeout" => {
                cfg.front_idle_timeout = parse_u32("MySQL_Proxy_Layer", "client_timeout", val)?;
            }
            "server_timeout" => {
                cfg.backend_idle_timeout = parse_u32("MySQL_Proxy_Layer", "server_timeout", val)?;
            }
            "conn_pool_socket_max_serve_client_times" => {
                cfg.conn_pool_socket_max_serve_client_times = parse_u64(
                    "MySQL_Proxy_Layer",
                    "conn_pool_socket_max_serve_client_times",
                    val,
                )?;
            }
            "stream_transport_enable" => {
                cfg.stream_on = if val == "1" || val == "true" || val == "on" {
                    1
                } else {
                    0
                };
            }
            "reload_interval" => {
                cfg.reload_interval_secs =
                    parse_u64("MySQL_Proxy_Layer", "reload_interval", val)?;
            }
            "default_charset" => {
                cfg.default_charset = parse_charset("MySQL_Proxy_Layer", "default_charset", val)?;
            }
            "max_sql_size" => {
                cfg.max_sql_size = parse_usize("MySQL_Proxy_Layer", "max_sql_size", val)?;
            }
            "max_query_size" => {
                cfg.max_query_size = parse_usize("MySQL_Proxy_Layer", "max_query_size", val)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_cluster(
    cfg: &mut AppConfig,
    section: &str,
    props: &Properties,
) -> Result<(), ConfigError> {
    let name = get_str(props, "name", section)?;
    let id = section
        .strip_prefix("Cluster")
        .unwrap_or(section)
        .trim_start_matches('_');
    if id.is_empty() {
        return Err(ConfigError::MissingField {
            section: section.into(),
            field: "id".into(),
        });
    }
    cfg.clusters
        .entry(id.to_string())
        .or_insert_with(|| Cluster {
            id: id.to_string(),
            name: name.to_string(),
            tablets: Vec::new(),
        });
    Ok(())
}

fn parse_tablet(cfg: &mut AppConfig, section: &str, props: &Properties) -> Result<(), ConfigError> {
    let name = get_str(props, "name", section)?;
    let rrule = props.get("rrule");

    let rest = section
        .strip_prefix("CTablet")
        .unwrap_or(section)
        .trim_start_matches('_');
    let parts: Vec<&str> = rest.splitn(2, '_').collect();
    let cluster_id = parts[0];
    let tablet_name = if parts.len() > 1 {
        parts[1].to_string()
    } else {
        name.to_string()
    };

    let cluster = cfg
        .clusters
        .get_mut(cluster_id)
        .ok_or_else(|| ConfigError::ClusterNotFound(cluster_id.to_string()))?;

    let index = cluster.tablets.len();
    let routes = rrule
        .map(|r| parse_route_rules(&tablet_name, r, cluster.tablets.len()))
        .unwrap_or_default();

    cluster.tablets.push(ClusterTablet {
        cluster_id: cluster_id.to_string(),
        tablet_id: tablet_name,
        index,
        groups: Vec::new(),
        routes,
        xenon: None,
    });
    Ok(())
}

fn parse_host(
    cfg: &mut AppConfig,
    section: &str,
    props: &Properties,
    ms: MasterSlave,
) -> Result<(), ConfigError> {
    let host_str = get_str(props, "host", section)?;
    let port = parse_u16(section, "port", get_str(props, "port", section)?)?;
    let max_pool_size = props
        .get("max_conn_pool_size")
        .map(|v| parse_u32(section, "max_conn_pool_size", v))
        .transpose()?
        .unwrap_or(16);
    let max_connections = props
        .get("max_connections")
        .map(|v| parse_u32(section, "max_connections", v))
        .transpose()?
        .unwrap_or(256);
    let connect_timeout = props
        .get("connect_timeout")
        .map(|v| parse_u32(section, "connect_timeout", v))
        .transpose()?
        .unwrap_or(5);
    let weight = props
        .get("weight")
        .map(|v| parse_u32(section, "weight", v))
        .transpose()?
        .unwrap_or(1);
    let tablet_name = props.get("cluster_tablet_name").map(|s| s.to_string());

    let prefix = match ms {
        MasterSlave::Master => "Master_Host",
        MasterSlave::Slave => "Slave_Host",
    };
    let group_id = section
        .strip_prefix(prefix)
        .unwrap_or(section)
        .trim_start_matches('_')
        .to_string();
    if group_id.is_empty() {
        return Err(ConfigError::MissingField {
            section: section.into(),
            field: "group_id".into(),
        });
    }

    // host 支持 IPv4 地址或主机名(Docker 服务名 / DNS 域名)。
    // 运行时由 tokio TcpStream::connect 解析,此处仅校验非空。
    if host_str.trim().is_empty() {
        return Err(ConfigError::InvalidValue {
            section: section.into(),
            field: "host".into(),
            value: host_str.to_string(),
            reason: "empty host".into(),
        });
    }

    let db = Database {
        host: host_str.to_string(),
        port,
        max_pool_size,
        max_connections,
        connect_timeout,
        weight,
        tablet_name: tablet_name.clone(),
    };

    // 查找目标 tablet
    let target_tablet = tablet_name.and_then(|tn| {
        cfg.clusters
            .values_mut()
            .flat_map(|c| c.tablets.iter_mut())
            .find(|t| t.tablet_id == tn)
    });

    let upsert_group = |tablet: &mut ClusterTablet| {
        if let Some(g) = tablet.groups.iter_mut().find(|g| g.group_id == group_id) {
            match ms {
                MasterSlave::Master => g.master = Some(db.clone()),
                MasterSlave::Slave => g.slave = Some(db.clone()),
            }
        } else {
            let mut g = DatabaseGroup {
                group_id: group_id.clone(),
                master: None,
                slave: None,
            };
            match ms {
                MasterSlave::Master => g.master = Some(db.clone()),
                MasterSlave::Slave => g.slave = Some(db.clone()),
            }
            tablet.groups.push(g);
        }
    };

    if let Some(tablet) = target_tablet {
        upsert_group(tablet);
    } else if let Some(first_cluster) = cfg.clusters.values_mut().next() {
        if let Some(first_tablet) = first_cluster.tablets.first_mut() {
            upsert_group(first_tablet);
        }
    }

    Ok(())
}

fn parse_db_user(
    cfg: &mut AppConfig,
    section: &str,
    props: &Properties,
) -> Result<(), ConfigError> {
    let username = get_str(props, "db_username", section)?;
    let password = get_str(props, "db_password", section)?;
    let default_db = props.get("default_db").map(|s| s.to_string());
    let cluster_name = get_str(props, "cluster_name", section)?;

    cfg.db_users.insert(
        username.to_string(),
        DbUser {
            username: username.to_string(),
            password: password.to_string(),
            default_db,
            cluster_name: cluster_name.to_string(),
        },
    );
    Ok(())
}

fn parse_product_user(
    cfg: &mut AppConfig,
    section: &str,
    props: &Properties,
) -> Result<(), ConfigError> {
    let username = get_str(props, "username", section)?;
    let password = get_str(props, "password", section)?;
    let db_username = get_str(props, "db_username", section)?;
    let max_connections = props
        .get("max_connections")
        .map(|v| parse_u32(section, "max_connections", v))
        .transpose()?
        .unwrap_or(256);
    let cluster_name = get_str(props, "cluster_name", section)?;

    cfg.product_users.insert(
        username.to_string(),
        ProductUser {
            username: username.to_string(),
            password: password.to_string(),
            db_username: db_username.to_string(),
            max_connections,
            cluster_name: cluster_name.to_string(),
            scramble_password: None,
        },
    );
    Ok(())
}

fn parse_auth_ip(
    cfg: &mut AppConfig,
    section: &str,
    props: &Properties,
) -> Result<(), ConfigError> {
    let ip_str = get_str(props, "ip", section)?;
    let ip = Ipv4Addr::from_str(ip_str).map_err(|_| ConfigError::InvalidValue {
        section: section.into(),
        field: "ip".into(),
        value: ip_str.to_string(),
        reason: "invalid IPv4 address".into(),
    })?;
    let mask = props
        .get("mask")
        .map(|v| parse_u32(section, "mask", v))
        .transpose()?
        .unwrap_or(32);
    let users: Vec<String> = props
        .get("users")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    cfg.auth_ips.push(AuthIp { ip, mask, users });
    Ok(())
}

fn parse_ignore_ip(
    cfg: &mut AppConfig,
    section: &str,
    props: &Properties,
) -> Result<(), ConfigError> {
    let ip_str = get_str(props, "ip", section)?;
    let ip = Ipv4Addr::from_str(ip_str).map_err(|_| ConfigError::InvalidValue {
        section: section.into(),
        field: "ip".into(),
        value: ip_str.to_string(),
        reason: "invalid IPv4 address".into(),
    })?;
    let mask = props
        .get("mask")
        .map(|v| parse_u32(section, "mask", v))
        .transpose()?
        .unwrap_or(32);
    cfg.ignore_ips.push((ip, mask));
    Ok(())
}

/// `[ConfigCenter]` 段:拓扑热更新的外部来源
/// 解析执行计划绑定段:[PlanBinding_N] sql_pattern=... hint=...
fn parse_plan_binding(cfg: &mut AppConfig, props: &Properties) -> Result<(), ConfigError> {
    let mut pattern = String::new();
    let mut hint = String::new();
    for (key, val) in props.iter() {
        match key {
            "sql_pattern" => pattern = val.to_string(),
            "hint" => hint = val.to_string(),
            _ => {}
        }
    }
    if pattern.is_empty() {
        return Err(ConfigError::MissingField {
            section: "PlanBinding".into(),
            field: "sql_pattern".into(),
        });
    }
    if hint.is_empty() {
        return Err(ConfigError::MissingField {
            section: "PlanBinding".into(),
            field: "hint".into(),
        });
    }
    cfg.plan_bindings.push(PlanBinding { sql_pattern: pattern, hint });
    Ok(())
}

fn parse_config_center(cfg: &mut AppConfig, props: &Properties) -> Result<(), ConfigError> {
    let kind = props
        .get("type")
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| "none".to_string());
    let endpoints: Vec<String> = props
        .get("endpoints")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let root = props
        .get("root")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "/newproxy".to_string());

    cfg.config_center = ConfigCenterCfg { kind, endpoints, root };
    Ok(())
}

// ─── Xenon Raft 分片段 ───

/// 解析 `[XenonRaft_<cluster>_<tablet>]` 段,挂到对应 ClusterTablet.xenon。
fn parse_xenon_raft(
    cfg: &mut AppConfig,
    section: &str,
    props: &Properties,
) -> Result<(), ConfigError> {
    let rest = section
        .strip_prefix("XenonRaft")
        .unwrap_or(section)
        .trim_start_matches('_');
    let parts: Vec<&str> = rest.splitn(2, '_').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(ConfigError::InvalidValue {
            section: section.into(),
            field: "section".into(),
            value: section.into(),
            reason: "期望格式 [XenonRaft_<cluster>_<tablet>]".into(),
        });
    }
    let cluster_id = parts[0];
    let tablet_name = parts[1];

    let tablet = cfg
        .clusters
        .get_mut(cluster_id)
        .and_then(|c| c.tablets.iter_mut().find(|t| t.tablet_id == tablet_name))
        .ok_or_else(|| ConfigError::TabletNotFound(tablet_name.to_string()))?;

    let mut x = XenonRaft::default();

    // members(host:port 列表)
    if let Some(members_raw) = props.get("members") {
        let mut members = Vec::new();
        for m in members_raw.split(',') {
            let m = m.trim();
            if m.is_empty() {
                continue;
            }
            let (host, port) = split_host_port(section, "members", m)?;
            members.push(RaftMember {
                host,
                mysql_port: port,
                raft_endpoint: None,
            });
        }
        x.members = members;
    }
    // raft_endpoints(与 members 对齐的可选列表;允许只给前几个/部分)
    if let Some(eps_raw) = props.get("raft_endpoints") {
        for (i, ep) in eps_raw.split(',').enumerate() {
            let ep = ep.trim();
            if ep.is_empty() {
                continue;
            }
            if let Some(m) = x.members.get_mut(i) {
                m.raft_endpoint = Some(ep.to_string());
            } else {
                return Err(ConfigError::InvalidValue {
                    section: section.into(),
                    field: "raft_endpoints".into(),
                    value: ep.to_string(),
                    reason: "成员数量超过 members".into(),
                });
            }
        }
    }
    if x.members.is_empty() {
        return Err(ConfigError::MissingField {
            section: section.into(),
            field: "members".into(),
        });
    }

    // 节奏与档位
    if let Some(v) = props.get("probe_interval") {
        x.probe_interval_ms = parse_u64(section, "probe_interval", v)?;
    }
    if let Some(v) = props.get("probe_timeout_ms") {
        x.probe_timeout_ms = parse_u64(section, "probe_timeout_ms", v)?;
    }
    if let Some(v) = props.get("leader_stale_ms") {
        x.leader_stale_ms = parse_u64(section, "leader_stale_ms", v)?;
    }
    if let Some(v) = props.get("read_consistency") {
        x.read_consistency = ReadConsistency::parse(v).ok_or_else(|| ConfigError::InvalidValue {
            section: section.into(),
            field: "read_consistency".into(),
            value: v.to_string(),
            reason: "期望 strong|causal|session|eventual".into(),
        })?;
    }
    if let Some(v) = props.get("read_consistency_db") {
        x.db_overrides = parse_consistency_overrides(section, "read_consistency_db", v)?;
    }
    if let Some(v) = props.get("read_consistency_user") {
        x.user_overrides = parse_consistency_overrides(section, "read_consistency_user", v)?;
    }
    if let Some(v) = props.get("barrier_wait_ms") {
        x.barrier_wait_ms = parse_u64(section, "barrier_wait_ms", v)?;
    }
    if let Some(v) = props.get("gtid_sample_cache_ms") {
        x.gtid_sample_cache_ms = parse_u64(section, "gtid_sample_cache_ms", v)?;
    }

    tablet.xenon = Some(x);
    Ok(())
}

/// 解析 `scope=level,scope=level` 形式的覆盖配置。
fn parse_consistency_overrides(
    section: &str,
    field: &str,
    v: &str,
) -> Result<HashMap<String, ReadConsistency>, ConfigError> {
    let mut map = HashMap::new();
    for item in v.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (k, lvl) = item.split_once('=').ok_or_else(|| ConfigError::InvalidValue {
            section: section.into(),
            field: field.into(),
            value: item.to_string(),
            reason: "期望 scope=level 形式".into(),
        })?;
        let lvl = ReadConsistency::parse(lvl).ok_or_else(|| ConfigError::InvalidValue {
            section: section.into(),
            field: field.into(),
            value: item.to_string(),
            reason: "期望 strong|causal|session|eventual".into(),
        })?;
        map.insert(k.trim().to_ascii_lowercase(), lvl);
    }
    Ok(map)
}

/// 解析 `host:port`(port 必填,1-65535)。
fn split_host_port(
    section: &str,
    field: &str,
    v: &str,
) -> Result<(String, u16), ConfigError> {
    let (host, port_str) = v.split_once(':').ok_or_else(|| ConfigError::InvalidValue {
        section: section.into(),
        field: field.into(),
        value: v.to_string(),
        reason: "期望 host:port".into(),
    })?;
    let host = host.trim();
    let port = parse_u16(section, field, port_str.trim())?;
    if host.is_empty() {
        return Err(ConfigError::InvalidValue {
            section: section.into(),
            field: field.into(),
            value: v.to_string(),
            reason: "host 为空".into(),
        });
    }
    Ok((host.to_string(), port))
}

// ─── 后处理 ───

fn rebuild_tablet_map(cfg: &mut AppConfig) {
    cfg.cluster_tablets.clear();
    for cluster in cfg.clusters.values() {
        for tablet in &cluster.tablets {
            let key = format!("{}.{}", cluster.id, tablet.tablet_id);
            cfg.cluster_tablets.insert(key, tablet.clone());
        }
    }
}

// ─── 辅助 ───

fn parse_route_rules(_tablet_name: &str, rrule: &str, _tablet_count: usize) -> Vec<RouteRule> {
    let parts: Vec<&str> = rrule.split(',').collect();
    if parts.len() < 3 {
        return vec![];
    }
    let table_part = parts[0].trim();
    let (table_name, partition_key) = table_part
        .split_once('.')
        .map(|(t, c)| (t.to_string(), c.to_string()))
        .unwrap_or_else(|| (table_part.to_string(), String::new()));

    let strategy = match parts[1].trim().to_lowercase().as_str() {
        "hash_mod" => ShardStrategy::HashMod,
        "md5_hash_mod" => ShardStrategy::Md5HashMod,
        "range" => ShardStrategy::Range,
        "list" => ShardStrategy::List,
        "pcre" => ShardStrategy::Pcre,
        _ => return vec![],
    };

    let tablet_indices: Vec<usize> = parts[2..]
        .iter()
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let pcre_pattern = if matches!(strategy, ShardStrategy::Pcre) {
        Some(
            parts
                .get(2)
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
        )
    } else {
        None
    };

    vec![RouteRule {
        table_name,
        strategy,
        partition_key,
        tablet_indices,
        pcre_pattern,
    }]
}

fn get_str<'a>(props: &'a Properties, key: &str, section: &str) -> Result<&'a str, ConfigError> {
    props.get(key).ok_or_else(|| ConfigError::MissingField {
        section: section.into(),
        field: key.to_string(),
    })
}

fn parse_u16(section: &str, field: &str, v: &str) -> Result<u16, ConfigError> {
    v.parse().map_err(|_| ConfigError::InvalidValue {
        section: section.into(),
        field: field.into(),
        value: v.into(),
        reason: "expected u16".into(),
    })
}

fn parse_u32(section: &str, field: &str, v: &str) -> Result<u32, ConfigError> {
    v.parse().map_err(|_| ConfigError::InvalidValue {
        section: section.into(),
        field: field.into(),
        value: v.into(),
        reason: "expected u32".into(),
    })
}

fn parse_u64(section: &str, field: &str, v: &str) -> Result<u64, ConfigError> {
    v.parse().map_err(|_| ConfigError::InvalidValue {
        section: section.into(),
        field: field.into(),
        value: v.into(),
        reason: "expected u64".into(),
    })
}

/// 解析 default_charset:接受人类可读的字符集名称(utf8/utf8mb4/gbk/...),
/// 也接受数字 collation id(向后兼容 `default_charset=33` 的旧写法)。
/// 存储值始终是握手用的 collation id。
fn parse_charset(section: &str, field: &str, v: &str) -> Result<u8, ConfigError> {
    let t = v.trim();
    if let Some(id) = crate::config::model::charset_name_to_id(t) {
        return Ok(id);
    }
    t.parse().map_err(|_| ConfigError::InvalidValue {
        section: section.into(),
        field: field.into(),
        value: v.into(),
        reason: "expected charset name (utf8/utf8mb4/gbk/gb2312/big5/latin1/binary) or numeric collation id".into(),
    })
}

fn parse_usize(section: &str, field: &str, v: &str) -> Result<usize, ConfigError> {
    v.parse().map_err(|_| ConfigError::InvalidValue {
        section: section.into(),
        field: field.into(),
        value: v.into(),
        reason: "expected usize".into(),
    })
}

// ─── 测试 ───

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model;

    const MINIMAL_CONF: &str = r#"
[MySQL_Proxy_Layer]
port=4051
mng_port=9111
max_threads=8
log_dir=logs
log_level=info
client_timeout=28800
server_timeout=28800
conn_pool_socket_max_serve_client_times=10000
max_sql_size=16777216
max_query_size=16777216
default_charset=utf8
stream_transport_enable=0

[Cluster_0]
name=test_cluster

[CTablet_0_t0]
name=t0

[Master_Host_g0]
host=127.0.0.1
port=3306
max_conn_pool_size=16
max_connections=256
connect_timeout=5
weight=1
cluster_tablet_name=t0

[Slave_Host_g0]
host=127.0.0.1
port=3307
max_conn_pool_size=16
max_connections=256
connect_timeout=5
weight=1
cluster_tablet_name=t0

[DB_User_dbu]
db_username=root
db_password=secret
default_db=test
cluster_name=test_cluster

[Product_User_pu]
username=app_user
password=app_pass
db_username=root
max_connections=128
cluster_name=test_cluster

[Auth_IP_0]
ip=10.0.0.0
mask=8
users=app_user
"#;

    #[test]
    fn parse_minimal_config() {
        let ini = Ini::load_from_str(MINIMAL_CONF).unwrap();
        let mut cfg = AppConfig::default();

        for (section, props) in ini.iter() {
            let sname = section.unwrap_or("").to_string();
            match &sname {
                s if s.starts_with("MySQL_Proxy_Layer") => {
                    parse_proxy_layer(&mut cfg, props).unwrap();
                }
                s if s.starts_with("Cluster") && !s.starts_with("CTablet") => {
                    parse_cluster(&mut cfg, s, props).unwrap();
                }
                s if s.starts_with("CTablet") => {
                    parse_tablet(&mut cfg, s, props).unwrap();
                }
                s if s.starts_with("Master_Host") => {
                    parse_host(&mut cfg, s, props, MasterSlave::Master).unwrap();
                }
                s if s.starts_with("Slave_Host") => {
                    parse_host(&mut cfg, s, props, MasterSlave::Slave).unwrap();
                }
                s if s.starts_with("DB_User") => {
                    parse_db_user(&mut cfg, s, props).unwrap();
                }
                s if s.starts_with("Product_User") => {
                    parse_product_user(&mut cfg, s, props).unwrap();
                }
                s if s.starts_with("Auth_IP") => {
                    parse_auth_ip(&mut cfg, s, props).unwrap();
                }
                _ => {}
            }
        }
        rebuild_tablet_map(&mut cfg);

        assert_eq!(cfg.port, 4051);
        assert_eq!(cfg.max_threads, 8);
        assert_eq!(cfg.log_level, LogLevel::Info);
        assert_eq!(cfg.clusters.len(), 1);
        let cluster = cfg.clusters.get("0").unwrap();
        assert_eq!(cluster.name, "test_cluster");
        assert_eq!(cluster.tablets.len(), 1);
        let tablet = &cluster.tablets[0];
        assert_eq!(tablet.groups.len(), 1);
        assert!(tablet.groups[0].master.is_some());
        assert!(tablet.groups[0].slave.is_some());
        assert_eq!(tablet.groups[0].master.as_ref().unwrap().port, 3306);
        assert_eq!(tablet.groups[0].slave.as_ref().unwrap().port, 3307);
        assert_eq!(cfg.db_users.len(), 1);
        assert_eq!(cfg.product_users.len(), 1);
        assert_eq!(cfg.auth_ips.len(), 1);

        let key = format!("{}.{}", cluster.id, tablet.tablet_id);
        assert!(cfg.cluster_tablets.contains_key(&key));
    }

    #[test]
    fn invalid_port_zero() {
        let conf_text = "[MySQL_Proxy_Layer]\nport=0\nmng_port=9111\nmax_threads=4\n";
        let ini = Ini::load_from_str(conf_text).unwrap();
        let mut cfg = AppConfig::default();
        for (section, props) in ini.iter() {
            let sname = section.unwrap_or("");
            if sname.starts_with("MySQL_Proxy_Layer") {
                let err = parse_proxy_layer(&mut cfg, props).unwrap_err();
                assert!(err.to_string().contains("port must be 1-65535"));
            }
        }
    }

    /// default_charset 支持人类可读名称(大小写/空白/连字符不敏感)。
    #[test]
    fn charset_name_mapping() {
        for (name, id) in [
            ("utf8", 33),
            ("UTF8", 33),
            (" utf8 ", 33),
            ("utf-8", 33),
            ("utf8mb3", 33),
            ("utf8mb4", 45),
            ("GBK", 28),
            ("gb2312", 24),
            ("big5", 1),
            ("latin1", 8),
            ("binary", 63),
        ] {
            assert_eq!(
                model::charset_name_to_id(name),
                Some(id),
                "name {name:?} should map to {id}"
            );
        }
        assert_eq!(model::charset_name_to_id("nosuch"), None);
        // 反向:id → 名称;未收录的 id 回落数字本身
        assert_eq!(model::charset_id_to_name(33), "utf8");
        assert_eq!(model::charset_id_to_name(45), "utf8mb4");
        assert_eq!(model::charset_id_to_name(47), "47");
    }

    /// 配置文件里 default_charset=utf8 应解析为 collation 33;
    /// 数字写法(33/45)保持向后兼容;非法值报错并提示合法取值。
    #[test]
    fn default_charset_accepts_names_and_numbers() {
        let load = |val: &str| {
            let conf_text = format!("[MySQL_Proxy_Layer]\ndefault_charset={val}\n");
            let ini = Ini::load_from_str(&conf_text).unwrap();
            let mut cfg = AppConfig::default();
            let props = ini
                .iter()
                .find(|(s, _)| s.unwrap_or("").starts_with("MySQL_Proxy_Layer"))
                .map(|(_, p)| p)
                .unwrap();
            parse_proxy_layer(&mut cfg, props).map(|_| cfg)
        };

        assert_eq!(load("utf8").unwrap().default_charset, 33);
        assert_eq!(load(" utf8 ").unwrap().default_charset, 33);
        assert_eq!(load("utf8mb4").unwrap().default_charset, 45);
        assert_eq!(load("gbk").unwrap().default_charset, 28);
        // 数字向后兼容
        assert_eq!(load("33").unwrap().default_charset, 33);
        assert_eq!(load("45").unwrap().default_charset, 45);
        // 非法值:既不是已知名称也不是数字
        let err = load("abc").unwrap_err();
        assert!(err.to_string().contains("expected charset name"), "got: {err}");
    }

    /// 2 分片容器测试场景配置(tests/perf/newproxy-2shard-docker.conf):
    /// 1 集群 2 分片,各挂独立 master 后端,且 sbtest1.id 声明 hash_mod 路由。
    #[test]
    fn parse_two_shard_docker_conf() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/perf/newproxy-2shard-docker.conf"
        );
        let cfg = load_config(path).expect("2-shard conf should load");
        assert_eq!(cfg.clusters.len(), 1);
        let cluster = cfg.clusters.get("0").expect("cluster 0");
        assert_eq!(cluster.name, "test_cluster");
        assert_eq!(cluster.tablets.len(), 2, "should have 2 tablets/shards");

        // 分片 0 → mysql-shard0:3306
        let t0 = &cluster.tablets[0];
        assert_eq!(t0.tablet_id, "t0");
        assert_eq!(t0.groups.len(), 1);
        let m0 = t0.groups[0].master.as_ref().expect("t0 master");
        assert_eq!(m0.host, "mysql-shard0");
        assert_eq!(m0.port, 3306);
        // 分片 1 → mysql-shard1:3306
        let t1 = &cluster.tablets[1];
        assert_eq!(t1.tablet_id, "t1");
        assert_eq!(t1.groups.len(), 1);
        let m1 = t1.groups[0].master.as_ref().expect("t1 master");
        assert_eq!(m1.host, "mysql-shard1");
        assert_eq!(m1.port, 3306);

        // 路由规则:sbtest1.id hash_mod → 分片 0/1
        for t in [t0, t1] {
            assert_eq!(t.routes.len(), 1, "tablet {} should carry rrule", t.tablet_id);
            let r = &t.routes[0];
            assert_eq!(r.table_name, "sbtest1");
            assert_eq!(r.partition_key, "id");
            assert_eq!(r.strategy, ShardStrategy::HashMod);
            assert_eq!(r.tablet_indices, vec![0, 1]);
        }

        // 集群分片映射表含两个分片
        assert!(cfg.cluster_tablets.contains_key("0.t0"));
        assert!(cfg.cluster_tablets.contains_key("0.t1"));
    }

    // ─── Xenon Raft 分片段 ───

    const XENON_CONF: &str = r#"
[MySQL_Proxy_Layer]
port=4051

[Cluster_0]
name=test_cluster

[CTablet_0_t0]
name=t0

[CTablet_0_t1]
name=t1

[XenonRaft_0_t0]
members=xenon1:3306,xenon2:3306,xenon3:3306
raft_endpoints=xenon1:8801,xenon2:8801
probe_interval=1500
probe_timeout_ms=600
leader_stale_ms=4000
read_consistency=eventual
read_consistency_db=report=causal,audit=session
read_consistency_user=finance=strong,report_api=eventual
barrier_wait_ms=250
gtid_sample_cache_ms=50
"#;

    #[test]
    fn load_xenon_raft_section() {
        let cfg = load_conf(XENON_CONF).unwrap();
        let cluster = cfg.clusters.get("0").unwrap();
        let t0 = cluster.tablets.iter().find(|t| t.tablet_id == "t0").unwrap();
        let t1 = cluster.tablets.iter().find(|t| t.tablet_id == "t1").unwrap();
        // t0 挂 xenon,t1 不挂
        assert!(t1.xenon.is_none(), "未配置的分片不应有 xenon");
        let x = t0.xenon.as_ref().expect("t0 should have xenon");
        assert_eq!(x.members.len(), 3);
        assert_eq!(x.members[0].host, "xenon1");
        assert_eq!(x.members[0].mysql_port, 3306);
        assert_eq!(x.members[0].raft_endpoint.as_deref(), Some("xenon1:8801"));
        assert_eq!(x.members[2].raft_endpoint, None, "endpoint 列表可短于 members");
        assert_eq!(x.probe_interval_ms, 1500);
        assert_eq!(x.probe_timeout_ms, 600);
        assert_eq!(x.leader_stale_ms, 4000);
        assert_eq!(x.barrier_wait_ms, 250);
        assert_eq!(x.gtid_sample_cache_ms, 50);
        assert_eq!(x.read_consistency, ReadConsistency::Eventual);
        assert_eq!(x.db_overrides.get("report"), Some(&ReadConsistency::Causal));
        assert_eq!(x.db_overrides.get("audit"), Some(&ReadConsistency::Session));
        assert_eq!(x.user_overrides.get("finance"), Some(&ReadConsistency::Strong));
        assert_eq!(x.user_overrides.get("report_api"), Some(&ReadConsistency::Eventual));
    }

    #[test]
    fn load_xenon_raft_defaults_when_partial() {
        let cfg = load_conf(
            r#"
[MySQL_Proxy_Layer]
port=4051
[Cluster_0]
name=c
[CTablet_0_t0]
name=t0
[XenonRaft_0_t0]
members=x1:3306,x2:3306
"#,
        )
        .unwrap();
        let x = cfg.clusters["0"].tablets[0].xenon.as_ref().unwrap();
        assert_eq!(x.probe_interval_ms, 1000);
        assert_eq!(x.leader_stale_ms, 3000);
        assert_eq!(x.read_consistency, ReadConsistency::Strong, "缺省档位必须 strong");
        assert!(x.db_overrides.is_empty());
        assert_eq!(x.members.len(), 2);
    }

    #[test]
    fn load_xenon_raft_errors() {
        // 缺 members
        assert!(load_conf(
            r#"
[MySQL_Proxy_Layer]
port=4051
[Cluster_0]
name=c
[CTablet_0_t0]
name=t0
[XenonRaft_0_t0]
read_consistency=causal
"#
        )
        .is_err());
        // 非法档位
        let e = load_conf(
            r#"
[MySQL_Proxy_Layer]
port=4051
[Cluster_0]
name=c
[CTablet_0_t0]
name=t0
[XenonRaft_0_t0]
members=x1:3306
read_consistency=bogus
"#,
        );
        assert!(e.is_err(), "非法档位应报错");
        // 分片不存在
        assert!(load_conf(
            r#"
[MySQL_Proxy_Layer]
port=4051
[Cluster_0]
name=c
[CTablet_0_t0]
name=t0
[XenonRaft_0_nope]
members=x1:3306
"#
        )
        .is_err());
        // members 无端口
        assert!(load_conf(
            r#"
[MySQL_Proxy_Layer]
port=4051
[Cluster_0]
name=c
[CTablet_0_t0]
name=t0
[XenonRaft_0_t0]
members=x1
"#
        )
        .is_err());
    }

    /// 写临时配置文件并 load_config(真实分发循环,而非手动重放)
    fn load_conf(text: &str) -> Result<AppConfig, ConfigError> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("newproxy-cfg-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.conf");
        std::fs::write(&path, text).unwrap();
        let r = load_config(&path);
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    /// 全 section 分发:Slave_Host / Auth_IP / Ignore_IP / ConfigCenter / PlanBinding
    #[test]
    fn load_config_full_sections() {
        let cfg = load_conf(
            r#"
[MySQL_Proxy_Layer]
port=4051
mng_port=9111
max_threads=8
log_dir=logs
log_level=warn
default_charset=utf8mb4
slow_query_ms=300
reload_interval_secs=5
conn_pool_socket_max_serve_client_times=999

[Cluster_0]
name=test_cluster

[CTablet_0_t0]
name=t0

[Master_Host_g0]
host=127.0.0.1
port=3306
max_conn_pool_size=16
max_connections=256
connect_timeout=5
weight=1
cluster_tablet_name=t0

[Slave_Host_g0]
host=127.0.0.2
port=3307
max_conn_pool_size=16
max_connections=256
connect_timeout=5
weight=1
cluster_tablet_name=t0

[DB_User_dbu]
db_username=root
db_password=secret
default_db=test
cluster_name=test_cluster

[Product_User_pu]
username=app_user
password=app_pass
db_username=root
max_connections=128
cluster_name=test_cluster

[Auth_IP_0]
ip=10.0.0.0
mask=8
users=app_user

[Ignore_IP_0]
ip=192.168.1.0
mask=24

[ConfigCenter_0]
type=etcd
endpoints=http://127.0.0.1:2379
root=/newproxy

[PlanBinding_0]
sql_pattern=SELECT * FROM orders
hint=/*+ INDEX(orders idx) */
"#,
        )
        .expect("full config should load");
        assert_eq!(cfg.log_level, LogLevel::Warn);
        assert_eq!(cfg.slow_query_ms, 300);
        assert_eq!(cfg.reload_interval_secs, 5);
        assert_eq!(cfg.conn_pool_socket_max_serve_client_times, 999);
        assert_eq!(cfg.default_charset, 45, "utf8mb4");
        let tablet = &cfg.clusters.get("0").unwrap().tablets[0];
        let g = &tablet.groups[0];
        assert!(g.slave.is_some(), "Slave_Host 应解析为从库");
        assert_eq!(g.slave.as_ref().unwrap().port, 3307);
        assert_eq!(cfg.auth_ips.len(), 1);
        assert_eq!(cfg.auth_ips[0].ip, Ipv4Addr::new(10, 0, 0, 0));
        assert_eq!(cfg.auth_ips[0].mask, 8);
        assert_eq!(cfg.auth_ips[0].users, vec!["app_user"]);
        assert_eq!(cfg.ignore_ips.len(), 1);
        assert_eq!(cfg.ignore_ips[0].0, Ipv4Addr::new(192, 168, 1, 0));
        assert_eq!(cfg.ignore_ips[0].1, 24);
        assert_eq!(cfg.config_center.kind, "etcd");
        assert_eq!(cfg.config_center.endpoints, vec!["http://127.0.0.1:2379"]);
        assert_eq!(cfg.config_center.root, "/newproxy");
        assert_eq!(cfg.plan_bindings.len(), 1);
        assert_eq!(cfg.plan_bindings[0].sql_pattern, "SELECT * FROM orders");
        assert!(cfg.plan_bindings[0].hint.contains("INDEX(orders"));
    }

    #[test]
    fn load_config_config_center_defaults() {
        // ConfigCenter 无 type/endpoints/root → kind=none,空端点
        let cfg = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\n[Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n\
             [ConfigCenter_0]\ntype=zk\n",
        )
        .expect("should load");
        assert_eq!(cfg.config_center.kind, "zk");
        assert!(cfg.config_center.endpoints.is_empty());
        assert_eq!(cfg.config_center.root, "/newproxy", "root 缺省为 /newproxy");
    }

    #[test]
    fn load_config_log_levels() {
        for (level, expect) in [
            ("debug", LogLevel::Debug),
            ("info", LogLevel::Info),
            ("warn", LogLevel::Warn),
            ("error", LogLevel::Error),
            ("unknown", LogLevel::Info), // 非法值回落 Info
        ] {
            let cfg = load_conf(&format!(
                "[MySQL_Proxy_Layer]\nport=4051\nlog_level={level}\n\
                 [Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
                 [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n"
            ))
            .unwrap();
            assert_eq!(cfg.log_level, expect, "log_level={level}");
        }
    }

    #[test]
    fn load_config_errors() {
        // max_threads 越界
        let err = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\nmax_threads=0\n\
             [Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("max_threads"));
        let err = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\nmax_threads=129\n\
             [Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("max_threads"));

        // 缺 Cluster id
        let err = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\n[Cluster]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("id"), "got: {err}");

        // Master_Host 缺 group_id
        let err = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\n[Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n\
             [Master_Host]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("group_id"), "got: {err}");

        // host 为空
        let err = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\n[Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=\nport=3306\ncluster_tablet_name=t0\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty host"), "got: {err}");

        // 数值解析错误:port 非数字
        let err = load_conf(
            "[MySQL_Proxy_Layer]\nport=abc\n[Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("u16"), "got: {err}");

        // Auth_IP 非法 IPv4
        let err = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\n[Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n\
             [Auth_IP_0]\nip=999.1.1.1\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("IPv4"), "got: {err}");

        // Ignore_IP 非法 mask(非数字)
        let err = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\n[Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n\
             [Ignore_IP_0]\nip=10.0.0.1\nmask=abc\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("u32"), "got: {err}");

        // PlanBinding 缺 sql_pattern / hint
        let err = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\n[Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n\
             [PlanBinding_0]\nhint=/*+ */\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("sql_pattern"), "got: {err}");
        let err = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\n[Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n\
             [PlanBinding_0]\nsql_pattern=SELECT 1\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("hint"), "got: {err}");

        // 配置不存在(IO 错误经 ini crate 包装)
        let err = load_config("/nonexistent/newproxy.conf").unwrap_err();
        assert!(err.to_string().contains("parse error"), "got: {err}");
    }

    #[test]
    fn load_config_host_fallbacks() {
        // 无 cluster_tablet_name 的 Master_Host → 回落第一个分片
        let cfg = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\n[Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [CTablet_0_t1]\nname=t1\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t1\n\
             [Master_Host_g1]\nhost=127.0.0.2\nport=3307\n",
        )
        .expect("should load");
        let cluster = cfg.clusters.get("0").unwrap();
        assert_eq!(cluster.tablets.len(), 2);
        // g1 无 tablet 名 → 追加到第一个分片
        let t0 = &cluster.tablets[0];
        let masters: Vec<_> = t0
            .groups
            .iter()
            .filter_map(|g| g.master.as_ref())
            .map(|m| m.port)
            .collect();
        assert!(masters.contains(&3307), "fallback master 应挂在第一个分片: {masters:?}");
    }

    #[test]
    fn route_rules_all_strategies() {
        // hash_mod(2shard 配置路径)
        let rules = parse_route_rules("t0", "sbtest1.id,hash_mod,0,1", 2);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].table_name, "sbtest1");
        assert_eq!(rules[0].partition_key, "id");
        assert_eq!(rules[0].strategy, ShardStrategy::HashMod);
        assert_eq!(rules[0].tablet_indices, vec![0, 1]);

        // md5_hash_mod / range / list / pcre
        assert_eq!(
            parse_route_rules("t0", "u.k,md5_hash_mod,0,1,2", 4)[0].strategy,
            ShardStrategy::Md5HashMod
        );
        assert_eq!(
            parse_route_rules("t0", "u.k,range,0,1", 2)[0].strategy,
            ShardStrategy::Range
        );
        assert_eq!(
            parse_route_rules("t0", "u.k,list,1,0", 2)[0].strategy,
            ShardStrategy::List
        );
        let pcre = &parse_route_rules("t0", "u.k,pcre,0,1", 2)[0];
        assert_eq!(pcre.strategy, ShardStrategy::Pcre);
        assert_eq!(pcre.pcre_pattern.as_deref(), Some("0"));

        // 无表名.列名 → 默认分片键
        let rules = parse_route_rules("t0", "mytable,hash_mod,0,1", 2);
        assert_eq!(rules[0].table_name, "mytable");

        // 非法:字段不足 / 未知策略 / 空 → 空规则
        assert!(parse_route_rules("t0", "a.b,hash_mod", 2).is_empty());
        assert!(parse_route_rules("t0", "a.b,bogus,0,1", 2).is_empty());
        assert!(parse_route_rules("t0", "", 2).is_empty());
        // 非 pcre 策略不设 pcre_pattern
        assert!(parse_route_rules("t0", "a.b,list,0,1", 2)[0].pcre_pattern.is_none());
    }

    #[test]
    fn db_user_product_user_defaults() {
        // 无 default_db 的 DB_User / 无密码的 Product_User 等默认值路径
        let cfg = load_conf(
            "[MySQL_Proxy_Layer]\nport=4051\n[Cluster_0]\nname=c\n[CTablet_0_t0]\nname=t0\n\
             [Master_Host_g0]\nhost=127.0.0.1\nport=3306\ncluster_tablet_name=t0\n\
             [DB_User_u]\ndb_username=root\ndb_password=x\ncluster_name=c\n\
             [Product_User_p]\nusername=u\npassword=p\ndb_username=root\ncluster_name=c\n\
             [Unknown_Section]\nignored_key=ignored_value\n",
        )
        .expect("should load");
        let u = cfg.db_users.get("root").unwrap();
        assert_eq!(u.cluster_name, "c");
        let p = cfg.product_users.get("u").unwrap();
        assert_eq!(p.cluster_name, "c");
        // 未知 section 忽略不报错
    }
}

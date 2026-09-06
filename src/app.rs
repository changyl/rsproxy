// 应用全局上下文(AppCtx)
// T2.1 实现:ArcSwap config + 连接计数 + 共享状态
//
// 对齐 C 侧 tr_cycle_t(线程级) + tr_instance_t(进程级)
// work-stealing 下用 Arc 共享替代 TLS

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use parking_lot::RwLock;
use tokio::sync::watch;
use tracing::info;

use crate::config::{AppConfig, MasterSlave};
use crate::config_center::RuntimeTopology;
use crate::metric::Metrics;
use crate::pool::backend::SrvPool;

/// 一个活跃前端连接的注册信息(`checkproxy show connections` / `kill` 的数据源)。
///
/// 语义:注册表的 key 是**前端连接 id**(cid,全局唯一),对应 MySQL 的 thread id。
/// 后端连接是前端任务私有的从属资源,不在此注册表寻址——杀掉前端任务后,
/// 其名下所有分片后端连接会随 `BackendSession` 析构自动归还/标记。
#[derive(Clone)]
pub struct ConnHandle {
    /// 客户端地址
    pub peer_addr: SocketAddr,
    /// 连接建立时间
    pub started_at: Instant,
    /// 认证后的用户名(认证前为空)
    pub user: Arc<RwLock<String>>,
    /// 当前库(未选库为 None)
    pub db: Arc<RwLock<Option<String>>>,
    /// 状态: Auth / Command
    pub state: Arc<RwLock<String>>,
    /// 会话绑定的后端 host:port(多分片场景可为多个)
    pub backends: Arc<RwLock<Vec<String>>>,
    /// kill 信号:true 表示运维请求关闭该连接
    kill_tx: watch::Sender<bool>,
}

/// 应用全局上下文(所有连接 task 共享)
pub struct AppCtx {
    /// 配置(ArcSwap 无锁读,reload 即时生效)
    pub config: Arc<ArcSwap<AppConfig>>,

    /// 后端连接池(所有前端 task 共享)
    pub srv_pool: Arc<SrvPool>,

    /// 全局运行指标(所有连接 task 共享,checkproxy show status/sql 的数据源)
    pub metrics: Arc<Metrics>,

    /// 运行时拓扑覆盖层(配置中心下发的动态拓扑;文件配置为静态基线)
    pub topology: Arc<RuntimeTopology>,

    /// 活跃连接注册表(cid -> 连接信息),show connections / kill 的数据源
    pub connections: DashMap<u32, ConnHandle>,

    /// 配置文件路径(热加载用)
    config_path: String,

    /// 连接 ID 自增计数器
    connection_id: AtomicU32,

    /// 服务端版本号
    pub server_version: String,

    /// 进程负载采样器(CPU/内存/FD;面板与 Prometheus 扩缩容指标用)
    pub proc_meter: parking_lot::Mutex<crate::mgmt::proc::ProcMeter>,
}

impl AppCtx {
    /// 从配置创建 AppCtx
    pub fn new(config: AppConfig, srv_pool: Arc<SrvPool>, config_path: String) -> Self {
        Self {
            config: Arc::new(ArcSwap::from_pointee(config)),
            srv_pool,
            metrics: Arc::new(Metrics::new()),
            topology: Arc::new(RuntimeTopology::new()),
            connections: DashMap::new(),
            config_path,
            connection_id: AtomicU32::new(1),
            server_version: "1.1.0".to_string(),
            proc_meter: parking_lot::Mutex::new(crate::mgmt::proc::ProcMeter::new()),
        }
    }

    /// 获取当前配置快照(ArcSwap::load 是无锁的)
    pub fn load_config(&self) -> arc_swap::Guard<Arc<AppConfig>> {
        self.config.load()
    }

    /// 配置文件路径(供管理面板修改配置后 reload 用)
    pub fn config_path(&self) -> &str {
        &self.config_path
    }

    /// 分配下一个连接 ID(线程安全)
    pub fn next_connection_id(&self) -> u32 {
        self.connection_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 注册一个前端连接,返回 (handle, kill_rx)。
    /// `kill_rx` 交给连接任务监听取消;连接任务退出时必须调用 `unregister_connection`。
    pub fn register_connection(
        &self,
        cid: u32,
        peer_addr: SocketAddr,
    ) -> (ConnHandle, watch::Receiver<bool>) {
        let (kill_tx, kill_rx) = watch::channel(false);
        let handle = ConnHandle {
            peer_addr,
            started_at: Instant::now(),
            user: Arc::new(RwLock::new(String::new())),
            db: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new("Auth".to_string())),
            backends: Arc::new(RwLock::new(Vec::new())),
            kill_tx,
        };
        self.connections.insert(cid, handle.clone());
        (handle, kill_rx)
    }

    /// 注销连接(连接任务退出时调用,幂等)
    pub fn unregister_connection(&self, cid: u32) {
        self.connections.remove(&cid);
    }

    /// `checkproxy kill <cid>`:请求关闭指定前端连接(幂等)。
    /// 连接已结束/不存在时返回错误;多分片下由连接任务级联回收其全部后端连接。
    pub fn kill_connection(&self, cid: u32) -> Result<(), String> {
        match self.connections.get(&cid) {
            Some(h) => {
                let _ = h.kill_tx.send(true);
                Ok(())
            }
            None => Err(format!("Unknown connection id: {cid}")),
        }
    }

    /// 热加载配置:重新解析配置文件 → 原子替换 → 清空连接池。
    ///
    /// 适用于主从切换 / 后端拓扑变更等场景,无需重启进程:
    /// - 新连接与新建后端连接立即使用新配置(连接任务每操作都重新 load_config)
    /// - 连接池清空,避免复用指向旧拓扑的空闲连接
    /// - 已有会话保持其当前后端连接直至结束(不打断在途事务)
    ///
    /// 解析失败时保持旧配置运行,返回 Err。
    pub fn reload_config(&self) -> Result<String, String> {
        let new_cfg = crate::config::load_config(&self.config_path)
            .map_err(|e| format!("解析 {} 失败: {}", self.config_path, e))?;

        let diff = topology_diff(&self.config.load(), &new_cfg);
        self.config.store(Arc::new(new_cfg));
        self.srv_pool.reset();
        info!("config reloaded ({}): {}", self.config_path, diff);
        Ok(diff)
    }
}

/// 生成新旧配置的拓扑变更摘要(供 reload 回显/日志)。
///
/// 对比每个 (cluster, tablet, group, 主从角色) 的后端 host:port 变化。
fn topology_diff(old: &AppConfig, new: &AppConfig) -> String {
    type Key = (String, String, String, String);

    let collect = |cfg: &AppConfig| -> BTreeMap<Key, (String, u16)> {
        let mut m = BTreeMap::new();
        for c in cfg.clusters.values() {
            for t in &c.tablets {
                for g in &t.groups {
                    let role = |ms: MasterSlave| match ms {
                        MasterSlave::Master => "master".to_string(),
                        MasterSlave::Slave => "slave".to_string(),
                    };
                    if let Some(d) = &g.master {
                        m.insert(
                            (
                                c.id.clone(),
                                t.tablet_id.clone(),
                                g.group_id.clone(),
                                role(MasterSlave::Master),
                            ),
                            (d.host.clone(), d.port),
                        );
                    }
                    if let Some(d) = &g.slave {
                        m.insert(
                            (
                                c.id.clone(),
                                t.tablet_id.clone(),
                                g.group_id.clone(),
                                role(MasterSlave::Slave),
                            ),
                            (d.host.clone(), d.port),
                        );
                    }
                }
            }
        }
        m
    };

    let old_map = collect(old);
    let new_map = collect(new);

    let mut keys: Vec<&Key> = old_map.keys().chain(new_map.keys()).collect();
    keys.sort();
    keys.dedup();

    let mut lines = Vec::new();
    for k in keys {
        let oo = old_map
            .get(k)
            .map(|(h, p)| format!("{h}:{p}"))
            .unwrap_or_else(|| "-".to_string());
        let nn = new_map
            .get(k)
            .map(|(h, p)| format!("{h}:{p}"))
            .unwrap_or_else(|| "-".to_string());
        if oo != nn {
            lines.push(format!("  {} {} {}: {} -> {}", k.0, k.1, k.3, oo, nn));
        }
    }

    if lines.is_empty() {
        format!(
            "topology unchanged (clusters={}, product_users={}, db_users={}, auth_ips={})",
            new.clusters.len(),
            new.product_users.len(),
            new.db_users.len(),
            new.auth_ips.len()
        )
    } else {
        format!("topology changed:\n{}", lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{Cluster, ClusterTablet, DatabaseGroup};

    fn base_conf_text() -> String {
        r#"
[MySQL_Proxy_Layer]
port=4051
mng_port=9111
max_threads=4
log_dir=logs
log_level=info

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
"#
        .to_string()
    }

    fn write_conf(text: &str, tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("newproxy-app-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.conf");
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn register_kill_unregister() {
        let ctx = Arc::new(AppCtx::new(AppConfig::default(), Arc::new(SrvPool::new()), "/tmp/x".into()));
        let cid = ctx.next_connection_id();
        assert_eq!(cid, 1);
        assert_eq!(ctx.next_connection_id(), 2);

        let addr: SocketAddr = "127.0.0.1:3307".parse().unwrap();
        let (handle, mut kill_rx) = ctx.register_connection(cid, addr);
        assert!(ctx.connections.contains_key(&cid));
        assert_eq!(handle.peer_addr, addr);
        assert_eq!(*handle.state.read(), "Auth");

        // kill → 信号到达
        ctx.kill_connection(cid).unwrap();
        let _ = kill_rx.changed();
        assert!(*kill_rx.borrow(), "kill 信号应置 true");

        // 幂等注销
        ctx.unregister_connection(cid);
        ctx.unregister_connection(cid);
        assert!(!ctx.connections.contains_key(&cid));
        // 已注销 → kill 返回 Unknown
        assert!(ctx.kill_connection(cid).is_err());
        assert!(ctx.kill_connection(999).is_err());
    }

    #[test]
    fn reload_config_ok_and_failure() {
        let path = write_conf(&base_conf_text(), "ok");
        let ctx = Arc::new(AppCtx::new(AppConfig::default(), Arc::new(SrvPool::new()), path.clone()));
        // reload 成功:配置被替换,返回拓扑摘要
        let diff = ctx.reload_config().unwrap();
        assert!(diff.contains("topology"), "got: {diff}");
        assert_eq!(ctx.load_config().port, 4051);
        // reload 失败:不存在的配置 → Err,旧配置保留
        let bad = Arc::new(AppCtx::new(AppConfig::default(), Arc::new(SrvPool::new()), "/nonexistent/xxx.conf".to_string()));
        assert!(bad.reload_config().is_err());
        assert_eq!(bad.load_config().port, 4051, "失败时旧配置应保留");
    }

    fn group(host: &str, port: u16, role: MasterSlave) -> DatabaseGroup {
        let mut g = DatabaseGroup { group_id: "g0".into(), master: None, slave: None };
        let d = crate::config::Database {
            host: host.into(),
            port,
            max_pool_size: 16,
            max_connections: 256,
            connect_timeout: 5,
            weight: 1,
            tablet_name: None,
        };
        match role {
            MasterSlave::Master => g.master = Some(d),
            MasterSlave::Slave => g.slave = Some(d),
        }
        g
    }

    fn one_cluster_cfg(groups: Vec<DatabaseGroup>) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.clusters.insert(
            "0".into(),
            Cluster {
                id: "0".into(),
                name: "c".into(),
                tablets: vec![ClusterTablet {
                    cluster_id: "0".into(),
                    tablet_id: "t0".into(),
                    index: 0,
                    groups,
                    routes: vec![],
                }],
            },
        );
        cfg
    }

    #[test]
    fn topology_diff_variants() {
        // 完全一致 → unchanged
        let old = one_cluster_cfg(vec![group("h1", 3306, MasterSlave::Master)]);
        let new = one_cluster_cfg(vec![group("h1", 3306, MasterSlave::Master)]);
        let d = topology_diff(&old, &new);
        assert!(d.contains("unchanged"), "got: {d}");

        // master host 变化 → changed
        let new2 = one_cluster_cfg(vec![group("h2", 3306, MasterSlave::Master)]);
        let d = topology_diff(&old, &new2);
        assert!(d.contains("changed") && d.contains("h1:3306 -> h2:3306"), "got: {d}");

        // 增加 slave → changed
        let new3 = one_cluster_cfg(vec![
            group("h1", 3306, MasterSlave::Master),
            group("s1", 3307, MasterSlave::Slave),
        ]);
        let d = topology_diff(&old, &new3);
        assert!(d.contains("slave") && d.contains("-> s1:3307"), "got: {d}");

        // 删除分片 → changed(旧键消失)
        let empty = AppConfig::default();
        let d = topology_diff(&old, &empty);
        assert!(d.contains("-> -") || d.contains("h1:3306 -> -"), "got: {d}");
    }
}

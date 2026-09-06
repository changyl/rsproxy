// 配置中心抽象:后端拓扑的拉取 / 订阅 / 回写
//
// 目标:主从切换、加节点等拓扑变更通过配置中心(etcd / ZooKeeper)下发,
// 代理按**分片粒度**增量生效,而不是"改文件 → 全量重解析 → 全池清空"。
//
// 设计:
//   - `TopologyStore` trait:统一 etcd / zookeeper / 内存(测试)后端的访问接口
//   - 数据模型:watch 的最小单元是**单个分片**(ShardTopology),key 按
//     `{root}/clusters/{cluster_id}/tablets/{tablet_id}` 组织
//   - `RuntimeTopology`:代理内的"运行时拓扑覆盖层"——文件配置是静态基线,
//     此层保存配置中心下发的动态拓扑;查询路径(ensure_backend)先查此层
//   - `run_watcher`:驱动循环,全量拉取打底 + 订阅增量应用 + 断线重建,
//     每次应用变更后回调上层(连接池按分片定向失效)

pub mod etcd;
pub mod zk;

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// 后端地址
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostAddr {
    pub host: String,
    pub port: u16,
}

impl HostAddr {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self { host: host.into(), port }
    }

    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// 分片拓扑(配置中心 watch 的最小单元)。
///
/// 序列化:JSON(etcd value / zk data)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardTopology {
    pub cluster_id: String,
    pub tablet_id: String,
    /// 当前主库;None 表示无主(只读/待切换)
    #[serde(default)]
    pub master: Option<HostAddr>,
    /// 从库列表
    #[serde(default)]
    pub slaves: Vec<HostAddr>,
}

impl ShardTopology {
    pub fn key(&self) -> (String, String) {
        (self.cluster_id.clone(), self.tablet_id.clone())
    }

    pub fn to_json(&self) -> Result<Vec<u8>, StoreError> {
        serde_json::to_vec(self).map_err(|e| StoreError::Parse(e.to_string()))
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, StoreError> {
        serde_json::from_slice(bytes).map_err(|e| StoreError::Parse(e.to_string()))
    }
}

/// 配置中心错误
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("配置中心连接失败: {0}")]
    Connect(String),
    #[error("配置中心操作失败: {0}")]
    Io(String),
    #[error("数据解析失败: {0}")]
    Parse(String),
    #[error("不支持的配置中心类型: {0}")]
    Unsupported(String),
    #[error("订阅终止: {0}")]
    Closed(String),
}

/// 拓扑变更事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyChange {
    /// 分片拓扑新增/更新
    ShardUpsert(ShardTopology),
    /// 分片拓扑删除
    ShardDelete { cluster_id: String, tablet_id: String },
}

impl TopologyChange {
    /// 受影响的分片 key (cluster_id, tablet_id)
    pub fn shard_key(&self) -> (String, String) {
        match self {
            TopologyChange::ShardUpsert(t) => t.key(),
            TopologyChange::ShardDelete { cluster_id, tablet_id } => {
                (cluster_id.clone(), tablet_id.clone())
            }
        }
    }
}

/// 变更回调(由 store 的订阅循环调用;必须快速返回,不要阻塞)
pub type ChangeCallback = Arc<dyn Fn(TopologyChange) + Send + Sync>;

/// 配置中心抽象:后端拓扑的拉取 / 订阅 / 回写。
///
/// 实现:
/// - [`etcd::EtcdStore`] — etcd v3(watch + prefix)
/// - [`zk::ZkStore`]     — ZooKeeper(children + data watch)
/// - 测试用内存 store(`tests` 内)
#[async_trait]
pub trait TopologyStore: Send + Sync {
    /// 全量拉取(启动基线 / watch 断开后重建)
    async fn fetch_all(&self) -> Result<Vec<ShardTopology>, StoreError>;

    /// 订阅变更:持续回调 `on_change`,直到连接断开返回 Err(调用方重建)或被取消。
    /// 实现内部自行处理一次性 watch 的重注册。
    async fn subscribe(
        &self,
        on_change: ChangeCallback,
    ) -> Result<(), StoreError>;

    /// 回写单分片拓扑(供 failover 脚本/管理命令使用;可选能力)
    async fn upsert_shard(&self, topo: &ShardTopology) -> Result<(), StoreError>;

    /// 删除单分片拓扑(可选能力)
    async fn delete_shard(&self, cluster_id: &str, tablet_id: &str) -> Result<(), StoreError>;

    /// 实现标识(日志/排障用)
    fn name(&self) -> &'static str;
}

/// 按配置构建配置中心 store(kind: `none` / `etcd` / `zookeeper`|`zk`)。
/// `none` 返回 Err(调用方应跳过);其余类型连接失败返回 Err。
pub async fn build_store(cfg: &crate::config::ConfigCenterCfg) -> Result<Box<dyn TopologyStore>, StoreError> {
    match cfg.kind.as_str() {
        "etcd" => Ok(Box::new(etcd::EtcdStore::connect(&cfg.endpoints, &cfg.root).await?)),
        "zookeeper" | "zk" => {
            let conn = cfg.endpoints.join(",");
            Ok(Box::new(zk::ZkStore::connect(&conn, &cfg.root)?))
        }
        other => Err(StoreError::Unsupported(other.to_string())),
    }
}

// ─── 运行时拓扑覆盖层 ───

/// 代理内的运行时拓扑覆盖层。
///
/// 文件配置是静态基线;此层保存配置中心下发的动态拓扑。
/// 查询路径(`ensure_backend`)对 (cluster, tablet) 先查此层,命中则用覆盖值,
/// 未命中回落文件配置。变更按分片应用,O(1),与总分片数无关。
#[derive(Default)]
pub struct RuntimeTopology {
    shards: DashMap<(String, String), ShardTopology>,
    /// 变更序号(每次 apply 递增,供展示/排障)
    revision: std::sync::atomic::AtomicU64,
}

impl RuntimeTopology {
    pub fn new() -> Self {
        Self::default()
    }

    /// 应用一条变更,返回受影响分片 key(供连接池定向失效)。
    pub fn apply(&self, change: &TopologyChange) -> (String, String) {
        let key = change.shard_key();
        match change {
            TopologyChange::ShardUpsert(t) => {
                self.shards.insert(key.clone(), t.clone());
            }
            TopologyChange::ShardDelete { cluster_id, tablet_id } => {
                self.shards.remove(&(cluster_id.clone(), tablet_id.clone()));
            }
        }
        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        key
    }

    /// 查询覆盖(命中返回 Some;未命中回落文件配置)
    pub fn get(&self, cluster_id: &str, tablet_id: &str) -> Option<ShardTopology> {
        self.shards
            .get(&(cluster_id.to_string(), tablet_id.to_string()))
            .map(|v| v.clone())
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.shards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }

    pub fn snapshot(&self) -> Vec<ShardTopology> {
        self.shards.iter().map(|e| e.value().clone()).collect()
    }
}

// ─── 驱动循环 ───

/// 配置中心 → 代理的驱动循环:
/// 全量拉取打底 → 订阅增量应用 → 断线重连并重建基线。
///
/// `on_applied` 在每条变更应用后同步回调(典型用途:连接池按分片失效)。
pub async fn run_watcher<S>(
    store: &S,
    runtime: Arc<RuntimeTopology>,
    on_applied: ChangeCallback,
) where
    S: TopologyStore + ?Sized,
{
    info!("config center watcher started: {}", store.name());
    loop {
        // 1. 全量拉取打底(启动 / 重连后)
        match store.fetch_all().await {
            Ok(list) => {
                let n = list.len();
                for t in list {
                    let change = TopologyChange::ShardUpsert(t);
                    runtime.apply(&change);
                    on_applied(change);
                }
                info!(
                    "config center baseline loaded: {} shards (revision {})",
                    n,
                    runtime.revision()
                );
            }
            Err(e) => {
                warn!("config center fetch_all failed: {e}");
            }
        }

        // 2. 订阅增量
        let runtime2 = runtime.clone();
        let on_applied2 = on_applied.clone();
        let cb: ChangeCallback = Arc::new(move |change| {
            let key = runtime2.apply(&change);
            info!(
                "config center change: {:?} (shard {}.{})",
                change,
                key.0,
                key.1
            );
            on_applied2(change);
        });

        match store.subscribe(cb).await {
            Ok(()) => {
                info!("config center subscription ended normally");
                return;
            }
            Err(e) => {
                warn!("config center subscription lost: {e}; reconnecting in 3s");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                // 重连后重建基线(见循环顶部)
            }
        }
    }
}

// ─── 测试 ───

#[cfg(test)]
mod tests {
    use super::*;

    fn topo(cid: &str, tid: &str, master: Option<(&str, u16)>) -> ShardTopology {
        ShardTopology {
            cluster_id: cid.to_string(),
            tablet_id: tid.to_string(),
            master: master.map(|(h, p)| HostAddr::new(h, p)),
            slaves: vec![],
        }
    }

    #[test]
    fn shard_topology_json_roundtrip() {
        let t = topo("c0", "t0", Some(("10.0.0.1", 3306)));
        let bytes = t.to_json().unwrap();
        let back = ShardTopology::from_json(&bytes).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn runtime_topology_apply_get_delete() {
        let rt = RuntimeTopology::new();
        assert_eq!(rt.len(), 0);

        let upsert = TopologyChange::ShardUpsert(topo("c0", "t0", Some(("10.0.0.1", 3306))));
        let key = rt.apply(&upsert);
        assert_eq!(key, ("c0".to_string(), "t0".to_string()));
        assert_eq!(rt.len(), 1);
        assert_eq!(rt.revision(), 1);
        assert!(rt.get("c0", "t0").is_some());
        assert!(rt.get("c0", "t1").is_none());

        // 更新同分片:revision 递增、值替换
        let upsert2 = TopologyChange::ShardUpsert(topo("c0", "t0", Some(("10.0.0.2", 3306))));
        rt.apply(&upsert2);
        assert_eq!(rt.revision(), 2);
        assert_eq!(rt.get("c0", "t0").unwrap().master.unwrap().host, "10.0.0.2");

        // 删除
        let del = TopologyChange::ShardDelete {
            cluster_id: "c0".to_string(),
            tablet_id: "t0".to_string(),
        };
        rt.apply(&del);
        assert_eq!(rt.len(), 0);
        assert!(rt.get("c0", "t0").is_none());
    }

    /// 内存 fake store:可注入初始数据与后续变更,验证 run_watcher 驱动逻辑
    struct FakeStore {
        baseline: Vec<ShardTopology>,
        pending: parking_lot::Mutex<std::sync::mpsc::Receiver<TopologyChange>>,
    }

    impl FakeStore {
        fn new(baseline: Vec<ShardTopology>, pending: Vec<TopologyChange>) -> Self {
            let (tx, rx) = std::sync::mpsc::channel();
            for c in pending {
                tx.send(c).unwrap();
            }
            Self {
                baseline,
                pending: parking_lot::Mutex::new(rx),
            }
        }
    }

    #[async_trait]
    impl TopologyStore for FakeStore {
        async fn fetch_all(&self) -> Result<Vec<ShardTopology>, StoreError> {
            Ok(self.baseline.clone())
        }
        async fn subscribe(
            &self,
            on_change: ChangeCallback,
        ) -> Result<(), StoreError> {
            // 投递 pending 中的变更后结束(正常退出)
            while let Ok(c) = self.pending.lock().try_recv() {
                on_change(c);
            }
            Ok(())
        }
        async fn upsert_shard(&self, _t: &ShardTopology) -> Result<(), StoreError> {
            Ok(())
        }
        async fn delete_shard(&self, _c: &str, _t: &str) -> Result<(), StoreError> {
            Ok(())
        }
        fn name(&self) -> &'static str {
            "fake"
        }
    }

    #[tokio::test]
    async fn run_watcher_seeds_baseline_and_applies_changes() {
        let baseline = vec![topo("c0", "t0", Some(("10.0.0.1", 3306)))];
        let change = TopologyChange::ShardUpsert(topo("c0", "t1", Some(("10.0.0.3", 3306))));
        let store = FakeStore::new(baseline, vec![change]);

        let rt = Arc::new(RuntimeTopology::new());
        let applied: Arc<std::sync::Mutex<Vec<(String, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let applied2 = applied.clone();
        let on_applied: ChangeCallback = Arc::new(move |c: TopologyChange| {
            applied2.lock().unwrap().push(c.shard_key());
        });

        run_watcher(&store, rt.clone(), on_applied).await;

        // 基线 + 增量都应用了
        assert_eq!(rt.len(), 2);
        assert!(rt.get("c0", "t0").is_some());
        assert!(rt.get("c0", "t1").is_some());
        // on_applied 被回调(连接池失效路径)
        assert_eq!(applied.lock().unwrap().len(), 2);
    }

    #[test]
    fn topology_change_shard_key_delete() {
        let t = topo("c0", "t0", Some(("10.0.0.1", 3306)));
        assert_eq!(
            TopologyChange::ShardUpsert(t.clone()).shard_key(),
            ("c0".to_string(), "t0".to_string())
        );
        assert_eq!(
            TopologyChange::ShardDelete { cluster_id: "c1".into(), tablet_id: "t2".into() }
                .shard_key(),
            ("c1".to_string(), "t2".to_string())
        );
    }

    #[test]
    fn runtime_topology_delete_and_meta() {
        let rt = RuntimeTopology::new();
        assert!(rt.is_empty());
        assert_eq!(rt.revision(), 0);
        rt.apply(&TopologyChange::ShardUpsert(topo("c0", "t0", Some(("10.0.0.1", 3306)))));
        rt.apply(&TopologyChange::ShardUpsert(topo("c0", "t1", Some(("10.0.0.2", 3306)))));
        assert_eq!(rt.len(), 2);
        assert_eq!(rt.revision(), 2);
        assert_eq!(rt.snapshot().len(), 2);
        // 删除
        rt.apply(&TopologyChange::ShardDelete { cluster_id: "c0".into(), tablet_id: "t0".into() });
        assert_eq!(rt.len(), 1);
        assert!(rt.get("c0", "t0").is_none());
        assert!(rt.get("c0", "t1").is_some());
        assert_eq!(rt.revision(), 3);
    }

    #[test]
    fn host_addr_endpoint() {
        let h = HostAddr::new("127.0.0.1", 3306);
        assert_eq!(h.endpoint(), "127.0.0.1:3306");
    }

    #[test]
    fn shard_topology_key_and_json_errors() {
        let t = topo("c0", "t0", Some(("10.0.0.1", 3306)));
        assert_eq!(t.key(), ("c0".to_string(), "t0".to_string()));
        // 非法 JSON → Err
        assert!(ShardTopology::from_json(b"not json").is_err());
        assert!(ShardTopology::from_json(b"").is_err());
    }

    #[tokio::test]
    async fn build_store_unsupported_kind() {
        let cfg = crate::config::ConfigCenterCfg {
            kind: "bogus".into(),
            endpoints: vec![],
            root: "/".into(),
        };
        // 未知 kind → Err(Unsupported),而非尝试连接
        match build_store(&cfg).await {
            Err(StoreError::Unsupported(k)) => assert_eq!(k, "bogus"),
            Ok(_) => panic!("bogus kind 不应连接成功"),
            Err(e) => panic!("期望 Unsupported,实际 {e:?}"),
        }
    }
}

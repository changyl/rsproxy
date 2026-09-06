// ZooKeeper 配置中心后端
//
// 节点布局:
//   {root}/clusters/{cluster_id}/tablets/{tablet_id}  → data = JSON(ShardTopology)
//
// zookeeper crate 是阻塞式客户端 + 连接级 watcher(一次性 watch 需重注册),
// 故订阅循环放到独立线程:注册 watch → 阻塞读事件 → 重读变更节点 → 重注册。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use zookeeper::{CreateMode, WatchedEvent, WatchedEventType, Watcher, ZooKeeper};

use super::{ChangeCallback, ShardTopology, StoreError, TopologyChange, TopologyStore};

/// 连接级 watcher:所有 watch 事件投递到 channel
struct EventWatcher {
    tx: std::sync::mpsc::Sender<WatchedEvent>,
}

impl Watcher for EventWatcher {
    fn handle(&self, event: WatchedEvent) {
        let _ = self.tx.send(event);
    }
}

struct ZkInner {
    /// zookeeper 客户端是阻塞式的,且只允许单线程访问,用 parking_lot Mutex 串行化
    zk: parking_lot::Mutex<ZooKeeper>,
    root: String,
    /// watch 事件通道(连接级 watcher 投递;parking_lot Mutex 包裹以满足 Sync)
    events: parking_lot::Mutex<std::sync::mpsc::Receiver<WatchedEvent>>,
}

/// ZooKeeper 后端
pub struct ZkStore {
    inner: Arc<ZkInner>,
}

impl ZkStore {
    /// 连接 ZooKeeper。`connect_string` 形如 `127.0.0.1:2181`。
    pub fn connect(connect_string: &str, root: &str) -> Result<Self, StoreError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let zk = ZooKeeper::connect(connect_string, Duration::from_secs(10), EventWatcher { tx })
            .map_err(|e| StoreError::Connect(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(ZkInner {
                zk: parking_lot::Mutex::new(zk),
                root: root.trim_matches('/').to_string(),
                events: parking_lot::Mutex::new(rx),
            }),
        })
    }

    fn clusters_path(&self) -> String {
        format!("/{}/clusters", self.inner.root)
    }

    fn tablets_path(&self, cid: &str) -> String {
        format!("/{}/clusters/{}/tablets", self.inner.root, cid)
    }

    fn shard_path(&self, cid: &str, tid: &str) -> String {
        format!("/{}/clusters/{}/tablets/{}", self.inner.root, cid, tid)
    }

    // ─── 阻塞式内部实现(在独立线程 / spawn_blocking 中运行) ───

    fn fetch_all_blocking(&self) -> Result<Vec<ShardTopology>, StoreError> {
        let mut out = Vec::new();
        let clusters = {
            let zk = self.inner.zk.lock();
            zk.get_children(&self.clusters_path(), false)
                .map_err(|e| StoreError::Io(format!("get_children {}: {e}", self.clusters_path())))?
        };
        for cid in clusters {
            let tp = self.tablets_path(&cid);
            let tids = {
                let zk = self.inner.zk.lock();
                zk.get_children(&tp, false)
                    .map_err(|e| StoreError::Io(format!("get_children {tp}: {e}")))?
            };
            for tid in tids {
                let sp = self.shard_path(&cid, &tid);
                let zk = self.inner.zk.lock();
                match zk.get_data(&sp, false) {
                    Ok((data, _)) => match ShardTopology::from_json(&data) {
                        Ok(t) => out.push(t),
                        Err(e) => tracing::warn!("zk shard data parse failed at {sp}: {e}"),
                    },
                    Err(e) => tracing::warn!("zk get_data {sp}: {e}"),
                }
            }
        }
        Ok(out)
    }

    /// 订阅循环(阻塞,运行在独立线程)
    fn subscribe_blocking(&self, on_change: ChangeCallback) -> Result<(), StoreError> {
        // 1. 当前拓扑快照 path → topo
        let mut local: HashMap<String, ShardTopology> = self
            .fetch_all_blocking()?
            .into_iter()
            .map(|t| (self.shard_path(&t.cluster_id, &t.tablet_id), t))
            .collect();

        // 2. 注册一次性 watch:每个分片数据节点 + 每个 cluster 的 tablets 子节点
        for path in local.keys() {
            let zk = self.inner.zk.lock();
            let _ = zk.get_data(path, true);
        }
        let clusters = {
            let zk = self.inner.zk.lock();
            zk.get_children(&self.clusters_path(), false).unwrap_or_default()
        };
        for cid in clusters {
            let zk = self.inner.zk.lock();
            let _ = zk.get_children(&self.tablets_path(&cid), true);
        }

        // 3. 事件循环(一次性 watch,每个事件后需重注册)
        loop {
            let event = {
                let rx = self.inner.events.lock();
                rx.recv()
                    .map_err(|_| StoreError::Closed("watch channel closed".into()))?
            };
            let Some(path) = event.path else { continue };

            match event.event_type {
                WatchedEventType::NodeDataChanged => {
                    let zk = self.inner.zk.lock();
                    if let Ok((data, _)) = zk.get_data(&path, true) {
                        if let Ok(t) = ShardTopology::from_json(&data) {
                            local.insert(path.clone(), t.clone());
                            on_change(TopologyChange::ShardUpsert(t));
                        }
                    }
                }
                WatchedEventType::NodeDeleted => {
                    if let Some(t) = local.remove(&path) {
                        on_change(TopologyChange::ShardDelete {
                            cluster_id: t.cluster_id,
                            tablet_id: t.tablet_id,
                        });
                    }
                }
                WatchedEventType::NodeChildrenChanged => {
                    // tablets 层级变化:对比新增/删除的分片
                    let prefix = format!("{path}/");
                    let removed: Vec<String> = local
                        .keys()
                        .filter(|p| p.starts_with(&prefix))
                        .cloned()
                        .collect();

                    let mut seen = Vec::new();
                    let zk = self.inner.zk.lock();
                    if let Ok(tids) = zk.get_children(&path, true) {
                        for tid in &tids {
                            let sp = format!("{path}/{tid}");
                            seen.push(sp.clone());
                            if !local.contains_key(&sp) {
                                if let Ok((data, _)) = zk.get_data(&sp, false) {
                                    if let Ok(t) = ShardTopology::from_json(&data) {
                                        local.insert(sp.clone(), t.clone());
                                        on_change(TopologyChange::ShardUpsert(t));
                                    }
                                }
                            }
                        }
                    }

                    // 删除已不存在的分片
                    for old in removed {
                        if !seen.contains(&old) {
                            if let Some(t) = local.remove(&old) {
                                on_change(TopologyChange::ShardDelete {
                                    cluster_id: t.cluster_id,
                                    tablet_id: t.tablet_id,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[async_trait]
impl TopologyStore for ZkStore {
    async fn fetch_all(&self) -> Result<Vec<ShardTopology>, StoreError> {
        let this = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let store = ZkStore { inner: this };
            store.fetch_all_blocking()
        })
        .await
        .map_err(|e| StoreError::Io(format!("spawn_blocking: {e}")))?
    }

    async fn subscribe(&self, on_change: ChangeCallback) -> Result<(), StoreError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            let store = ZkStore { inner };
            let res = store.subscribe_blocking(on_change);
            let _ = tx.blocking_send(res);
        });
        rx.recv()
            .await
            .unwrap_or_else(|| Err(StoreError::Closed("subscriber thread ended".into())))
    }

    async fn upsert_shard(&self, topo: &ShardTopology) -> Result<(), StoreError> {
        let path = self.shard_path(&topo.cluster_id, &topo.tablet_id);
        let data = topo.to_json()?;
        let root = self.clusters_path();
        let (cid_path, tp_path) = (
            format!("{root}/{}", topo.cluster_id),
            self.tablets_path(&topo.cluster_id),
        );
        let this = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let zk = this.zk.lock();
            // 确保父节点存在
            for p in [&root, &cid_path, &tp_path] {
                if zk.exists(p, false).ok().flatten().is_none() {
                    zk.create(p, vec![], zookeeper::Acl::open_unsafe().clone(), CreateMode::Persistent)
                        .map_err(|e| StoreError::Io(format!("create {p}: {e}")))?;
                }
            }
            if zk.exists(&path, false).ok().flatten().is_some() {
                zk.set_data(&path, data, None)
                    .map_err(|e| StoreError::Io(format!("set_data {path}: {e}")))?;
            } else {
                zk.create(&path, data, zookeeper::Acl::open_unsafe().clone(), CreateMode::Persistent)
                    .map_err(|e| StoreError::Io(format!("create {path}: {e}")))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| StoreError::Io(format!("spawn_blocking: {e}")))?
    }

    async fn delete_shard(&self, cluster_id: &str, tablet_id: &str) -> Result<(), StoreError> {
        let path = self.shard_path(cluster_id, tablet_id);
        let this = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let zk = this.zk.lock();
            zk.delete(&path, None)
                .map_err(|e| StoreError::Io(format!("delete {path}: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| StoreError::Io(format!("spawn_blocking: {e}")))?
    }

    fn name(&self) -> &'static str {
        "zookeeper"
    }
}

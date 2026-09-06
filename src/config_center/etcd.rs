// etcd v3 配置中心后端
//
// key 布局:`{root}/clusters/{cluster_id}/tablets/{tablet_id}` → JSON(ShardTopology)
// watch:`{root}/clusters/` 前缀,增量事件按 key 解析出 (cluster, tablet)。
//
// etcd-client 0.19:Client 方法均为 `&mut self`,故内部用 tokio Mutex 包一层;
// `watch()` 返回拥有的 WatchStream,直接 `message()` 拉取事件。

use async_trait::async_trait;
use etcd_client::{Client, EventType, GetOptions, WatchOptions};
use tokio::sync::Mutex;

use super::{ChangeCallback, ShardTopology, StoreError, TopologyChange, TopologyStore};

/// etcd 后端
pub struct EtcdStore {
    client: Mutex<Client>,
    /// key 根前缀,如 `/newproxy`
    root: String,
}

impl EtcdStore {
    /// 连接 etcd。`endpoints` 形如 `http://127.0.0.1:2379`。
    pub async fn connect(endpoints: &[String], root: &str) -> Result<Self, StoreError> {
        if endpoints.is_empty() {
            return Err(StoreError::Connect("endpoints 为空".into()));
        }
        let client = Client::connect(endpoints, None)
            .await
            .map_err(|e| StoreError::Connect(e.to_string()))?;
        Ok(Self {
            client: Mutex::new(client),
            root: root.trim_end_matches('/').to_string(),
        })
    }

    fn shard_key(&self, cluster_id: &str, tablet_id: &str) -> String {
        format!("{}/clusters/{}/tablets/{}", self.root, cluster_id, tablet_id)
    }

    fn shard_prefix(&self) -> String {
        format!("{}/clusters/", self.root)
    }

    /// 从完整 key 解析 (cluster_id, tablet_id)
    fn parse_shard_key(&self, key: &str) -> Option<(String, String)> {
        let rest = key.strip_prefix(&self.shard_prefix())?;
        let mut parts = rest.split('/');
        let cid = parts.next()?;
        if parts.next()? != "tablets" {
            return None;
        }
        let tid = parts.next()?;
        Some((cid.to_string(), tid.to_string()))
    }
}

#[async_trait]
impl TopologyStore for EtcdStore {
    async fn fetch_all(&self) -> Result<Vec<ShardTopology>, StoreError> {
        let resp = self
            .client
            .lock()
            .await
            .get(self.shard_prefix(), Some(GetOptions::new().with_prefix()))
            .await
            .map_err(|e| StoreError::Io(format!("get: {e}")))?;

        let mut out = Vec::new();
        for kv in resp.kvs() {
            match ShardTopology::from_json(kv.value()) {
                Ok(t) => out.push(t),
                Err(e) => {
                    // 单个分片数据损坏不拖垮全量拉取
                    tracing::warn!("etcd shard data parse failed at {:?}: {e}", kv.key_str());
                }
            }
        }
        Ok(out)
    }

    async fn subscribe(&self, on_change: ChangeCallback) -> Result<(), StoreError> {
        let stream = self
            .client
            .lock()
            .await
            .watch(self.shard_prefix(), Some(WatchOptions::new().with_prefix()))
            .await
            .map_err(|e| StoreError::Io(format!("watch: {e}")))?;

        let mut stream = stream;
        while let Some(resp) = stream
            .message()
            .await
            .map_err(|e| StoreError::Io(format!("watch stream: {e}")))?
        {
            for ev in resp.events() {
                let Some(kv) = ev.kv() else { continue };
                let key = kv.key_str().unwrap_or_default();
                let Some((cid, tid)) = self.parse_shard_key(key) else {
                    continue;
                };
                match ev.event_type() {
                    EventType::Put => {
                        if let Ok(topo) = ShardTopology::from_json(kv.value()) {
                            on_change(TopologyChange::ShardUpsert(topo));
                        }
                    }
                    EventType::Delete => {
                        on_change(TopologyChange::ShardDelete {
                            cluster_id: cid,
                            tablet_id: tid,
                        });
                    }
                }
            }
        }
        Err(StoreError::Closed("watch stream ended".into()))
    }

    async fn upsert_shard(&self, topo: &ShardTopology) -> Result<(), StoreError> {
        let key = self.shard_key(&topo.cluster_id, &topo.tablet_id);
        let val = topo.to_json()?;
        self.client
            .lock()
            .await
            .put(key, val, None)
            .await
            .map_err(|e| StoreError::Io(format!("put: {e}")))?;
        Ok(())
    }

    async fn delete_shard(&self, cluster_id: &str, tablet_id: &str) -> Result<(), StoreError> {
        self.client
            .lock()
            .await
            .delete(self.shard_key(cluster_id, tablet_id), None)
            .await
            .map_err(|e| StoreError::Io(format!("delete: {e}")))?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "etcd"
    }
}

// 仅编译期校验:EtcdStore 可 Send+Sync(其内部 client 满足)
fn _assert_send_sync(_: &EtcdStore) {
    fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<EtcdStore>();
}

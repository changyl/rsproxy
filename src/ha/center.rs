// Xenon raft 发现运行时:受管分片 worker + supervisor + 拓扑生效
//
// 数据流:每受管分片一个循环任务 → 探测全部成员 raft 状态表 → judge 判主 →
// 变化时 `topology.apply(ShardUpsert)` + `srv_pool.invalidate_shard`(与配置中心
// watch 同一生效链路)。另含:探测失败降级(keep-last-known → TTL 后回退基线)、
// 半同步状态监控(仅告警/指标,不参与读判定)、force_probe(故障路径触发)。

use std::sync::Arc;

use dashmap::DashMap;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::app::AppCtx;
use crate::config::{DbUser, XenonRaft};
use crate::config_center::{HostAddr, ShardTopology, TopologyChange};
use crate::ha::model::{decide, now_ms, Decision, JudgeOut, ProbeRow};

pub type ShardKey = (String, String);

/// 单分片 HA 运行时状态
#[derive(Debug, Clone)]
pub struct HaState {
    /// 当前判定的 raft leader(MySQL 地址);None = 无有效信息(回退基线)
    pub leader: Option<HostAddr>,
    /// followers(成员减 leader,保序)
    pub followers: Vec<HostAddr>,
    pub view_id: u64,
    pub epoch_id: u64,
    pub updated_at_ms: u64,
    /// 最近一次成功探测时间(ms)
    pub last_ok_ms: u64,
    /// 探测期间无有效信息(降级中)
    pub degraded: bool,
    /// leader 侧半同步状态(Some(ON/OFF);None = 未知/未监控)
    pub semisync_on: Option<bool>,
    /// 最近一次 leader 变更时间(ms)
    pub last_change_ms: u64,
    /// 最近一次探测错误数(本周期)
    pub last_probe_errors: usize,
}

impl HaState {
    fn new() -> Self {
        Self {
            leader: None,
            followers: Vec::new(),
            view_id: 0,
            epoch_id: 0,
            updated_at_ms: 0,
            last_ok_ms: 0,
            degraded: false,
            semisync_on: None,
            last_change_ms: 0,
            last_probe_errors: 0,
        }
    }
}

/// 高可用运行时中心(挂在 AppCtx 上)
#[derive(Default)]
pub struct HaCenter {
    /// 受管分片运行时状态
    pub states: DashMap<ShardKey, HaState>,
    /// 存活 worker 集(防止重复 spawn)
    alive: DashMap<ShardKey, ()>,
    /// 手动重探请求(supervisor 每 tick 消费;管理 API 触发,无需 Arc<AppCtx>)
    requests: parking_lot::Mutex<Vec<ShardKey>>,
}

impl HaCenter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求一次手动重探(管理命令;supervisor 下个 tick 消费)
    pub fn request_reprobe(&self, cid: &str, tid: &str) {
        let mut q = self.requests.lock();
        let key = (cid.to_string(), tid.to_string());
        if !q.contains(&key) {
            q.push(key);
        }
    }

    /// 启动 supervisor(常驻;无受管分片时每秒空转,开销可忽略)
    pub fn spawn_supervisor(ctx: Arc<AppCtx>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(1000));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                // 消费手动重探请求
                let ha = ctx.ha.clone();
                let reqs: Vec<ShardKey> = std::mem::take(&mut *ha.requests.lock());
                for (cid, tid) in reqs {
                    let c = ctx.clone();
                    tokio::spawn(async move {
                        HaCenter::force_probe_async(c, cid, tid).await;
                    });
                }
                reconcile(ctx.clone());
            }
        });
    }

    /// 主动触发一次单分片探测(连接失败/管理命令调用;幂等)
    pub fn force_probe(ctx: Arc<AppCtx>, cid: String, tid: String) {
        tokio::spawn(async move {
            Self::force_probe_async(ctx, cid, tid).await;
        });
    }

    /// 同步等待一次单分片探测完成(前端连接失败路径用它探明新 leader 再重试)
    pub async fn force_probe_async(ctx: Arc<AppCtx>, cid: String, tid: String) {
        let key = (cid.clone(), tid.clone());
        run_one_pass(&ctx, &key, None).await;
    }

    pub fn snapshot(&self, cid: &str, tid: &str) -> Option<HaState> {
        self.states.get(&(cid.to_string(), tid.to_string())).map(|e| e.clone())
    }

    /// 全部受管分片状态(供管理 API)
    pub fn all_states(&self) -> Vec<((String, String), HaState)> {
        self.states.iter().map(|e| (e.key().clone(), e.value().clone())).collect()
    }
}

/// 从当前配置收集受管分片:((cluster,tablet), xenon 配置, cluster 名)
fn managed(ctx: &AppCtx) -> Vec<(ShardKey, XenonRaft, String)> {
    let cfg = ctx.load_config();
    let mut out = Vec::new();
    for c in cfg.clusters.values() {
        for t in &c.tablets {
            if let Some(x) = &t.xenon {
                out.push((
                    (c.id.clone(), t.tablet_id.clone()),
                    x.clone(),
                    c.name.clone(),
                ));
            }
        }
    }
    out
}

/// 每秒对账:新增受管分片 → spawn worker;worker 自身在配置摘除后退出
fn reconcile(ctx: Arc<AppCtx>) {
    for (key, _x, cluster_name) in managed(ctx.as_ref()) {
        if ctx.ha.alive.contains_key(&key) {
            continue;
        }
        ctx.ha.alive.insert(key.clone(), ());
        let ctx = ctx.clone();
        let k2 = key.clone();
        let cname = cluster_name.clone();
        tokio::spawn(async move {
            info!(cluster = %k2.0, tablet = %k2.1, "xenon raft HA worker started");
            let mut no_user_logged = false;
            loop {
                let cfg = ctx.load_config();
                // 配置摘除 → worker 退出
                let still_managed = cfg
                    .clusters
                    .get(&k2.0)
                    .and_then(|c| c.tablets.iter().find(|t| t.tablet_id == k2.1))
                    .and_then(|t| t.xenon.clone());
                let Some(x) = still_managed else {
                    info!(cluster = %k2.0, tablet = %k2.1, "xenon raft HA worker stopped (config removed)");
                    ctx.ha.alive.remove(&k2);
                    return;
                };
                // 后端账号:该集群的 db_user(需 mysql.xenon_raft_status SELECT 权限)
                let db_user = cfg
                    .db_users
                    .values()
                    .find(|u| u.cluster_name.eq_ignore_ascii_case(&cname))
                    .cloned();
                match db_user {
                    Some(_) => {
                        no_user_logged = false;
                        run_one_pass(&ctx, &k2, Some(&x)).await;
                    }
                    None => {
                        if !no_user_logged {
                            warn!(cluster = %k2.0, tablet = %k2.1, cluster_name = %cname,
                                "no db_user for cluster; HA probe disabled (SELECT 权限: db_user 需可查 mysql.xenon_raft_status)");
                            no_user_logged = true;
                        }
                    }
                }
                let interval = cfg
                    .clusters
                    .get(&k2.0)
                    .and_then(|c| c.tablets.iter().find(|t| t.tablet_id == k2.1))
                    .and_then(|t| t.xenon.as_ref())
                    .map(|x| x.probe_interval_ms.max(200))
                    .unwrap_or(1000);
                drop(cfg);
                sleep(Duration::from_millis(interval)).await;
            }
        });
    }
}

/// 单次探测通过(force_probe 与周期 worker 共用)。
/// `x` 为 None 时(force)从配置现读。
async fn run_one_pass(ctx: &Arc<AppCtx>, key: &ShardKey, x: Option<&XenonRaft>) {
    let (cid, tid) = (&key.0, &key.1);
    let cfg = ctx.load_config();
    let Some(xenon) = x.cloned().or_else(|| {
        cfg.clusters
            .get(cid)
            .and_then(|c| c.tablets.iter().find(|t| &t.tablet_id == tid))
            .and_then(|t| t.xenon.clone())
    }) else {
        return;
    };
    let cluster_name = cfg
        .clusters
        .get(cid)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    let db_user = cfg
        .db_users
        .values()
        .find(|u| u.cluster_name.eq_ignore_ascii_case(&cluster_name))
        .cloned();
    let charset = cfg.default_charset;
    drop(cfg);
    let Some(db_user) = db_user else { return };

    // 1) 并发探测所有成员
    let mut probes: Vec<(String, Result<ProbeRow, String>)> = Vec::new();
    for m in &xenon.members {
        let fut = crate::ha::probe::probe_raft_status(
            &m.host,
            m.mysql_port,
            &db_user,
            charset,
            xenon.probe_timeout_ms,
        );
        probes.push((m.host.clone(), fut.await));
    }
    let errors = probes.iter().filter(|(_, r)| r.is_err()).count();
    let rows: Vec<ProbeRow> = probes
        .iter()
        .filter_map(|(_, r)| r.clone().ok())
        .collect();
    if errors > 0 {
        ctx.metrics.ha_probe_errors.add(errors as u64);
        for (host, r) in probes.iter().filter(|(_, r)| r.is_err()) {
            debug_span_err(cid, tid, host, r.as_ref().err().unwrap_or(&"".to_string()));
        }
    }

    let now = now_ms();
    let decision = decide(&xenon.members, now, xenon.leader_stale_ms, rows);
    apply_outcome(ctx, key, &xenon, decision, errors, db_user, charset).await;
}

fn debug_span_err(cid: &str, tid: &str, host: &str, err: &str) {
    tracing::debug!(cluster = cid, tablet = tid, member = host, err, "HA probe failed");
}

/// 把判主结果落到状态 + 拓扑 + 连接池;附带半同步采样(每周期对 leader 一次)。
async fn apply_outcome(
    ctx: &Arc<AppCtx>,
    key: &ShardKey,
    x: &XenonRaft,
    decision: Decision,
    probe_errors: usize,
    db_user: DbUser,
    charset: u8,
) {
    let (cid, tid) = (key.0.clone(), key.1.clone());
    let now = now_ms();
    match decision {
        Decision::Valid(out) => {
            let JudgeOut { leader, followers, view_id, epoch_id, updated_at_ms, .. } = out;
            let (first, changed) = {
                let mut st = ctx.ha.states.entry(key.clone()).or_insert_with(HaState::new);
                let s = st.value_mut();
                let first = s.leader.is_none();
                let changed = s.leader.as_ref() != Some(&leader)
                    || s.view_id != view_id
                    || s.epoch_id != epoch_id;
                s.leader = Some(leader.clone());
                s.followers = followers.clone();
                s.view_id = view_id;
                s.epoch_id = epoch_id;
                s.updated_at_ms = updated_at_ms;
                s.last_ok_ms = now;
                s.last_probe_errors = probe_errors;
                let was_degraded = s.degraded;
                s.degraded = false;
                if changed {
                    s.last_change_ms = now;
                }
                (first, changed || (was_degraded && first))
            };
            if first || changed {
                let topo = ShardTopology {
                    cluster_id: cid.clone(),
                    tablet_id: tid.clone(),
                    master: Some(leader.clone()),
                    slaves: followers.clone(),
                };
                ctx.topology.apply(&TopologyChange::ShardUpsert(topo));
                ctx.srv_pool.invalidate_shard(&cid, &tid);
                if !first {
                    ctx.metrics.ha_leader_changes.inc();
                }
                info!(cluster = %cid, tablet = %tid, leader = %leader.endpoint(),
                    view_id, epoch_id, first, "xenon raft leader resolved");
            }
            // 半同步状态采样(周期内对当前 leader 一次)
            sample_semisync(ctx, &cid, &tid, &leader, &db_user, charset).await;
        }
        Decision::Unknown => {
            let stale = {
                let mut st = ctx.ha.states.entry(key.clone()).or_insert_with(HaState::new);
                let s = st.value_mut();
                s.last_probe_errors = probe_errors;
                if !s.degraded {
                    ctx.metrics.ha_degraded_cycles.inc();
                }
                s.degraded = true;
                // TTL 内保留 last-known(topology 不清除,resolve 仍用旧 leader);
                // 超过 TTL → 清 topology 回退配置基线 master
                let stale = s.leader.is_some()
                    && now.saturating_sub(s.last_ok_ms) > x.leader_stale_ms;
                if stale {
                    s.leader = None;
                }
                stale
            };
            if stale {
                warn!(cluster = %cid, tablet = %tid,
                    "xenon raft leader unknown beyond TTL; falling back to config baseline master");
                ctx.topology.apply(&TopologyChange::ShardDelete {
                    cluster_id: cid.clone(),
                    tablet_id: tid.clone(),
                });
                ctx.srv_pool.invalidate_shard(&cid, &tid);
            }
        }
    }
}

/// 采样 leader 侧半同步状态(仅供告警/指标;读一致性不依赖它)。
async fn sample_semisync(
    ctx: &Arc<AppCtx>,
    cid: &str,
    tid: &str,
    leader: &HostAddr,
    db_user: &DbUser,
    charset: u8,
) {
    const SQL: &str = "SHOW STATUS LIKE 'Rpl_semi_sync_master_status'";
    match crate::ha::probe::query_text(
        &leader.host,
        leader.port,
        db_user,
        charset,
        SQL,
        800,
    )
    .await
    {
        Ok(res) => {
            let on = res
                .rows
                .first()
                .and_then(|_| res.row_map(0).get("value").cloned())
                .map(|v| v.eq_ignore_ascii_case("ON"));
            let prev = {
                let mut st = ctx.ha.states.entry((cid.to_string(), tid.to_string())).or_insert_with(HaState::new);
                let s = st.value_mut();
                let prev = s.semisync_on;
                s.semisync_on = on;
                prev
            };
            if on == Some(false) && prev != Some(false) {
                ctx.metrics.ha_semisync_degrade_events.inc();
                warn!(cluster = cid, tablet = tid, "semi-sync degraded to async (Rpl_semi_sync_master_status=OFF); leader crash may lose tail transactions");
            }
        }
        Err(e) => {
            tracing::debug!(cluster = cid, tablet = tid, err = %e, "semisync status sample failed");
        }
    }
}

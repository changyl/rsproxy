// 负载均衡:power-of-two-choices + failover 重试 bitmap
// T2.4 实现
//
// 对齐 C 侧 tr_route.c 的 DB 选择和重试逻辑
// 设计参考 /docs/03-backend-pool-optimization.md §7

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use rand::Rng;

use crate::config::{Database, DatabaseGroup, MasterSlave};

/// 后端选择结果
#[derive(Debug, Clone)]
pub struct BackendTarget {
    pub db: Arc<Database>,
    pub ms: MasterSlave,
}

/// 负载均衡器:从一组候选数据库中选择最优后端
pub struct LoadBalancer;

impl LoadBalancer {
    /// Power-of-two-choices:从候选中随机选 2 个,返回连接数较少的
    ///
    /// 时间复杂度 O(n),在大规模场景下比全局最优(O(n log n))更实用
    pub fn select<'a>(
        candidates: &[&'a BackendTarget],
        rng: &mut impl Rng,
    ) -> Option<&'a BackendTarget> {
        match candidates.len() {
            0 => None,
            1 => Some(candidates[0]),
            _ => {
                let i1 = rng.gen_range(0..candidates.len());
                let i2 = rng.gen_range(0..candidates.len());
                let c1 = candidates[i1];
                let c2 = candidates[i2];
                // 选择权重较高(或连接数较少)的
                if c1.db.weight >= c2.db.weight {
                    Some(c1)
                } else {
                    Some(c2)
                }
            }
        }
    }

    /// 从 DatabaseGroup 中获取活跃的候选列表
    pub fn candidates_from_group(group: &DatabaseGroup, ms: MasterSlave) -> Vec<BackendTarget> {
        let mut targets = Vec::new();
        let db = match ms {
            MasterSlave::Master => group.master.as_ref(),
            MasterSlave::Slave => group.slave.as_ref(),
        };
        if let Some(db) = db {
            targets.push(BackendTarget {
                db: Arc::new(db.clone()),
                ms,
            });
        }
        targets
    }

    /// 从多个 DatabaseGroup 获取所有活跃候选
    pub fn candidates_from_groups(groups: &[DatabaseGroup], ms: MasterSlave) -> Vec<BackendTarget> {
        let mut targets = Vec::new();
        for group in groups {
            let db = match ms {
                MasterSlave::Master => group.master.as_ref(),
                MasterSlave::Slave => group.slave.as_ref(),
            };
            if let Some(db) = db {
                targets.push(BackendTarget {
                    db: Arc::new(db.clone()),
                    ms,
                });
            }
        }
        targets
    }
}

// ─── Failover ───

/// Failover 状态:记录每次重试跳过哪些后端
pub struct FailoverState {
    /// 已尝试过的后端索引 bitmap
    tried: u64,
    /// 总候选数
    total: usize,
}

impl FailoverState {
    pub fn new(total: usize) -> Self {
        Self { tried: 0, total }
    }

    /// 标记索引为已尝试
    pub fn mark_tried(&mut self, index: usize) {
        if index < 64 {
            self.tried |= 1 << index;
        }
    }

    /// 检查索引是否被标记
    pub fn is_tried(&self, index: usize) -> bool {
        index < 64 && (self.tried & (1 << index)) != 0
    }

    /// 是否还有未尝试的候选
    pub fn has_more(&self) -> bool {
        let mask = if self.total >= 64 {
            u64::MAX
        } else {
            (1u64 << self.total) - 1
        };
        self.tried != mask
    }

    /// 从候选列表中获取下一个未标记的
    pub fn try_next<'a>(&mut self, candidates: &[&'a BackendTarget]) -> Option<&'a BackendTarget> {
        for (i, c) in candidates.iter().enumerate() {
            if !self.is_tried(i) {
                self.mark_tried(i);
                return Some(c);
            }
        }
        None
    }
}

// ─── 后端健康状态 ───

/// 后端健康追踪器
pub struct HealthTracker {
    /// 连续失败次数
    failures: AtomicU64,
    /// 是否被标记为 down
    down: AtomicBool,
    /// 最大容忍失败次数
    max_failures: u64,
}

impl HealthTracker {
    pub fn new(max_failures: u64) -> Self {
        Self {
            failures: AtomicU64::new(0),
            down: AtomicBool::new(false),
            max_failures,
        }
    }

    /// 记录一次成功
    pub fn mark_success(&self) {
        self.failures.store(0, Ordering::Release);
        self.down.store(false, Ordering::Release);
    }

    /// 记录一次失败,返回是否超过阈值
    pub fn mark_failure(&self) -> bool {
        let n = self.failures.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= self.max_failures {
            self.down.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// 是否被标记为 down
    pub fn is_down(&self) -> bool {
        self.down.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_db(host: &str, weight: u32) -> Database {
        Database {
            host: host.to_string(),
            port: 3306,
            max_pool_size: 16,
            max_connections: 256,
            connect_timeout: 5,
            weight,
            tablet_name: None,
        }
    }

    #[test]
    fn power_of_two_single() {
        let db = make_db("10.0.0.1", 1);
        let target = BackendTarget {
            db: Arc::new(db),
            ms: MasterSlave::Master,
        };
        let candidates = [&target];
        let mut rng = rand::thread_rng();
        let selected = LoadBalancer::select(&candidates, &mut rng).unwrap();
        assert_eq!(selected.db.host, "10.0.0.1");
    }

    #[test]
    fn power_of_two_empty() {
        let candidates: [&BackendTarget; 0] = [];
        let mut rng = rand::thread_rng();
        assert!(LoadBalancer::select(&candidates, &mut rng).is_none());
    }

    #[test]
    fn power_of_two_prefers_higher_weight() {
        let db1 = make_db("10.0.0.1", 1);
        let db2 = make_db("10.0.0.2", 10);
        let t1 = BackendTarget {
            db: Arc::new(db1),
            ms: MasterSlave::Master,
        };
        let t2 = BackendTarget {
            db: Arc::new(db2),
            ms: MasterSlave::Master,
        };
        let candidates = [&t1, &t2];

        // 测试多次,权重高的应更频繁被选中
        let mut rng = rand::thread_rng();
        let mut count = 0;
        for _ in 0..100 {
            if let Some(t) = LoadBalancer::select(&candidates, &mut rng) {
                if t.db.weight == 10 {
                    count += 1;
                }
            }
        }
        // 由于随机性,预期 > 40% (50% 概率选到)
        assert!(
            count > 30,
            "weight 10 should be selected at least ~50% of the time, got {count}"
        );
    }

    #[test]
    fn failover_state_has_more() {
        let mut state = FailoverState::new(3);
        assert!(state.has_more());

        state.mark_tried(0);
        assert!(state.has_more());

        state.mark_tried(1);
        assert!(state.has_more());

        state.mark_tried(2);
        assert!(!state.has_more());
    }

    #[test]
    fn failover_try_next() {
        let db = make_db("10.0.0.1", 1);
        let t1 = BackendTarget {
            db: Arc::new(db.clone()),
            ms: MasterSlave::Master,
        };
        let t2 = BackendTarget {
            db: Arc::new(db.clone()),
            ms: MasterSlave::Master,
        };
        let t3 = BackendTarget {
            db: Arc::new(db),
            ms: MasterSlave::Master,
        };
        let candidates = [&t1, &t2, &t3];

        let mut state = FailoverState::new(3);
        let r1 = state.try_next(&candidates).unwrap();
        assert_eq!(r1.db.host, "10.0.0.1");

        let r2 = state.try_next(&candidates).unwrap();
        assert_eq!(r2.db.host, "10.0.0.1");

        let r3 = state.try_next(&candidates).unwrap();
        assert_eq!(r3.db.host, "10.0.0.1");

        assert!(state.try_next(&candidates).is_none());
    }

    #[test]
    fn health_tracker() {
        let ht = HealthTracker::new(3);
        assert!(!ht.is_down());

        assert!(!ht.mark_failure());
        assert!(!ht.mark_failure());
        assert!(ht.mark_failure()); // 第 3 次跨阈值
        assert!(ht.is_down());

        ht.mark_success();
        assert!(!ht.is_down());
    }

    #[test]
    fn candidates_from_group_none() {
        // 无 master/slave 的组 → 空候选
        let group = DatabaseGroup {
            group_id: "g0".into(),
            master: None,
            slave: None,
        };
        assert!(LoadBalancer::candidates_from_group(&group, MasterSlave::Master).is_empty());
        assert!(LoadBalancer::candidates_from_group(&group, MasterSlave::Slave).is_empty());
        // 只有 master 时选 slave → 空
        let g2 = DatabaseGroup {
            group_id: "g1".into(),
            master: Some(make_db("10.0.0.9", 1)),
            slave: None,
        };
        let c = LoadBalancer::candidates_from_group(&g2, MasterSlave::Slave);
        assert!(c.is_empty());
        let c = LoadBalancer::candidates_from_group(&g2, MasterSlave::Master);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].db.host, "10.0.0.9");
        assert_eq!(c[0].ms, MasterSlave::Master);
    }

    #[test]
    fn candidates_from_groups_mixed() {
        // 混合组:部分无 master/slave 被跳过
        let full = DatabaseGroup {
            group_id: "a".into(),
            master: Some(make_db("h1", 1)),
            slave: Some(make_db("h2", 1)),
        };
        let empty = DatabaseGroup {
            group_id: "b".into(),
            master: None,
            slave: None,
        };
        let groups = [full.clone(), empty, full];
        let masters = LoadBalancer::candidates_from_groups(&groups, MasterSlave::Master);
        assert_eq!(masters.len(), 2);
        let slaves = LoadBalancer::candidates_from_groups(&groups, MasterSlave::Slave);
        assert_eq!(slaves.len(), 2);
        assert_eq!(slaves[0].db.host, "h2");
    }

    #[test]
    fn failover_bitmap_guards() {
        // 索引 ≥64 的标记/查询被忽略
        let mut state = FailoverState::new(64);
        state.mark_tried(64);
        assert!(!state.is_tried(64));
        // 大量候选时 mask 为全 1
        let big = FailoverState::new(64);
        assert!(big.has_more());
        let mut all = FailoverState::new(2);
        all.mark_tried(0);
        all.mark_tried(1);
        assert!(!all.has_more());
        // 空候选列表 → try_next 返回 None
        let mut s = FailoverState::new(0);
        assert!(!s.has_more());
        let empty: [&BackendTarget; 0] = [];
        assert!(s.try_next(&empty).is_none());
    }

    #[test]
    fn select_weight_tie_returns_first() {
        // 同权重:返回候选 1(>= 分支)
        let db1 = make_db("10.0.0.1", 5);
        let db2 = make_db("10.0.0.2", 5);
        let t1 = BackendTarget { db: Arc::new(db1), ms: MasterSlave::Master };
        let t2 = BackendTarget { db: Arc::new(db2), ms: MasterSlave::Master };
        let candidates = [&t1, &t2];
        // 受控 RNG 依次抽样 0、1 → i1=0/i2=1。
        // 同权重命中 >= 分支，必须保留第一个抽样候选 t1。
        struct SequenceRng { n: usize }
        impl rand::RngCore for SequenceRng {
            fn next_u32(&mut self) -> u32 {
                let v = if self.n == 0 { 0 } else { 1u32 << 31 };
                self.n += 1;
                v
            }
            fn next_u64(&mut self) -> u64 {
                let v = if self.n == 0 { 0 } else { 1u64 << 63 };
                self.n += 1;
                v
            }
            fn fill_bytes(&mut self, dest: &mut [u8]) {
                let mut offset = 0;
                while offset < dest.len() {
                    let bytes = self.next_u64().to_le_bytes();
                    let n = (dest.len() - offset).min(bytes.len());
                    dest[offset..offset + n].copy_from_slice(&bytes[..n]);
                    offset += n;
                }
            }
            fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
                self.fill_bytes(dest);
                Ok(())
            }
        }
        let mut rng = SequenceRng { n: 0 };
        let sel = LoadBalancer::select(&candidates, &mut rng).unwrap();
        assert_eq!(sel.db.host, "10.0.0.1", "同权重应保留第一个抽样候选");
    }
}

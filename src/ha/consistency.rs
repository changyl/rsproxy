// 读分流决策引擎(纯逻辑,会话状态机在 front.rs 维护)
//
// 档位(见 config::ReadConsistency):Strong / Causal / Session / Eventual。
// 语义:
// - Strong:    一律 leader(不产生分流);
// - Causal:    每次可分流读都等全局高水位 GTID(跨客户端因果);
// - Session:   仅当本会话在本分片写过数据(session_dirty)才等水位
//              (读己之写 + 本会话单调读);从未写过 → 免屏障直读;
// - Eventual:  免屏障直读 follower(允许任意滞后/乱序,业务自担)。
//
// 强制 leader 的条件(in_txn / 会话 pin / 语句不可分流)由调用方先判定,
// 本模块只做"剩余可分流空间内"的档位决策,保证各档差分的纯逻辑可单测。

use crate::config::ReadConsistency;
use crate::parser::analyze::ReadClass;

/// 后端角色选择结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRole {
    Leader,
    Follower,
}

/// 分流读需要执行的屏障类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Barrier {
    /// 免屏障(直接读)
    None,
    /// 需要先等 leader 高水位 GTID(因果=总是;会话=仅会话写过)
    WaitHighWater,
}

/// 会话视角(由 front.rs 会话状态维护)
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionView {
    /// 显式事务中(BEGIN 后 / COMMIT 前)
    pub in_txn: bool,
    /// 会话被 pin 到 leader(SET / @变量 / 临时表 / prepared / autocommit=0)
    pub pinned: bool,
    /// 本会话在本分片是否执行过写(自上次"干净"以来)
    pub session_dirty_shard: bool,
}

/// 路由决策:角色 + 屏障 + 拒绝原因(记录用)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteDecision {
    pub role: RouteRole,
    pub barrier: Barrier,
    /// 走 leader 的原因(供指标/日志;None = 无)
    pub leader_reason: Option<&'static str>,
}

pub const LEADER: RouteDecision = RouteDecision {
    role: RouteRole::Leader,
    barrier: Barrier::None,
    leader_reason: None,
};

/// 依据生效档位 + 语句可分流性 + 会话状态做出路由决策。
pub fn decide(
    effective: ReadConsistency,
    read_class: ReadClass,
    sess: SessionView,
) -> RouteDecision {
    // 事务 / pin / 语句不可分流 → 强制 leader
    if sess.in_txn {
        return RouteDecision {
            role: RouteRole::Leader,
            barrier: Barrier::None,
            leader_reason: Some("in-transaction"),
        };
    }
    if sess.pinned {
        return RouteDecision {
            role: RouteRole::Leader,
            barrier: Barrier::None,
            leader_reason: Some("session-pinned"),
        };
    }
    if let ReadClass::LeaderRequired(reason) = read_class {
        return RouteDecision {
            role: RouteRole::Leader,
            barrier: Barrier::None,
            leader_reason: Some(reason),
        };
    }
    match effective {
        ReadConsistency::Strong => RouteDecision {
            role: RouteRole::Leader,
            barrier: Barrier::None,
            leader_reason: Some("level-strong"),
        },
        ReadConsistency::Causal => RouteDecision {
            role: RouteRole::Follower,
            barrier: Barrier::WaitHighWater,
            leader_reason: None,
        },
        ReadConsistency::Session => {
            if sess.session_dirty_shard {
                RouteDecision {
                    role: RouteRole::Follower,
                    barrier: Barrier::WaitHighWater,
                    leader_reason: None,
                }
            } else {
                RouteDecision {
                    role: RouteRole::Follower,
                    barrier: Barrier::None,
                    leader_reason: None,
                }
            }
        }
        ReadConsistency::Eventual => RouteDecision {
            role: RouteRole::Follower,
            barrier: Barrier::None,
            leader_reason: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn follower(barrier: Barrier) -> RouteDecision {
        RouteDecision {
            role: RouteRole::Follower,
            barrier,
            leader_reason: None,
        }
    }

    #[test]
    fn strong_never_splits() {
        for rc in [ReadClass::Eligible] {
            let d = decide(ReadConsistency::Strong, rc, SessionView::default());
            assert_eq!(d.role, RouteRole::Leader);
        }
    }

    #[test]
    fn eventual_splits_without_barrier() {
        assert_eq!(
            decide(ReadConsistency::Eventual, ReadClass::Eligible, SessionView::default()),
            follower(Barrier::None)
        );
    }

    #[test]
    fn causal_always_barriers() {
        assert_eq!(
            decide(ReadConsistency::Causal, ReadClass::Eligible, SessionView::default()),
            follower(Barrier::WaitHighWater)
        );
        // 即使会话干净也 barrier(cross-client 语义需要)
        let sess = SessionView { session_dirty_shard: false, ..Default::default() };
        assert_eq!(
            decide(ReadConsistency::Causal, ReadClass::Eligible, sess),
            follower(Barrier::WaitHighWater)
        );
    }

    #[test]
    fn session_only_barriers_after_own_write() {
        let clean = SessionView::default();
        let dirty = SessionView { session_dirty_shard: true, ..Default::default() };
        assert_eq!(
            decide(ReadConsistency::Session, ReadClass::Eligible, clean),
            follower(Barrier::None)
        );
        assert_eq!(
            decide(ReadConsistency::Session, ReadClass::Eligible, dirty),
            follower(Barrier::WaitHighWater)
        );
    }

    #[test]
    fn txn_pin_and_hazard_force_leader_at_any_level() {
        for lvl in [
            ReadConsistency::Eventual,
            ReadConsistency::Session,
            ReadConsistency::Causal,
        ] {
            // 事务内
            let sess = SessionView { in_txn: true, ..Default::default() };
            let d = decide(lvl, ReadClass::Eligible, sess);
            assert_eq!(d.role, RouteRole::Leader, "{lvl}");
            // 会话 pin
            let sess = SessionView { pinned: true, ..Default::default() };
            let d = decide(lvl, ReadClass::Eligible, sess);
            assert_eq!(d.role, RouteRole::Leader, "{lvl}");
            // 不可分流语句
            let d = decide(lvl, ReadClass::LeaderRequired("for-update"), SessionView::default());
            assert_eq!(d.role, RouteRole::Leader, "{lvl}");
        }
    }
}

# Xenon raft 高可用适配

> newproxy 对 Xenon(Raft+半同步 MySQL 主从)集群的适配设计、配置与运维核对清单。

## 1. 解决什么

Xenon 用 Raft 管理主从,**leader 随选举漂移**,且半同步可能退化为异步(超过
`rpl_semi_sync_master_timeout` 自动降级;降级期间从库可滞后、leader 崩溃有丢尾部事务风险)。
代理无法修复降级窗口的丢事务,但可以:

1. **自动发现并跟随 raft leader**:每条写/读都落到真实主库,漂移自动切换,不依赖手工改配置。
2. **一致性档位可选**:读流量按业务需求选择语义(默认全走 leader,零风险)。
3. **半同步状态可观测**:降级出告警与指标,提示风险窗口。

## 2. 发现契约(mysql.xenon_raft_status)

按真实部署核对的结构(不同 fork 可能有出入,先跑 `ha_diag` 核对):

```sql
mysql.xenon_raft_status(
  id tinyint PK,
  leader varchar(80) COMMENT 'current leader endpoint, e.g. xenon1:8801',
  view_id bigint COMMENT 'leader view id',   -- 任期单调
  epoch_id bigint COMMENT 'leader epoch id', -- 任期单调
  updated_at timestamp(3)
)
```

- `leader` 是 **xenon raft 端点(host:8801)**,不是 MySQL 端口;代理按配置的
  `members`(MySQL 地址)映射 host → 真实 MySQL 地址;
- `(view_id, epoch_id)` 最大者代表最新任期,用于多节点回包排序与防抖;
- `updated_at` 表示自述新鲜度,超 `leader_stale_ms` 视为无效。

判主规则(纯函数,见 `src/ha/model.rs::decide`):
新鲜行 → leader 端点必须能映射到成员 → 权威性 = 同 leader 被 ≥2 探源一致上报
或 leader 自身单机自述 → 多候选取任期最高;冲突/无新鲜信息 → 保持旧状态(宁旧勿错)。

## 3. 配置

在分片配置上启用(示例,附在 `conf/newproxy.conf` 注释中):

```ini
[XenonRaft_<cluster>_<tablet>]
members=xenon1:3306,xenon2:3306,xenon3:3306
raft_endpoints=xenon1:8801,xenon2:8801,xenon3:8801   ; 可短于 members(对齐 leader 列)
probe_interval=1000        ; 发现周期 ms
probe_timeout_ms=500
leader_stale_ms=3000       ; 状态行新鲜度阈值
read_consistency=strong    ; strong|causal|session|eventual(分片默认档)
read_consistency_db=report=causal,user_list=session   ; 库级覆盖
read_consistency_user=finance=strong,report_api=eventual ; 产品用户级覆盖
barrier_wait_ms=200        ; GTID 屏障预算
gtid_sample_cache_ms=30
```

无 `XenonRaft` 段的分片行为与未适配前完全一致(默认 strong 全走 leader)。
同一分片不要同时让配置中心(etcd/zk)与 XenonRaft 写拓扑,二者以后者胜出,部署上互斥。

## 4. 一致性档位(只作用于"可分流纯读")

| 档 | 承诺 | 实现 | 每读成本 |
|---|---|---|---|
| strong(默认) | 线性化(全走 leader) | 不分流 | 0 |
| causal | 跨客户端因果(读 = 主库某提交前缀) | 全局高水位 GTID 屏障 | leader 采样(短 TTL 缓存摊薄)+ WAIT 1 RTT |
| session | 读己之写 + 本会话单调读(不约束他人) | 会话写过才采样/等待(会话水位按分片) | 写后才产生屏障;纯读会话 0 |
| eventual | 无承诺(任意滞后/乱序,业务自担) | 免屏障直读 follower | 0 |

**强制 leader(任何档位都不绕过)**:显式事务内、`FOR UPDATE/FOR SHARE/LOCK IN
SHARE MODE`、`SELECT ... INTO`、用户/系统变量 `@x/@@`、`LAST_INSERT_ID`/`FOUND_ROWS`
等会话函数、prepared 语句、`SET`/临时表等会话状态、多语句包(判定见
`parser::analyze::classify_read_only` 与 `ha::consistency::decide`)。

档位选择优先级:**产品用户 > 库 > 分片默认 > strong(内置)**。
`gtid-mode=ON` 是 causal/session 档的前提(未开启则自动退 strong 并日志一次)。

## 5. 运行时数据流

```
[每受管分片 worker] 并发探测成员 mysql.xenon_raft_status
   → judge 判主 → 变化时 topology.apply(ShardUpsert{master=leader,slaves=followers})
   → srv_pool.invalidate_shard(cluster,tablet)   (连接池按分片失效)
   → /api/ha 可见状态;leader 建连失败时 force_probe 立即重探并按新主重试一次(仅 ensure 阶段)

[读分流(档位非 strong)] plan_read_route(生效档 × 会话状态)
   → ensure_backend_ex(role=Slave):follower 候选链
   → 屏障:取 leader @@GLOBAL.gtid_executed → follower 同连接
     SELECT WAIT_FOR_EXECUTED_GTID_SET(set, wait_ms) → 成功才读,超时回 leader
```

## 6. 运维核对清单(部署/排障)

- **权限**:该分片 `db_user`(集群名下第一个 db_user)需 `mysql.xenon_raft_status`
  的 SELECT 权限,否则 worker 日志提示并停探(配置日志 `ha probe` 相关关键字排查)。
- **gtid**:需要因果/会话档时 `gtid-mode=ON`、`enforce-gtid-consistency=ON`。
- **members 一致性**:`members`/`raft_endpoints` 与实际节点核对;`leader` 列 host
  若不在成员表会判为不确定(不切换,日志可见)。
- **半同步监控**:代理采样 `SHOW STATUS LIKE 'Rpl_semi_sync_master_status'`,
  `ha_semisync_off` 计数/warn 日志提示丢尾部事务风险(代理无法修复,需数据库侧处理)。
- **首启核对**:`HA_DIAG_CONF=conf/xxx.conf cargo test --test ha_diag -- --ignored --nocapture`
  打印每个成员的状态行原始值与本代理判主结论,用于核对表结构/复制传播/时间语义。
- **运行状态**:`GET /api/ha`(状态/档位/成员);`POST /api/ha/reprobe` 手动触发重探;
  Prometheus:`newproxy_ha_*`。

## 7. 测试

- `src/ha/{model,consistency,result}.rs`、`src/parser/analyze.rs`、`src/config/loader.rs`
  单元测试;
- `tests/ha_discovery.rs`:成员 fake 两节点,mock `mysql.xenon_raft_status` 行,
  验证探测→判主→拓扑覆盖→池失效→leader 漂移自动跟随/幂等;
- `tests/ha_readsplit.rs`:causal/session/eventual 三种档 + 屏障超时回退的
  进程内端到端(写走 leader、可分流纯读走 follower、事务与 FOR UPDATE 强制 leader);
- `tests/ha_diag.rs`:真实集群核对工具(手动,`#[ignore]`)。

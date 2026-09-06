# 后端连接池优化

> 承接 [02-并发模型](./02-concurrency-model.md) §6。work-stealing 下 backend 池是所有连接 task 共享的——这是该模型相对 C 侧"连接亲和"的主要代价点。本文档给出池的数据结构、锁竞争分析与优化策略。

## 1. 池的语义与 C 侧对照

C 侧 `tr_srv_pool_t` 是 4 层嵌套 `GHashTable`:`cluster → tablet → username → 8 个属性桶`,每桶 `w_queue`/`r_queue`,每 `tr_conn_pool_t` 自带 `pthread_mutex_t`(`tr_conn_pool.h:7`)。关键 CAS 契约(已逐行核实):

| 操作 | C 实现 | 位置 |
|---|---|---|
| acquire | `tr_conn_pool_get` pop → `is_in_svr_pool` CAS `1→0`,失败则放回重 pop | `tr_srv_pool.c:30-32` |
| release | `served_times` 检查 → `is_in_svr_pool` set `1` → push 回 queue | `tr_srv_pool.c:320,377,385` |
| 复用上限 | `served_client_times >= conn_pool_socket_max_serve_client_times` 则不复用 | `tr_srv_pool.c:320` |
| close 时归还结构体 | `is_in_svr_pool==1` 检查避免 double-put | `tr_srv_pool.c:540` |

**复用上限的语义**:一个 backend 连接服务过多次客户端后强制关闭重建——防止后端会话状态漂移(临时表、变量、未提交事务残留)。Rust 版必须保留。

## 2. Rust 数据结构

```rust
use dashmap::DashMap;
use parking_lot::Mutex;
use std::sync::Arc;

struct SrvPool {
    /// cluster -> tablet -> user -> AttrBucket
    /// 三层 DashMap 提供读路径的分片并发,避免顶层单锁
    map: DashMap<ClusterId, DashMap<TabletId, DashMap<UserId, Arc<AttrBucket>>>>,
}

struct AttrBucket {
    /// 写库池(主)。原 w_queue
    write: Mutex<VecDeque<Arc<BackConn>>>,
    /// 读库池(从)。原 r_queue
    read:  Mutex<VecDeque<Arc<BackConn>>>,
    /// 桶级配置(复用上限、容量),从 Config 拷贝快照,reload 时整体换桶
    cfg: PoolCfg,
}

struct PoolCfg {
    max_serve_times: u64,     // conn_pool_socket_max_serve_client_times
    max_idle_secs: u64,
    max_size: usize,          // NETWORK_SOCKET_MAX_POOL_SIZE
}

struct BackConn {
    stream: TcpStream,
    state: ConnState,
    db: Arc<Database>,
    ct: Arc<ClusterTablet>,
    ms: MasterSlave,
    in_pool: AtomicBool,          // 替代 is_in_svr_pool
    served_times: AtomicU64,      // 替代 served_client_times
    last_active: AtomicI64,       // unix micros,空闲判定
}
```

**为什么 `parking_lot::Mutex` 而非 `std::sync::Mutex`**:backend 池是热路径,std 的 Mutex 在无竞争时也要走一次系统调用 fast path 检查;parking_lot 在无竞争时是纯用户态自旋+park,延迟更低,且**不会跨 `.await` 持有**(见 §5)。这是 DB 代理池最值得的一个依赖。

**为什么 `AttrBucket` 用 `Arc` 而非直接存值**:桶可能被多个正在执行的 task 同时持有(一个 task 取出连接后,桶配置可能被 reload 换掉)。`Arc<AttrBucket>` 让旧桶在所有引用释放后才回收,无需手写排空。

## 3. 取连接(acquire)

```rust
impl AttrBucket {
    fn queue_for(&self, ms: MasterSlave) -> &Mutex<VecDeque<Arc<BackConn>>> {
        match ms { MasterSlave::Master => &self.write, MasterSlave::Slave => &self.read }
    }

    /// 取一个可用后端连接。同步函数(持锁窗口内不做 IO),可安全在 async 上下文调用
    fn acquire(&self, ms: MasterSlave) -> Option<Arc<BackConn>> {
        let q = self.queue_for(ms);
        let mut q = q.lock();          // parking_lot,无 await
        while let Some(c) = q.pop_front() {
            // CAS 1->0:成功占有。失败说明被并发 acquire 抢走,继续 pop
            if c.in_pool.compare_exchange(true, false, AcqRel, Relaxed).is_ok() {
                // 空闲超时 / 复用上限检查
                if c.is_expired(&self.cfg) {
                    drop(q);                        // 先释放锁再关连接(关连接可能慢)
                    let _ = c.stream.into_raw();    // 由专门的清理 task 关 fd
                    continue;
                }
                return Some(c);
            }
        }
        None
    }
}

impl BackConn {
    fn is_expired(&self, cfg: &PoolCfg) -> bool {
        if self.served_times.load(Relaxed) >= cfg.max_serve_times { return true; }
        let now = now_micros();
        let last = self.last_active.load(Relaxed);
        now - last > (cfg.max_idle_secs as i64) * 1_000_000
    }
}
```

**关键:acquire 是同步函数,持锁窗口内不做任何 IO。** 这保证锁持有时间极短(仅 pop + 几个原子读),不会被网络 RTT 拖长。

## 4. 还连接(release)——异步归还的困境

backend 归还在 Rust 里是 async 的(可能要 reset 会话状态),但 **`Drop` 不能 `.await`**。这是 work-stealing 下必须显式处理的点:

```rust
impl FrontConn {
    async fn drive(&mut self) -> Result<DriveOutcome> {
        // ... 业务逻辑 ...
        Ok(DriveOutcome::Continue)
    }
}

/// 连接 task 退出前的显式清理(替代 C 侧 client_free 里的 tr_srv_pool_add)
async fn finalize_conn(mut conn: FrontConn) -> Result<()> {
    // 1. 归还持有的 backend 连接(异步:可能发 RESET / SET 命令重置会话)
    if let Some(backend) = conn.backend.take() {
        release_backend(backend).await;   // async,可在这里 await
    }
    // 2. Drop conn:关 front stream、释放缓冲 —— 这些是同步的,放 Drop 没问题
    Ok(())
}

async fn conn_task(stream: TcpStream, peer: SocketAddr, ctx: Arc<AppCtx>) {
    let mut conn = FrontConn::new(stream, peer, ctx.clone());
    loop {
        match conn.drive().await {
            Ok(DriveOutcome::Continue) => {}
            Ok(DriveOutcome::ShutDown) | Err(_) => break,
        }
    }
    let _ = finalize_conn(conn).await;   // 显式异步归还
}

async fn release_backend(c: Arc<BackConn>) {
    // CAS 0->1:若失败说明连接已被 force-close 路径标记,直接销毁
    if c.in_pool.compare_exchange(false, true, AcqRel, Relaxed).is_err() {
        return;   // 已在销毁流程,不重复归还
    }
    // 复用上限检查
    let bucket = c.ct.bucket_for(&c.ms);
    if c.served_times.fetch_add(1, Relaxed) + 1 >= bucket.cfg.max_serve_times {
        c.in_pool.store(false, Release);   // 不入池,等 Drop 关 fd
        return;
    }
    c.last_active.store(now_micros(), Release);
    bucket.queue_for(c.ms).lock().push_back(c);   // 同步 push,无 await
}
```

**对照 C 侧**:`tr_srv_pool.c:540` 的 `is_in_svr_pool==1` 检查(double-put 防护)在 Rust 里由 `compare_exchange(false, true)` 的原子语义天然保证——CAS 失败即等价于"已在池/已销毁",不可能 double-put。

## 5. 锁竞争分析与优化策略

### 5.1 竞争点定位
work-stealing 下,所有 task 抢同一 `AttrBucket` 的 `Mutex`。竞争强度取决于**同一 (cluster, tablet, user) 三元组的并发查询数**。典型场景:
- 单分片热点表 + 单 dbuser → 同一桶高并发 → `write`/`read` 队列锁是热点。
- 多分片 + 多 user → 天然分散到不同桶 → 竞争低。

### 5.2 优化阶梯(按需启用,压测驱动)

| 阶梯 | 措施 | 适用 | 代价 |
|---|---|---|---|
| 0(基线) | 每桶 `parking_lot::Mutex<VecDeque>` | 默认 | 无 |
| 1 | **读写分桶已有**(write/read 独立锁) | 读多写少场景 | 已实现 |
| 2 | **桶内分片**:`Vec<Mutex<VecDeque>>`,hash 连接 id 取分片 | 单桶极高并发 | 归还时要选分片,统计需聚合 |
| 3 | **lock-free 栈**:`crossbeam::TreiberStack` 无锁 push/pop | acquire/release 是 LIFO 语义即可 | 栈语义(最近归还优先复用,cache 更热)反而更好 |
| 4 | **连接预热**:启动时按桶预建 min_idle 条连接 | 避免冷启动毛刺 | 见 §6 |

**阶梯 3 说明**:backend 池的 acquire/release 本质是 LIFO(后归还的连接 session 状态最新、cache 最热,优先复用最合理)。Treiber 栈正好是 LIFO 且无锁,比 `Mutex<VecDeque>` 的 FIFO 更贴合语义。但 Treiber 栈的 ABA 问题需要 epoch 回收——`crossbeam` 已处理。**建议阶梯 0 跑通后,压测若桶锁成热点,直接上阶梯 3。**

### 5.3 分片决策树
```
压测 → p99 是否因池等待升高?
├─ 否 → 保持阶梯 0
└─ 是 → tokio-console 定位是否桶锁
        ├─ 桶锁不热点 → 问题在别处(IO/协议)
        └─ 桶锁热点 → 单桶并发 QPS?
                 ├─ < 5k  → 阶梯 0 + parking_lot(已足够)
                 ├─ 5k~20k → 阶梯 2 桶内分片
                 └─ > 20k  → 阶梯 3 Treiber 栈
```

## 6. 连接预热与空闲回收

### 6.1 预热
C 侧无预热(连接按需建)。Rust 版可选预热以消除冷启动毛刺:

```rust
/// 启动时为每个桶预建 min_idle 条连接
async fn warmup_pool(pool: &SrvPool, cfg: &Config) {
    for bucket in pool.all_buckets() {
        for _ in 0..bucket.cfg.min_idle {
            if let Ok(c) = connect_backend(&bucket.db, bucket.ms).await {
                c.in_pool.store(true, Release);
                bucket.queue_for(bucket.ms).lock().push_back(c);
            }
        }
    }
}
```

### 6.2 空闲回收
独立 task 周期扫描各桶,关闭超过 `max_idle_secs` 的空闲连接(对应 C 侧 idle-timeout event,`tr_srv_pool.c` 的 `EV_READ` 超时):

```rust
async fn idle_reaper(pool: Arc<SrvPool>, tick: Duration) {
    let mut interval = tokio::time::interval(tick);
    loop {
        interval.tick().await;
        for bucket in pool.all_buckets() {
            let mut to_close = Vec::new();
            {
                let mut q = bucket.write.lock();
                q.retain(|c| {
                    if c.is_expired(&bucket.cfg) && c.in_pool.compare_exchange(true, false, AcqRel, Relaxed).is_ok() {
                        to_close.push(c.clone()); false
                    } else { true }
                });
            }
            for c in to_close { let _ = close_backend(c).await; }
        }
    }
}
```

`retain` + CAS 保证:正在被 acquire 抢走的连接(`in_pool` 已被翻成 false)不会被回收。

## 7. 新建连接(load_balance + failover)

池空时需新建。对应 C 侧 `tr_back_lb.c` 的 power-of-two-choices + 重试 bitmap:

```rust
/// 池空 → 选 DB → 建连 → 失败 failover
async fn acquire_or_connect(pool: &SrvPool, ct: &Arc<ClusterTablet>, ms: MasterSlave) -> Result<Arc<BackConn>> {
    let bucket = ct.bucket_for(ms);
    if let Some(c) = bucket.acquire(ms) { return Ok(c); }

    // 池空:负载均衡选一个可用 DB
    let db = load_balance(&bucket.dbs, ms).ok_or(Err::NoBackend)?;
    let conn = connect_with_failover(&bucket, ms).await?;   // 含重试 bitmap
    Ok(Arc::new(conn))
}

/// power-of-two-choices:随机抽两个,选当前连接数少的
fn load_balance(dbs: &[Arc<Database>], ms: MasterSlave) -> Option<Arc<Database>> {
    let avail: Vec<_> = dbs.iter().filter(|d| d.available(ms)).collect();
    if avail.is_empty() { return None; }
    // 抽两个取较闲的(原 tr_back_lb.c:60-95 语义)
    let a = avail.choose(&mut thread_rng())?;
    let b = avail.choose(&mut thread_rng())?;
    Some(if a.active_conns() <= b.active_conns() { a.clone() } else { b.clone() }.clone())
}
```

**注意**:`choose` 需要 RNG。tokio 上下文用 `rand` crate(非 `Math.random`,避免阻塞)。

## 8. 复用上限的会话重置

backend 达到 `max_serve_times` 后**不复用直接关闭重建**。但更精细的做法是:复用前发 `COM_RESET_CONNECTION` 重置会话状态(清临时表/变量/未提交事务),这样可放宽复用上限。C 侧走的是"强制关闭"路径(简单但浪费连接);Rust 版可选 reset 路径以提升连接复用率——需压测权衡 reset 的 RTT 成本 vs 重建成本。

## 9. 风险与验证

| 风险 | 缓解 |
|---|---|
| 桶锁热点 | 压测 + tokio-console 定位,按 §5.3 决策树升级 |
| double-put | `compare_exchange` 原子语义保证,不可能 |
| 连接泄漏(task panic 未归还) | `finalize_conn` 用 RAII guard 包裹:`struct BackendGuard(Arc<BackConn>); impl Drop { ...spawn async release... }` —— panic 也能触发归还 |
| 空闲回收误杀在用连接 | `retain` 内 CAS `true→false` 抢占有权,抢不到说明在用,跳过 |
| 新建连接风暴(池空时并发 task 同时建连) | 桶级 `Semaphore` 限制并发建连数,多余 task 等池释放 |

### 连接泄漏的 RAII 兜底
```rust
struct BackendGuard {
    conn: Option<Arc<BackConn>>,
    pool: Arc<SrvPool>,
}
impl Drop for BackendGuard {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            // 同步 Drop 不能 await,把归还投递到后台 task
            let pool = self.pool.clone();
            tokio::spawn(async move { release_backend(c, &pool).await; });
        }
    }
}
```
这样即便 task 中途 panic/返回未显式归还,guard 的 Drop 也会把归还投递出去——比 C 侧靠人工调用 `client_free` 健壮得多。

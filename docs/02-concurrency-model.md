# 并发模型设计(tokio work-stealing)

> 本文档定义 Rust 版 newproxy 的并发与线程/协程处理模型。
> 设计原则:**按 Rust 原生模型重构,不参照 C 侧的线程处理架构**。C 侧架构仅作为功能需求来源与对照。

## 1. 设计基线:抛弃"连接亲和"

C 侧靠"连接钉死 worker 线程"来免锁访问每线程资源(parser、decomposer、连接回收池)。**work-stealing 模型彻底放弃这条不变量**——一个连接的 task 可以在任意 worker 上跑,甚至跨 `.await` 点迁移到另一个线程。

这意味着:**所有资源默认按"可被任意线程访问"设计**,只在能用单线程占有的地方(统计累加、TLS 日志)退回单线程优化。这是 Rust async 的默认心智模型。

## 2. 运行时拓扑

```
┌─────────────────────────────────────────────────────────────┐
│  Tokio multi-thread runtime  (worker_threads = N, 默认=CPU核数) │
│  work-stealing 调度器                                        │
│                                                              │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  Accept loop  (1 个 task, 持 TcpListener)            │   │
│  │   loop { TcpListener::accept() -> spawn(conn_task) } │   │
│  └──────────────────────────────────────────────────────┘   │
│           │  每个连接 spawn 一个独立 task (可被任意 worker steal)│
│           ▼                                                  │
│  conn_task_1   conn_task_2   conn_task_3  ...  (成千上万)    │
│   ├─ front 协议状态机                                         │
│   ├─ 取/还 backend 连接 (从共享 SrvPool)                      │
│   ├─ scatter/gather 子请求 (fan-out 多个 backend task)        │
│   └─ 合并结果返回 client                                      │
│                                                              │
│  旁路 task (独立 spawn,与连接解耦):                            │
│   - metric reporter (interval tick)                          │
│   - user-access stats reporter                               │
│   - async-ip 白名单刷新                                       │
│   - reload 协调器 (单例,触发时才活跃) [已实现]               │
│                                                              │
│  共享状态 (Arc 包裹,跨 task 访问):                             │
│   - config: ArcSwap<Config>                                  │
│   - SrvPool: 共享连接池 (DashMap)                             │
│   - 统计                                                      │
└─────────────────────────────────────────────────────────────┘
```

与 C 侧的根本差异:**没有"主线程 + N worker"的固定角色分工**。accept 只是一个普通 task,连接处理是海量独立 task,调度器自己决定谁在哪跑。

## 3. 连接生命周期:一个连接 = 一个 task

这是 Rust async 代理的标准范式,也是与 C 侧"状态机 + 函数指针回调"最大的形状差异。

```rust
/// 一个前端连接 = 一个 task,生命周期 = 连接生命周期
async fn conn_task(stream: TcpStream, peer: SocketAddr, ctx: Arc<AppCtx>) {
    // 连接级状态全部在 task 栈帧 / 局部变量里 —— 天然单线程占有,无需 Arc
    let mut conn = FrontConn::new(stream, peer, ctx.clone());

    // 驱动协议状态机(替代 core_driver_machine + conn_handler_pt)
    loop {
        match conn.drive().await {
            DriveOutcome::Continue => {}
            DriveOutcome::ShutDown => break,
        }
    }
    // Drop 自动:关 fd、释放缓冲 —— RAII,无需手写 client_free
    // 注意:backend 连接归还是 async,需在 break 前显式调用,见 §6
}

#[tokio::main]
async fn main() -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, peer) = listener.accept().await?;
        tokio::spawn(conn_task(stream, peer, ctx.clone()));
    }
}
```

**关键点:连接状态是 task 局部变量,不是共享结构。** C 侧 `tr_conn_t` 那个 200 字段 god-struct 被拆成 task 内的局部 `FrontConn`——task 本身就是"连接"的抽象。

```rust
struct FrontConn {
    stream: TcpStream,          // tokio 异步 socket,替代裸 fd + struct event
    send_buf: BytesMut,         // 替代 tr_byte_array_t send_buf
    recv_buf: BytesMut,
    state: ConnState,           // 协议状态机
    user: Arc<ProductUser>,
    prepared: PreparedRegistry, // 局部,无需 Arc
    middle_cmds: Vec<MiddleCmd>,
    ctx: Arc<AppCtx>,           // 共享上下文的句柄
}
```

## 4. 前后端关系:从"互指裸指针"到"前端持有 backend `Arc`"

C 侧前后端是双向裸指针(`union tr_peer_u`),手工置空。work-stealing 下:

- **后端连接**是池里的共享资源,类型 `Arc<BackConn>`。
- **前端 task** 取出一个 `Arc<BackConn>`,用完归还。前端持有 backend 强引用,backend **不持有**前端(避免循环)。
- 前端连接关闭时 `Drop` 自动释放持有的 `Arc<BackConn>`。

```rust
struct BackConn {
    stream: TcpStream,
    state: ConnState,
    db: Arc<Database>,
    ct: Arc<ClusterTablet>,
    ms: MasterSlave,
    in_pool: AtomicBool,        // 替代 is_in_svr_pool CAS
    last_active: Mutex<Instant>,
    // 不持有 front —— 打破循环
}
```

后端需回写前端结果的场景:work-stealing 下不能存裸指针。解法——**结果通过 channel 回传给前端 task**:

```rust
let (tx, rx) = tokio::sync::oneshot::channel();
tokio::spawn(backend_query_task(backend.clone(), query, tx));
let result = rx.await?;   // 前端 task 在此 await,调度器可让出
```

## 5. Scatter/Gather:fan-out 子请求

work-stealing 下,每个子请求可并发打到不同 backend——性能提升机会:

```rust
impl FrontConn {
    /// scatter/gather:一个分片查询可能 fan-out 到多个 backend,并发执行后合并
    async fn exec_distributed(&mut self, sub_reqs: Vec<SubRequest>) -> Result<MergedResult> {
        let futs = sub_reqs.into_iter().map(|r| {
            let pool = self.ctx.srv_pool.clone();
            async move {
                let backend = acquire_backend(&pool, &r.ct, r.ms).await?;
                run_backend_query(backend, r.sql).await
            }
        });
        let results = futures::future::try_join_all(futs).await?;
        Ok(merge_query_results(results)?)   // 对应 tr_result.c 的合并逻辑
    }
}
```

`merge_query_results` 是同步纯函数(原 `tr_result.c` 的 min-heap merge / GROUP BY / ORDER BY / 聚合),搬到 Rust 不涉及并发。

## 6. Backend 连接池

backend 池是**所有连接 task 共享**的(对应 C 侧 `srv_pool_` 本就是 instance 级共享)。4 层 `GHashTable` → 嵌套并发 map。**详细设计与锁竞争优化见 [03-后端连接池优化](./03-backend-pool-optimization.md)。**

## 7. 每线程资源的处理(parser / decomposer)

C 侧 parser "非线程安全",靠每线程一份 + TLS。work-stealing 下 task 会迁移,**不能用 `thread_local!` 持有可变状态**(跨 `.await` 持借用会 panic)。解法是 parser 对象池 + `Mutex`。**完整实现见 [05-SQL 解析器对象池](./05-parser-pool.md)。**

## 8. 统计:lock-free 累加

C 侧 `sql_statistic_list`/`ip_statistic_list` 用手写 CAS 自旋锁(`tr_util.h:187-196`)。Rust 直接用原子累加,统计周期性 flush:

```rust
struct SqlStats {
    count: AtomicU64,
    total_time_us: AtomicU64,
}
stats.count.fetch_add(1, Ordering::Relaxed);   // 热路径无锁
// reporter task 定时 snapshot + reset
```

复杂统计(按 SQL 模板聚合)用 `DashMap<SqlTemplate, SqlStats>`。

## 9. 热加载(reload)——已实现

> ✅ **已实现**(`src/app.rs::AppCtx::reload_config`)。三种触发方式:
> `checkproxy reload` 命令、`SIGHUP` 信号、配置文件变更自动热加载(默认 5s 轮询,`reload_interval=0` 关闭)。

C 侧 reload 在屏障锁下交换 `current_instance`/`config_`,靠状态机 CAS + 版本号扫描 + worker 排空回收,且承认有内存泄漏。Rust 版用 `ArcSwap` 替代:

```rust
struct AppCtx {
    config: Arc<ArcSwap<Config>>,    // 原子发布,无锁读
    srv_pool: Arc<SrvPool>,
    config_path: String,             // reload 时重解析文件
}

pub fn reload_config(&self) -> Result<String, String> {
    let new_cfg = config::load_config(&self.config_path)?;  // 解析失败 → 保持旧配置,返回 Err
    let diff = topology_diff(&self.config.load(), &new_cfg); // 拓扑变更摘要(回显/日志)
    self.config.store(Arc::new(new_cfg));                    // ArcSwap 原子替换,读侧立即生效
    self.srv_pool.reset();                                   // 清空连接池:旧拓扑的空闲连接作废
    Ok(diff)
}
```

**生效语义**:
- 读侧无需改动:连接任务每操作都 `load_config()`,替换后新连接/新建后端连接立即用新拓扑。
- `srv_pool.reset()` 丢弃全部空闲连接(指向旧主库的连接不再被复用);已有会话保持其当前后端连接直至结束,不打断在途事务。
- 解析失败(如配置写了一半)时保持旧配置运行,仅记 ERROR 日志,不中断服务。

**首版处理配置变更的方式**曾为进程重启;现以热加载为主,重启仅用于日志参数(`log_level`/`log_dir` 等)等一次性初始化项的变更。

## 10. 与原 C 线程架构的对照(说明形状变化,非约束)

| C 侧概念 | Rust work-stealing 对应 | 形状变化 |
|---|---|---|
| 主线程 accept + N worker 固定角色 | accept task + 海量 conn task,无固定角色 | 角色分工消失 |
| 连接钉死 worker,每线程免锁资源 | 连接 task 可迁移,资源 Arc+锁或池化 | 亲和不变量放弃 |
| `tr_conn_t` god-struct 全局可见 | task 局部 `FrontConn` | 结构体消失,状态进栈帧 |
| 前后端 `union tr_peer_u` 裸指针互指 | 前端持 `Arc<BackConn>`,后端不持前端 | 双向指针→单向 + channel 回传 |
| `conn_handler_pt` 函数指针 + `abort()` | `enum ConnState` + `match` + `Result` | 函数指针→枚举分发,崩溃→受控错误 |
| SPSC 环形缓冲 + mfence + self-pipe | `tokio::mpsc` / `tokio::sync` | 手写屏障消失 |
| 状态机 CAS + 屏障锁 + 版本号排空 reload | `ArcSwap` + broadcast + `select!` [推迟] | 手写 RCU 消失 |
| TLS parser + 非线程安全 | parser 对象池 + `Mutex` | 隐式 TLS→显式池化 |
| 手写 CAS 自旋锁统计 | `AtomicU64` + `DashMap` | 自旋锁消失 |
| `goto` 错误清理(560 处) | `?` + `Result` + `Drop` | goto 消失 |

## 11. 风险与验证

| 风险 | 量级 | 缓解 |
|---|---|---|
| backend 池锁竞争 | 中 | 桶粒度分锁;压测后必要时换 `parking_lot` 或分片,详见 [03](./03-backend-pool-optimization.md) |
| task 迁移破坏 cache 局部性 | 中 | tokio 默认有 affinity 倾向;热路径用 `BytesMut`/零拷贝 |
| parser 池锁 | 低 | 池化复用,持锁窗口小,详见 [05](./05-parser-pool.md) |
| 性能对齐 | 高 | 662+59 黑盒用例做行为等价验收;压测对比 QPS/p99;`tokio-console` 分析热点 |
| `Drop` 不能 `.await` | 中 | backend 归还是 async,需显式 `async fn close()` 在 task 退出前调用,详见 [03](./03-backend-pool-optimization.md) §5 |

## 12. 最小可运行骨架

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let ctx = Arc::new(AppCtx::load("newproxy.conf").await?);
    let listener = TcpListener::bind(ctx.listen_addr).await?;

    spawn_metric_reporter(ctx.clone());
    // spawn_reload_coordinator(ctx.clone());   // [推迟]

    loop {
        let (stream, peer) = listener.accept().await?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = conn_task(stream, peer, ctx).await {
                tracing::warn!(%peer, error=%e, "conn task ended");
            }
        });
    }
}
```

`AppCtx::load` 解析配置(原 `tr_config.c`);`conn_task` 内先实现 MySQL 握手 + 简单透传,再逐步加分片/改写/合并。**每加一层功能,都用 662 个 MySQL 兼容用例回归。**

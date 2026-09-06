# NewProxy → Rust 重写成本评估

> 本文档为立项与排期提供量化依据。基于对 `src/`(C99,~36.4K LOC)、`modules/sqlparser`(~29.6K LOC)、`modules/d3-modules`(~1.6K LOC)的逐结构分析。

## 1. 项目定性

NewProxy 是一套 **MySQL 分库分表代理(newproxy)**,基于 libevent 事件驱动,核心用 **C99** 编写(非 C++)。

| 模块 | 语言 | 规模 | 性质 |
|---|---|---|---|
| `src/` 核心 | C99 | ~36.4K LOC(42 .c + 43 .h) | 代理主程序 |
| `modules/sqlparser` | C + bison/flex | ~29.6K LOC(含 13.5K 行语法) | 自研 SQL 解析器子模块 |
| `modules/d3-modules` | C | ~1.6K LOC | 共享基建(cclog/limiter/atomic/shm) |
| **合计** | | **~67K LOC** | |

**结论**:这是一次"重构架构"而非"逐行翻译"。成本由三个因素主导——SQL 解析器、并发模型、协议层——其中解析器是最大的成本乘子。但项目有一张强力的安全网(700+ 测试用例),且 CHANGELOG 的 bug 史恰好是 Rust 能消除的那一类,因此重写在技术上正当且可论证。

## 2. 架构复杂度(决定成本的根因)

### 2.1 线程模型
主线程持 listen socket,`accept()` 后通过 SPSC 环形缓冲 + self-pipe 唤醒,轮询分发给 N 个 worker。每个 worker 拥有自己的 `event_base`、连接池、SQL 解析器,**连接绑定到单个 worker 线程**——这是整个设计的承重不变量。另有 3 个辅助线程(metric / user-access / async-ip)。

### 2.2 环境式 TLS 上下文
```c
__thread tr_cycle_t  *tls_cycle_key;   // tr_core.c:3
__thread tr_logger_t *tls_logger_key;
```
全代码库通过 `tr_current_cycle()`/`tr_current_instance()`/`tr_current_logger()` 等 inline 访问器(`tr_cycle.h:64-98`)**隐式**获取"当前线程的一切"。上下文散布在数千个调用点。

### 2.3 `tr_conn_t` 巨型结构 + 前后端双向指针图
`tr_conn_t`(~200 字段,`tr_conn.h:124-341`)是 god-struct:连接状态机、`struct event*`、函数指针 `conn_handler_pt`、收发缓冲、MySQL 协议会话、预编译语句数组(大量 `GArray`/`GPtrArray`)。前后端连接通过 `union tr_peer_u`(`tr_conn.h:75-78`)**互持裸指针**,在 `client_free`/`server_free`(`tr_srv_pool.c:464/525`)手工置空。

### 2.4 手写 RCU 式热加载
`tr_reload.c` 实现了 4 种 reload 策略,核心是**在屏障锁下原地交换进程级 `current_instance`/`pre_instance` 指针和 `instance->config_`**,靠状态标志位 + 版本号扫描 + worker 排空到 0 连接来做回收(`tr_reload.c:1216-1301`),**代码注释里承认有内存泄漏**(`tr_reload.c:1295`)。

### 2.5 共享可变状态与手工同步
`src/` 内有 **46** 处 `pthread_mutex_lock`、7 处 `g_rec_mutex_lock`、2 处 `g_mutex_lock`(均为 reload 屏障)、5 处 `cond_wait`、**140** 处原子操作、3 处 `__thread`,外加自旋 CAS 锁(`tr_util.h:187-196`)和带内存屏障的 SPSC 环形缓冲(`tr_ring_buffer.c:115-142`,且 `mb/rmb/wmb` 三个宏全是 `mfence`,过度保守)。

### 2.6 SQL 协议层(~7K LOC)
MySQL 专用、中等完整。仅 `mysql_native_password` 认证,**无 SSL/TLS、无 caching_sha2_password、无 auth_switch**;完整 COM_* 命令集含预编译语句;走 pre-8.0 EOF 风格协议。`libevent` 只用裸 event,未用 bufferevent/evbuffer(全文 0 处)——降低协议层移植难度。

### 2.7 SQL 解析器(成本乘子)
自研 bison/flex 解析器,MySQL 方言 + 分片 DDL 扩展。C AST 用"虚基类"宏模式,每个节点内嵌 `to_string`/`to_stencil`/`clone` 函数指针;arena/pool 分配。newproxy 依赖**完整的 parse→mutate→unparse 往返**。`src/` 对解析器有 **433 处 API 调用**,直接遍历 ~15 种 AST 节点类型。

## 3. Rust 重写核心难点(按成本排序)

| # | 难点 | 原因 | Rust 映射 | 难度 |
|---|---|---|---|---|
| 1 | SQL 解析器去留 | 30K LOC 语法+AST,深度依赖 unparse/clone | FFI 保留 / 或换 sqlparser-rs | 极高 |
| 2 | 并发架构重设计 | 环境式 TLS、god-struct 别名图、手写 RCU reload | 显式上下文 + ArcSwap + tokio | 极高 |
| 3 | 分片/改写/合并核心 | ~10K LOC 直接操作解析器 AST 的 vtable | 取决于解析器路径 | 高 |
| 4 | libevent 回调所有权 | 单回调 + `void*` arg,手动 re-arm | mio/tokio,放弃手动 re-arm | 高 |
| 5 | 协议层 | MySQL 协议完整但无 TLS | 手写或用 mysql_common crate | 中 |
| 6 | `goto` 错误清理 | 560 处 goto | `?`/`Result`/`Drop`,机械但量大 | 中 |
| 7 | GLib 容器 | ~3596 处引用,含 4 层嵌套哈希后端池 | HashMap/DashMap/Vec | 中 |

## 4. 依赖 → Rust 生态映射

| C 依赖 | 用量 | Rust 替代 | 契合度 |
|---|---|---|---|
| libevent(裸 event) | 41 | tokio / mio | 好 |
| glib(GHashTable/GMutex/GList…) | **3596** | HashMap / DashMap / ArcSwap / Vec | 好 |
| jansson | 78 | serde_json | 好 |
| PCRE | 23 | regex / pcre2 crate | 好 |
| d3-modules(cclog/limiter/shm) | 内嵌 | tracing + 自研限流 + shared_memory | 中 |
| curl(tr_http 出站) | 少 | reqwest | 好 |
| jemalloc | 仅链接 | 默认 allocator / jemallocator | 好 |
| **sqlparser** | **433 调用** | FFI 保留 或 sqlparser-rs | **关键岔路** |

## 5. 测试资产(去风险因素)

最有利的条件:测试是**黑盒的**,可原样复用作为 Rust 版本的预言机。
- **662 个 MySQL 兼容性用例** + **59 个 newproxy 专用用例**,通过 `mysql_case/totalTest.sh` 与 JDBC 驱动(`mysql-connector-j-8.4.0`)驱动。
- `build.sh` 已集成 valgrind;`test/asan/` 针对 `tr_stmt_prepare.c` 的 `MYSQL_TYPE_NULL` 1 字节堆溢出有专门 ASAN 用例。
- 行为等价性测试是金标准——与内部并发模型无关。

## 6. 成本估算

按"自研解析器保留(FFI)+ 其余 Rust 重写"的务实路径估算(1 名熟练 Rust 工程师):

| 阶段 | 范围 | 估算(人月) |
|---|---|---|
| 0. 协议层 clean-room | tr_packet/result/auth/front_cmd/back_cmd(~7K LOC) | 2–3 |
| 1. 解析器 FFI 封装 | bindgen AST + pool + vtable 桥接 | 1–2 |
| 2. 并发核心重设计 | TLS 上下文、conn 模型、reload、线程模型 | 3–4 |
| 3. 分片/改写/合并核心 | decomposer/partition/result/route(~10K LOC) | 3–4 |
| 4. 配置/metric/管理/日志 | tr_config/metric/mng/log + d3 替代 | 2 |
| 5. 集成/测试迁移/性能对齐/加固 | 对照 700+ 用例 + 压测 | 3–4 |
| **合计** | | **~14–19 人月** |

若选"换 sqlparser-rs"路径,阶段 1+3 膨胀约 +4–6 人月(需重实现 MySQL 方言覆盖、DDL 扩展、unparse/clone 语义),合计 **~18–25 人月**。小团队(3–4 人)并行约 **5–7 个月**,即 1–2 个季度量级。

**主要风险点**:① 解析器 FFI 的 unsafe 指针遍历面大;② 并发重设计后性能对齐(DB 代理吞吐/延迟敏感,Rust async 相对精调 libevent 有不确定性);③ 手写 RCU reload 的等价语义验证(已决定推迟实现,降低首版风险)。

## 7. 收益(为什么值得)

CHANGELOG 的 bug 史恰好是 Rust 能消除的那一类:
- 内存安全:`reload master_logger duplicate free`、`srv_pool conns free twice`、多处 `memleak`、`heap memory overflow`、`thread leak after reload`。
- 线程安全:`fix queue thread unsafe`、`log_destory thread unsafe`、`multi-thread GHashTable unsafe`、`fix reload deadlock`、`read notify-pipe block`。

这些是裸指针 + 手工锁 + 手写回收的典型病灶,Rust 的所有权与 `Send/Sync` 能在编译期根除。

## 8. 建议路径

**推荐分阶段、解析器走 FFI 的渐进式重写**:
1. **决策点(先定)**:解析器保留(FFI)还是替换(sqlparser-rs)。建议保留。
2. **阶段 1**:先重写协议层(最自包含、测试覆盖最厚),用现有 662 用例当预言机跑通端到端代理。
3. **阶段 2**:重写并发核心(真正的安全收益所在),`ArcSwap`+`tokio`。
4. **阶段 3**:重写分片/合并核心,挂回 FFI 解析器。
5. 每阶段以现有黑盒测试集做行为等价验收。

**预算/风险受限的替代方案**:不做整体重写,而是**增量加固**——CI 接入 ASAN/TSAN、把最易出 bug 的模块(reload 路径、srv_pool、log)用 Rust 局部重写并通过 FFI 替换,其余保持 C。这条路 1–2 个月可见效,但拿不到"全 Rust"的长期可维护性红利。

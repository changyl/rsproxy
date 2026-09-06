# 14 对外性能监控指标梳理(Metrics Exposure Catalog)

> 状态:**梳理稿(2026-09-02)→ 已按 §6 实施(同日)**。P0 死指标修复、P1 Prometheus 补齐、
> P2 直方图/错误码拆分/uptime/分片慢查询均已落地,见 [§8 实施记录](#8-实施记录2026-09-02)。
> 核对基准:2026-09-02 工作区代码(含未提交改动),行号改动后需同步。

本文承接 [13-可观测性设计](./13-observability-design.md)(外部通道/端点设计,已实现),
聚焦**指标级清单**:内部采集了什么 → 哪些已对外暴露、经哪个通道 → 哪些失真/缺失 →
暴露边界与补齐建议。

---

## 1. 阅读地图(相关代码位置)

| 内容 | 文件 |
|---|---|
| 指标采集层(计数器/DashMap/环形缓冲/摘要) | `src/metric.rs` |
| 对外通道:Prometheus 渲染、JSON API、探针、面板 | `src/mgmt/http.rs` |
| 进程负载采样(CPU/RSS/FD,Linux `/proc`) | `src/mgmt/proc.rs` |
| `checkproxy` 管理命令(业务端口文本) | `src/mgmt.rs` |
| 打点现场:连接生命周期、查询 5 阶段、错误记录 | `src/conn/front.rs` |
| codec 全局包/字节收发计数(前端+后端全部 socket) | `src/proto/codec.rs` |
| 连接注册表(cid/addr/user/db/state) | `src/app.rs` |
| 后端连接池桶(write/read 队列深度) | `src/pool/backend.rs` |

---

## 2. 内部采集了什么(数据源全景)

| 数据源 | 内容 | 容量/上限 |
|---|---|---|
| `Metrics` 原子计数器 | `connections_total / active / rejected`、`queries_total / errors / slow`、`pool_acquires / pool_acquire_fails`、5 阶段耗时累计 `stage_{interval,parse,setup,send,forward}_us` | 无界(单调累加,64 位) |
| `sql_stats` DashMap | SQL 模板 × 分片 → count / total_time_us / max_time_us / rows_sent | **有界 1024 条**,超限不再登记新模板 |
| `shard_query_counts` / `shard_stage_us` | 分片查询数;分片 5 阶段耗时累计(平均 = 累计/查询数) | 随拓扑分片数 |
| `db_traffic` / `node_traffic` | 按库 / 按后端节点 → [收字节, 发字节, 收包, 发包](仅**客户端方向**记账:收=命令包,发=本地应答+转发响应) | 随库数/节点数 |
| 慢查询环形缓冲 | `SlowQuery`(ts/sql/db/puser/shard/elapsed + 5 阶段) | 有界 200 条 |
| 最近查询环形缓冲 | `QueryRecord`(同上 + slow 标记;**全部**查询单次阶段) | 有界 500 条 |
| 解析失败环形缓冲 + 计数 | `ParseFailure`(ts/reason/sql 截断 512) | 有界 200 条 |
| 后端错误(明细 + 按码聚合) | `BackendError`/`BackendErrStat`(ts/code/sql) | 有界 200 条 |
| codec 全局静态 | 进程内**全部 MySQL socket(前端+后端)**包/字节收发 | 无界 |
| `ProcMeter` | 进程 CPU% / RSS / FD(仅 Linux) | — |
| 连接注册表 `ConnHandle` | cid/addr/started_at/user/db/state/backends/kill 信号 | 随活跃连接 |
| 池桶 `AttrBucket` | 每 (cluster,tablet,user,db) 桶:write/read `VecDeque` 深度 + 配置(max_size/min_idle/max_serve_times/max_idle_secs/stale_on_acquire_secs) | 随桶数 |

> 口径提醒:`newproxy_traffic_*`(codec 全局)= 前后端全部协议字节;
> `db_traffic/node_traffic` = 仅客户端方向按库/按节点记账,两者不可直接互推。

---

## 3. 对外暴露通道与鉴权现状

| 通道 | 端点 | 鉴权 | 用途 |
|---|---|---|---|
| Prometheus | `GET /metrics` | Basic Auth(`mng_user/mng_password`,未配则免鉴权) | 监控系统采集主通道 |
| JSON API | `GET /api/*` | 登录会话 cookie / Basic Auth | 内部平台、面板数据源 |
| Web 面板 | `GET /` | 同上(未认证返回登录页) | 人工运维 |
| 探针 | `GET /healthz /readyz` | **不鉴权** | K8s/编排 |
| 管理命令 | `checkproxy show/kill/reload`(业务端口 SQL) | 业务连接认证 | 人工运维(MySQL 客户端) |

服务端仅挂在 `mng_port`,与 MySQL 主端口隔离;启动失败不阻断业务(仅记日志)。

---

## 4. 已对外暴露指标清单

### 4.1 `GET /metrics` Prometheus 文本(基线 19 个序列族)

命名规范:`newproxy_<category>_<name>`,counter 带 `_total`,时长单位秒。渲染见
`src/mgmt/http.rs::render_metrics`。

| 指标 | 类型 | 语义 | 数据源 / 打点 | 状态 |
|---|---|---|---|---|
| `newproxy_connections_total` | counter | 累计接入连接数 | `conn_task` 入口 inc(`front.rs`) | 正常 |
| `newproxy_connections_active` | gauge | 当前活跃(认证后)连接 | 认证通过 inc / 退出 dec(`front.rs`) | 正常 |
| `newproxy_connections_rejected_total` | counter | 认证/IP 拒绝数 | 3 处拒绝路径 inc | 正常 |
| `newproxy_queries_total` | counter | 完成转发的 COM_QUERY 数(含后端返回 ERR) | `record_query`(每次转发完成) | 正常 |
| `newproxy_queries_errors_total` | counter | 查询错误数(与后端 ERR 同源,见 §4.1 注) | 后端 ERR 路径 inc | 已修复(§8) |
| `newproxy_queries_slow_total` | counter | 超 `slow_query_ms`(默认 200ms)慢查询 | `record_query` 阈值判断 | 正常(HELP 已按配置阈值表述) |
| `newproxy_sql_parse_failures_total` | counter | 命令包解析失败 / SQL 非 UTF-8 | `record_parse_failure` | 正常 |
| `newproxy_backend_errors_total` | counter | 后端返回 ERR 包(1064 等) | `record_backend_error` | 正常 |
| `newproxy_traffic_received_bytes_total` | counter | 前后端全部 MySQL 协议收字节 | codec 全局 | 正常 |
| `newproxy_traffic_sent_bytes_total` | counter | 同上发送方向 | codec 全局 | 正常 |
| `newproxy_pool_acquires_total` | counter | 池命中数(取到空闲连接) | `ensure_backend` 命中分支 inc | 已修复(§8) |
| `newproxy_pool_acquire_fails_total` | counter | 取空且新建连接失败(连接/握手/认证) | `ensure_backend` 新建失败路径 inc | 已修复(§8) |
| `newproxy_process_cpu_percent` | gauge | 进程 CPU%(两采样间隔均值) | `ProcMeter`(Linux) | 正常(Linux 外 = NaN) |
| `newproxy_process_memory_bytes` | gauge | 进程 RSS | `ProcMeter`(Linux) | 正常(Linux 外 = -1) |
| `newproxy_process_fd_count` | gauge | 打开 FD 数 | `ProcMeter`(Linux) | 正常(Linux 外 = -1) |
| `newproxy_sql_total{template,shard}` | counter | SQL 模板 × 分片计数 | `sql_stats`(有界) | 正常 |
| `newproxy_sql_duration_seconds_total{template,shard}` | counter | 模板累计耗时(秒) | `sql_stats.total_time_us` | 正常 |
| `newproxy_sql_rows_total{template,shard}` | counter | 模板返回行数(结果集逐行统计) | 转发层 `ForwardCount.rows` → `record_query` | 已修复(§8) |
| `newproxy_sql_max_duration_seconds{template,shard}` | gauge | 模板单次最大耗时(秒) | `sql_stats.max_time_us` | 正常 |

labels:`template` = 归一化 SQL(字面量 → `?`,高基数治理:≥1024 模板后不再登记);
`shard` = `cluster.tablet`,空归 `"-"`。
> 更新(2026-09-02):§8 新增序列族已全部上线(直方图、分片查询/耗时/慢查询、按库/按节点流量、
> 池空闲连接、连接按状态、后端错误按错误码、进程 uptime),见 [§8 实施记录](#8-实施记录2026-09-02)。

### 4.2 `GET /api/status` JSON(比 /metrics 多出的维度)

| 字段组 | 内容 | 说明 |
|---|---|---|
| `connections_*` / `queries_*` | 同 §4.1 全局计数 | `queries_errors` 已与后端 ERR 同源打点(§8) |
| `packets_received/sent` | codec 全局包计数 | 仅 JSON,未上 Prometheus |
| `stage_avg_us{interval,parse,setup,send,forward}` | 阶段平均耗时 µs(累计/查询数) | 无直方图 → 只有平均 |
| `shards[{shard,queries}]` | 分片查询数 | 未上 Prometheus |
| `shard_stages[{shard,queries,avg_us{5 阶段}}]` | 分片阶段平均 | 未上 Prometheus |
| `db_traffic[{db,recv/sent_bytes,recv/sent_pkts}]` | 按库客户端方向流量 | 未上 Prometheus |
| `node_traffic[{node,...}]` | 按后端节点流量 | 未上 Prometheus |
| `process{cpu_percent,memory_bytes,fd_count,connections_active}` | 进程负载 + 活跃连接 | 进程部分与 /metrics 同源 |

### 4.3 其余 JSON 端点(排障/操作,非时序指标)

| 端点 | 内容 |
|---|---|
| `GET /api/connections` | 活跃连接:cid/addr/user/db/state/backends/uptime_secs |
| `GET /api/sql` | Top 100 SQL 模板(count/total/max/avg/rows_sent) |
| `GET /api/slow` `?sql=&db=&user=&shard=` | 最近慢查询 ≤200 条,含 5 阶段耗时 |
| `GET /api/recent` `?sql=&db=&user=&shard=` | 最近全部查询 ≤500 条,含单次 5 阶段 + slow 标记 |
| `GET /api/parsefailures` | 最近解析失败 ≤200 条(ts/reason/sql) |
| `GET /api/backenderrors` | 后端错误:总数 + 按码聚合 + 最近 ≤200 条 |
| `GET /api/pool` | 每桶 cluster/tablet/user/db、write/read 队列深度、容量配置 |
| `GET /api/config` | 配置摘要 + 拓扑(host:port),**已剔除全部密码** |
| `GET /api/check` | 一键健康检测(异步探测各分片连通性) |
| `POST /api/kill?cid=N` / `POST /api/reload` / `POST /api/slow/clear` 等 | 操作类 |

### 4.4 探针

- `GET /healthz`:恒 200 `ok`,不鉴权。
- `GET /readyz`:仅检查 `cfg.port != 0` + 集群数>0 提示,不鉴权。
  (设计稿 §4.2 的"池可用性激增 → 503"未实现;主 listener 是否在监听未做实际探测。)

### 4.5 `checkproxy` 文本命令(业务端口)

| 命令 | 现状 |
|---|---|
| `checkproxy show status` | summary 子集(连接/查询/流量/池),其中 errors 与池两行恒 0 |
| `checkproxy show connections` | 已接真实注册表(前端拦截渲染) |
| `checkproxy show pool` | ⚠️ 占位文本 `Pool: (not yet populated)`,未接池(真实池状态仅在 `GET /api/pool`) |
| `checkproxy show sql` | Top 10 SQL 模板 |
| `checkproxy show config` | 运行配置文本(无密码) |
| `checkproxy kill <cid>` / `reload` | 已实现 |

---

## 5. 关键发现:对外指标中的失真/缺口

| # | 问题 | 严重度 | 依据 |
|---|---|---|---|
| 1 | `newproxy_queries_errors_total`、`newproxy_pool_acquires_total`、`newproxy_pool_acquire_fails_total` **从未打点,恒 0**(仅定义 + 摘要/渲染读取;`Metrics::queries_errors` 全仓无 `.inc()`,池取用点在 `front.rs` 亦无计数) | 高 | `metric.rs`、`front.rs::try_acquire` |
| 2 | `newproxy_sql_rows_total` 生产恒 0:`record_query` 唯一生产调用点 rows 实参传 0,结果集转发层未统计返回行数 | 高 | `front.rs` record_query 调用 |
| 3 | `/metrics` **无查询耗时直方图**,只有全局/模板的累计与平均 → 监控侧无法算 p50/p95/长尾;设计稿 §4.3 直方图未落地 | 高 | `metric.rs` stage 累计字段 |
| 4 | `newproxy_queries_slow_total` 的 HELP 写死 `(>1s)`,真实阈值 = 配置 `slow_query_ms`(默认 200ms)且热加载动态生效 → 语义误导 | 中 | `/metrics` HELP vs `front.rs` 阈值 |
| 5 | 大量已采集数据**只进 JSON API、未上 Prometheus**:分片查询数/阶段耗时、按库/按节点流量、池桶队列深度、连接状态、错误码维度 | 中 | `/metrics` 渲染 vs `/api/status`、`/api/pool` |
| 6 | `checkproxy show pool` 是占位文本;`Metrics.bytes_received/sent` 字段本身未记账(真实值取自 codec 全局) | 低 | `mgmt.rs`、`metric.rs` |
| 7 | 进程指标仅 Linux `/proc` 有值(非 Linux NaN/-1);限流器 `TokenBucket` 仅存在于单元测试,未接入生产,故无限流计数指标 | 低 | `mgmt/proc.rs`、`limit.rs` |
> 更新(2026-09-02):#1~#5 已修复/补齐(见 [§8 实施记录](#8-实施记录2026-09-02));
> #6 `checkproxy show pool` 占位文本与 #7(限流器未接入生产)维持现状(低危,不在 P0-P2 清单)。

---

## 6. 建议补齐的对外指标

> 实施状态(2026-09-02):P0 / P1 / P2 清单已全部落地,见 [§8 实施记录](#8-实施记录2026-09-02);
> P2 中"慢查询计数按分片/库"按**分片**维度实现。

### P0 — 修复死指标(纯打点,无新采集)

- `queries_errors_total`:后端 ERR 或驱动错误路径计数(与 `record_backend_error` 同点或其上)。
- `pool_acquires / pool_acquire_fails`:`front.rs` 池取用点,命中成功 / 空池且建连失败分别计数
  (语义需定:命中 = 取到空闲连接;失败 = 取空且新建连接失败)。
- `sql_rows_total`:转发层统计结果集行数后传入 `record_query`,或下线该系列(避免恒 0 误导)。
- 修正 `queries_slow_total` HELP 为真实阈值来源(配置 `slow_query_ms`)。

### P1 — Prometheus 补齐(渲染层与 JSON 同源,零新采集)

| 建议指标 | 类型 | 数据源 |
|---|---|---|
| `newproxy_shard_queries_total{shard}` | counter | `shard_query_counts` |
| `newproxy_shard_duration_seconds_total{shard}` | counter | `shard_stage_us`(5 阶段合计) |
| `newproxy_db_traffic_received_bytes_total{db}` / `..._sent_bytes_total` | counter | `db_traffic` |
| `newproxy_node_traffic_received_bytes_total{node}` / `..._sent_bytes_total` | counter | `node_traffic` |
| `newproxy_pool_idle_connections{cluster,tablet,user,db,role}` | gauge | `SrvPool::all_buckets()` 队列深度 |
| `newproxy_connections_by_state{state}` | gauge | 连接注册表 |

### P2 — 新采集(小改 `metric.rs`)

- **`newproxy_queries_duration_seconds` histogram**:固定分桶
  `[0.0001, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1, 5, +Inf]` 秒,渲染 `_bucket/_sum/_count`;
  `record_query` 已有 `elapsed_us`,按桶 `fetch_add`,无锁零分配。**这是支撑 P95 告警的核心指标。**
- 慢查询计数按分片/库维度拆 label(当前仅全局累计)。
- `newproxy_backend_errors_total{code}`:错误码拆 label,替代单值。
- `newproxy_process_uptime_seconds`(进程启动时长)。

> QPS/Bps/Pps 等速率**由监控侧对 counter 做 `rate()`**,无需代理进程内实现。

---

## 7. 对外暴露边界(能 / 慎 / 不)

| 等级 | 内容 | 理由 |
|---|---|---|
| ✅ 可直接对外(监控/第三方) | 连接/查询/慢/流量/池的聚合计数与 gauge、CPU/RSS/FD、延迟直方图 | 无 SQL、无用户、无 IP |
| ⚠️ 仅建议内网监控 | `newproxy_sql_total{template,...}` 及库/分片/节点 label 维度 | SQL 模板含业务查询模式(带表名),库/表/分片名可能泄露业务结构;label 基数受拓扑规模约束 |
| 🔒 仅内网运维面板/API | `/api/slow` `/api/recent` `/api/parsefailures` `/api/backenderrors`(含 SQL 原文)、`/api/connections`(客户端 IP+user+db)、`/api/config` 拓扑 host:port | 均为鉴权通道;对第三方暴露需先脱敏 SQL 与库名 |
| ❌ 不暴露 | 任何凭据与配置原文 | `api_config` 已剔除密码 |

安全基线:除 `/healthz` `/readyz` 外全部端点要求认证;生产部署必须显式配置
`mng_user/mng_password`,否则面板可任意 kill/reload(见 13-可观测性设计 §9.2)。

---

## 8. 实施记录(2026-09-02)

按 §6 清单落地,与 §4/§5 状态更新同步。改动文件:`src/metric.rs`、`src/proto/result.rs`、
`src/conn/front.rs`、`src/mgmt/http.rs`、`src/mgmt/proc.rs`。

### P0 — 死指标修复

| 指标 | 修复方式 |
|---|---|
| `newproxy_queries_errors_total` | `handle_query` 后端返回 ERR 处 inc(与 `record_backend_error` 同源,对客户端可见的查询失败) |
| `newproxy_pool_acquires_total` | `ensure_backend` 池命中(取到空闲连接)分支 inc |
| `newproxy_pool_acquire_fails_total` | `ensure_backend` 新建连接失败:TCP 连接失败 / 后端握手(含认证)失败,均 inc |
| `newproxy_sql_rows_total{template}` | `result.rs::ForwardCount` 新增 `rows`,`forward_backend_response` 在 ReadingRows 阶段每转发一行 +1;`handle_query` 将 `fc.rows` 传入 `record_query`(OK/ERR/0 列结果集为 0) |
| `queries_slow_total` HELP 文案 | 改为 "elapsed above config slow_query_ms threshold",不再写死 1s |

### P1 — /metrics 补齐(渲染层与 JSON 同源)

新增序列族:`newproxy_shard_queries_total{shard}`、`newproxy_shard_duration_seconds_total{shard}`
(5 阶段合计)、`newproxy_shard_slow_queries_total{shard}`、`newproxy_db_traffic_{received,sent}_bytes_total{db}`、
`newproxy_node_traffic_{received,sent}_bytes_total{node}`、`newproxy_pool_idle_connections{cluster,tablet,user,db,role}`、
`newproxy_connections_by_state{state}`。

### P2 — 新采集

- **`newproxy_queries_duration_seconds` histogram**:`metric.rs::LatencyHistogram`(10 桶,
  上界 µs = 100/1k/5k/10k/50k/100k/500k/1M/5M/∞,内存非累计存储、渲染转累计)+
  `queries_elapsed_us` 累计(`_sum`);`record_query` 每次执行入桶。监控侧可算 p50/p95/p99。
- **后端错误按错误码**:`newproxy_backend_errors_total{code="..."}`(总计数保持,按码拆分)。
- **进程 uptime**:`newproxy_process_uptime_seconds`(gauge,基准 = `ProcMeter` 创建 ≈ 进程启动);
  `/api/status` 的 `process` 对象同步增加 `uptime_secs`。
- **慢查询按分片**:`shard_slow_counts`(DashMap),`record_query` 超阈值时按分片累计。

> 未做(明确边界):`checkproxy show pool` 文本占位(#6)、限流器 `TokenBucket` 接入生产(#7)、
> JSON 结构化日志与 trace_id(docs/13 §5)、`readyz` 深度检查——均非本文 P0-P2 范围。

---

## 9. 维护约定

1. 本清单是"指标字典",改动 `metric.rs` 采集字段或 `mgmt/http.rs` 渲染时,**同步更新 §4 各表**(名称/类型/语义/状态)。
2. 新增对外指标遵循命名规范:`newproxy_` 前缀、counter `_total`、时长单位秒;label 必须评估基数(模板 1024 有界、分片/库随拓扑)。
3. 直方图桶上界在 `metric.rs::LATENCY_EDGES_US` 单点定义,`/metrics` 渲染自动派生,禁止两处手抄秒值。
4. 本文与 [13-可观测性设计](./13-observability-design.md) 配套阅读:13 讲通道与端点,本文讲指标口径与清单。

# 13 可观测性设计(Observability Design)

> 状态:**已实现(2026-08-31)**:管理 HTTP 服务(Web 面板 + JSON API +
> Prometheus 指标 + 健康检查,Basic Auth)已落地于 `src/mgmt/http.rs`,
> 挂在 `mng_port`;本文 §4~§6 为设计稿,§9 记录实现与使用说明。
> 设计结论:项目具备"内部可观测"骨架(内存指标、tracing 日志、
> checkproxy 管理命令),此前缺失全部外部可观测通道——现经管理端口打通:
> 监控系统(Prometheus)、编排探针(/healthz /readyz)、运维面板(Web)
> 均可接入。指标格式为 **Prometheus 文本**(零新依赖手写渲染)。

---

## 1. 现状确认

### 1.1 已有能力

| 能力 | 实现 | 位置 |
|---|---|---|
| 指标采集 | 10 个原子计数器(Counter)+ SQL 模板统计(DashMap,上限 1024 条,含 count/总耗时/max/rows) | `src/metric.rs` |
| 日志 | tracing 双通道:每日滚动文件(保留 30 个)+ stderr;支持 `RUST_LOG` 与配置 `log_level`;事件带 `addr/cid/user` 等字段 | `src/logging.rs` |
| 管理命令 | `checkproxy show status/sql/connections/pool/config`、`kill <cid>`、`reload`(经 MySQL 协议) | `src/mgmt.rs` |
| 诊断辅助 | `show connections`(cid/addr/db/state 注册表)、`show pool`(桶/队列) | `src/app.rs` ConnHandle、`src/pool/backend.rs` |

### 1.2 缺口(本设计要补的)

| 缺失项 | 对应原规划 | 影响 |
|---|---|---|
| **指标对外导出** | T4.2 metric 上报(对照 C 版 `tr_metric.c` 127.0.0.1:788 上报,未实现) | Prometheus/Grafana 无法采集 |
| **管理端口** | T4.3 mng_port(3d,未实现) | `mng_port=9111` 仅配置字段,进程唯一 listener 是 MySQL 端口 |
| **健康检查/探针** | 未规划 | K8s/负载均衡无法探活与摘流 |
| **trace_id/span** | T4.4 规划 trace_id/span_id(未实现) | 日志无法按连接/请求关联,跨阶段排障靠肉眼 |
| **结构化日志** | T4.4 | fmt 文本格式;依赖已带 json feature 但未启用 |
| **运行时诊断** | 未规划 | 无堆/连接/慢日志快照的外部入口 |

---

## 2. 设计目标与原则

1. **外部可观测三支柱齐全**:Metrics(采集导出)、Logging(结构化+关联)、Health(探活)。
2. **零新依赖优先**:HTTP 端点用 tokio 手写极简 server(仅 GET,无路由框架);
   Prometheus 文本格式手写渲染;JSON 日志用现有 `tracing-subscriber` json feature。
3. **复用现有采集**:不重写 `Metrics`,在其上做渲染与增量;SQL 统计已有模板化
   聚合,直接导出,注意高基数治理(见 §4.3)。
4. **渐进落地**:P0 打通监控通道 → P1 结构化关联 → P2 管理 API,每阶段可独立验收。

---

## 3. 总体架构

```
                        ┌───────────────────────────────┐
  Prometheus/Grafana ──▶│  mng_port HTTP (T4.3 落地)    │
  K8s 探针          ──▶│  GET /metrics   Prometheus    │
  运维脚本          ──▶│  GET /healthz   存活           │
                        │  GET /readyz    就绪(池+监听)  │
                        │  GET /api/*     JSON(P2)      │
                        └──────────────┬────────────────┘
                                       │ 读取
                        ┌──────────────▼────────────────┐
                        │  Metrics(内存,现有)            │
                        │  + 新增直方图/标签字段(§4.2)    │
                        └──────────────┬────────────────┘
                                       │ 写入(tracing)
                        ┌──────────────▼────────────────┐
                        │  结构化日志(JSON, P1)          │
                        │  连接级 span/trace_id(cid)     │
                        └───────────────────────────────┘
```

服务端仅一个新增模块 `src/mgmt/http.rs`(或 `src/observability.rs`),由 main 在
`mng_port` 起 listener;与 MySQL 主端口(业务流量)完全隔离,互不影响。

---

## 4. P0 — 指标与健康(推荐先做)

### 4.1 HTTP 服务(`src/mgmt/http.rs`,零新依赖)

- `tokio::TcpListener::bind(("0.0.0.0", mng_port))`;每连接一 task;
  解析 HTTP/1.1 请求行 + Host(仅 GET,无 body 解析);其余方法/路径回 404/405。
- 请求处理均为同步渲染(µs 级),无需连接池;设置读取超时防挂死。
- 启动失败(端口占用)→ `error!` 但**不阻断**主服务启动(可观测不得影响可用性)。

### 4.2 端点与指标清单

**`GET /metrics`** — Prometheus 文本格式,`Content-Type: text/plain; version=0.0.4`。
命名规范:`newproxy_<category>_<name>`,单位后缀 `_total`(counter)/`_seconds`。

| 指标 | 类型 | 语义 | 来源 |
|---|---|---|---|
| `newproxy_connections_total` | counter | 累计接入连接数 | `Metrics.connections_total` |
| `newproxy_connections_active` | gauge | 当前活跃连接数 | `connections_active` |
| `newproxy_connections_rejected_total` | counter | 认证/IP 拒绝数 | `connections_rejected` |
| `newproxy_queries_total` | counter | 累计查询数 | `queries_total` |
| `newproxy_queries_errors_total` | counter | 查询错误数 | `queries_errors` |
| `newproxy_queries_slow_total` | counter | 慢查询(>1s)数 | `queries_slow` |
| `newproxy_queries_duration_seconds` | histogram | 查询耗时分布(新增,见 §4.3) | 新增 `LatencyHistogram` |
| `newproxy_traffic_received_bytes_total` | counter | 接收字节 | `bytes_received` |
| `newproxy_traffic_sent_bytes_total` | counter | 发送字节 | `bytes_sent` |
| `newproxy_pool_acquires_total` | counter | 池获取成功数 | `pool_acquires` |
| `newproxy_pool_acquire_fails_total` | counter | 池获取失败数 | `pool_acquire_fails` |
| `newproxy_process_cpu_percent` | gauge | 进程 CPU 利用率(两次采集间隔平均,扩缩容信号) | `/proc/self/stat` |
| `newproxy_process_memory_bytes` | gauge | 进程 RSS 内存 | `/proc/self/statm` |
| `newproxy_process_fd_count` | gauge | 打开的文件描述符数 | `/proc/self/fd` |
| `newproxy_pool_connections` | gauge | 池中空闲连接数(新增) | 遍历 `SrvPool` buckets |
| `newproxy_sql_total{template="<归一化SQL>"}` | counter | 按 SQL 模板计数 | `sql_stats.count` |
| `newproxy_sql_duration_seconds_total{template=...}` | counter | 按模板累计耗时 | `sql_stats.total_time_us` |
| `newproxy_sql_rows_total{template=...}` | counter | 按模板返回行数 | `sql_stats.rows_sent` |
| `newproxy_sql_max_duration_seconds{template=...}` | gauge | 按模板最大耗时 | `sql_stats.max_time_us` |

高基数治理:SQL 模板 label 是**有界**的(≥1024 条后不再登记新模板,现有代码
已防呆);指标渲染直接遍历 `sql_stats_snapshot()`,与 `checkproxy show sql` 同源,
无额外内存。

**`GET /healthz`** — 进程存活:恒 200,body `ok`。
**`GET /readyz`** — 就绪:
- 主 MySQL listener 在监听(`AppCtx` 记录启动时 listener 状态)→ 失败 503;
- 配置已加载(必有)→ 失败 503;
- 可选:后端池可用性(最近 N 秒内 `pool_acquire_fails` 激增)→ 503。
body 为 JSON 或纯文本原因,便于排查。

### 4.3 新增采集字段(小改 `src/metric.rs`)

1. **查询耗时直方图**(取代"只数慢查询"):固定分桶
   `[0.0001, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1, 5, +Inf]` 秒,
   实现为 `[AtomicU64; N]` 桶 + 计数,渲染成 `_bucket`/`_sum`/`_count`。
   `record_query` 已有 `elapsed_us`,按桶 `fetch_add` 即可,无锁、零分配。
2. **池空闲连接 gauge**:`SrvPool::all_buckets()` 遍历写/读队列长度合计
   (µs 级,仅采集时执行)。

### 4.4 配置

`[MySQL_Proxy_Layer]` 新增:
```ini
# 管理 HTTP 端口(可观测端点);0 = 关闭(默认建议开启)
mng_port = 9111
```
复用现有 `mng_port` 字段即可,无需新配置项;`mng_port=0` 时不启动 HTTP。

### 4.5 验收(P0)

- `curl localhost:9111/metrics` 返回合法 Prometheus 文本;`promtool check metrics` 通过;
- 压测后 `/metrics` 中 `queries_total`、`sql_total` 与 `checkproxy show status/sql` 一致;
- `/healthz` 恒 200;主端口停止后 `/readyz` 503;
- 管理端口故障不影响业务流量(杀掉 HTTP task,MySQL 端口照常)。

---

## 5. P1 — 结构化日志与请求关联

### 5.1 JSON 日志(小改 `src/logging.rs`)

- 文件通道启用 `.json()`(`tracing-subscriber` json feature 已引入);
- 字段约定:保留 `timestamp/level/target`;业务事件统一携带
  `cid`(连接 id)、`addr`、`user`、`elapsed_ms`(已有,保持);
- stderr 终端通道保持人类可读 fmt(不 JSON),双通道互不干扰。

### 5.2 连接级 trace_id/span

不引入 OTel;用**连接 id 作为关联键**(cid 全局唯一、日志早已带):

- `conn_task` 入口 `let span = tracing::info_span!("conn", cid, addr)`(per-conn span,
  不跨进程传播,解决"同连接多事件关联");
- 查询事件补阶段耗时:前端读 → 后端 RTT → 转发,拆分 `record_query` 数据点,
  在 `handle_query` 打 `tracing::debug!(stage = "backend_rtt_us", ...)`;
- 未来若需跨进程追踪(客户端→代理→后端),再评估 OTel;当前 cid 关联已满足
  单进程排障。

### 5.3 验收(P1)

- 日志文件行是合法 JSON,可按 `cid` 过滤整连接生命周期;
- `show status` 与日志中的查询计数一致。

---

## 6. P2 — 管理 API 化(可选)

`mng_port` 追加 JSON 端点,与 `checkproxy` 命令同源渲染:

| 端点 | 内容 |
|---|---|
| `GET /api/status` | `MetricsSummary`(直接 `serde_json`) |
| `GET /api/connections` | 注册表快照(`ConnHandle` 字段) |
| `GET /api/sql?top=N` | `top_queries(N)` |
| `GET /api/pool` | 桶/队列/服务次数 |
| `GET /api/config` | 当前配置摘要 |

价值:自动化采集不再依赖 MySQL 客户端与 checkproxy 命令,为内部平台对接铺路。

---

## 7. 落地顺序与工作量(估)

| 阶段 | 内容 | 工作量 | 依赖 |
|---|---|---|---|
| P0 | `/metrics` + `/healthz` + `/readyz` + 直方图 + 池 gauge | 1~2d | 无 |
| P1 | JSON 日志 + 连接 span + 阶段耗时 | 0.5~1d | P0(可选) |
| P2 | `/api/*` JSON 端点 | 0.5~1d | P0 |

推荐一次合入 P0+P1(共用 `src/mgmt/http.rs` 与 logging 改动,评审一次)。

---

## 8. 明确不做(边界)

- **OpenTelemetry SDK**:重依赖、需 Collector,单机代理收益低;需要时以
  cid span 导出 OTLP 的事件日志替代;
- **pprof/堆分析**:macOS/生产环境可用 `sample`/`heaptrack` 外部工具,
  不内嵌;
- **日志告警规则**:属监控平台侧,不在代理内实现。

---

## 9. 实现记录与使用说明(2026-08-31)

### 9.1 已落地(相对设计稿的增量)

管理 HTTP 服务 `src/mgmt/http.rs`(零新依赖,含设计稿 §4 全部端点 +
**Web 管理面板**):

| 端点 | 说明 |
|---|---|
| `GET /` | Web 管理面板(内嵌 `src/mgmt/dashboard.html`:状态卡片、**5 阶段平均耗时卡片**、QPS/连接/延迟 Canvas 趋势图、活跃连接表含 kill、Top SQL、**最近慢查询表(含阶段耗时)**、**最近查询表(全部查询单次阶段耗时,带 SQL 过滤框)**、连接池、配置摘要与 reload 按钮;2s 轮询) |
| `GET /metrics` | Prometheus 文本(§4.2 全部指标;SQL 模板 label 有界) |
| `GET /healthz` | 存活探针,恒 200,不鉴权 |
| `GET /readyz` | 就绪探针(port 配置检查),不鉴权 |
| `GET /api/status` | 运行指标 JSON |
| `GET /api/connections` | 活跃连接列表(cid/addr/user/db/state/backends/uptime) |
| `GET /api/sql` | Top 50 SQL 统计 |
| `GET /api/slow` | 最近慢查询(≤200 条,含 5 阶段耗时);**阈值由配置 `slow_query_ms` 控制(默认 200ms),热加载动态生效** |
| `GET /api/recent[?sql=]` | 最近**所有**查询(≤500 条,含单次 5 阶段耗时+慢标记);`?sql=` 子串过滤,查看单独某条 SQL 的阶段耗时 |
| `GET /api/status` | 含 `stage_avg_us`(各阶段平均耗时 µs)与 `process`(进程负载:CPU%/内存/FD,扩缩容参考) |
| `GET /api/pool` | 连接池桶状态 |
| `GET /api/config` | 配置摘要(不含任何密码) |
| `POST /api/kill?cid=N` | kill 连接 |
| `POST /api/reload` | 热加载配置 |

### 9.2 鉴权

- 凭据来自配置 `[MySQL_Proxy_Layer] mng_user / mng_password`;
- **登录页**:未认证访问 `GET /` 返回登录表单页(HTML),`POST /login` 校验
  成功后发放会话 cookie(`newproxy_session`,HttpOnly,8h 有效)——不依赖
  浏览器 Basic Auth 弹窗;API 请求未登录返回 401,前端自动跳回登录页;
- **兼容 Basic Auth**:`Authorization: Basic` 头仍然有效(便于 curl/脚本);
- **默认不鉴权**:配置文件中未写 `mng_user` 时面板/API 无保护
  (内网/测试部署方便);**生产必须显式配置**,否则面板可任意 kill/reload;
- 除 `/healthz` `/readyz`(编排探针无凭据)外全部端点要求认证;
- 示例(conf/newproxy.conf / tests/perf/newproxy-perf-docker.conf):
  ```ini
  [MySQL_Proxy_Layer]
  mng_user=admin
  mng_password=admin
  ```

### 9.3 使用

```ini
[MySQL_Proxy_Layer]
mng_port=9111
mng_user=admin
mng_password=admin
```

启动后浏览器访问 `http://<host>:9111/`,输入凭据即见面板;
`curl -u admin:admin localhost:9111/metrics` 接入 Prometheus。

### 9.4 后续(P1/P2 设计稿部分,未实现)

- JSON 结构化日志与连接级 trace_id/span(§5);
- `/api/*` 其余管理命令化。

> 更新(2026-09-02):§4.3 的**查询耗时直方图**已实现(`newproxy_queries_duration_seconds`,
> 见 [14-对外性能监控指标梳理](./14-metrics-exposure.md) §8);池空闲连接 gauge、分片/库/节点
> 维度与错误码拆分等亦随 docs/14 落地。

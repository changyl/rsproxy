# newproxy（rsproxy）

> MySQL 分库分表代理 —— **Rust / tokio 原生重写实现**，对齐 C 版 newproxy 的黑盒行为。
> 一个客户端连接 = 一个 tokio task，前端与后端均说 MySQL 原生协议，支持分片路由、
> 主从读写分离、连接池、拓扑热更新与完整可观测性。

`Cargo.toml` 包名与二进制名均为 `newproxy`。仓库目录名为 `rsproxy`。

---

## 目录

- [核心能力总览](#核心能力总览)
- [架构设计](#架构设计)
- [能力详解](#能力详解)
  - [1. MySQL 协议与连接模型](#1-mysql-协议与连接模型)
  - [2. 分库分表与路由](#2-分库分表与路由)
  - [3. SQL 解析与改写](#3-sql-解析与改写)
  - [4. 后端连接池与负载均衡](#4-后端连接池与负载均衡)
  - [5. 用户体系与访问控制](#5-用户体系与访问控制)
  - [6. 高可用与读写分离](#6-高可用与读写分离)
  - [7. 配置热更新与配置中心](#7-配置热更新与配置中心)
  - [8. 运维管理与可观测性](#8-运维管理与可观测性)
- [快速开始](#快速开始)
- [配置速览](#配置速览)
- [目录结构](#目录结构)
- [测试](#测试)
- [文档索引](#文档索引)

---

## 核心能力总览

| 能力域 | 说明 | 状态 |
|---|---|---|
| **MySQL 原生协议** | 前端（对客户端）+ 后端（对 MySQL）双向握手、认证、命令分发、结果集转发 | ✅ |
| **TLS 加密** | 握手阶段 SSLRequest 升级，内建自签名证书（rustls） | ✅ |
| **分片路由** | HASH_MOD / MD5_HASH_MOD / RANGE / LIST / PCRE 五种策略 + 表尾号路由 | ✅ |
| **SQL 解析器** | 自研零拷贝词法器 + 结构化分析（CTE / UNION / 子查询 / 多表写） | ✅ |
| **Scatter-Gather** | 跨分片广播执行计划 + 结果合并（Concat / 聚合 SUM 等） | ✅ |
| **连接池** | 按分片分桶、权重负载均衡、失败转移、健康探测、空闲回收 | ✅ |
| **主从读写分离** | 按语句类型分流，支持 Xenon Raft leader 自动发现 | ✅ |
| **一致性档位** | strong / causal（GTID 屏障）/ session / eventual 四级 | ✅ |
| **拓扑热更新** | `checkproxy reload` / `SIGHUP` / 文件 mtime 自动监听 | ✅ |
| **配置中心** | etcd / ZooKeeper，按分片粒度增量生效 + 断线容错 | ✅ |
| **访问控制** | 产品用户 → 数据库用户映射、IP 白/黑名单、Token Bucket 限流 | ✅ |
| **可观测性** | Prometheus `/metrics`、`/healthz` `/readyz`、Web 面板、JSON API | ✅ |
| **运维命令** | `checkproxy show status/config/sql/connections`、`kill`、`reload` | ✅ |
| **测试体系** | 300+ Rust 用例 + 容器化集成/冒烟/2 分片/sysbench 压测套件 | ✅ |

---

## 架构设计

```
              MySQL 客户端 / sysbench
                       │  MySQL 协议 (可选 TLS)
                       ▼
        ┌──────────────────────────────────┐
        │  front.rs  一个连接 = 一个 task   │
        │  handshake → auth → command loop  │
        │  本地拦截:help / checkproxy 管理命令│
        └───────────────┬──────────────────┘
                        │ SQL
        ┌───────────────▼──────────────────┐
        │  parser  词法 → 结构化分析 → 分类  │
        │  表名/分片键提取、hint、改写、路由 │
        └───────────────┬──────────────────┘
                        │ ShardPlan(单个 / scatter)
        ┌───────────────▼──────────────────┐
        │  RuntimeTopology(配置中心覆盖层)  │
        │  SrvPool  按分片分桶 + 负载均衡    │
        │  HaCenter Xenon 判主 / 一致性档位  │
        └───────────────┬──────────────────┘
                        │ 后端 MySQL 协议
        ┌───────────────▼──────────────────┐
        │  Master / Slave (tablet 分片组)   │
        └──────────────────────────────────┘

  旁路:mgmt/http  → /metrics /healthz /readyz /api/* + Web 面板
```

**设计基线**（详见 [docs/README.md](docs/README.md)）：

- **运行时**：tokio multi-thread work-stealing 调度；worker 线程数由配置 `max_threads` 决定。
- **连接模型**：一个连接 = 一个 task，连接状态是 task 局部变量（替代 C 侧 god-struct `tr_conn_t`）。
- **共享资源**：`Arc` / `ArcSwap` / `DashMap` 显式标注跨 task 访问点；无手写自旋锁与内存屏障。
- **热更新**：`ArcSwap` 原子替换配置快照，读侧无锁；连接池按分片失效，避免全量重建风暴。

---

## 能力详解

### 1. MySQL 协议与连接模型

- **完整握手/认证链路**：server greeting → 客户端 auth response → `mysql_native_password`
  校验（SHA1/SHA256/MD5），失败返回标准错误码 1045。
- **命令循环**：`COM_QUERY`、`COM_INIT_DB`、`COM_PING`、`COM_QUIT` 等命令分发；
  `COM_STMT_PREPARE/EXECUTE/CLOSE` 等预处理语句命令码已建模。
- **结果集转发**：`proto/result.rs` 流式转发后端响应（列定义 / 行 / OK / ERR），
  统计转发行数，不做全量缓冲。
- **TLS**：握手阶段处理 `SSLRequest`，通过 `PrefixedStream` 解决应用层预读字节与
  TLS ClientHello 的边界问题；`rcgen` 生成自签名证书，兼容旧版密码套件。

### 2. 分库分表与路由

- **层级模型**：`Cluster`（集群）→ `CTablet`（分片）→ `Master_Host` / `Slave_Host`（主从组）。
- **路由策略**（配置项 `rrule=表名.分片键,策略,分片索引...`）：

  | 策略 | 说明 |
  |---|---|
  | `hash_mod` | 分片键哈希取模 |
  | `md5_hash_mod` | MD5 哈希取模 |
  | `range` | 范围分片 |
  | `list` | 枚举值分片 |
  | `pcre` | PCRE 正则匹配 |

- **表尾号路由**：`sbtest_0` / `sbtest_1` 形式按 `尾号 % 分片数` 直接定位，适配分表命名约定。
- **Scatter-Gather**：无分片键或匹配不到规则时生成广播计划；支持 `Concat` 与聚合
  （`SUM/COUNT/MAX/MIN`）合并，Top-N 用 `BinaryHeap` 归并。
- **Router Hint**：SQL 注释形式的提示可显式指定路由目标。

### 3. SQL 解析与改写

自研 Rust 原生解析器，**不依赖字符串子串扫描**，结构上正确处理复杂 SQL：

- `lex.rs`：单遍、零拷贝词法器，输出 token 流（正确处理字符串/注释/反引号中的关键字）。
- `analyze.rs`：在 token 流上做结构化分析——查询块、CTE、UNION、子查询、派生表、多表写，
  提取表名与顶层 WHERE 分片键。
- `classify.rs`：语句分类 + 热路径轻量表名提取（非 DML 只做前缀窥探即返回）。
- `hint.rs` / `rewrite.rs` / `route.rs`：hint 解析、SQL/执行计划改写、路由决策。
- 兼容保留：C 版 sqlparser 的 FFI 绑定作为可选 `sqlparser-ffi` feature（默认走自研实现）。

### 4. 后端连接池与负载均衡

- **按分片分桶**：`SrvPool` 以 `(cluster, tablet, user, db, role)` 维度分桶，降低全局锁竞争。
- **负载均衡**：权重轮询 + 候选集选择；`FailoverState` 保证一次请求内不重复尝试同一后端。
- **健康探测**：`HealthTracker` 按连续失败次数标记后端 down，避免雪崩式重试。
- **连接生命周期**：`max_conn_pool_size` 控制池容量，`max_serve_client_times` 限制单连接
  最大服务次数；`idle_reaper` 定期回收空闲连接。
- **按分片失效**：配置中心/热加载变更时只清对应分片桶（`invalidate_shard`），其他分片零扰动。

### 5. 用户体系与访问控制

- **两级用户**：
  - **产品用户** `[Product_User_*]`：客户端连代理的身份，含并发上限 `max_connections`；
  - **数据库用户** `[DB_User_*]`：代理连后端的身份，产品用户通过 `db_username` 映射。
- **IP 控制**：`[Auth_IP_*]`（白名单/需认证）与 `[Ignore_IP_*]`（放行）支持网段掩码与用户绑定。
- **限流**：无锁 Token Bucket（原子 CAS，恒定速率填充）。

### 6. 高可用与读写分离

- **Xenon Raft 集成**：代理内建探测 `mysql.xenon_raft_status`，自动发现 raft leader 并跟随，
  主从切换无需人工改配置（`[XenonRaft_*]` 按分片启用）。
- **读写分离**：按语句类型（只读/写）与一致性档位决定走 master 还是 slave。
- **一致性档位**（优先级：产品用户 > 库 > 分片默认 > strong）：

  | 档位 | 语义 |
  |---|---|
  | `strong` | 全部走 leader，强一致 |
  | `causal` | GTID 高水位屏障读，跨客户端因果一致 |
  | `session` | 读己之写 / 会话单调读 |
  | `eventual` | 免屏障分流，最大吞吐（业务自担一致性） |

### 7. 配置热更新与配置中心

**文件热加载**三种触发方式，均为原子替换、在途会话不打断：

1. `checkproxy reload` 管理命令（走业务端口）；
2. `kill -HUP <pid>` 信号；
3. 配置文件 mtime 自动监听（默认每 5s 轮询，`reload_interval=0` 关闭）。

生效语义：新连接/新建后端连接使用新拓扑；旧主库空闲连接作废；解析失败时保持旧配置运行
并返回 `RELOAD failed: ...`。

**配置中心**（etcd / ZooKeeper）面向上千分片场景，解决文件全量替换的线性放大：

```ini
[ConfigCenter]
type=etcd                  # none | etcd | zookeeper
endpoints=http://127.0.0.1:2379
root=/newproxy
```

- 启动时拉取全量分片拓扑作运行时基线（覆盖文件静态基线）；
- 增量订阅（etcd watch / zk children+data watch），单分片变更只更新该分片；
- 只失效变更分片的连接池；
- 配置中心不可达时用最后已知拓扑继续服务，3s 后自动重连重建基线。

数据布局：`{root}/clusters/{cluster_id}/tablets/{tablet_id}` → JSON
`{"cluster_id":"0","tablet_id":"t0","master":{...},"slaves":[...]}`。

### 8. 运维管理与可观测性

**`checkproxy` 管理命令**（以 SQL 形式走业务端口，无需额外管理连接）：

| 命令 | 说明 | 状态 |
|---|---|---|
| `checkproxy show status` / `stats` | 服务概况（连接/查询/流量/连接池） | ✅ |
| `checkproxy show config` / `config` | 当前完整配置 | ✅ |
| `checkproxy show sql` / `show query` | Top 10 高频 SQL 统计 | ✅ |
| `checkproxy show connections` / `processlist` | 活跃连接列表 | ✅ |
| `checkproxy kill <id>` | 断开指定连接 | ✅ |
| `checkproxy reload` | 热加载配置 | ✅ |
| `checkproxy show pool` | 连接池状态 | 🚧 文本占位 |

**管理 HTTP 服务**（`mng_port`，Basic Auth，与业务端口隔离）：

| 端点 | 用途 |
|---|---|
| `GET /metrics` | Prometheus 文本格式指标（20+ 指标族） |
| `GET /healthz` / `GET /readyz` | 存活 / 就绪探针（K8s、LB 摘流） |
| `GET /` | Web 运维面板（状态、连接、慢查询、HA、配置） |
| `GET /api/status\|connections\|sql\|slow\|recent\|pool\|config\|ha` | JSON API |
| `GET /api/parsefailures` / `api/backenderrors` | 解析失败 / 后端错误明细 |
| `POST /api/kill` / `api/reload` / `api/ha/reprobe` | 运维动作 |

**指标覆盖**：连接数/拒绝数、查询数/错误数/慢查询、查询耗时直方图（10 桶，可算
p50/p95/p99）、流量（全局/按库/按节点）、连接池获取与空闲数、**8 段链路耗时分解**
（interval / parse / setup / send / exec / recv / cli_send / forward）、按分片查询与慢查询、
后端错误按错误码、进程 CPU/内存/FD/uptime、按 SQL 模板统计（有界 1024 条防高基数）。

---

## 快速开始

### 依赖

| 依赖 | 版本 | 用途 |
|---|---|---|
| Rust 工具链 | stable ≥ 1.70 | 编译 |
| MySQL 客户端 | 5.7 / 8.x | 连接验证 |
| Docker（可选） | 任意 | 本地起后端/集成测试 |

### 编译

```bash
./build.sh              # release（LTO + strip）→ target/release/newproxy
./build.sh debug        # 快速调试构建
./build.sh check        # 仅类型检查
./build.sh test         # release 构建 + cargo test --all-targets
./build.sh clippy       # 静态检查（-D warnings）
./build.sh ci           # fmt + clippy + build + test
./build.sh install      # 安装到 $PREFIX/bin（默认 /usr/local/bin）
./build.sh help         # 全部命令
```

服务管理：`./build.sh start | stop | restart | status`（nohup + PID 文件 + 端口预检）。

### 启动

```bash
# 本地后端（root/test_password，库 test，自动执行 tools/init.sql）
docker compose up -d

# 启动代理（配置文件为必填参数，缺失即退出码 2）
./target/release/newproxy -c conf/newproxy-test.conf
# 启动成功输出: listening on 0.0.0.0:4051
```

### 连接

客户端参数与直连 MySQL 一致，仅端口指向代理；用户名/密码使用**产品用户**：

```bash
mysql -h 127.0.0.1 -P 4051 -u app_user -papp_pass test -e "SELECT * FROM users"

# 运维命令
mysql -h 127.0.0.1 -P 4051 -u app_user -papp_pass -e "checkproxy show status" test

# 管理面板 / 指标
open http://127.0.0.1:9111/          # admin / admin（生产务必修改）
curl -u admin:admin http://127.0.0.1:9111/metrics
curl http://127.0.0.1:9111/healthz
```

---

## 配置速览

配置文件为 GKeyFile 风格 INI（`[section] key=value`，`#` 注释），完整示例见
[`conf/newproxy.conf`](conf/newproxy.conf)。

```ini
[MySQL_Proxy_Layer]
port=4051                # 业务端口
mng_port=9111            # 管理 HTTP 端口（0 = 关闭）
max_threads=8            # tokio worker 线程数
log_dir=logs
log_level=info
client_timeout=28800
server_timeout=28800
slow_query_ms=200        # 慢查询阈值
reload_interval=5        # 配置文件自动热加载轮询秒数（0 = 关闭）

[Cluster_0]
name=test_cluster

[CTablet_0_t0]
name=t0
rrule=user.id,hash_mod,0,1,2,3     # 可选：路由规则

[Master_Host_g0]
host=127.0.0.1
port=3306
max_conn_pool_size=16
max_connections=256
weight=1
cluster_tablet_name=t0

[Slave_Host_g0]
host=127.0.0.1
port=3307
cluster_tablet_name=t0

[DB_User_dbu]
db_username=root
db_password=secret
default_db=test
cluster_name=test_cluster

[Product_User_pu]
username=app_user
password=app_pass
db_username=root
max_connections=128

[Auth_IP_0]
ip=10.0.0.0
mask=8
users=app_user
```

配置项逐条说明见 [docs/11-usage-guide.md](docs/11-usage-guide.md) §6。

---

## 目录结构

```
rsproxy/
├── build.sh                 # 统一编译 / 服务管理入口
├── Cargo.toml               # 包名 newproxy，binary newproxy
├── conf/                    # 生产与测试配置示例
├── src/
│   ├── main.rs              # 参数解析、运行时构建、listener、信号处理
│   ├── app.rs               # AppCtx 全局上下文、连接注册表、reload
│   ├── config/              # INI 加载与配置模型
│   ├── config_center/       # etcd / zookeeper 拓扑订阅
│   ├── conn/                # 前端连接状态机（front.rs）
│   ├── parser/              # 词法/分析/分类/hint/改写/路由
│   ├── proto/               # 握手、认证、编解码、命令、结果集、TLS
│   ├── pool/                # 分片分桶连接池、负载均衡
│   ├── ha/                  # Xenon 判主、一致性档位、路由决策
│   ├── mgmt/                # checkproxy 命令 + HTTP 面板/API/Prometheus
│   ├── ffi/                 # C sqlparser 可选 FFI 绑定（stub/pool/raw）
│   ├── metric.rs            # 指标采集（原子计数 + 直方图 + 分片/库/节点维度）
│   ├── limit.rs             # Token Bucket 限流 + IP 黑白名单
│   └── logging.rs           # tracing 双通道日志（滚动文件 + stderr）
├── tests/                   # 单元/集成/性能测试与容器化套件
├── tools/                   # 测试、压测、fake 后端等脚本
└── docs/                    # 设计文档与操作手册
```

---

## 测试

```bash
# 单元 + 集成测试
./build.sh test
cargo test --all-targets

# 统一入口：容器化冒烟/全量/2 分片/覆盖率/压测
./tests/integration/run.sh smoke      # 起代理并验证 8 项核心功能
./tests/integration/run.sh full       # 全量集成测试
./tests/integration/run.sh env        # 环境检查
./tests/integration/run.sh 2shard all # 2 分片容器场景
./tests/integration/run.sh perf       # sysbench 压测
./tests/integration/run.sh cov        # 覆盖率
./tests/integration/run.sh clippy     # 静态检查
./tests/integration/run.sh logs proxy # 跟踪代理日志
```

- 单元/集成用例内联于各模块 `#[cfg(test)]` + `tests/*.rs`，覆盖协议解析、SQL 语法、
  路由、连接池、HA、错误路径等。
- 性能压测：`tests/perf/` 提供 sysbench 容器套件（直连 vs 代理对比，报告输出 Markdown），
  详见 [docs/08-sysbench-perf-test.md](docs/08-sysbench-perf-test.md) 与
  [tests/perf/sysbench_report_2shard.md](tests/perf/sysbench_report_2shard.md)。
- 辅助脚本：`tools/test.sh`、`tools/itest.sh`、`tools/perf.sh`、`tools/fake_mysql.py`。

---

## 文档索引

| 文档 | 内容 |
|---|---|
| [docs/README.md](docs/README.md) | 文档总索引、设计基线、关键决策（ADR） |
| [01-成本评估](docs/01-rewrite-cost-assessment.md) | 重写定性、规模、难点矩阵、人月估算 |
| [02-并发模型](docs/02-concurrency-model.md) | tokio 运行时拓扑、连接=task、reload 设计 |
| [03-后端连接池优化](docs/03-backend-pool-optimization.md) | 锁竞争分析、分片策略、预热与回收 |
| [04-MySQL 协议状态机](docs/04-mysql-protocol-statemachine.md) | packet 帧、握手/认证、命令分发、结果集 |
| [05-SQL 解析器对象池](docs/05-parser-pool.md) | parser 池化封装与 FFI 边界 |
| [06-任务计划](docs/06-task-plan.md) | 阶段里程碑、任务卡片、排期与风险登记 |
| [07-任务卡详细版](docs/07-task-cards-detail.md) | 35 张任务卡细化至可执行级 |
| [08-sysbench 性能测试](docs/08-sysbench-perf-test.md) | 容器化压测套件与报告口径 |
| [09-测试框架](docs/09-test-framework.md) | `tests/integration/run.sh` 使用说明 |
| [10-管理命令](docs/10-management-commands.md) | `checkproxy` 运维命令详解 |
| [11-编译与使用指南](docs/11-usage-guide.md) | 编译、配置、启动、接入、FAQ（**操作手册**） |
| [12-性能基准](docs/12-perf-bench.md) | 压测方法与基准数据 |
| [13-可观测性设计](docs/13-observability-design.md) | `/metrics`、探针、面板、JSON API 设计 |
| [14-性能监控指标梳理](docs/14-metrics-exposure.md) | 指标字典、暴露清单、失真点与边界 |
| [15-Xenon 高可用](docs/15-xenon-ha.md) | Raft 判主与一致性档位 |

---

## License

UNLICENSED（内部项目）。

# 任务计划

> 基于 `/docs` 设计文档(01–05),拆解为可执行任务卡片。
> **团队规模**:3–4 人可并行(标注并行/串行)。**reload 推迟**:首版重启生效,不进关键路径。
> **验收基准**:现有 662 个 MySQL 兼容用例(`mysql_case/totalTest.sh`)+ 59 个 newproxy 用例(`newproxy_case/run.sh`)+ 压测(`tools/load.sh` + `run_env/script/select.lua`)。

## 0. 阶段总览

| 阶段 | 目标 | 关键里程碑 | 估时(3–4人并行) | 依赖 |
|---|---|---|---|---|
| P0 | 工程脚手架 + FFI 打通 | Rust 工程能编译链接 sqlparser | 1 周 | — |
| P1 | MySQL 协议层(官方规范驱动,握手/auth/透传) | 端到端代理跑通 SELECT 1 | 3–4 周 | P0 |
| P2 | 并发核心 + 后端池 | 多连接并发、连接池复用 | 2–3 周 | P1 |
| P3 | 分片/改写/合并核心 | 分库分表查询正确 | 4–5 周 | P1, P0(FFI) |
| P4 | 配置/metric/管理/日志 | 线上可运维 | 2 周 | P2 |
| P5 | 集成/测试迁移/性能对齐 | 行为等价 + 压测达标 | 3–4 周 | P2,P3,P4 |
| **合计** | | | **~15–19 周(4–5 个月)** | |

并行结构:P0 完成后,**P1(协议)、P2(并发)的前期、P3(FFI 封装)** 可三人并行;P3 业务逻辑依赖 P1 透传能力;P5 收口需 P2/P3/P4 基本就绪。

```
时间轴(周)  1   2   3   4   5   6   7   8   9  10  11  12  13  14  15  16
P0 脚手架   ██
P1 协议层       ████████
P2 并发核心         ██████
P3 FFI封装      ██
P3 分片核心             ████████████
P4 运维层                       ██████
P5 收口                              ████████████
                       ↑M1      ↑M2        ↑M3          ↑M4
```

- **M1(约 W4)**:协议层透传跑通,SELECT 1 经代理返回。
- **M2(约 W7)**:并发 + 连接池就绪,多连接并发透传稳定。
- **M3(约 W11)**:分片/合并核心就绪,分库分表查询通过 662 用例。
- **M4(约 W16)**:压测对齐达标,可灰度。

---

## 1. 任务卡片

> 每张卡片:`[ID] 标题` | 负责角色 | 估时 | 依赖 | 入口条件 → 出口条件 | 验收标准

### P0 — 工程脚手架

#### T0.1 Rust 工程骨架与构建
- 角色:基建 | 估时:2d | 依赖:—
- 入口:`/docs` 设计文档就绪
- 出口:`cargo build` 产出空 binary,链接 sqlparser 静态库
- 验收:`cargo build --release` 成功;CI(GitHub Actions/GitLab CI)跑通编译

#### T0.2 sqlparser FFI bindgen
- 角色:基建 | 估时:3d | 依赖:T0.1
- 入口:sqlparser 子模块可独立编译
- 出口:`bindgen` 生成 `sqlparser_ffi.rs`,Rust 侧能声明并调用 `mp_init`/`sql_parser_init`/`parse_sql`/`sql_parser_free`
- 验收:单元测试调 `parse_sql("SELECT 1")` 返回 rc=0 且 `sql_cmd` 非空;fuzz 1000 条随机 SQL 不 crash
- 参考:[05-parser-pool.md](./05-parser-pool.md) §8

#### T0.3 配置解析(最小)
- 角色:基建 | 估时:2d | 依赖:T0.1
- 入口:`conf/newproxy.conf` 格式已核实
- 出口:`AppCtx::load()` 解析 port/max_threads/cluster 等核心字段
- 验收:能读 `newproxy.conf` 输出结构化配置;字段缺失时报错清晰
- 参考:[01](./01-rewrite-cost-assessment.md) §config

---

### P1 — MySQL 协议层(参考 [04](./04-mysql-protocol-statemachine.md))

#### T1.1 packet 帧编解码
- 角色:协议 | 估时:3d | 依赖:T0.1
- 入口:T0.1
- 出口:`read_packet`/`send_packet` 实现,自动跨多次 read 拼帧、处理 16MB 分包
- 验收:单元测试覆盖正常/分片/partial read;`tokio::net::TcpStream` 驱动
- 参考:[04](./04-mysql-protocol-statemachine.md) §2

#### T1.2 前端握手 + auth(mysql_native_password)
- 角色:协议 | 估时:3d | 依赖:T1.1
- 入口:T1.1
- 出口:`build_handshake`/`parse_client_auth`/SHA1 scramble 校验;前端状态机到 CommandLoop
- 验收:用 `mysql` CLI(宣告 `mysql_native_password`)能连上代理并认证(密码对/错两路)
- 参考:[04](./04-mysql-protocol-statemachine.md) §3-4.3,官方 Connection Phase

#### T1.3 后端握手 + auth(mysql_native_password)
- 角色:协议 | 估时:3d | 依赖:T1.1
- 入口:T1.1
- 出口:proxy 对后端 MySQL 做 `mysql_native_password` scramble;后端状态机到 CommandLoop
- 验收:proxy 能连上真实 MySQL(后端账号用 `mysql_native_password`)并通过 auth
- 参考:[04](./04-mysql-protocol-statemachine.md) §4.3

#### T1.7 caching_sha2_password + AuthSwitch(官方规范,必做)
- 角色:协议 | 估时:4d | 依赖:T1.2, T1.3
- 入口:T1.2/T1.3 跑通
- 出口:`caching_sha2_password` 的 SHA256 scramble + fast-auth 三路径(缓存命中/明文/RSA);`AuthSwitchRequest` 插件协商;`AuthMoreData`(0x01)处理
- 验收:用 **MySQL 8 默认账号**(caching_sha2_password,不改回 native)连上 Rust 版代理,fast-auth 缓存命中路径通过;构造插件不匹配场景验证 AuthSwitch 成功
- 参考:[04](./04-mysql-protocol-statemachine.md) §4.4-4.5,官方 Connection Phase · Auth Methods

#### T1.4 命令分发 + 透传
- 角色:协议 | 估时:4d | 依赖:T1.2, T1.3
- 入口:T1.2/T1.3 跑通(T1.7 视后端账号决定是否阻塞 M1)
- 出口:`Command` enum + `dispatch`;COM_QUERY/PING/QUIT/INIT_DB 透传
- 验收:**M1** —— 端到端 `SELECT 1`/`SELECT * FROM t` 经代理返回正确结果;`mysql_case` 简单用例通过
- 参考:[04](./04-mysql-protocol-statemachine.md) §5,官方 Command Phase

#### T1.5 结果集流式转发
- 角色:协议 | 估时:3d | 依赖:T1.4
- 入口:T1.4
- 出口:`proxy_result_set` 流式转发,不缓冲全量
- 验收:大结果集(100w 行)内存恒定;EOF/ERR 包正确终止
- 参考:[04](./04-mysql-protocol-statemachine.md) §6

#### T1.6 OK/ERR/EOF 包构造
- 角色:协议 | 估时:2d | 依赖:T1.1
- 入口:T1.1
- 出口:`build_ok`/`build_error`/`build_eof`
- 验收:单元测试字节级对照 C 版输出
- 参考:[04](./04-mysql-protocol-statemachine.md) §6.1

---

### P2 — 并发核心 + 后端池(参考 [02](./02-concurrency-model.md)、[03](./03-backend-pool-optimization.md))

#### T2.1 连接 task 模型 + 状态机骨架
- 角色:并发 | 估时:3d | 依赖:T1.4
- 入口:T1.4 透传跑通
- 出口:`conn_task` + `FrontConn` 局部状态;`drive()` 状态机
- 验收:单连接透传迁到 task 模型,行为不变
- 参考:[02](./02-concurrency-model.md) §3

#### T2.2 后端连接池数据结构
- 角色:并发 | 估时:4d | 依赖:T2.1
- 入口:T2.1
- 出口:`SrvPool`/`AttrBucket`(DashMap 嵌套 + parking_lot Mutex)
- 验收:acquire/release 单元测试;CAS 契约(`in_pool` 1↔0)正确
- 参考:[03](./03-backend-pool-optimization.md) §2-4

#### T2.3 acquire/release + 异步归还
- 角色:并发 | 估时:4d | 依赖:T2.2
- 入口:T2.2
- 出口:`acquire_or_connect`/`release_backend`;`BackendGuard` RAII 兜底
- 验收:连接复用计数正确;`served_times` 达上限不复用;task panic 不泄漏连接
- 参考:[03](./03-backend-pool-optimization.md) §3-4,§9

#### T2.4 负载均衡 + failover
- 角色:并发 | 估时:3d | 依赖:T2.3
- 入口:T2.3
- 出口:power-of-two-choices 选 DB;重试 bitmap failover
- 验收:多后端时连接分布均匀;单后端宕机自动 failover
- 参考:[03](./03-backend-pool-optimization.md) §7

#### T2.5 空闲回收 + 预热(可选)
- 角色:并发 | 估时:3d | 依赖:T2.3
- 入口:T2.3
- 出口:`idle_reaper` task;预热 `warmup_pool`(可后置)
- 验收:空闲连接超时关闭;冷启动无毛刺(若启用预热)
- 参考:[03](./03-backend-pool-optimization.md) §6

#### T2.6 多连接并发压测
- 角色:并发 | 估时:2d | 依赖:T2.3, T2.4
- 入口:T2.3/T2.4
- 出口:**M2** —— 100 并发连接稳定透传
- 验收:`tools/load.sh` 100 并发无错;`tokio-console` 无锁热点告警
- 参考:[02](./02-concurrency-model.md) §11

---

### P3 — 分片/改写/合并核心

#### T3.1 parser 对象池
- 角色:FFI | 估时:3d | 依赖:T0.2
- 入口:T0.2 bindgen 跑通
- 出口:`ParserPool`/`ParserGuard`/`AstHandle`(借用防逃逸)
- 验收:并发解析不 crash;`AstHandle` 借用编译期防逃逸;guard panic 归还
- 参考:[05-parser-pool.md](./05-parser-pool.md) §3-5

#### T3.2 SQL 分类 + 路由 hint 解析
- 角色:分片 | 估时:4d | 依赖:T3.1
- 入口:T3.1
- 出口:命令类型分类;`/*{router}*/`/`force_tbl` 等 hint 解析
- 验收:对照 C 版 `tr_sql.c` 行为;hint 正确影响路由
- 参考:原 `tr_sql.c`(2110)

#### T3.3 分片路由(表→tablet)
- 角色:分片 | 估时:5d | 依赖:T3.2
- 入口:T3.2
- 出口:HASH_MOD/MD5_HASH_MOD/RANGE/LIST 分片;PCRE 路由规则
- 验收:对照 C 版 `tr_sql_partition.c`/`tr_route.c`;分片命中正确 tablet
- 参考:原 `tr_sql_partition.c`(1777)、`tr_route.c`

#### T3.4 SQL 改写(decomposer)
- 角色:分片 | 估时:6d | 依赖:T3.1, T3.3
- 入口:T3.3
- 出口:WHERE 拆解 → 子请求;`to_string` 生成改写后 SQL
- 验收:INSERT/UPDATE/DELETE/SELECT 改写后语义等价 C 版
- 参考:原 `tr_sql_decomposer.c`(2486)、[05](./05-parser-pool.md) §6

#### T3.5 scatter/gather + 结果合并
- 角色:分片 | 估时:6d | 依赖:T3.4, T2.3
- 入口:T3.4
- 出口:`exec_distributed` 并发 fan-out;`merge_query_results`(min-heap/GROUP BY/ORDER BY/聚合)
- 验收:**M3** —— 分库分表查询通过 `mysql_case` 分片用例;AVG/COUNT(DISTINCT) 合并正确
- 参考:[02](./02-concurrency-model.md) §5、原 `tr_result.c`(2924)

#### T3.6 读写分离
- 角色:分片 | 估时:2d | 依赖:T3.3
- 入口:T3.3
- 出口:SELECT→slave / 写→master;只读白名单
- 验收:读写分流正确;只读集群拒绝写
- 参考:原 `tr_multi_site.c`、`tr_packet.c:2549`

#### T3.7 预编译语句(prepare/execute)
- 角色:分片 | 估时:5d | 依赖:T3.4, T3.1
- 入口:T3.4
- 出口:AST 缓存;binary protocol 参数解析;re-exec 跳过解析
- 验收:`MYSQL_TYPE_NULL` 用例不溢出(迁移 `test/asan/`);prepare/exec 正确
- 参考:[04](./04-mysql-protocol-statemachine.md) §7、原 `tr_stmt_prepare.c`(1302)、[05](./05-parser-pool.md) §7

---

### P4 — 配置/metric/管理/日志

#### T4.1 完整配置解析
- 角色:运维 | 估时:3d | 依赖:T0.3
- 入口:T0.3 最小配置
- 出口:全部 39 个 switch_/enable_ 字段 + Cluster/Tablet/Master/Slave/User/AuthIP 段
- 验收:对照 `conf/newproxy.conf` 全字段;非法配置报错清晰
- 参考:[01](./01-rewrite-cost-assessment.md) config

#### T4.2 metric 上报
- 角色:运维 | 估时:3d | 依赖:T2.1
- 入口:T2.1
- 出口:metric reporter task;SQL/IP 统计(`AtomicU64`+`DashMap`)
- 验收:对照 C 版 metric 字段;周期上报正确
- 参考:[02](./02-concurrency-model.md) §8、原 `tr_metric.c`(689)

#### T4.3 管理端口(mng_port)
- 角色:运维 | 估时:3d | 依赖:T4.1
- 入口:T4.1
- 出口:9111 管理命令(show stats / reload[推迟] / switch master[推迟])
- 验收:show 类命令返回正确;reload/switch 返回"未实现,请重启"
- 参考:原 `tr_mng.c`

#### T4.4 日志(tracing)
- 角色:运维 | 估时:2d | 依赖:T0.1
- 入口:T0.1
- 出口:`tracing` 日志;滚动;log_level 过滤;trace_id/span_id
- 验收:对照 C 版日志格式;级别过滤正确
- 参考:原 `tr_log.c`(632)、d3-modules cclog

#### T4.5 优雅关闭
- 角色:运维 | 估时:2d | 依赖:T2.1
- 入口:T2.1
- 出口:SIGTERM → drain 现有连接 → 退出
- 验收:SIGTERM 后无连接中断报错;进程干净退出
- 参考:[02](./02-concurrency-model.md) §9、原 `tr_signal.c`

---

### P5 — 集成/测试迁移/性能对齐

#### T5.1 测试用例迁移
- 角色:测试 | 估时:4d | 依赖:T3.5
- 入口:T3.5 分片核心就绪
- 出口:`mysql_case/totalTest.sh` + `newproxy_case/run.sh` 跑 Rust 版
- 验收:662 + 59 用例通过率对照 C 版;失败用例逐个分析
- 参考:[01](./01-rewrite-cost-assessment.md) §5

#### T5.2 行为等价对照
- 角色:测试 | 估时:3d | 依赖:T5.1
- 入口:T5.1
- 出口:Rust 版与 C 版同用例输出 diff(packet_id/payload)
- 验收:字节级一致(除允许的差异如时间戳)
- 参考:[04](./04-mysql-protocol-statemachine.md) §9

#### T5.3 性能压测对齐
- 角色:性能 | 估时:5d | 依赖:T2.6, T3.5
- 入口:M2/M3 就绪
- 出口:`tools/load.sh` + `select.lua` 压测;QPS/p99 对照 C 版
- 验收:QPS ≥ C 版 80%;p99 不劣化;`tokio-console` 无热点
- 参考:[03](./03-backend-pool-optimization.md) §5.3 决策树

#### T5.4 锁竞争优化(按需)
- 角色:性能 | 估时:3d | 依赖:T5.3
- 入口:T5.3 发现桶锁热点
- 出口:按决策树升级(阶梯 2 分片 / 阶梯 3 Treiber 栈)
- 验收:压测后 p99 改善
- 参考:[03](./03-backend-pool-optimization.md) §5

#### T5.5 稳定性长跑
- 角色:测试 | 估时:3d | 依赖:T5.3
- 入口:T5.3
- 出口:72h 长跑 + 内存监控
- 验收:无 crash;RSS 无泄漏趋势;连接数稳定
- 参考:[05](./05-parser-pool.md) §9

#### T5.6 灰度准备
- 角色:运维 | 估时:2d | 依赖:T5.5
- 入口:T5.5
- 出口:部署脚本;回滚方案;监控告警接入
- 验收:可在测试环境灰度;回滚 < 5min
- 参考:原 `run_env/`、`tools/supervise.newproxy`

---

## 2. 并行排期(3–4 人)

| 人 | P0(1w) | P1(3–4w) | P2(2–3w) | P3(4–5w) | P4(2w) | P5(3–4w) |
|---|---|---|---|---|---|---|
| A 基建/FFI | T0.1 T0.2 T0.3 | — | — | T3.1 | T4.4 | T5.2 |
| B 协议 | — | T1.1→T1.6 | T2.1 | — | T4.5 | T5.1 |
| C 并发/分片 | — | — | T2.2→T2.6 | T3.2→T3.7 | T4.2 T4.3 | T5.3 T5.4 |
| D 测试/运维 | — | (协助T1.4验收) | — | (协助T3.5验收) | T4.1 | T5.5 T5.6 |

- **关键路径**:T0.2 → T1.1 → T1.4(M1)→ T2.3 → T3.4 → T3.5(M3)→ T5.3 → T5.5。
- **P0 后立即三人并行**:A 做 FFI(T0.2/T3.1)、B 做协议(T1.x)、C 做并发(T2.x)。
- **P3 是最长段**(T3.1–T3.7 约 5 周),C 主攻,D 协助验收。
- **P4 可穿插**:T4.4 日志、T4.5 优雅关闭不阻塞主路径,可提前。

---

## 3. 关键路径与里程碑

| 里程碑 | 周 | 含义 | 阻塞条件 |
|---|---|---|---|
| M0 | W1 | FFI 打通,工程可编译 | T0.2 |
| M1 | W4 | 协议透传跑通(SELECT 1 经代理) | T1.4 |
| M2 | W7 | 并发+连接池就绪(100 并发稳定) | T2.6 |
| M3 | W11 | 分片/合并核心就绪(662 用例通过) | T3.5 |
| M4 | W16 | 压测达标,可灰度 | T5.5 |

---

## 4. 风险登记

| 风险 | 概率 | 影响 | 缓解 | 触发应对 |
|---|---|---|---|---|
| FFI unsafe 面大,解析器 crash | 中 | 高 | fuzz 测试;`AstHandle` 借用防逃逸 | T0.2 加 fuzz;若 crash 频发,评估换 sqlparser-rs |
| 性能不达标(低于 C 版 80%) | 中 | 高 | T5.3 压测驱动;`tokio-console` 定位 | 触发 T5.4 锁优化;必要时局部降级为 LocalSet |
| parser 非线程安全被误用 | 低 | 高 | 池+Mutex;只 Send 不 Sync;clippy | code review 卡 `unsafe` 边界 |
| prepared clone 入口 C 侧缺失 | 中 | 中 | T3.7 前与 sqlparser 维护者协调 | 降级为 re-parse(性能略损) |
| 测试用例不兼容(协议细节差异) | 中 | 中 | T5.2 字节级对照 | 逐用例修复,记录允许差异 |
| reload 推迟导致线上不便 | 低 | 低 | 首版重启生效;架构预留 ArcSwap | P5 后补 reload(T3.x 之外独立任务) |
| 团队 Rust 经验差异 | 中 | 中 | A(基建)带 review;关键路径交叉 review | 关键模块双人 review |

---

## 5. 推迟项(明确不进首版)

| 项 | 原因 | 补齐时机 |
|---|---|---|
| 在线 reload(ArcSwap + broadcast) | 复杂状态机,核心链路优先 | P5 后独立任务,~2 周 |
| switch master(主从切换) | 依赖 reload 机制 | 随 reload 一起 |
| SSL/TLS(`SSLRequest`/TLS 握手) | C 版未实现;结构预留(stream 泛型),首版明文可跑 | 若后端强制加密账号时补 RSA/TLS |
| 连接预热(T2.5) | 优化项,非必需 | 压测后按需 |
| 桶锁 Treiber 栈(T5.4) | 仅在压测热点时 | T5.3 触发 |

> 注:`caching_sha2_password` + `AuthSwitchRequest` + `CLIENT_DEPRECATE_EOF` + 0xFFFFFF 分包重组 **不在推迟项**——它们是连接 MySQL 8 默认账号的前提(T1.7 必做)。协议层以官方文档 https://dev.mysql.com/doc/dev/mysql-server/latest/PAGE_PROTOCOL.html 为标准。

---

## 6. 验收基准汇总

- **官方规范符合性**:协议层实现对照官方协议文档各章节字段定义(非照抄 C 代码)。
- **行为等价**:662 + 59 用例通过率 ≥ C 版;字节级 diff 一致(T5.1/T5.2);C 版无对照的新能力(caching_sha2/DEPRECATE_EOF)以官方规范为准。
- **性能**:QPS ≥ C 版 80%,p99 不劣化(T5.3)。
- **稳定性**:72h 长跑无 crash、无内存泄漏(T5.5)。
- **内存安全**:ASAN 用例(`MYSQL_TYPE_NULL`)通过;无 unsafe 越界(T3.7)。
- **可运维**:管理端口 show 类命令、日志、优雅关闭、灰度回滚(T4.3/T4.4/T4.5/T5.6)。

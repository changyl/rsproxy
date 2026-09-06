# NewProxy → Rust 重写文档

本目录记录 newproxy(MySQL 分库分表代理,C 实现)以 Rust 重写的并发模型与核心子模块设计。
设计基于对 `src/` 源码的逐结构分析,目标是按 Rust/tokio 原生模型重构,而非逐行翻译。

> **范围说明**:reload(热加载)已实现——`checkproxy reload` 命令、`SIGHUP` 信号、
> 以及配置文件变更自动热加载(mtime 轮询,`reload_interval` 可配/可关)三种触发方式,
> 见 [10-管理命令](./10-management-commands.md) 与 [11-编译与使用指南](./11-usage-guide.md) §热加载。

## 文档索引

| 文档 | 内容 | 状态 |
|---|---|---|
| [01-成本评估](./01-rewrite-cost-assessment.md) | 项目定性、规模、难点矩阵、依赖映射、人月估算、收益与风险 | 完成 |
| [02-并发模型](./02-concurrency-model.md) | tokio work-stealing 运行时拓扑、连接=task、前后端关系、reload 设计(已实现:命令/SIGHUP/自动热加载) | 完成 |
| [03-后端连接池优化](./03-backend-pool-optimization.md) | 共享池锁竞争分析、分片策略、parking_lot、连接预热、预热与回收 | 完成 |
| [04-MySQL 协议状态机](./04-mysql-protocol-statemachine.md) | packet 帧编解码、握手/auth、命令分发、结果集构造的 async 实现 | 完成 |
| [05-SQL 解析器对象池](./05-parser-pool.md) | 非线程安全 parser 的池化封装、生命周期、与 FFI 边界 | 完成 |
| [06-任务计划](./06-task-plan.md) | 阶段里程碑、任务卡片(入口/出口/依赖/验收/估时)、3–4 人并行排期、风险登记 | 完成 |
| [07-任务卡详细版](./07-task-cards-detail.md) | 全部 35 张任务卡细化至可执行级(步骤/代码骨架/验收操作/常见坑),含源码勘误 | 完成 |
| [08-sysbench 性能测试](./08-sysbench-perf-test.md) | sysbench 容器化压测套件：直连 vs 代理对比、5 场景 × 4 并发 × 30s、Markdown 报告输出 | 完成 |
| [09-测试框架使用说明](./09-test-framework.md) | 统一入口 `run.sh`：smoke/full/env/perf/clippy/stop/help、选项、CI 集成、旧版兼容 | 完成 |
| [10-管理命令](./10-management-commands.md) | `checkproxy` 运维命令：show status/connections/pool/sql、kill、reload、实现状态 | 完成 |
| [11-编译与使用指南](./11-usage-guide.md) | 操作手册：编译（build.sh）、配置文件详解、启动/连接、checkproxy 摘要、测试环境、FAQ | 完成 |
| [12-性能基准](./12-perf-bench.md) | 压测方法、指标口径与基准数据 | 完成 |
| [13-可观测性设计](./13-observability-design.md) | 外部可观测三支柱：/metrics + /healthz /readyz + Web 面板/JSON API（mng_port，已实现） | 完成 |
| [14-对外性能监控指标梳理](./14-metrics-exposure.md) | 指标字典：内部采集全景、已对外暴露清单（/metrics 19 族 + JSON API）、失真点与补齐建议、暴露边界 | 完成（§8 已按清单实施） |

## 设计基线

- **运行时**:tokio multi-thread,work-stealing 调度(放弃 C 侧"连接钉死 worker"的亲和模型)。
- **连接模型**:一个连接 = 一个 task,连接状态是 task 局部变量(替代 C 侧 god-struct `tr_conn_t`)。
- **共享资源**:`Arc`/`ArcSwap`/`DashMap` 显式标注跨 task 访问点(instance/config/srv_pool/统计)。
- **手写并发原语全部消除**:SPSC 环形缓冲 + mfence → channel;状态机 CAS + 屏障锁 → `ArcSwap`;CAS 自旋锁 → `AtomicU64`/`DashMap`。
- **验证基准**:复用现有 662 个 MySQL 兼容用例 + 59 个 newproxy 用例作为黑盒行为等价预言机。

## 关键决策记录(ADR)

1. **调度模型选 work-stealing 而非 LocalSet 钉线程**——用满 tokio 生态、负载自动均衡,代价是 backend 池需共享加锁(详见 [03](./03-backend-pool-optimization.md))。
2. **SQL 解析器走 FFI 保留而非换 sqlparser-rs**——30K LOC 语法是最大资产,newproxy 深度依赖其 unparse/clone 语义;FFI 风险可控(详见 [05](./05-parser-pool.md))。
3. **reload 已实现(非推迟)**——基于 `ArcSwap` 原子替换 + 连接池清空,`checkproxy reload` / `SIGHUP` / 配置文件自动监听均可触发,主从切换无需重启。

# 08-sysbench 性能测试

sysbench 容器化性能测试套件，对比「直连 MySQL」与「通过 NewProxy 代理」两种路径的吞吐和延迟，
用于评估代理层引入的开销以及不同并发度下的扩展性。

## 前提条件

- Docker 运行中
- `cargo build --release` 已完成

## 目录结构

```
tests/perf/
├── docker-compose.yml         # 3 容器环境（mysql + newproxy + sysbench）
├── Dockerfile                 # proxy 镜像定义
├── newproxy-perf.conf          # 代理性能配置（宿主机直连用）
├── newproxy-perf-docker.conf   # 代理性能配置（Docker 环境，host=mysql）
└── init_sysbench.sql          # sbtest 表初始化

tests/integration/
└── run.sh                     # 统一入口（perf 子命令）
```

## 使用方法

```bash
# ===== 完整流程（推荐）=====
./tests/integration/run.sh perf all

# ===== 分步执行 =====
./tests/integration/run.sh perf prepare   # ① 启动容器 + 生成测试数据
./tests/integration/run.sh perf run       # ② 执行全部压测场景 + 生成报告
./tests/integration/run.sh perf cleanup   # ③ 清理数据

# ===== 自定义参数 =====
TABLE_SIZE=50000 THREADS="8 16 32 64" TEST_TIME=60 ./tests/integration/run.sh perf all
```

| 环境变量 | 默认值 | 说明 |
|----------|--------|------|
| `TABLE_SIZE` | `10000` | sbtest1 表行数 |
| `THREADS` | `4 8 16 32` | sysbench 并发线程（空格分隔） |
| `TEST_TIME` | `30` | 每场景运行秒数 |

## 测试场景

| 场景标签 | sysbench 脚本 | 说明 |
|----------|-------------|------|
| 点查询 | `oltp_point_select.lua` | 主键等值 `SELECT`，纯索引查找 |
| 只读 | `oltp_read_only.lua` | 10× 点查 + 1× 范围查 + 1× 聚合，无写 |
| 读写混合 | `oltp_read_write.lua` | 读为主 + UPDATE/DELETE/INSERT |
| 只写 | `oltp_write_only.lua` | UPDATE/INSERT/DELETE，无读 |
| 索引更新 | `oltp_update_index.lua` | 对索引列 `k` 执行 `UPDATE` |

**测试矩阵**：5 场景 × 4 并发（4/8/16/32）× 2 路径（直连/代理）= 40 次 sysbench run。

每个 run 先执行「直连 MySQL」（基线），再启动代理执行「通过 NewProxy」，确保同一 MySQL 实例下可比。

## 输出

### 终端实时输出

每轮 sysbench 完成后即时打印 TPS、QPS、P95 延迟：

```
点查询 | threads=4 | via proxy...
    TPS=12345.67   QPS=12345.67   P95=0.52ms
```

### 报告文件

`tests/perf/sysbench_report.md` — Markdown 格式对比表格：

| 场景 | 并发 | 直连 TPS | 代理 TPS | 直连 P95 | 代理 P95 |
|------|------|----------|----------|----------|----------|
| 点查询 | 4 | 28500.0 | 24100.0 | 0.28ms | 0.35ms |
| 只读 | 8 | 18200.0 | 15400.0 | 0.92ms | 1.15ms |
| ... | ... | ... | ... | ... | ... |

### 代理日志

`logs/proxy_perf.log` — `warn` 级别，压测期间应为 **零输出**（无 warning/error 表示运行正常）。

## 性能配置说明

`tests/perf/newproxy-perf.conf` 相对于集成测试配置做了以下调整：

| 参数 | 集成测试值 | 性能测试值 | 原因 |
|------|----------|----------|------|
| `max_threads` | 4 | 8 | 充分利用多核 |
| `max_connections` | 128 | 512 | 支撑 32+ 并发 |
| `max_conn_pool_size` | 16 | 32 | 更大的后端连接缓冲池 |
| `conn_pool_socket_max_serve_client_times` | 10000 | 100000 | 避免压测中途复用触发重连 |
| `log_level` | debug | warn | 减少日志 I/O 干扰 |
| `default_db` | test | sbtest | 指向 sysbench 数据库 |

## 常见问题

### sysbench prepare 失败：`Access denied`

确认 MySQL 后端 `root` 用户有 `CREATE DATABASE` 权限。容器化 MySQL 默认具备。

### 代理报 `backend auth failed`

`sbtest` 数据库尚未创建。手动执行：

```bash
mysql -h 127.0.0.1 -P 3306 -u root -ptest_password -e "CREATE DATABASE IF NOT EXISTS sbtest"
```

或重启 MySQL 容器（已更新 `tests/integration/init.sql` 包含 `CREATE DATABASE IF NOT EXISTS sbtest`）。

### sysbench cleanup 报 segfault（exit 139）

部分 sysbench Docker 镜像的已知问题，不影响实际数据清理。可手动清理：

```bash
mysql -h 127.0.0.1 -P 3306 -u root -ptest_password -e "DROP DATABASE IF EXISTS sbtest"
```

### 如何只测代理、跳过直连基线？

使用 `perf proxy` 子命令直接运行代理测试：
```bash
./tests/integration/run.sh perf proxy
```

### 如何添加自定义 sysbench 场景？

编辑 `tests/integration/run.sh` 中 `PERF_TESTS` 数组，追加 `"标签:脚本文件名.lua:描述"` 即可。

## 补充:进程内基准(不依赖 Docker)

除 sysbench 外,仓库内置**进程内基准** `tests/perf_proxy.rs`(mock backend +
in-process proxy + 虚拟客户端),用于隔离代理层开销、快速定位瓶颈:

```bash
./tools/perf.sh bench        # → tests/perf/bench_report.md
./tools/perf.sh long         # 60s 长跑,配合 sample/Instruments 采样
```

方法与结论见 [`docs/12-perf-bench.md`](./12-perf-bench.md)。

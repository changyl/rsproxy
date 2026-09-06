# 09-测试框架使用说明

NewProxy 测试框架以 `tests/integration/run.sh` 为统一入口，覆盖单元测试、集成测试、端到端全量测试、代码覆盖率、性能压测、静态检查。

## 快速开始

```bash
# 默认冒烟测试（8 项功能验证）
./tests/integration/run.sh

# 查看帮助
./tests/integration/run.sh help

# 完整测试 + 并发
./tests/integration/run.sh full

# 启动手动测试环境
./tests/integration/run.sh env

# 代码覆盖率（单元 + 端到端）
./tests/integration/run.sh cov

# 性能压测
./tests/integration/run.sh perf

# 静态检查
./tests/integration/run.sh clippy

# 清理一切
./tests/integration/run.sh stop
```

## 命令一览

| 命令 | 说明 | 启动 MySQL | 启动代理 |
|------|------|----------|----------|
| `smoke` | 快速冒烟测试（默认）：连接、SELECT、INSERT、UPDATE、DELETE、SHOW、管理命令、认证拒绝（8 项） | ✓ | ✓ |
| `full` | **E2E 功能全量测试**：基础 SQL/DDL/事务/预处理/错误处理/慢查询链路/checkproxy 管理命令/管理 HTTP API/解析失败/kill 连接/10 并发（约 40 项） | ✓ | ✓ |
| `env` | 仅启动环境，打印连接信息，**阻塞等待 Ctrl+C** | ✓ | ✓ |
| `perf` | sysbench 性能测试（5 场景 × 4 并发 × 直连/代理对比） | — | ✓ |
| `2shard` | 2 分片容器场景测试（`up`\|`check`\|`test`\|`all`\|`cleanup`）：`test` 为纯测试，不碰环境 | 容器 | 容器 |
| `cov` | 代码覆盖率（`unit` \| `e2e` \| `all`，默认 all；依赖 cargo-llvm-cov，报告在 `target/llvm-cov/`） | e2e 时 ✓ | e2e 时 ✓ |
| `clippy` | `cargo clippy --workspace` 静态检查 | — | — |
| `stop` | 停止所有容器和代理 | — | — |
| `help` | 显示帮助 | — | — |

## 选项

| 选项 | 适用命令 | 说明 |
|------|---------|------|
| `--debug` | smoke / full / env / perf | 使用 debug 编译（默认 release） |
| `--log` | smoke / full / env / perf | 完整日志输出到 `tools/itest_output.txt` |

```bash
# debug 版本 + 日志
./tests/integration/run.sh smoke --debug --log

# 启动 debug 版手动环境
./tests/integration/run.sh env --debug
```

## 测试分层

```
                     ┌─────────────────────────────────┐
                     │   tests/integration/run.sh      │
                     │        统一入口                   │
                     └─────────────┬───────────────────┘
                                   │
          ┌────────────────────────┼──────────────────────────┐
          │                        │                          │
          ▼                        ▼                          ▼
   ┌──────────────┐       ┌──────────────┐          ┌──────────────┐
   │ smoke / full │       │     env      │          │     perf     │
   │  集成测试     │       │  手动环境     │          │  sysbench    │
   └──────┬───────┘       └──────────────┘          └──────┬───────┘
          │                                                │
          ▼                                                ▼
   ┌──────────────┐                               ┌──────────────┐
   │  MySQL 容器   │                               │  sysbench    │
   │  + newproxy   │                               │  容器 + 报告  │
   └──────────────┘                               └──────────────┘
```

### 单元测试（Rust `cargo test`）

独立于 `run.sh`，直接使用 cargo：

```bash
cargo test --lib                              # 150+ 项 lib 测试
cargo test --test mysql_syntax_tests           # 22 项 SQL 语法分类
cargo test --test front_error_paths            # 错误注入:mock 后端驱动 front.rs 错误/边界路径
cargo test --test backend_diag -- --nocapture  # 后端直连诊断（需本机 3306 有 MySQL）
cargo test --test skeleton                     # 编译骨架
```

### 集成测试（smoke / full）

通过代理端口 4051 执行真实 MySQL 查询，验证端到端链路：

| # | 测试项 | SQL | 预期 |
|---|--------|-----|------|
| 1 | 连接 | `SELECT 1` | 返回 `1` |
| 2 | SELECT | `SELECT COUNT(*) FROM users` | 返回 `3` |
| 3 | INSERT | `INSERT INTO users(name) VALUES(...)` | OK |
| 4 | UPDATE | `UPDATE users SET name=...` | OK |
| 5 | DELETE | `DELETE FROM users WHERE ...` | OK |
| 6 | SHOW | `SHOW DATABASES` | 列表 |
| 7 | 管理命令 | `checkproxy show status` | 状态信息 |
| 8 | 认证拒绝 | 错误密码 → 拒绝访问 | ERR 1045 |

full 模式运行 `run_e2e_battery()`：除冒烟 8 项外，还覆盖：

- **SQL 功能**：WHERE/JOIN/聚合/排序分页、多行 INSERT、UPDATE、DELETE、REPLACE、DDL 建删表、USE 切换库、SET 会话变量、SHOW/DESC、SELECT DATABASE()、事务 COMMIT/ROLLBACK、文本协议 PREPARE/EXECUTE
- **错误处理**：语法错误 1064、未知表 1146、错误密码认证拒绝 1045、未知用户拒绝
- **慢查询链路**：`SELECT SLEEP(0.5)` 触发后 `/api/slow` 出现记录
- **checkproxy 管理命令**：help / show status / show connections / show pool / show sql / show config / 未知命令 1064
- **管理 HTTP API**：未登录 401、登录会话、/healthz、/readyz、/api/status（含 shard_stages）、/api/connections、/api/sql、/api/slow（含分片过滤）、/api/recent（含库过滤）、/api/parsefailures、/api/pool、/api/config、/api/check、/metrics、慢查询/最近查询清除、kill 真实连接
- **并发**：10 连接并行 `SELECT SLEEP(0.1)`

### 代码覆盖率（cov）

依赖 `cargo-llvm-cov` 与 rustup `llvm-tools-preview`（版本须匹配 rustc 内置 LLVM）：

```bash
cargo install cargo-llvm-cov --locked
rustup component add llvm-tools-preview

./tests/integration/run.sh cov unit   # 单元/解析测试覆盖率
./tests/integration/run.sh cov e2e    # 插桩代理 + E2E 全量测试覆盖率
./tests/integration/run.sh cov        # 两者（默认）
COV_MIN_LINES=60 ./tests/integration/run.sh cov unit   # 低于 60% 行覆盖则失败
```

报告输出到:

- `cov unit`: `target/llvm-cov/html-unit/html/index.html`(仅单元/解析测试)
- `cov e2e`: `target/llvm-cov/html-e2e/html/index.html`(仅插桩代理 + E2E)
- `cov all`: `target/llvm-cov/html-all/html/index.html`(先 e2e 后追加 unit 的合并报告)

每次独立命令都会先清理 `target/llvm-cov-target/*.profraw` 建立干净基线,避免历史 profile 污染。`cov e2e` 流程：用 `cargo llvm-cov run --no-report --bin newproxy -- --help` 验证并编译当前源码的插桩二进制(构建错误不再被吞掉) → 以 `LLVM_PROFILE_FILE=.../e2e-%p-%m.profraw` 启动 → 跑 `run_e2e_battery()` → SIGTERM 优雅退出(代理已支持 SIGTERM/SIGINT,正常返回保证 profraw 落盘) → `cargo llvm-cov report` 生成 HTML。`%m` 和 `target/llvm-cov-target` 是 LLVM profile 合并的必要约定。

### 性能测试（perf）

自动启动 3 容器（MySQL + Proxy + sysbench），执行 sysbench 压测。详细说明见 [08-sysbench 性能测试](./08-sysbench-perf-test.md)。

```bash
./tests/integration/run.sh perf all          # 完整流程（prepare + baseline + proxy + 报告）
./tests/integration/run.sh perf prepare      # 仅准备数据
./tests/integration/run.sh perf baseline     # 仅直连基线
./tests/integration/run.sh perf proxy        # 仅代理测试
./tests/integration/run.sh perf run          # baseline + proxy + 报告
./tests/integration/run.sh perf cleanup      # 清理数据
TABLE_SIZE=50000 THREADS="8 16 32 64" TEST_TIME=60 \
  ./tests/integration/run.sh perf all         # 自定义参数
```

### 静态检查（clippy）

```bash
./tests/integration/run.sh clippy
```

等价于 `cargo clippy --workspace --all-targets --all-features -- -D warnings`。

## 环境说明

`env` 模式下，脚本打印连接信息后阻塞，用户可另开终端操作：

```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  测试环境已就绪 (release build)

  代理: 127.0.0.1:4051  管理: 127.0.0.1:9111
  MySQL: 127.0.0.1:3306  (root / test_password)
  数据库: test / sbtest

  登录: mysql -h 127.0.0.1 -P 4051 -u root -ptest_password test
  按 Ctrl+C 停止并清理环境
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

按 `Ctrl+C` 自动清理代理进程 + MySQL 容器。

## 配置文件

| 文件 | 用途 |
|------|------|
| `tests/integration/docker-compose.yml` | MySQL 8.0 容器（native_password，utf8mb4） |
| `tests/integration/newproxy-test.conf` | 代理集成测试配置（4 线程，128 连接） |
| `tests/integration/init.sql` | 测试表 users / orders + sbtest 数据库 |
| `tests/perf/docker-compose.yml` | sysbench 容器（host 网络） |
| `tests/perf/newproxy-perf.conf` | 代理性能测试配置（8 线程，512 连接） |

## 旧版脚本

以下脚本已被 `tests/integration/run.sh` 统一入口取代，保留可用但不推荐：

| 脚本 | 替代命令 |
|------|---------|
| `tools/itest.sh smoke` | `./tests/integration/run.sh smoke --log` |
| `tools/itest_debug.sh smoke` | `./tests/integration/run.sh smoke --debug --log` |
| `tools/test.sh` | `./tests/integration/run.sh smoke` |
| `tools/clippy_dump.sh` | `./tests/integration/run.sh clippy` |

## CI 集成

```yaml
# GitHub Actions 示例
- name: 集成测试
  run: ./tests/integration/run.sh smoke

- name: 静态检查
  run: ./tests/integration/run.sh clippy

- name: 单元测试
  run: cargo test --lib
```

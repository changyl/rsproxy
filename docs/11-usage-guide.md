# 11-编译与使用指南

> 面向部署/使用者的操作手册：如何编译、配置、启动、连接、运维 newproxy 代理。
> 产物二进制名：`newproxy`。适用版本：1.0.0（`Cargo.toml`）。

## 1. 项目简介

newproxy 是一个 MySQL 分库分表代理的 **Rust 原生实现**（对齐 C 版 newproxy 行为），基于 tokio 异步运行时：

- 一个客户端连接 = 一个 tokio task，天然支持高并发；
- 前端协议（与客户端）与后端协议（与 MySQL）均为 MySQL 原生协议；
- 支持集群 / 分片（tablet）/ 主从 / 连接池 / 产品用户映射 / IP 黑白名单；
- 内置运维命令 `checkproxy`（见 [10-管理命令](./10-management-commands.md)）。

架构与设计细节见 [docs/README.md](./README.md) 的文档索引。

## 2. 环境要求

| 依赖 | 版本 | 用途 |
|------|------|------|
| Rust 工具链（rustc + cargo） | stable（≥1.70 即可；开发环境 1.96） | 编译。安装：[rustup.rs](https://rustup.rs) |
| mysql 客户端 | 8.x / 5.7 | 连接代理做功能验证（`mysql -h ...`） |
| Docker + Colima / docker desktop | 任意 | 可选，本地起 MySQL 后端跑集成测试 |
| 磁盘/内存 | ≥2 GB 空闲 | 首次全量编译（LTO）较吃资源 |

```bash
# 检查工具链
rustc --version && cargo --version
```

## 3. 目录结构

```
rust_proxy/
├── build.sh               # 编译脚本（本手册第 4 节）
├── Cargo.toml             # 工程定义（包名 newproxy，binary newproxy）
├── conf/
│   ├── newproxy.conf       # 生产示例配置
│   └── newproxy-test.conf  # 测试配置（对接 docker 里的 MySQL）
├── src/                   # 源码（config/conn/pool/parser/proto/mgmt/metric...）
├── tests/                 # 单元/集成/性能测试
├── tools/                 # 辅助脚本（test.sh、init.sql 等）
├── docs/                  # 设计文档与使用手册（本文件）
└── docker-compose.yml     # 本地测试用 MySQL 8.0
```

## 4. 编译

仓库根目录的 [`build.sh`](../build.sh) 是统一编译入口（等价历史脚本 `tools/build.sh` 亦可用）。

### 4.1 快速开始

```bash
cd rust_proxy

# 默认：release 编译（生产二进制）
./build.sh

# 等价写法
./build.sh release
```

产物：`target/release/newproxy`。

### 4.2 命令一览

| 命令 | 说明 | 产物 |
|------|------|------|
| `./build.sh release`（默认） | release 编译（LTO + strip） | `target/release/newproxy` |
| `./build.sh debug` | debug 编译（快，适合调试） | `target/debug/newproxy` |
| `./build.sh check` | 仅类型检查，不产生二进制 | 无 |
| `./build.sh test` | release 编译 + `cargo test --all-targets` | `target/release/newproxy` |
| `./build.sh clippy` | 静态检查（`-D warnings`） | 无 |
| `./build.sh ci` | fmt + clippy + build + test，等同 CI | `target/release/newproxy` |
| `./build.sh clean` | `cargo clean` 清理 target | 无 |
| `./build.sh install` | release 编译并安装到 `PREFIX/bin`（默认 `/usr/local/bin`） | `/usr/local/bin/newproxy` |
| `./build.sh help` | 打印帮助 | 无 |

### 4.3 服务管理(start/stop/restart/status)

`build.sh` 内置服务管理(nohup 后台 + PID 文件,路径可用环境变量覆盖):

```bash
./build.sh start       # 启动(二进制缺失时先编译 release;端口预检防重复实例)
./build.sh status      # 查看 pid/端口/配置/二进制
./build.sh restart     # 停止后重新启动
./build.sh stop        # SIGTERM,10s 未退出强杀
```

**容器模式**(测试容器 `tests/perf`,Dockerfile 以 build.sh 为入口,自动检测 `/.dockerenv`):

```bash
docker compose -f tests/perf/docker-compose.yml up -d --build   # 启动(入口 build.sh start,监督 newproxy)
docker exec newproxy-perf-newproxy build.sh status
docker exec newproxy-perf-newproxy build.sh restart              # 换新实例,容器保持运行
docker exec newproxy-perf-newproxy build.sh stop                 # 停止,容器干净退出(restart: unless-stopped 自动恢复)
```

容器入口为监督循环:newproxy 存活则容器运行;stop 清除 pidfile → 容器 exit 0;
进程异常退出(pidfile 残留)→ 容器 exit 1,均由 compose 的 `restart: unless-stopped` 恢复。
强制容器模式可用 `NEWPROXY_CONTAINER=1`(本机模拟容器路径)。

### 4.4 环境变量

| 变量 | 作用 |
|------|------|
| `CARGO_ARGS` | 追加到所有 cargo 构建命令的参数，如 `CARGO_ARGS="--features xxx" ./build.sh` |
| `PREFIX` | `install` 模式的安装前缀，默认 `/usr/local` |

### 4.5 release 优化项（`Cargo.toml`）

```toml
[profile.release]
opt-level = 3
lto = "fat"        # 全程序链接优化，首次编译较慢，之后增量很快
codegen-units = 4
strip = true       # 剥离符号，产物更小
```

## 5. 命令行参数

```bash
newproxy [OPTIONS] -c <CONFIG>
newproxy [OPTIONS] <CONFIG>
```

| 参数 | 说明 |
|------|------|
| `<CONFIG>` | 配置文件路径（INI 格式），必填。位置参数与 `-c` 二选一，不可同时给 |
| `-c, --config <PATH>` | 显式指定配置文件路径 |
| `-h, --help` | 打印帮助并退出 |
| `-V, --version` | 打印版本号并退出 |

```bash
# 查看帮助/版本
./target/release/newproxy -h
./target/release/newproxy -V

# 启动（两种写法等价）
./target/release/newproxy -c conf/newproxy.conf
./target/release/newproxy conf/newproxy.conf
```

> 注意：配置文件不存在时进程以退出码 2 直接报错，不会静默使用内置默认值。

## 6. 配置文件详解

格式：GKeyFile 风格 INI——`[section] key=value`，`#` 注释。完整示例见
[`conf/newproxy.conf`](../conf/newproxy.conf)，本节按区块说明全部可配置项。

### 6.1 `[MySQL_Proxy_Layer]` — 代理层全局参数

| Key | 默认值 | 说明 |
|-----|--------|------|
| `port` | 4051 | 业务监听端口（1–65535，不可为 0） |
| `mng_port` | 9111 | 管理端口（兼容字段；管理命令走业务端口的 `checkproxy`） |
| `max_threads` | 4 | tokio worker 线程数（1–128） |
| `log_dir` | logs | 日志目录（滚动文件日志输出到此处，见 §7.3） |
| `log_level` | info | 文件日志级别：`debug` / `info` / `warn` / `error` |
| `client_timeout` | 28800 | 客户端空闲超时（秒） |
| `server_timeout` | 28800 | 后端空闲超时（秒） |
| `conn_pool_socket_max_serve_client_times` | 10000 | 连接池内一条后端连接最多服务请求次数 |
| `max_sql_size` | 16777216 | SQL 报文大小上限（字节） |
| `max_query_size` | 16777216 | 查询大小上限（字节） |
| `default_charset` | 33 | 默认字符集编号（33 = utf8） |
| `stream_transport_enable` | 0 | 流式传输开关（0/1） |
| `reload_interval` | 5 | 配置文件变更自动热加载的轮询间隔（秒）；`0` 关闭自动热加载 |

### 6.2 集群与分片

```ini
# 集群：ID 取自 section 名中的序号（Cluster_0 → id "0"）
[Cluster_0]
name=test_cluster

# 分片（tablet）：CTablet_<cluster_id>_<tablet_name>
[CTablet_0_t0]
name=t0
rrule=user.id,hash_mod,0,1,2,3    # 可选：路由规则，见 §6.6
```

### 6.3 后端主机（主/从）

`Master_Host_<group_id>` / `Slave_Host_<group_id>`，同一 group_id 的一组 M/S 归入同一分片：

| Key | 默认值 | 说明 |
|-----|--------|------|
| `host` | —（必填） | IPv4 地址或主机名（Docker 服务名 / 域名均可） |
| `port` | —（必填） | 后端 MySQL 端口 |
| `max_conn_pool_size` | 16 | 连接池容量 |
| `max_connections` | 256 | 该后端最大连接数 |
| `connect_timeout` | 5 | 建连超时（秒） |
| `weight` | 1 | 负载均衡权重 |
| `cluster_tablet_name` | — | 归属的分片名（如 `t0`） |

```ini
[Master_Host_g0]
host=127.0.0.1
port=3306
max_conn_pool_size=16
max_connections=256
connect_timeout=5
weight=1
cluster_tablet_name=t0
```

### 6.4 用户

**数据库用户**（代理 → 后端 MySQL 的认证身份）：

```ini
[DB_User_dbu]
db_username=root
db_password=secret
default_db=test
cluster_name=test_cluster
```

**产品用户**（客户端 → 代理的认证身份，映射到数据库用户）：

```ini
[Product_User_pu]
username=app_user      # 客户端使用的用户名
password=app_pass      # 客户端使用的密码
db_username=root       # 代理以该数据库用户身份连后端
max_connections=128    # 该产品用户并发上限（默认 256）
cluster_name=test_cluster
```

### 6.5 IP 黑白名单

```ini
[Auth_IP_0]
ip=10.0.0.0
mask=8            # 子网掩码（默认 32）
users=app_user    # 允许的用户，逗号分隔；空 = 允许所有人

[Ignore_IP_0]     # 与 Auth_IP 结构相同，表示忽略（放行）
ip=127.0.0.1
mask=32
```

### 6.6 路由规则 `rrule`（可选）

格式：`表名.分片键,策略,分片索引...`，多个规则用逗号分隔。

```ini
[CTablet_0_t0]
name=t0
rrule=user.id,hash_mod,0,1,2,3
```

| 策略 | 说明 |
|------|------|
| `hash_mod` | 对分片键哈希后取模 |
| `md5_hash_mod` | MD5 哈希取模 |
| `range` | 范围分片 |
| `list` | 枚举值分片 |
| `pcre` | PCRE 正则匹配（第 3 段为正则表达式） |

> 注意：当前解析要求规则至少 3 段（表名.键,策略,索引…），格式不符会被忽略。

## 7. 启动与验证

### 7.1 启动

```bash
# 方式一：显式 -c（不设 RUST_LOG 时终端/文件默认 info）
./target/release/newproxy -c conf/newproxy.conf

# 方式二：位置参数
RUST_LOG=info ./target/release/newproxy conf/newproxy-test.conf
```

启动成功会出现：

```
listening on 0.0.0.0:4051
```

### 7.2 验证连通

```bash
# 用测试配置（对接 docker MySQL，root/test_password）
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password -e "SELECT 1" test

# 查看运行状态 / 当前配置
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password -e "checkproxy show status" test
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password -e "checkproxy show config" test
```

### 7.3 日志

日志由 `tracing` 双通道输出，**无需设置任何环境变量即有日志**：

| 通道 | 位置 | 说明 |
|------|------|------|
| 文件 | `<log_dir>/newproxy.YYYY-MM-DD.log` | 每日滚动，最多保留 30 个文件；级别跟随配置 `log_level`（默认 info） |
| 终端 | stderr | 级别默认 info |

**级别规则**：`RUST_LOG` 显式设置（非空）时两个通道都遵循 `RUST_LOG`；
未设置时终端默认 `info`、文件遵循配置 `log_level`。

```bash
# 未设置 RUST_LOG：终端与文件都按配置级别输出（默认 info）
./target/release/newproxy -c conf/newproxy.conf

# 显式指定：双通道都按 RUST_LOG（debug 最详细）
RUST_LOG=debug ./target/release/newproxy -c conf/newproxy.conf

# 排查时跟踪文件日志
tail -f logs/newproxy.*.log
```

> 注意：二进制 crate 名为 `newproxy`，库 crate 名为 `newproxy`——`RUST_LOG=newproxy=debug`
> 只显示库（协议/连接）日志，**不显示** `main` 的启动日志（目标为 `newproxy`）；
> 想看全部请用 `RUST_LOG=debug` 或 `RUST_LOG=info`。

**日志内容（方便排查）**：

- 连接生命周期：`new connection from <ip>` → `handshake sent cid=N` → `auth success user=...` → `connection closed cid=N`；认证失败/IP 拒绝以 `WARN` 记录
- 每条查询：`DEBUG query start/done cid=N user=... elapsed_ms=... interval_us=... parse_us=... setup_ms=... send_ms=... forward_ms=... sql=...`
  — **完整阶段耗时分解**（慢查询超阈值以 `WARN slow query` 输出同样字段，定位慢在哪个环节）：
    - `interval_us`：命令间隔（距上一条命令的等待，**含客户端空闲**——非代理耗时；数据到达后的实际读包为 µs 级，与等待无法分离故归此）
    - `parse_us`：命令解析 + 本地拦截检测（help/管理命令/单次小写化）
    - `setup_ms`：后端准备（池获取连接 / 新建连接+握手）
    - `send_ms`：发送命令到后端
    - `forward_ms`：后端执行 + 响应转发（**通常占大头**；大 = 后端 SQL 慢/锁等待/大结果集）
- 慢查询阈值：`[MySQL_Proxy_Layer] slow_query_ms`（默认 **200ms**），超过即计入
  `WARN slow query` / `/api/slow` / 面板慢查询表；修改配置后 `checkproxy reload`
  或面板 RELOAD 按钮即时生效
- 管理命令审计：`INFO management command ... cmd=checkproxy show status`
- 后端连接池：`reused backend connection from pool` / `connecting to backend (pool miss)`
- 以上事件均带 `cid`（连接 ID）与 `addr`（客户端地址），可串起一条连接的完整轨迹

日志写入为异步非阻塞（worker 线程），进程被 `kill -9` 时可能丢失最近少量日志；正常退出（`kill`/Ctrl+C）会冲刷。

## 8. 客户端接入

客户端连接参数与直连 MySQL 一致，只是端口指向代理：

| 参数 | 值 |
|------|-----|
| 主机 | 代理所在主机（如 `127.0.0.1`） |
| 端口 | 代理 `port`（默认 4051） |
| 用户名/密码 | **产品用户**（`[Product_User_*]`），非后端数据库用户 |
| 数据库 | 产品用户映射的 `db_user.default_db` |

```bash
# 产品用户 app_user / app_pass（对应 conf/newproxy.conf）
mysql -h 127.0.0.1 -P 4051 -u app_user -papp_pass -e "SELECT * FROM users" test

# 测试环境产品用户 root / test_password（对应 conf/newproxy-test.conf）
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password test
```

认证失败（密码错误）返回 MySQL 错误 1045 并拒绝连接。

## 9. 管理命令（checkproxy）

以 `checkproxy` 前缀的 SQL 形式通过业务端口执行，无需额外管理连接：

```bash
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password \
  -e "checkproxy show status" test
```

| 命令 | 说明 | 状态 |
|------|------|------|
| `checkproxy show status` / `stats` | 服务概况（连接/查询/流量/连接池） | ✓ 已实现 |
| `checkproxy show config` / `config` | 当前完整配置 | ✓ 已实现 |
| `checkproxy show sql` / `show query` | Top 10 高频 SQL 统计 | ✓ 已实现 |
| `checkproxy show connections` / `processlist` | 连接列表 | ✗ 占位 |
| `checkproxy show pool` | 连接池状态 | ✗ 占位 |
| `checkproxy kill <id>` | 断开连接 | ✗ 占位 |
| `checkproxy reload` | 热加载配置（命令/SIGHUP/文件自动监听） | ✓ 已实现 |

完整说明见 [10-管理命令](./10-management-commands.md)。

### 9.1 热加载（主从切换/拓扑变更无需重启）

修改配置文件后有三种方式让新拓扑生效：

```bash
# 方式一：管理命令（通过业务端口）
mysql -h 127.0.0.1 -P 4051 -u <用户> -p<密码> -e "checkproxy reload"

# 方式二：SIGHUP 信号
kill -HUP <代理pid>

# 方式三：自动监听（默认开启，每 5s 检查一次文件 mtime；reload_interval=0 关闭）
# 直接编辑配置文件，无需任何操作，5s 内自动生效
```

生效语义：

- **立即生效**：新连接与新建后端连接使用新拓扑（`ArcSwap` 原子替换，读侧无锁）
- **连接池自动清空**：指向旧主库的空闲连接作废，不会被复用
- **在途会话不打断**：已有连接保持其当前后端直至结束
- **容错**：配置解析失败（如写了一半）时保持旧配置运行，返回 `RELOAD failed: ...`，不中断服务
- 日志参数（`log_level`/`log_dir` 等）属一次性初始化项，变更后需重启

典型主从切换流程：

```bash
# 1. 运维把新主库写入 conf 的 [Master_Host_*]（或切换 host/port）
# 2. 代理自动热加载（或手动 checkproxy reload / kill -HUP）
# 3. 新连接即路由到新主库；旧连接自然耗尽
```

### 9.2 配置中心（etcd / ZooKeeper）——大规模分片下的拓扑热更新

文件热加载是「整文件全量替换 + 连接池全清」，**上千分片时全量失效代价线性放大**。
接入配置中心后，拓扑变更按**分片粒度**毫秒级增量生效，其他分片零扰动。

```ini
# conf 中配置(可选,默认 none)
[ConfigCenter]
type=etcd                  # none | etcd | zookeeper
endpoints=http://127.0.0.1:2379
root=/newproxy
```

数据布局与格式（etcd key / zk 节点路径）：

```
{root}/clusters/{cluster_id}/tablets/{tablet_id}
  值 = JSON: {"cluster_id":"0","tablet_id":"t0",
              "master":{"host":"10.0.0.1","port":3306},
              "slaves":[{"host":"10.0.0.2","port":3306}]}
```

效果与语义：

- **全量打底**：启动时从配置中心拉取全量分片拓扑作为运行时基线（文件配置仍为静态基线）
- **增量订阅**：etcd watch / zk children+data watch，单分片变更 → 只更新该分片
- **按分片失效连接池**：只清变更分片的桶（`invalidate_shard`），其他分片连接保留，无重建风暴
- **断线容错**：配置中心不可达时代理用最后已知拓扑继续服务，3s 后自动重连重建基线
- **优先级**：运行时拓扑覆盖层优先于文件配置（`ensure_backend` 先查覆盖层）

```bash
# etcd 示例:写入/切换分片主库
etcdctl put /newproxy/clusters/0/tablets/t0 \
  '{"cluster_id":"0","tablet_id":"t0","master":{"host":"10.0.0.2","port":3306},"slaves":[]}'
```

抽象接口 `config_center::TopologyStore`（fetch_all / subscribe / upsert_shard / delete_shard）
已实现 etcd（`etcd.rs`）与 ZooKeeper（`zk.rs`）两个后端，新增配置中心实现只需实现该 trait。

## 10. 本地测试环境

```bash
# 1. 启动本地 MySQL 8.0（root/test_password，库 test，自动执行 tools/init.sql）
colima start          # 或确保 docker 可用
docker compose up -d

# 2. 编译
./build.sh

# 3. 冒烟测试（统一入口，自动起代理并验证 8 项功能）
./tests/integration/run.sh smoke

# 4. 手动体验：起代理后另开终端连库
./target/release/newproxy conf/newproxy-test.conf &
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password test

# 5. 结束清理
kill %1
docker compose down
```

> 冒烟测试后代理日志可在 `logs/newproxy.*.log` 查看（`./tests/integration/run.sh logs proxy` 可跟踪）。

测试框架更多用法（full / env / perf / clippy）见 [09-测试框架](./09-test-framework.md)，
性能压测见 [08-sysbench 性能测试](./08-sysbench-perf-test.md)。

## 11. 常见问题（FAQ）

| 现象 | 原因与处理 |
|------|-----------|
| `error: 配置文件不存在: xxx`（退出码 2） | 配置文件路径不对或漏传 `-c`；用 `newproxy -h` 查看用法 |
| 启动即报 `Address already in use` | 端口被占用；改 `[MySQL_Proxy_Layer] port` 或释放端口 |
| 客户端报 1045 认证失败 | 使用的是产品用户口令不匹配；检查 `[Product_User_*]` 的 `username/password` |
| 查询报后端连接失败/超时 | 后端 MySQL 未启动或 `host/port` 配置错误；`connect_timeout` 控制超时 |
| 启动后找不到日志 | 文件日志在 `<log_dir>/newproxy.*.log`（默认 `logs/`）；想看终端输出直接看启动进程的 stderr（默认 info）。`kill -9` 会丢最近少量异步日志 |
| 设置了 `RUST_LOG=newproxy=debug` 却没有启动日志 | 二进制 crate 目标名是 `newproxy` 而非 `newproxy`；用 `RUST_LOG=debug`（全部）或 `RUST_LOG=newproxy=debug,newproxy=debug` |
| 首次编译很慢 | release 全量 LTO 编译属正常现象；之后增量编译很快 |
| `checkproxy reload` 不生效 | 确认返回的是 `RELOAD: OK`；解析失败会保持旧配置并返回 `RELOAD failed: ...`，检查配置语法 |
| 部署机报 glibc 版本错误 | Rust 二进制依赖编译机 glibc；保持编译机与部署机 glibc 一致，或用 musl 静态目标重新编译 |

## 12. 相关文档

| 文档 | 内容 |
|------|------|
| [docs/README.md](./README.md) | 文档总索引与设计基线 |
| [10-管理命令](./10-management-commands.md) | checkproxy 运维命令详解 |
| [09-测试框架](./09-test-framework.md) | 测试统一入口 `tests/integration/run.sh` |
| [08-sysbench 性能测试](./08-sysbench-perf-test.md) | sysbench 压测套件 |
| [04-MySQL 协议状态机](./04-mysql-protocol-statemachine.md) | 协议实现细节 |
| [06-任务计划](./06-task-plan.md) / [07-任务卡详细版](./07-task-cards-detail.md) | 实现状态与剩余任务 |

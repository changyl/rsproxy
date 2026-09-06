# 10-管理命令

NewProxy 代理通过业务端口（默认 4051）以 `checkproxy` 前缀 SQL 格式接入运维命令，
无需额外管理端口即可执行状态查看、连接管理、配置重载等操作。

## 使用方式

```bash
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password -e "checkproxy <子命令>"
```

所有管理命令均以 `checkproxy` 开头，不区分大小写。

## 命令列表

### show status — 服务概况

```sql
checkproxy show status
checkproxy stats          -- 别名
```

返回代理运行状态摘要：

```
Uptime: N/A
Connections: total=12 active=3 rejected=0
Queries: total=15420 errors=0 slow=0
Traffic: recv=2048576 sent=4097152
Pool: acquires=15420 fails=0
```

| 字段 | 含义 |
|------|------|
| `Uptime` | 运行时长（尚未实现） |
| `Connections total` | 累计客户端连接数 |
| `Connections active` | 当前活跃连接数 |
| `Connections rejected` | 被 IP 检查拒绝的连接数 |
| `Queries total` | 累计处理查询数 |
| `Queries errors` | 累计错误数 |
| `Queries slow` | 慢查询数 |
| `Traffic recv/sent` | 累计接收/发送字节数 |
| `Pool acquires` | 后端连接池获取次数 |
| `Pool fails` | 后端连接池获取失败次数 |

### show connections — 连接列表

```sql
checkproxy show connections
checkproxy show processlist    -- 别名（兼容 MySQL 语法）
```

列出当前所有客户端连接详情（**尚未实现，返回占位信息**）：

```
Connections: (not yet tracked per-connection)
```

### show pool — 连接池状态

```sql
checkproxy show pool
```

查看后端连接池的容量、空闲、活跃等指标（**尚未实现，返回占位信息**）：

```
Pool: (not yet populated)
```

### show sql — SQL 统计

```sql
checkproxy show sql
checkproxy show query           -- 别名
```

返回 Top 10 高频 SQL 的执行统计：

```
Top Queries:
1. SELECT * FROM users WHERE id = ? (count=8230, avg=120us, max=3500us)
2. INSERT INTO orders (...) VALUES (...) (count=4120, avg=85us, max=2100us)
...
```

| 字段 | 含义 |
|------|------|
| `count` | 累计执行次数 |
| `avg` | 平均耗时（微秒） |
| `max` | 最大耗时（微秒） |

### show config — 当前配置

```sql
checkproxy show config
checkproxy config              -- 别名
```

返回当前加载的完整代理配置：

```
[MySQL_Proxy_Layer]
port            = 4051
mng_port        = 9111
max_threads     = 4
log_dir         = logs
log_level       = Debug
client_timeout  = 28800s
server_timeout  = 28800s
max_serve_times = 10000
max_sql_size    = 16777216
max_query_size  = 16777216
default_charset = 33
stream_on       = 0

Clients: 1 total
  user=root -> db_user=root cluster=test_cluster max_conn=128

Backends: 1 total
  db_user=root default_db='test' cluster=test_cluster

Clusters: 1
  [0] name=test_cluster tablets=1
    tablet=t0 masters=1 slaves=0
      master 127.0.0.1:3306 pool=16 conn=256 weight=1

Auth IPs: 0 rules
Ignore IPs: 0 rules
```

| 区块 | 内容 |
|------|------|
| `[MySQL_Proxy_Layer]` | 代理层全局参数 |
| `Clients` | 产品用户列表、映射的数据库用户、连接上限 |
| `Backends` | 数据库用户、默认库、所属集群 |
| `Clusters` | 集群拓扑：分片 → 主库 ip:port/连接池/权重 |

### kill — 断开连接

```sql
checkproxy kill <connection_id>
checkproxy kill connection <connection_id>
```

强制断开指定客户端连接（**尚未实现，返回占位信息**）：

```
KILL CONNECTION 42: not implemented
```

### reload — 热加载配置

```sql
checkproxy reload
```

运行时重新解析配置文件并原子替换，**无需重启代理**。适用于主从切换、后端拓扑变更、用户/黑白名单调整等：

- 新连接与新建后端连接立即使用新配置（连接任务每操作都重新读配置，`ArcSwap` 无锁替换）
- 连接池自动清空，避免复用指向旧拓扑的空闲连接
- 已有会话保持其当前后端连接直至结束，不打断在途事务
- 配置解析失败（如文件写了一半）时保持旧配置运行，仅报错，不中断服务

返回拓扑变更摘要（`master/slave host:port` 的 before → after），无变化时返回统计计数：

```
RELOAD: OK
topology changed:
  cluster 0 tablet t0 master: 10.0.0.1:3306 -> 10.0.0.2:3306
```

**除 `checkproxy reload` 外还有两种触发方式**（等价）：

| 方式 | 操作 | 说明 |
|------|------|------|
| 管理命令 | `checkproxy reload` | 通过业务端口触发 |
| SIGHUP | `kill -HUP <pid>` | Unix 经典运维方式 |
| 自动监听 | 修改配置文件 | 默认每 5s 检查 mtime，变更即热加载；`reload_interval=0` 关闭 |

## 实现状态

| 命令 | 状态 | 说明 |
|------|------|------|
| `show status` | ✓ 已实现 | 基于 `Metrics` 实时统计 |
| `show config` | ✓ 已实现 | 完整配置：代理参数/客户端/后端/集群拓扑 |
| `show connections` | ✗ 桩代码 | 待实现 per-connection 追踪 |
| `show pool` | ✗ 桩代码 | 待接入 `SrvPool` 状态查询 |
| `show sql` | ✓ 已实现 | Top 10 by count，含 avg/max 耗时 |
| `kill` | ✗ 桩代码 | 待实现连接 ID 到 task handle 的映射 |
| `reload` | ✓ 已实现 | `ArcSwap` 原子替换 + 连接池清空；支持命令/SIGHUP/文件自动监听三方式（见上） |

## 错误处理

- 输入非 `checkproxy` 前缀的 SQL → 正常路由到后端处理
- 输入未识别的子命令 → 返回 MySQL 错误 `1064 (42000): Unknown checkproxy command`
- `kill <id>` 中的 id 非数字 → 默认为 0

## 实现架构

管理命令在 `src/mgmt.rs` 中实现，入口在 `src/conn/front.rs` 的命令循环中：

```
客户端 SQL → parse_command_packet → 匹配 "checkproxy" 前缀
                                          │
                          ┌───────────────┴───────────────┐
                          │ MgmtCommand::parse(sql)       │
                          │ 返回 None → 走正常路由        │
                          │ 返回 Some → execute()         │
                          └───────────────────────────────┘
```

- `MgmtCommand::parse()` — 解析 SQL 文本，不区分大小写
- `MgmtCommand::execute()` — 调用 `Metrics` 统计接口，构建 OK/ERR 响应包
- 响应通过 `codec::send_packet` 原路返回客户端

## 示例

```bash
# 服务概况
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password -e "checkproxy show status"

# 当前配置
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password -e "checkproxy config"

# SQL 统计
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password -e "checkproxy show sql"

# 连接管理（预留）
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password -e "checkproxy kill 42"

# 热加载（预留）
mysql -h 127.0.0.1 -P 4051 -u root -ptest_password -e "checkproxy reload"
# 等价触发:kill -HUP <代理pid>;或直接修改配置文件(默认 5s 内自动生效)
```

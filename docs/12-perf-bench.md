# 12 内置性能基准与瓶颈分析

## 背景

`docs/08-sysbench-perf-test.md` 的 docker+sysbench 流程依赖真实 MySQL,
且历史报告(`tests/perf/sysbench_report.md`)为空——从未产出过有效数据。
本文件对应**进程内基准**(`tests/perf_proxy.rs`):mock backend + in-process proxy +
虚拟客户端,不依赖 Docker/MySQL,可重复、可 CI。

目标:把「代理层开销」从「MySQL 服务端性能」中隔离出来,回答:
1. 代理相对直连慢多少;
2. 瓶颈在前端解析、转发,还是协议/网络细节。

## 运行

```bash
./tools/perf.sh bench        # 默认:3s/场景,并发 1 4 16 64 → tests/perf/bench_report.md
PERF_SECS=5 PERF_CONNS="1 16" ./tools/perf.sh bench
./tools/perf.sh long         # 60s 长跑(配合 macOS `sample`/Instruments 采样)
```

必须 `--release`(脚本已处理);机器负载高时数值噪声大,对比应看**同轮次内**
直连 vs 代理的相对差距。

## 架构

```
[virtual client]×N ──TCP──▶ [newproxy proxy] ──TCP──▶ [mock backend]
```

- mock backend:预构建 wire 级响应字节流(含 4B header),单次 write 回包,零思考速度;
  实现 greeting / auth / COM_QUERY(1 行与 100 行结果集)/ PREPARE / EXECUTE / INIT_DB。
- 虚拟客户端:完整 mysql_native_password 握手(复用 `scramble_native`)+ 命令循环,
  逐操作记录延迟。
- 场景:ping(代理内联,无后端)、select1(5 包透传)、rows100(104 包透传)、
  prepare+execute、连接+认证;每场景直连与代理各跑一轮。
- 统计:QPS / avg / p50 / p95 / p99(µs)/ 错误数。

## 已确认的瓶颈与修复(2026-08)

### 瓶颈 1:前端 socket 未禁用 Nagle + 每包两次小写入 — 已修复

`send_packet` 原实现把 header 与 payload 分两次 `write_all`(每包 2 个 write syscall),
且前端 socket 从未 `set_nodelay`(后端有,前端没有)。ping-pong 流量下 Nagle 会滞留
第二次小写入直到首个写入被 ACK,每次往返额外等一个 ACK 周期。

修复:
- `conn_task` 入口对前端 socket `set_nodelay(true)`(`src/conn/front.rs`);
- `codec::send_packet` 改为 header+payload 一次 `write_vectored`(`src/proto/codec.rs`)。

效果(ping,纯前端路径,单连接):

| 指标 | 修复前 | 修复后 |
|------|-------|-------|
| QPS | 5416 | 7634~9384 |
| p50 | 159µs | 57~107µs |

修复后 ping 与直连 select1 同量级(单连接 p50 ≈ 直连),前端路径开销基本消除。

### 瓶颈 2:转发路径每包无条件构建 hex 预览串 — 已修复

`forward_backend_response` 每收到一个后端包就做
`payload.iter().take(10).map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")`
(~10 次 `format!` + 一次 join 分配),**即使 TRACE 日志未开启**。已改为
`tracing::level_enabled!(TRACE)` 时才构建(`src/proto/result.rs`)。

### 瓶颈 3:每个 COM_QUERY 两次小写化分配 + DbUser 克隆 — 已修复

`is_help_query` 与 `MgmtCommand::parse` 各自对 SQL 做 `trim().to_lowercase()`(两次
堆分配),已合并为一次小写化并新增 `MgmtCommand::parse_normalized`(`src/mgmt.rs`、
`src/conn/front.rs`)。同时 `handle_query` 等每查询 `cfg.db_users.get(..).cloned()`
(DbUser 含 4 个 String,每查询 4 次分配)改为直接借用 guard 内的引用。

## 实测数据(修复后,机器负载 ~30,PERF_SECS=5)

| 场景 | 路径 | 并发 | QPS | avg(µs) | p50(µs) | p95(µs) | p99(µs) |
|------|------|------|------:|--------:|--------:|--------:|--------:|
| select1(5包) | 直连 | 1 | 4927 | 203 | 177 | 332 | 563 |
| select1(5包) | 直连 | 16 | 18189 | 880 | 791 | 1678 | 2295 |
| rows100(104包) | 直连 | 16 | 4670 | 3426 | 2480 | 7465 | 25711 |
| ping | 直连 | 16 | 18553 | 862 | 787 | 1582 | 2026 |
| ping | 代理 | 1 | 5178 | 193 | 175 | 301 | 483 |
| ping | 代理 | 16 | 15631 | 1023 | 1002 | 1408 | 2016 |
| select1(5包) | 代理 | 1 | 1690 | 592 | 579 | 754 | 957 |
| select1(5包) | 代理 | 16 | 6412 | 2496 | 1682 | 4408 | 5863 |
| rows100(104包) | 代理 | 1 | 326 | 3071 | 1950 | 6940 | 10597 |
| rows100(104包) | 代理 | 16 | 309 | 51966 | 47387 | 73945 | 212680 |
| prepare+execute | 代理 | 1 | 1696 | 589 | 573 | 763 | 1055 |
| prepare+execute | 代理 | 16 | 4037 | 3964 | 3765 | 5301 | 9100 |
| 连接+认证 | 直连 | 8 | 2821 | 2777 | 2516 | 4227 | 10249 |
| 连接+认证 | 代理 | 8 | 398* | 4489 | 2978 | 4694 | 18316 |

\* 连接场景代理路径 5s 内出现 5322 次客户端可见错误(直连 0 错误),见「遗留问题 2」。

## 结论(确认的瓶颈)

1. **前端路径已不再是瓶颈**:代理 ping ≈ 直连 select1(单连接 p50 175µs vs 177µs)。
2. **剩余主要瓶颈是「每包转发成本」**:rows100(104 包)@16 并发,代理 p50=47ms
   而直连 p50=2.5ms——每包 ~450µs vs ~24µs;单连接时代理每包 ~19µs 与直连相当,
   高并发下每包成本膨胀 ~25×。每包 = 一次后端 `read_packet` + 一次前端
   `send_packet`(各自一次 syscall + 分配),并发增大时调度/唤醒开销叠加。
3. **并发扩展性差**:select1 QPS 1.7K@1 → 6.4K@16(3.8×,线性应为 16×);
   rows100 326@1 → 309@16(**不增长**)。直连同样有扩展瓶颈(共享 runtime +
   Rosetta 环境),但代理的绝对吞吐比直连低约 3~15×。
4. 直连基线本身也随并发恶化(ping 直连 @16 p50 787µs),说明本机(共享负载、
   Rosetta x86 模拟)整体调度开销偏高,绝对数值仅作参考;**结构对比(同轮次
   直连 vs 代理)是可信的**。

## 连接池与 prepared statement(2026-08 追加)

会话内使用过 prepared statement 的后端连接**不再回池**:

- `COM_STMT_PREPARE` 成功(`forward_stmt_prepare_response` 返回 `true`)
  或文本协议 `PREPARE stmt FROM ...`(`is_text_prepare` 无分配前缀检测)时,
  `BackConn::mark_no_reuse()` 置位;
- `AttrBucket::release` / `acquire` 检查该标志:归还时直接丢弃连接
  (底层 TCP 关闭,服务端孤儿语句随之释放),避免下个会话继承残留
  prepared statement,累积撞上 MySQL `max_prepared_stmt_count` 上限;
- 池单元测试:`no_reuse_connection_discarded_on_release` /
  `normal_connection_reuses_after_release`。

## 遗留问题(后续优化方向)

1. **批量写出**:`forward_backend_response` 逐包 `send_packet`(每包一次 writev)。
   小结果集可先缓冲 N 包/字节预算(如 64KB)再一次写出,减少 5~100× syscall;
   需保持大结果集的内存上界(流式)。
2. **连接建立场景的"数千错误"已定位为测试工具问题,非代理缺陷(已修复)**:错误分类诊断显示
   全部为 `EADDRNOTAVAIL (os error 49)`——高频 connect/断开 churn 把 macOS
   仅 16384 个临时端口(49152–65535)耗尽:TIME_WAIT 60s 内不可复用,连接越快
   越早耗尽(故"负载低反而更糟",直连同样中招)。修复:压测客户端 `set_linger(0)`
   drop 时 RST 关闭跳过 TIME_WAIT。修复后:代理 connect@8 = **8527 QPS / 0 错误**
   (直连 9961 QPS),p50 887µs——连接建立路径健康,非瓶颈。
   附带修复:基准内 accept 循环偶发错误应 `continue` 而非 `break`(break 会让
   监听静默停止接受,曾复现 connect QPS 掉到个位数;生产 `main.rs` 一直是 continue)。
3. **真实 MySQL 基线**:本基准隔离的是代理层;端到端绝对吞吐仍建议用
   `tests/perf` 的 sysbench 流程(docker)补测,两块互补。

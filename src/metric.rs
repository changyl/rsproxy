// Metric 采集+上报
// T4.2 实现:lock-free 原子计数器 + DashMap 聚合
//
// 对齐 C 侧 tr_metric.c (硬编码 127.0.0.1:788) + tr_sql.c (滚动计数器)

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::Mutex;
use serde::Serialize;

/// SQL 模板统计表的最大条目数(超出后不再登记新模板,防止内存无限增长)
const MAX_SQL_TEMPLATES: usize = 1024;

/// 慢查询环形缓冲最大条数(超出丢弃最旧,内存有界)
const MAX_SLOW_QUERIES: usize = 200;

/// 解析失败记录环形缓冲最大条数(有界)
const MAX_PARSE_FAILURES: usize = 200;

/// 后端错误记录环形缓冲最大条数(有界)
const MAX_BACKEND_ERRORS: usize = 200;

/// 查询耗时直方图桶数(9 个有限上界 + 1 个 +Inf 兜底桶)
const LATENCY_BUCKETS: usize = 10;

/// 各桶上界(µs):桶 i 计 (edges[i-1], edges[i]],桶 0 计 (0, edges[0]],
/// 末桶(索引 9)= (>5s]。渲染时转秒并输出累计 `_bucket{le=...}`。
pub(crate) const LATENCY_EDGES_US: [u64; LATENCY_BUCKETS] = [
    100,
    1_000,
    5_000,
    10_000,
    50_000,
    100_000,
    500_000,
    1_000_000,
    5_000_000,
    u64::MAX,
];

/// 查询耗时直方图(lock-free;内存按分桶**非累计**存储,渲染时转累计)。
/// 每次查询仅一次 `fetch_add`(先线性扫描边界,≤10 次比较,热路径可忽略)。
pub struct LatencyHistogram {
    buckets: [AtomicU64; LATENCY_BUCKETS],
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// 记录一次查询耗时(µs),落入首个 >= 耗时的上界桶
    pub fn observe(&self, elapsed_us: u64) {
        let idx = LATENCY_EDGES_US
            .iter()
            .position(|&e| elapsed_us <= e)
            .unwrap_or(LATENCY_BUCKETS - 1);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
    }

    /// 各桶计数快照(非累计;长度 = LATENCY_BUCKETS)
    pub fn snapshot(&self) -> Vec<u64> {
        self.buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect()
    }
}

/// 一条 SQL 解析失败记录(命令包解析失败 / SQL 非 UTF-8 等)
#[derive(Debug, Clone, Serialize)]
pub struct ParseFailure {
    /// 时间戳(Unix 秒)
    pub ts: u64,
    /// 失败原因
    pub reason: String,
    /// SQL(截断为可打印文本,最多 512 字符)
    pub sql: String,
}

/// 一条后端错误记录(后端返回 ERR 包,如 1064 语法错误/权限拒绝等)
#[derive(Debug, Clone, Serialize)]
pub struct BackendError {
    /// 时间戳(Unix 秒)
    pub ts: u64,
    /// 错误码(MySQL 错误码字符串,如 "1064";未知为 "-")
    pub code: String,
    /// SQL(截断,最多 512 字符)
    pub sql: String,
}

/// 后端错误按错误码聚合统计(面板"按码统计"表)
#[derive(Debug, Clone, Default, Serialize)]
pub struct BackendErrStat {
    /// 该错误码出现次数
    pub count: u64,
    /// 最近一次发生时间(Unix 秒)
    pub last_ts: u64,
    /// 最近一次 SQL
    pub last_sql: String,
}

/// 最近查询环形缓冲最大条数(记录**所有**查询的单次 5 阶段耗时,
/// 供面板按 SQL 检索单条执行的阶段;超出丢弃最旧)
const MAX_RECENT_QUERIES: usize = 500;

/// 一条查询执行记录(含 5 阶段耗时,用于单条 SQL 的慢环节定位)
#[derive(Debug, Clone, Serialize)]
pub struct QueryRecord {
    /// 时间戳(Unix 秒)
    pub ts: u64,
    /// SQL(单行截断)
    pub sql: String,
    /// 会话当前库(可空)
    pub db: String,
    /// 产品用户
    pub puser: String,
    /// 后端分片(cluster.tablet,空=未绑定后端)
    pub shard: String,
    /// 总耗时(µs)
    pub elapsed_us: u64,
    /// 是否慢查询(超过配置阈值)
    pub slow: bool,
    /// 阶段耗时(µs):interval=命令间隔(含客户端空闲等待,非代理耗时),
    /// parse=解析+拦截, setup=后端准备, send=发后端, forward=后端执行+转发
    pub interval_us: u64,
    pub parse_us: u64,
    pub setup_us: u64,
    pub send_us: u64,
    pub forward_us: u64,
}

/// 一条慢查询记录(含 5 阶段耗时,用于面板/排障定位慢在哪个环节)
#[derive(Debug, Clone, Serialize)]
pub struct SlowQuery {
    /// 时间戳(Unix 秒)
    pub ts: u64,
    /// SQL(单行截断)
    pub sql: String,
    /// 会话当前库(可空)
    pub db: String,
    /// 产品用户
    pub puser: String,
    /// 后端分片(cluster.tablet,空=未绑定后端)
    pub shard: String,
    /// 总耗时(µs)
    pub elapsed_us: u64,
    /// 阶段耗时(µs):interval=命令间隔(含客户端空闲等待,非代理耗时),
    /// parse=解析+拦截, setup=后端准备, send=发后端, forward=后端执行+转发
    pub interval_us: u64,
    pub parse_us: u64,
    pub setup_us: u64,
    pub send_us: u64,
    pub forward_us: u64,
}

// ─── 单值计数器 ───

/// Lock-free 原子计数器
#[derive(Default)]
pub struct Counter {
    value: AtomicU64,
}

impl Counter {
    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    pub fn dec(&self) {
        self.value.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn snapshot_and_reset(&self) -> u64 {
        self.value.swap(0, Ordering::AcqRel)
    }
}

// ─── SQL 统计模板 ───

/// SQL 统计(按 SQL 模板聚合)
#[derive(Debug, Clone, Serialize)]
pub struct SqlStat {
    pub count: u64,
    pub total_time_us: u64,
    pub max_time_us: u64,
    pub rows_sent: u64,
    pub rows_examined: u64,
}

// ─── 全局 Metrics ───

/// 代理全局指标(所有连接 task 共享)
pub struct Metrics {
    // 连接统计
    pub connections_total: Counter,
    pub connections_active: Counter,
    pub connections_rejected: Counter,

    // 查询统计
    pub queries_total: Counter,
    pub queries_errors: Counter,
    pub queries_slow: Counter,

    // 流量统计
    pub bytes_received: Counter,
    pub bytes_sent: Counter,

    // 池统计
    pub pool_acquires: Counter,
    pub pool_acquire_fails: Counter,

    // 查询阶段耗时累计(µs;除以 queries_total 得平均,用于面板展示请求时间分布)
    pub stage_interval_us: Counter,
    pub stage_parse_us: Counter,
    pub stage_setup_us: Counter,
    pub stage_send_us: Counter,
    pub stage_forward_us: Counter,

    /// 查询总耗时累计(µs;直方图 `_sum` 数据源,除以 queries_total 得平均总耗时)
    pub queries_elapsed_us: Counter,

    /// 查询耗时直方图(分桶非累计存储;`/metrics` 渲染为 Prometheus histogram,
    /// 支撑 p50/p95/p99 分位计算)
    pub latency_histogram: LatencyHistogram,

    /// 分片慢查询计数(cluster.tablet → 慢查询次数;面板/监控按分片定位慢查询分布)
    pub shard_slow_counts: DashMap<String, Counter>,

    // SQL 模板统计(按 模板×分片 复合键聚合,DashMap 提供并发安全)
    pub sql_stats: DashMap<(String, String), SqlStat>,

    /// 分片查询数统计(cluster.tablet → 查询次数;面板展示流量分布)
    pub shard_query_counts: DashMap<String, Counter>,

    /// 分片阶段耗时累计(µs):cluster.tablet → [interval, parse, setup, send, forward]。
    /// 面板按分片展示平均耗时(平均 = 累计 / shard_query_counts 对应计数)
    pub shard_stage_us: DashMap<String, [u64; 5]>,

    /// 最近慢查询(>1s)环形缓冲,有界;面板 /api/slow 数据源
    pub slow_queries: Mutex<VecDeque<SlowQuery>>,

    /// 最近**所有**查询环形缓冲(含单次 5 阶段耗时),有界;
    /// 面板 /api/recent 数据源,可按 SQL 检索单条执行
    pub recent_queries: Mutex<VecDeque<QueryRecord>>,

    /// SQL 解析失败计数(监控指标:/metrics newproxy_sql_parse_failures_total)
    pub parse_failures: Counter,

    /// 最近解析失败记录(有界;面板 /api/parsefailures 数据源)
    pub parse_failure_list: Mutex<VecDeque<ParseFailure>>,

    /// 后端错误计数(监控指标:/metrics newproxy_backend_errors_total)
    pub backend_errors: Counter,

    /// 最近后端错误记录(有界;面板 /api/backenderrors 数据源)
    pub backend_error_list: Mutex<VecDeque<BackendError>>,

    /// 后端错误按错误码聚合统计(错误码 → 计数/最近一次)
    pub backend_error_stats: DashMap<String, BackendErrStat>,

    /// 按库流量统计(db → [recv_bytes, sent_bytes, recv_pkts, sent_pkts];
    /// 面板"按库流量"数据源,空库归入 "-")
    pub db_traffic: DashMap<String, [u64; 4]>,

    /// 按后端 MySQL 节点流量统计(cluster.tablet → [recv_bytes, sent_bytes,
    /// recv_pkts, sent_pkts];面板"后端节点流量"图数据源,未绑定节点归 "-")
    pub node_traffic: DashMap<String, [u64; 4]>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            connections_total: Counter::default(),
            connections_active: Counter::default(),
            connections_rejected: Counter::default(),
            queries_total: Counter::default(),
            queries_errors: Counter::default(),
            queries_slow: Counter::default(),
            bytes_received: Counter::default(),
            bytes_sent: Counter::default(),
            pool_acquires: Counter::default(),
            pool_acquire_fails: Counter::default(),
            stage_interval_us: Counter::default(),
            stage_parse_us: Counter::default(),
            stage_setup_us: Counter::default(),
            stage_send_us: Counter::default(),
            stage_forward_us: Counter::default(),
            queries_elapsed_us: Counter::default(),
            latency_histogram: LatencyHistogram::new(),
            shard_slow_counts: DashMap::new(),
            sql_stats: DashMap::new(),
            shard_query_counts: DashMap::new(),
            shard_stage_us: DashMap::new(),
            slow_queries: Mutex::new(VecDeque::new()),
            recent_queries: Mutex::new(VecDeque::new()),
            parse_failures: Counter::default(),
            parse_failure_list: Mutex::new(VecDeque::new()),
            backend_errors: Counter::default(),
            backend_error_list: Mutex::new(VecDeque::new()),
            backend_error_stats: DashMap::new(),
            db_traffic: DashMap::new(),
            node_traffic: DashMap::new(),
        }
    }

    /// 记录一次 SQL 执行
    ///
    /// `slow_threshold_us`:慢查询阈值(µs),由配置 slow_query_ms 动态提供
    pub fn record_query(
        &self,
        sql_template: &str,
        shard: &str,
        elapsed_us: u64,
        rows: u64,
        slow_threshold_us: u64,
    ) {
        self.queries_total.inc();
        // 耗时入直方图与累计(与模板是否登记无关,保证 count/sum/分位数完整)
        self.queries_elapsed_us.add(elapsed_us);
        self.latency_histogram.observe(elapsed_us);
        // 分片维度查询计数(空分片归入 "-")
        let shard_key = if shard.is_empty() { "-" } else { shard };
        self.shard_query_counts
            .entry(shard_key.to_string())
            .or_default()
            .inc();
        // 慢查询判断(超过配置阈值);同时按分片累计,供按分片定位慢查询
        if elapsed_us > slow_threshold_us {
            self.queries_slow.inc();
            self.shard_slow_counts
                .entry(shard_key.to_string())
                .or_default()
                .inc();
        }

        // 防呆:SQL 高基数(带字面量的原始 SQL)时防止统计表无限增长——
        // 超过上限后不再登记新模板(已有模板仍继续累计)。
        let key = (sql_template.to_string(), shard_key.to_string());
        if self.sql_stats.len() >= MAX_SQL_TEMPLATES && !self.sql_stats.contains_key(&key) {
            return;
        }

        // 按 SQL 模板 × 分片 聚合
        self.sql_stats
            .entry(key)
            .and_modify(|s| {
                s.count += 1;
                s.total_time_us += elapsed_us;
                if elapsed_us > s.max_time_us {
                    s.max_time_us = elapsed_us;
                }
                s.rows_sent += rows;
            })
            .or_insert_with(|| SqlStat {
                count: 1,
                total_time_us: elapsed_us,
                max_time_us: elapsed_us,
                rows_sent: rows,
                rows_examined: 0,
            });
    }

    /// 记录查询各阶段耗时(µs),供面板展示请求时间分布(平均 = 累计/查询数);
    /// 同时按分片累计,供面板按分片对比各阶段耗时
    pub fn record_query_stages(
        &self,
        shard: &str,
        read_us: u64,
        parse_us: u64,
        setup_us: u64,
        send_us: u64,
        forward_us: u64,
    ) {
        self.stage_interval_us.add(read_us);
        self.stage_parse_us.add(parse_us);
        self.stage_setup_us.add(setup_us);
        self.stage_send_us.add(send_us);
        self.stage_forward_us.add(forward_us);
        // 分片维度累计(空分片归入 "-",与 record_query 的 shard_query_counts 键一致)
        let shard_key = if shard.is_empty() { "-" } else { shard };
        self.shard_stage_us
            .entry(shard_key.to_string())
            .and_modify(|a| {
                a[0] += read_us;
                a[1] += parse_us;
                a[2] += setup_us;
                a[3] += send_us;
                a[4] += forward_us;
            })
            .or_insert([read_us, parse_us, setup_us, send_us, forward_us]);
    }

    /// 记录一条慢查询(有界环形缓冲,淘汰最旧)
    pub fn record_slow_query(&self, q: SlowQuery) {
        let mut buf = self.slow_queries.lock();
        if buf.len() >= MAX_SLOW_QUERIES {
            buf.pop_front();
        }
        buf.push_back(q);
    }

    /// 最近慢查询(新→旧,最多 MAX_SLOW_QUERIES 条)
    pub fn slow_queries_snapshot(&self) -> Vec<SlowQuery> {
        self.slow_queries.lock().iter().rev().cloned().collect()
    }

    /// 清空慢查询缓冲(面板"清除"按钮)
    pub fn clear_slow_queries(&self) {
        self.slow_queries.lock().clear();
    }

    /// 记录一条查询执行(所有查询,有界环形缓冲,淘汰最旧)
    pub fn record_recent_query(&self, q: QueryRecord) {
        let mut buf = self.recent_queries.lock();
        if buf.len() >= MAX_RECENT_QUERIES {
            buf.pop_front();
        }
        buf.push_back(q);
    }

    /// 最近查询(新→旧,最多 MAX_RECENT_QUERIES 条)
    pub fn recent_queries_snapshot(&self) -> Vec<QueryRecord> {
        self.recent_queries.lock().iter().rev().cloned().collect()
    }

    /// 清空最近查询缓冲(面板"清除"按钮)
    pub fn clear_recent_queries(&self) {
        self.recent_queries.lock().clear();
    }

    /// 记录一条 SQL 解析失败(计数 + 有界环形缓冲)
    pub fn record_parse_failure(&self, reason: &str, sql: &str) {
        self.parse_failures.inc();
        let mut buf = self.parse_failure_list.lock();
        if buf.len() >= MAX_PARSE_FAILURES {
            buf.pop_front();
        }
        buf.push_back(ParseFailure {
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            reason: reason.to_string(),
            sql: sql.chars().take(512).collect(),
        });
    }

    /// 最近解析失败记录(新→旧)
    pub fn parse_failures_snapshot(&self) -> Vec<ParseFailure> {
        self.parse_failure_list
            .lock()
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    /// 记录一条后端错误(后端返回 ERR 包,如 1064 语法错误):
    /// 计数 + 有界环形缓冲 + 按错误码聚合(供面板"后端错误统计")
    pub fn record_backend_error(&self, code: &str, sql: &str) {
        self.backend_errors.inc();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let sql_t: String = sql.chars().take(512).collect();
        let mut buf = self.backend_error_list.lock();
        if buf.len() >= MAX_BACKEND_ERRORS {
            buf.pop_front();
        }
        buf.push_back(BackendError {
            ts,
            code: code.to_string(),
            sql: sql_t.clone(),
        });
        let mut st = self
            .backend_error_stats
            .entry(code.to_string())
            .or_default();
        st.count += 1;
        st.last_ts = ts;
        st.last_sql = sql_t;
    }

    /// 最近后端错误(新→旧,最多 MAX_BACKEND_ERRORS 条)
    pub fn backend_errors_snapshot(&self) -> Vec<BackendError> {
        self.backend_error_list
            .lock()
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    /// 后端错误按错误码聚合统计(按计数降序)
    pub fn backend_error_stats_snapshot(&self) -> Vec<(String, BackendErrStat)> {
        let mut v: Vec<_> = self
            .backend_error_stats
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        v.sort_by_key(|(_, s)| std::cmp::Reverse(s.count));
        v
    }

    /// 记录按库流量(客户端命令收/发字节与包数;空库归入 "-")
    pub fn record_db_traffic(
        &self,
        db: &str,
        recv_bytes: u64,
        sent_bytes: u64,
        recv_pkts: u64,
        sent_pkts: u64,
    ) {
        let key = if db.is_empty() { "-" } else { db };
        self.db_traffic
            .entry(key.to_string())
            .and_modify(|a| {
                a[0] += recv_bytes;
                a[1] += sent_bytes;
                a[2] += recv_pkts;
                a[3] += sent_pkts;
            })
            .or_insert([recv_bytes, sent_bytes, recv_pkts, sent_pkts]);
    }

    /// 按库流量快照(按 收+发 总字节降序)
    pub fn db_traffic_snapshot(&self) -> Vec<(String, [u64; 4])> {
        let mut v: Vec<_> = self
            .db_traffic
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect();
        v.sort_by_key(|(_, a)| std::cmp::Reverse(a[0] + a[1]));
        v
    }

    /// 记录按后端节点流量(客户端命令收/发字节与包数;节点为空归入 "-")
    pub fn record_node_traffic(
        &self,
        node: &str,
        recv_bytes: u64,
        sent_bytes: u64,
        recv_pkts: u64,
        sent_pkts: u64,
    ) {
        let key = if node.is_empty() { "-" } else { node };
        self.node_traffic
            .entry(key.to_string())
            .and_modify(|a| {
                a[0] += recv_bytes;
                a[1] += sent_bytes;
                a[2] += recv_pkts;
                a[3] += sent_pkts;
            })
            .or_insert([recv_bytes, sent_bytes, recv_pkts, sent_pkts]);
    }

    /// 按后端节点流量快照(按 收+发 总字节降序;面板节点流量图数据源)
    pub fn node_traffic_snapshot(&self) -> Vec<(String, [u64; 4])> {
        let mut v: Vec<_> = self
            .node_traffic
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect();
        v.sort_by_key(|(_, a)| std::cmp::Reverse(a[0] + a[1]));
        v
    }

    /// 获取所有 SQL 统计快照((模板, 分片) → 统计)
    pub fn sql_stats_snapshot(&self) -> Vec<((String, String), SqlStat)> {
        self.sql_stats
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    /// 按查询次数排序的 Top-N
    pub fn top_queries(&self, n: usize) -> Vec<((String, String), SqlStat)> {
        let mut stats: Vec<_> = self.sql_stats_snapshot();
        stats.sort_by_key(|b| std::cmp::Reverse(b.1.count));
        stats.truncate(n);
        stats
    }

    /// 分片查询数快照(按查询数降序),供面板展示各分片流量分布
    pub fn shard_queries_snapshot(&self) -> Vec<(String, u64)> {
        let mut v: Vec<_> = self
            .shard_query_counts
            .iter()
            .map(|e| (e.key().clone(), e.value().get()))
            .collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        v
    }

    /// 分片慢查询数快照(按计数降序),供监控/面板按分片定位慢查询分布
    pub fn shard_slow_snapshot(&self) -> Vec<(String, u64)> {
        let mut v: Vec<_> = self
            .shard_slow_counts
            .iter()
            .map(|e| (e.key().clone(), e.value().get()))
            .collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        v
    }

    /// 分片阶段耗时快照:返回 (分片, 查询数, [interval, parse, setup, send, forward] 累计 µs),
    /// 按查询数降序;查询数取自 shard_query_counts(与阶段累计同键)
    pub fn shard_stages_snapshot(&self) -> Vec<(String, u64, [u64; 5])> {
        let mut v: Vec<_> = self
            .shard_stage_us
            .iter()
            .map(|e| {
                let count = self
                    .shard_query_counts
                    .get(e.key())
                    .map(|c| c.get())
                    .unwrap_or(0);
                (e.key().clone(), count, *e.value())
            })
            .collect();
        v.sort_by_key(|(_, count, _)| std::cmp::Reverse(*count));
        v
    }

    /// 生成指标摘要(JSON)
    pub fn summary(&self) -> MetricsSummary {
        MetricsSummary {
            connections_total: self.connections_total.get(),
            connections_active: self.connections_active.get(),
            connections_rejected: self.connections_rejected.get(),
            queries_total: self.queries_total.get(),
            queries_errors: self.queries_errors.get(),
            queries_slow: self.queries_slow.get(),
            bytes_received: crate::proto::codec::bytes_read_total(),
            bytes_sent: crate::proto::codec::bytes_written_total(),
            pool_acquires: self.pool_acquires.get(),
            pool_acquire_fails: self.pool_acquire_fails.get(),
            stage_interval_us: self.stage_interval_us.get(),
            stage_parse_us: self.stage_parse_us.get(),
            stage_setup_us: self.stage_setup_us.get(),
            stage_send_us: self.stage_send_us.get(),
            stage_forward_us: self.stage_forward_us.get(),
        }
    }
}

/// Metrics JSON 摘要
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSummary {
    pub connections_total: u64,
    pub connections_active: u64,
    pub connections_rejected: u64,
    pub queries_total: u64,
    pub queries_errors: u64,
    pub queries_slow: u64,
    pub bytes_received: u64,
    pub bytes_sent: u64,
    pub pool_acquires: u64,
    pub pool_acquire_fails: u64,
    /// 查询阶段累计耗时(µs;平均 = /queries_total)
    pub stage_interval_us: u64,
    pub stage_parse_us: u64,
    pub stage_setup_us: u64,
    pub stage_send_us: u64,
    pub stage_forward_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_inc() {
        let c = Counter::default();
        c.inc();
        c.inc();
        assert_eq!(c.get(), 2);
    }

    #[test]
    fn counter_snapshot() {
        let c = Counter::default();
        c.add(42);
        assert_eq!(c.snapshot_and_reset(), 42);
        assert_eq!(c.get(), 0);
    }

    #[test]
    fn record_query_updates_stats() {
        let m = Metrics::new();
        m.record_query("SELECT * FROM t WHERE id = ?", "0.t0", 500, 100, 1_000_000);
        m.record_query("SELECT * FROM t WHERE id = ?", "0.t0", 1500, 200, 1_000_000);
        // 不同分片独立聚合
        m.record_query("SELECT * FROM t WHERE id = ?", "0.t1", 300, 10, 1_000_000);

        let stats = m
            .sql_stats
            .get(&(
                "SELECT * FROM t WHERE id = ?".to_string(),
                "0.t0".to_string(),
            ))
            .unwrap();
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_time_us, 2000);
        assert_eq!(stats.max_time_us, 1500);
        assert_eq!(stats.rows_sent, 300);
        // 分片维度计数
        let shards = m.shard_queries_snapshot();
        assert_eq!(shards.len(), 2);
        assert_eq!(shards.iter().find(|(k, _)| k == "0.t0").unwrap().1, 2);
        assert_eq!(shards.iter().find(|(k, _)| k == "0.t1").unwrap().1, 1);
    }

    #[test]
    fn parse_failure_recorded() {
        let m = Metrics::new();
        m.record_parse_failure("命令包解析失败", "SELEC\u{fffd}T");
        m.record_parse_failure("SQL 非 UTF-8", "SELECT \u{fffd}");
        assert_eq!(m.parse_failures.get(), 2);
        let list = m.parse_failures_snapshot();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].reason, "SQL 非 UTF-8"); // 新→旧
        assert!(list[0].sql.len() <= 512);
    }

    #[test]
    fn backend_error_recorded() {
        let m = Metrics::new();
        m.record_backend_error("1064", "SELEC * FROM t");
        m.record_backend_error("1064", "SELEC * FROM t2");
        m.record_backend_error("1146", "SELECT * FROM nope");
        assert_eq!(m.backend_errors.get(), 3);
        // 最近明细(新→旧)
        let list = m.backend_errors_snapshot();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].code, "1146");
        assert_eq!(list[2].code, "1064");
        assert!(list[0].sql.len() <= 512);
        // 按码聚合(按计数降序,1064 在前)
        let stats = m.backend_error_stats_snapshot();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].0, "1064");
        assert_eq!(stats[0].1.count, 2);
        assert_eq!(stats[1].0, "1146");
        assert_eq!(stats[1].1.count, 1);
        // 最近 SQL/时间随最后一次更新
        assert_eq!(stats[0].1.last_sql, "SELEC * FROM t2");
        assert!(stats[0].1.last_ts > 0);
    }

    #[test]
    fn db_traffic_recorded() {
        let m = Metrics::new();
        m.record_db_traffic("sbtest", 100, 200, 3, 5);
        m.record_db_traffic("sbtest", 50, 100, 1, 2);
        m.record_db_traffic("", 10, 10, 1, 1);
        let v = m.db_traffic_snapshot();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0, "sbtest"); // 总流量大者在前
        assert_eq!(v[0].1, [150, 300, 4, 7]);
        assert_eq!(v[1].0, "-");
        assert_eq!(v[1].1, [10, 10, 1, 1]);
    }

    #[test]
    fn node_traffic_recorded() {
        let m = Metrics::new();
        m.record_node_traffic("0.t0", 100, 200, 3, 5);
        m.record_node_traffic("0.t0", 50, 100, 1, 2);
        m.record_node_traffic("0.t1", 10, 10, 1, 1);
        let v = m.node_traffic_snapshot();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0, "0.t0"); // 总流量大者在前
        assert_eq!(v[0].1, [150, 300, 4, 7]);
        assert_eq!(v[1].0, "0.t1");
        assert_eq!(v[1].1, [10, 10, 1, 1]);
    }

    #[test]
    fn slow_query_detection() {
        let m = Metrics::new();
        // 阈值 200ms:2s 超阈值记慢,100ms 不记
        m.record_query("SLOW", "0.t0", 2_000_000, 0, 200_000);
        m.record_query("FAST", "0.t0", 100_000, 0, 200_000);
        assert_eq!(m.queries_slow.get(), 1);
        assert_eq!(m.queries_total.get(), 2);
    }

    #[test]
    fn shard_stages_aggregate() {
        let m = Metrics::new();
        // 查询计数(与阶段累计同键)
        m.record_query("Q", "0.t0", 1000, 0, 1_000_000);
        m.record_query("Q", "0.t0", 2000, 0, 1_000_000);
        m.record_query("Q", "0.t1", 500, 0, 1_000_000);
        // 分片阶段累计:不同分片独立聚合,空分片归入 "-"
        m.record_query_stages("0.t0", 10, 20, 30, 40, 50);
        m.record_query_stages("0.t0", 15, 25, 35, 45, 55);
        m.record_query_stages("0.t1", 1, 2, 3, 4, 5);
        m.record_query_stages("", 7, 7, 7, 7, 7);

        let snap = m.shard_stages_snapshot();
        assert_eq!(snap.len(), 3);
        let t0 = snap.iter().find(|(k, _, _)| k == "0.t0").unwrap();
        assert_eq!(t0.1, 2);
        assert_eq!(t0.2, [25, 45, 65, 85, 105]);
        let t1 = snap.iter().find(|(k, _, _)| k == "0.t1").unwrap();
        assert_eq!(t1.1, 1);
        assert_eq!(t1.2, [1, 2, 3, 4, 5]);
        let dash = snap.iter().find(|(k, _, _)| k == "-").unwrap();
        assert_eq!(dash.1, 0); // 无查询计数但阶段有累计
        assert_eq!(dash.2, [7, 7, 7, 7, 7]);
    }

    #[test]
    fn sql_template_cap() {
        let m = Metrics::new();
        // 超过 MAX_SQL_TEMPLATES 后不再登记新模板,已有模板继续累计
        for i in 0..MAX_SQL_TEMPLATES + 50 {
            m.record_query(&format!("TPL{i}"), "0.t0", 100, 0, 1_000_000);
        }
        assert!(m.sql_stats.len() <= MAX_SQL_TEMPLATES);
        // 已有模板仍累计
        m.record_query("TPL0", "0.t0", 100, 0, 1_000_000);
        assert_eq!(
            m.sql_stats
                .get(&("TPL0".into(), "0.t0".into()))
                .unwrap()
                .count,
            2
        );
        // top_queries 排序:次数最多在前
        let top = m.top_queries(3);
        assert!(top[0].1.count >= top[1].1.count);
    }

    #[test]
    fn ring_buffer_overflow() {
        let m = Metrics::new();
        // 慢查询环形缓冲:超过上限丢弃最旧
        for i in 0..MAX_SLOW_QUERIES + 10 {
            m.record_slow_query(SlowQuery {
                ts: i as u64,
                sql: format!("SLOW{i}"),
                db: String::new(),
                puser: String::new(),
                shard: String::new(),
                elapsed_us: 1_000_000,
                interval_us: 0,
                parse_us: 0,
                setup_us: 0,
                send_us: 0,
                forward_us: 0,
            });
        }
        let list = m.slow_queries_snapshot();
        assert_eq!(list.len(), MAX_SLOW_QUERIES);
        assert_eq!(list[0].ts, (MAX_SLOW_QUERIES + 9) as u64, "最新在前");
        // 最近查询环形缓冲
        for i in 0..MAX_RECENT_QUERIES + 10 {
            m.record_recent_query(QueryRecord {
                ts: i as u64,
                sql: format!("Q{i}"),
                db: String::new(),
                puser: String::new(),
                shard: String::new(),
                elapsed_us: 1,
                slow: false,
                interval_us: 0,
                parse_us: 0,
                setup_us: 0,
                send_us: 0,
                forward_us: 0,
            });
        }
        assert_eq!(m.recent_queries_snapshot().len(), MAX_RECENT_QUERIES);
        // 清除
        m.clear_slow_queries();
        assert!(m.slow_queries_snapshot().is_empty());
    }

    #[test]
    fn summary_and_stage_accumulation() {
        let m = Metrics::new();
        m.record_query("Q", "0.t0", 1_000, 5, 1_000_000);
        m.record_query_stages("0.t0", 10, 20, 30, 40, 50);
        let s = m.summary();
        assert_eq!(s.queries_total, 1);
        assert_eq!(s.stage_parse_us, 20);
        assert_eq!(s.stage_forward_us, 50);
        assert_eq!(s.pool_acquires, 0);
    }

    #[test]
    fn latency_histogram_buckets() {
        let h = LatencyHistogram::new();
        // 边界:50 → 桶0(≤100);1500 → 桶2(1000<1500≤5000);
        // 6000ms → 末桶(>5s);恰好 1s → 桶7(≤1s)
        h.observe(50);
        h.observe(1500);
        h.observe(6_000_000);
        h.observe(1_000_000);
        let snap = h.snapshot();
        assert_eq!(snap.len(), LATENCY_BUCKETS);
        assert_eq!(snap[0], 1); // 50
        assert_eq!(snap[2], 1); // 1500
        assert_eq!(snap[7], 1); // 1_000_000(edges[7]=1e6)
        assert_eq!(snap[9], 1); // 6s > 5s → 兜底桶
        assert_eq!(snap.iter().sum::<u64>(), 4);
    }

    #[test]
    fn record_query_histogram_sum_and_shard_slow() {
        let m = Metrics::new();
        // 2 快 1 慢(阈值 200ms),慢落在 0.t0
        m.record_query("F1", "0.t0", 50_000, 3, 200_000);
        m.record_query("F2", "0.t1", 80_000, 5, 200_000);
        m.record_query("S1", "0.t0", 2_000_000, 10, 200_000);
        assert_eq!(m.queries_total.get(), 3);
        assert_eq!(m.queries_slow.get(), 1);
        assert_eq!(m.queries_elapsed_us.get(), 50_000 + 80_000 + 2_000_000);
        // 直方图:3 个样本分别 50ms(edges[4]=50ms)/80ms(edges[5]=100ms)/2s(edges[8]=5s)
        let snap = m.latency_histogram.snapshot();
        assert_eq!(snap.iter().sum::<u64>(), 3);
        assert_eq!(snap[4], 1);
        assert_eq!(snap[5], 1);
        assert_eq!(snap[8], 1);
        // 分片慢查询计数:仅 0.t0 有 1 条
        let slow = m.shard_slow_snapshot();
        assert_eq!(slow.len(), 1);
        assert_eq!(slow[0], ("0.t0".to_string(), 1));
        // 空分片慢查询归 "-"
        m.record_query("S2", "", 2_000_000, 0, 200_000);
        let slow2 = m.shard_slow_snapshot();
        assert_eq!(slow2.len(), 2);
        assert!(slow2.iter().any(|(k, n)| k == "-" && *n == 1));
        // 模板 rows_sent 由调用方传入并聚合
        let st = m
            .sql_stats
            .get(&("F1".to_string(), "0.t0".to_string()))
            .unwrap();
        assert_eq!(st.rows_sent, 3);
    }
}

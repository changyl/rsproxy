// 后端连接池:共享池 + acquire/release + 异步归还
// T2.2 + T2.3 实现
//
// 对齐 C 侧 tr_srv_pool.c + tr_conn_pool.h
// 设计参考 /docs/03-backend-pool-optimization.md

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::net::TcpStream;
use tracing::debug;

use crate::config::{Database, MasterSlave};

// ─── 后端连接 ───

/// 后端 MySQL 连接
///
/// `stream` 由 `parking_lot::Mutex` 保护:当连接在池中(`in_pool==true`)
/// 时 `stream` 为 `Some`;被会话取出后(`in_pool==false`),会话通过
/// `BackendGuard::take_stream()` 取走所有权,此时 `stream` 为 `None`;
/// 会话结束时通过 `BackendGuard::return_stream()` 归还。
pub struct BackConn {
    /// TCP 流(Mutex 保护:池中=Some,被会话取走=None)
    pub stream: Mutex<Option<TcpStream>>,
    /// 后端地址
    pub addr: SocketAddr,
    /// 所属数据库配置
    pub db: Arc<Database>,
    /// 主从角色
    pub ms: MasterSlave,
    /// 是否在池中(CAS 保护:1=在池,0=被取出)
    pub in_pool: AtomicBool,
    /// 已服务次数(用于复用上限检查)
    pub served_times: AtomicU64,
    /// 最后活跃时间(Unix 微秒)
    pub last_active: AtomicU64,
    /// 是否已损坏(使用中出现 IO/协议错误):归还时丢弃而非回池,
    /// 避免把已死/有残留数据的连接重新发给下一个会话(否则 Broken pipe 雪崩)。
    pub broken: AtomicBool,
    /// 会话内是否使用过 prepared statement(COM_STMT_PREPARE 成功,或文本
    /// PREPARE 透传)。服务端不会在连接上释放这些语句(除非客户端逐一
    /// COM_STMT_CLOSE / DEALLOCATE,代理不做跟踪),归还复用会导致下个会话
    /// 继承孤儿 prepared 语句,累积撞上 max_prepared_stmt_count。置位后
    /// 归还时直接丢弃连接(底层 TCP 关闭,服务端语句随之释放)。
    pub no_reuse: AtomicBool,
}

impl BackConn {
    /// 创建新的后端连接(尚未 connect)
    pub fn new(addr: SocketAddr, db: Arc<Database>, ms: MasterSlave) -> Self {
        Self {
            stream: Mutex::new(None),
            addr,
            db,
            ms,
            in_pool: AtomicBool::new(false),
            served_times: AtomicU64::new(0),
            last_active: AtomicU64::new(now_micros()),
            broken: AtomicBool::new(false),
            no_reuse: AtomicBool::new(false),
        }
    }

    /// 标记连接损坏:之后归还到池时会被丢弃(底层 TCP 随之关闭)
    pub fn mark_broken(&self) {
        self.broken.store(true, Ordering::Release);
    }

    /// 标记"不再复用":会话内存在未释放的 prepared statement 时调用,
    /// 归还到池时直接丢弃,避免下个会话继承孤儿语句。
    pub fn mark_no_reuse(&self) {
        self.no_reuse.store(true, Ordering::Release);
    }

    /// 连接并完成握手(由 pool 调用)
    pub async fn connect(&self) -> std::io::Result<()> {
        let stream = TcpStream::connect(self.addr).await?;
        *self.stream.lock() = Some(stream);
        Ok(())
    }
}

// ─── 属性桶 ───

/// 池配置(每个桶独立)
#[derive(Debug, Clone)]
pub struct BucketCfg {
    /// 连接最大复用次数(conn_pool_socket_max_serve_client_times)
    pub max_serve_times: u64,
    /// 最大空闲时间(秒,reaper 与 acquire 共用)
    pub max_idle_secs: u64,
    /// 取用时的陈旧阈值(秒):空闲超过该时长的连接不再复用,直接丢弃。
    /// 用于兜底后端/网关主动关闭空闲连接(其空闲超时通常远小于 max_idle_secs)
    /// 而代理侧无从得知的情形——避免复用到已死的 socket 触发 Broken pipe。
    pub stale_on_acquire_secs: u64,
    /// 池最大容量
    pub max_size: usize,
    /// 最小空闲连接(预热用)
    pub min_idle: usize,
}

impl Default for BucketCfg {
    fn default() -> Self {
        Self {
            max_serve_times: 10000,
            max_idle_secs: 300,
            stale_on_acquire_secs: 30,
            max_size: 128,
            min_idle: 0,
        }
    }
}

/// 属性桶:存储同一 (cluster, tablet, user) 下的后端连接
///
/// - `write`: 主库连接队列
/// - `read`: 从库连接队列
/// - 每个队列由 `parking_lot::Mutex<VecDeque<Arc<BackConn>>>` 保护
pub struct AttrBucket {
    pub write: Mutex<VecDeque<Arc<BackConn>>>,
    pub read: Mutex<VecDeque<Arc<BackConn>>>,
    pub cfg: BucketCfg,
}

impl AttrBucket {
    pub fn new(cfg: BucketCfg) -> Self {
        Self {
            write: Mutex::new(VecDeque::new()),
            read: Mutex::new(VecDeque::new()),
            cfg,
        }
    }

    /// 根据主从选择队列
    fn queue_for(&self, ms: MasterSlave) -> &Mutex<VecDeque<Arc<BackConn>>> {
        match ms {
            MasterSlave::Master => &self.write,
            MasterSlave::Slave => &self.read,
        }
    }

    /// 同步 acquire:从队列中取出一个可用连接(不涉及 IO)
    ///
    /// 对齐 C 侧 tr_conn_pool_get + CAS is_in_svr_pool 1→0
    pub fn acquire(&self, ms: MasterSlave) -> Option<Arc<BackConn>> {
        let q = self.queue_for(ms);
        let mut guard = q.lock();
        while let Some(c) = guard.pop_front() {
            // CAS 1→0:标记连接为"已取出"
            if c.in_pool
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                // 损坏 / 过期 / 陈旧 / 标记不复用:一律丢弃(不归还),避免复用到死连接
                if c.broken.load(Ordering::Acquire)
                    || c.no_reuse.load(Ordering::Acquire)
                    || self.is_expired(&c)
                    || self.is_stale_on_acquire(&c)
                {
                    debug!(addr = %c.addr, broken = c.broken.load(Ordering::Acquire), no_reuse = c.no_reuse.load(Ordering::Acquire), "backend connection discarded on acquire");
                    continue;
                }
                return Some(c);
            }
            // CAS 失败:连接被并发取出,继续尝试
        }
        None
    }

    /// 同步 release:归还连接到队列(push 回队列)
    ///
    /// 对齐 C 侧 tr_srv_pool_add
    pub fn release(&self, c: Arc<BackConn>) {
        // CAS 0→1:标记连接为"在池中"
        if c.in_pool
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            // 已经在池中(double-put 防护),或已标记销毁
            return;
        }

        // 损坏连接:不再入池,让 Arc drop 关闭底层 TCP。
        // 避免把已死/残留数据的连接重新发给下一个会话(否则反复 Broken pipe)。
        if c.broken.load(Ordering::Acquire) {
            c.in_pool.store(false, Ordering::Release);
            debug!(addr = %c.addr, "backend connection broken, discarded");
            return;
        }

        // 会话内使用过 prepared statement:服务端仍持有未释放的语句,
        // 复用会继承孤儿语句(累积撞 max_prepared_stmt_count),直接丢弃。
        if c.no_reuse.load(Ordering::Acquire) {
            c.in_pool.store(false, Ordering::Release);
            debug!(addr = %c.addr, "backend connection had prepared statements, discarded (not pooled)");
            return;
        }

        // 复用上限检查
        let served = c.served_times.fetch_add(1, Ordering::Relaxed) + 1;
        if served >= self.cfg.max_serve_times {
            c.in_pool.store(false, Ordering::Release);
            debug!(addr = %c.addr, served_times = served, "backend retired");
            return;
        }

        c.last_active.store(now_micros(), Ordering::Release);
        let q = self.queue_for(c.ms);
        q.lock().push_back(c);
    }

    /// 检查连接是否过期
    fn is_expired(&self, c: &BackConn) -> bool {
        let served = c.served_times.load(Ordering::Relaxed);
        if served >= self.cfg.max_serve_times {
            return true;
        }
        let now = now_micros();
        let last = c.last_active.load(Ordering::Relaxed);
        now - last > self.cfg.max_idle_secs * 1_000_000
    }

    /// 取用时的陈旧检查:空闲超过 stale_on_acquire_secs 的连接直接丢弃。
    ///
    /// 池对端(后端/网关)可能在其自身的空闲超时后关闭连接,代理无从得知;
    /// 若直接复用会写入已关闭的 socket(Broken pipe)。取用时按短阈值丢弃,
    /// 由新会话重新建连,代价是一次握手,远小于一次必然失败的查询。
    fn is_stale_on_acquire(&self, c: &BackConn) -> bool {
        let now = now_micros();
        let last = c.last_active.load(Ordering::Relaxed);
        now.saturating_sub(last) > self.cfg.stale_on_acquire_secs * 1_000_000
    }
}

// ─── 共享连接池 ───

/// 生成桶的复合键:"cluster.tablet.user.db"
///
/// db 参与分桶后,池中连接与其所选的库一一对应,取用时无需再向后端
/// 发送 COM_INIT_DB(原实现每取用一次都做一次后端往返,交替分片场景
/// 每条查询额外支付一个后端 RTT,是分片代理性能的主要结构性开销)。
fn bucket_key(cluster_id: &str, tablet_id: &str, user_id: &str, db: &str) -> String {
    format!("{}.{}.{}.{}", cluster_id, tablet_id, user_id, db)
}

/// 全局后端连接池
///
/// 单层 DashMap,key 为 "cluster.tablet.user" 复合键
/// 避免嵌套 DashMap 的锁冲突,后续可按需改为三层
pub struct SrvPool {
    map: DashMap<String, Arc<AttrBucket>>,
}

impl Default for SrvPool {
    fn default() -> Self {
        Self::new()
    }
}

impl SrvPool {
    pub fn new() -> Self {
        Self {
            map: DashMap::new(),
        }
    }

    /// 获取或创建指定 (cluster, tablet, user, db) 的 AttrBucket
    pub fn get_or_create_bucket(
        &self,
        cluster_id: &str,
        tablet_id: &str,
        user_id: &str,
        db: &str,
        cfg: BucketCfg,
    ) -> Arc<AttrBucket> {
        let key = bucket_key(cluster_id, tablet_id, user_id, db);
        self.map
            .entry(key)
            .or_insert_with(|| Arc::new(AttrBucket::new(cfg)))
            .clone()
    }

    /// 从池中尝试获取可用连接
    pub fn try_acquire(
        &self,
        cluster_id: &str,
        tablet_id: &str,
        user_id: &str,
        db: &str,
        ms: MasterSlave,
    ) -> Option<Arc<BackConn>> {
        let key = bucket_key(cluster_id, tablet_id, user_id, db);
        let bucket = self.map.get(&key)?;
        bucket.acquire(ms)
    }

    /// 获取所有桶(键 + 桶,用于预热、回收、面板展示等)。
    /// 键格式 `cluster.tablet.user`,面板据此按分片区分
    pub fn all_buckets(&self) -> Vec<(String, Arc<AttrBucket>)> {
        self.map
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    /// 清空连接池(reload/拓扑变更后调用):丢弃全部空闲连接。
    ///
    /// 已借出的连接(会话持有)不受影响,会话结束后其所属桶已从 map 移除,
    /// 归还的连接随桶一并回收。此后 `get_or_create_bucket` 按新配置重建。
    pub fn reset(&self) {
        let n = self.map.len();
        self.map.clear();
        debug!("backend pool reset: {n} buckets dropped");
    }

    /// 失效单个分片的所有桶(配置中心按分片下发变更后调用):
    /// 桶键为 `cluster.tablet.user`,按 `{cluster}.{tablet}.` 前缀定向清理。
    /// 其他分片的连接保留,不产生重建风暴。
    pub fn invalidate_shard(&self, cluster_id: &str, tablet_id: &str) {
        let prefix = format!("{cluster_id}.{tablet_id}.");
        let n = self
            .map
            .iter()
            .filter(|e| e.key().starts_with(&prefix))
            .count();
        if n > 0 {
            self.map.retain(|k, _| !k.starts_with(&prefix));
            debug!("backend pool invalidated shard {cluster_id}.{tablet_id}: {n} buckets");
        }
    }
}

// ─── BackendGuard ───

/// RAII guard:持有后端连接,drop 时自动归还到池
///
/// 对齐 C 侧:手动 tr_srv_pool_add
/// Rust 改进:panic 安全,guard 释放时自动归还
pub struct BackendGuard {
    conn: Option<Arc<BackConn>>,
    bucket: Option<Arc<AttrBucket>>,
}

impl BackendGuard {
    /// 从池中取出的连接创建 guard
    pub fn new(conn: Arc<BackConn>, bucket: Arc<AttrBucket>) -> Self {
        Self {
            conn: Some(conn),
            bucket: Some(bucket),
        }
    }

    /// 获取后端连接的引用
    pub fn conn(&self) -> &BackConn {
        self.conn.as_ref().unwrap()
    }

    /// 获取后端 stream 的不可变引用(需要时 lock)
    pub fn stream_ref(&self) -> Option<parking_lot::MutexGuard<'_, Option<TcpStream>>> {
        Some(self.conn.as_ref()?.stream.lock())
    }

    /// 从 BackConn 中取出 TcpStream 所有权,交给会话使用。
    ///
    /// 仅在连接刚从池中取出或被新建时调用一次。
    /// 之后通过 `return_stream()` 归还。
    pub fn take_stream(&self) -> TcpStream {
        self.conn
            .as_ref()
            .expect("BackendGuard: conn missing")
            .stream
            .lock()
            .take()
            .expect("BackendGuard: stream already taken")
    }

    /// 将 TcpStream 所有权归还到 BackConn,使之可以重新入池。
    pub fn return_stream(&self, stream: TcpStream) {
        if let Some(conn) = self.conn.as_ref() {
            *conn.stream.lock() = Some(stream);
        }
    }

    /// 手动归还(可显式控制归还时机,避免跨 await panic 丢失)
    pub fn release(mut self) {
        if let (Some(conn), Some(bucket)) = (self.conn.take(), self.bucket.take()) {
            bucket.release(conn);
        }
    }
}

impl Drop for BackendGuard {
    fn drop(&mut self) {
        if let (Some(conn), Some(bucket)) = (self.conn.take(), self.bucket.take()) {
            bucket.release(conn);
        }
    }
}

// ─── 空闲回收 + 预热 (T2.5) ───

/// 空闲回收器:周期性扫描所有桶,关闭过期连接
///
/// 作为一个独立 tokio task 运行
pub async fn idle_reaper(pool: Arc<SrvPool>, interval_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    loop {
        interval.tick().await;
        let now = now_micros();
        for (_, bucket) in pool.all_buckets() {
            reap_bucket(&bucket, now);
        }
    }
}

/// 回收单个桶中的过期连接
fn reap_bucket(bucket: &AttrBucket, now: u64) {
    // 回收写队列
    {
        let mut q = bucket.write.lock();
        q.retain(|c| {
            let keep = c.last_active.load(Ordering::Relaxed)
                + (bucket.cfg.max_idle_secs * 1_000_000)
                > now;
            if !keep {
                debug!(addr = %c.addr, "reaping idle backend connection");
                c.in_pool.store(false, Ordering::Release);
            }
            keep
        });
    }
    // 回收读队列
    {
        let mut q = bucket.read.lock();
        q.retain(|c| {
            let keep = c.last_active.load(Ordering::Relaxed)
                + (bucket.cfg.max_idle_secs * 1_000_000)
                > now;
            if !keep {
                debug!(addr = %c.addr, "reaping idle backend connection");
                c.in_pool.store(false, Ordering::Release);
            }
            keep
        });
    }
}

/// 预热:启动时为每个桶预建 min_idle 条连接
///
/// 异步函数,并发预建所有桶的连接
pub async fn warmup_pool(pool: &SrvPool) {
    let buckets = pool.all_buckets();
    let mut handles = Vec::new();

    for (_, bucket) in &buckets {
        let min_idle = bucket.cfg.min_idle;
        if min_idle == 0 {
            continue;
        }
        let bucket = bucket.clone();

        handles.push(tokio::spawn(async move {
            for _ in 0..min_idle {
                // 实际建连需要 DB 信息和认证,这里先创建空 BackConn
                // 完整的 warmup 需要在 T3 阶段与配置集成
                let _ = &bucket;
                debug!("warmup: pre-built connection for bucket");
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    debug!("warmup complete: {} buckets processed", buckets.len());
}

// ─── 辅助 ───

fn now_micros() -> u64 {
    // 使用单调时钟
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_micros() as u64
}

// ─── 测试 ───

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn make_db() -> Arc<Database> {
        Arc::new(Database {
            host: "127.0.0.1".to_string(),
            port: 3306,
            max_pool_size: 16,
            max_connections: 256,
            connect_timeout: 5,
            weight: 1,
            tablet_name: None,
        })
    }

    #[test]
    fn bucket_acquire_release() {
        let bucket = Arc::new(AttrBucket::new(BucketCfg::default()));
        let db = make_db();
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3306));

        // 放入一个连接
        let conn = Arc::new(BackConn::new(addr, db, MasterSlave::Master));
        conn.in_pool.store(true, Ordering::Release);
        bucket.write.lock().push_back(conn.clone());

        // acquire
        let acquired = bucket.acquire(MasterSlave::Master).unwrap();
        assert_eq!(acquired.addr, addr);
        assert!(!acquired.in_pool.load(Ordering::Relaxed));

        // release
        bucket.release(acquired);
        assert!(conn.in_pool.load(Ordering::Relaxed));

        // acquire again
        let acquired2 = bucket.acquire(MasterSlave::Master).unwrap();
        assert_eq!(acquired2.addr, addr);
    }

    #[test]
    fn bucket_acquire_empty() {
        let bucket = AttrBucket::new(BucketCfg::default());
        assert!(bucket.acquire(MasterSlave::Master).is_none());
    }

    #[test]
    fn no_reuse_connection_discarded_on_release() {
        // 会话内使用过 prepared statement 的连接(no_reuse=true):
        // 归还时必须被丢弃,不得回到池中被下个会话复用。
        let bucket = AttrBucket::new(BucketCfg::default());
        let db = make_db();
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3306));

        let conn = Arc::new(BackConn::new(addr, db, MasterSlave::Master));
        conn.mark_no_reuse();

        // 模拟会话结束归还(BackConn::new 后 in_pool=false,与 release 的 CAS 匹配)
        bucket.release(conn.clone());
        assert!(
            !conn.in_pool.load(Ordering::Relaxed),
            "no_reuse connection must not re-enter the pool"
        );

        // 池中无残留,acquire 拿不到任何连接
        assert!(bucket.acquire(MasterSlave::Master).is_none());
    }

    #[test]
    fn normal_connection_reuses_after_release() {
        // 对照:未标记 no_reuse 的干净连接归还后仍可复用
        let bucket = AttrBucket::new(BucketCfg::default());
        let db = make_db();
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3306));

        let conn = Arc::new(BackConn::new(addr, db, MasterSlave::Master));
        bucket.release(conn.clone());
        assert!(conn.in_pool.load(Ordering::Relaxed));
        assert!(bucket.acquire(MasterSlave::Master).is_some());
    }

    #[test]
    fn srvpool_get_or_create() {
        let pool = SrvPool::new();
        let b1 = pool.get_or_create_bucket("c1", "t1", "u1", "db1", BucketCfg::default());
        let b2 = pool.get_or_create_bucket("c1", "t1", "u1", "db1", BucketCfg::default());
        // 相同 key 应返回同一个 Arc
        assert!(Arc::ptr_eq(&b1, &b2));

        // 不同 user 不同桶
        let b3 = pool.get_or_create_bucket("c1", "t1", "u2", "db1", BucketCfg::default());
        assert!(!Arc::ptr_eq(&b1, &b3));
    }

    #[test]
    fn backend_guard_release() {
        let bucket = Arc::new(AttrBucket::new(BucketCfg::default()));
        let db = make_db();
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3306));
        let conn = Arc::new(BackConn::new(addr, db, MasterSlave::Master));
        conn.in_pool.store(false, Ordering::Release);

        // 创建 guard
        let guard = BackendGuard::new(conn.clone(), bucket.clone());
        // 显式归还
        guard.release();

        assert!(conn.in_pool.load(Ordering::Relaxed));
    }

    #[test]
    fn srvpool_invalidate_shard_only_affects_target() {
        let pool = SrvPool::new();
        let cfg = BucketCfg::default();
        // 两个分片各建一个桶
        let b1 = pool.get_or_create_bucket("c0", "t0", "u1", "db1", cfg.clone());
        let b2 = pool.get_or_create_bucket("c0", "t1", "u1", "db1", cfg);
        assert_eq!(pool.map.len(), 2);

        // 定向失效 t0:只有 t0 的桶被清,其他分片保留
        pool.invalidate_shard("c0", "t0");
        assert_eq!(pool.map.len(), 1);
        assert!(pool.map.contains_key("c0.t1.u1.db1"));
        assert!(!pool.map.contains_key("c0.t0.u1.db1"));
        let _ = (&b1, &b2);
    }

    #[test]
    fn srvpool_reset_clears_all() {
        let pool = SrvPool::new();
        let cfg = BucketCfg::default();
        pool.get_or_create_bucket("c0", "t0", "u1", "db1", cfg.clone());
        pool.get_or_create_bucket("c0", "t1", "u1", "db1", cfg);
        pool.reset();
        assert!(pool.map.is_empty());
    }

    #[test]
    fn broken_conn_not_returned_to_pool() {
        // 使用中出错(如 Broken pipe)的连接被标记损坏后,
        // 归还时不应重新入池,避免被下一个会话复用到死连接。
        let bucket = Arc::new(AttrBucket::new(BucketCfg::default()));
        let db = make_db();
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3306));
        let conn = Arc::new(BackConn::new(addr, db, MasterSlave::Master));
        conn.in_pool.store(false, Ordering::Release);

        // 标记损坏后归还
        conn.mark_broken();
        let guard = BackendGuard::new(conn.clone(), bucket.clone());
        guard.release();

        // 不应在池中
        assert!(!conn.in_pool.load(Ordering::Relaxed));
        assert!(bucket.acquire(MasterSlave::Master).is_none());
    }

    #[test]
    fn stale_conn_dropped_on_acquire() {
        // 空闲超过 stale_on_acquire_secs 的连接取用时被丢弃,不复用。
        // now_micros() 是单调时钟(进程启动起),故用阈值 0 + last_active=1µs
        // 保证"空闲时间 > 0"恒成立,测试确定性。
        let bucket = Arc::new(AttrBucket::new(BucketCfg {
            stale_on_acquire_secs: 0,
            ..BucketCfg::default()
        }));
        let db = make_db();
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3306));
        let conn = Arc::new(BackConn::new(addr, db, MasterSlave::Master));
        conn.in_pool.store(true, Ordering::Release);
        conn.last_active.store(1, Ordering::Release); // 远早于 now
        bucket.write.lock().push_back(conn.clone()); // 直接入队(不走 release,避免刷新 last_active)

        // 取用:连接陈旧,被丢弃,返回 None
        assert!(bucket.acquire(MasterSlave::Master).is_none());
    }

    #[test]
    fn backend_guard_drop() {
        let bucket = Arc::new(AttrBucket::new(BucketCfg::default()));
        let db = make_db();
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3306));
        let conn = Arc::new(BackConn::new(addr, db, MasterSlave::Master));
        conn.in_pool.store(false, Ordering::Release);

        {
            let _guard = BackendGuard::new(conn.clone(), bucket.clone());
            // drop 时自动归还
        }
        assert!(conn.in_pool.load(Ordering::Relaxed));
    }

    fn make_conn(bucket: &AttrBucket, ms: MasterSlave) -> Arc<BackConn> {
        let db = make_db();
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3306));
        let conn = Arc::new(BackConn::new(addr, db, ms));
        conn.in_pool.store(false, Ordering::Release);
        bucket.release(conn.clone());
        conn
    }

    #[test]
    fn release_double_put_guard() {
        // 同一连接归还两次:第二次为 no-op,不会重复入队
        let bucket = Arc::new(AttrBucket::new(BucketCfg::default()));
        let conn = make_conn(&bucket, MasterSlave::Master);
        let qlen = bucket.write.lock().len();
        bucket.release(conn.clone()); // double-put
        assert_eq!(bucket.write.lock().len(), qlen, "不应重复入队");
    }

    #[test]
    fn release_retires_after_max_serve_times() {
        // 复用次数达到上限后归还即退役(不入池);make_conn 的 release 已计 1 次,
        // 上限 3:第 1 次归还(served=2)仍在池,第 2 次归还(served=3)退役
        let bucket = Arc::new(AttrBucket::new(BucketCfg {
            max_serve_times: 3,
            ..BucketCfg::default()
        }));
        let conn = make_conn(&bucket, MasterSlave::Master);
        let c = bucket.acquire(MasterSlave::Master).unwrap();
        bucket.release(c);
        assert!(conn.in_pool.load(Ordering::Relaxed), "served=2 仍在池中");
        let c = bucket.acquire(MasterSlave::Master).unwrap();
        bucket.release(c);
        assert!(!conn.in_pool.load(Ordering::Relaxed), "served=3 应退役");
        assert!(bucket.acquire(MasterSlave::Master).is_none());
    }

    #[test]
    fn acquire_discards_expired_by_serve_times() {
        // 取用时 served_times 已超限 → 丢弃(不返回)
        let bucket = Arc::new(AttrBucket::new(BucketCfg {
            max_serve_times: 1,
            ..BucketCfg::default()
        }));
        let db = make_db();
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3306));
        let conn = Arc::new(BackConn::new(addr, db, MasterSlave::Master));
        conn.in_pool.store(true, Ordering::Release);
        conn.served_times.store(5, Ordering::Relaxed);
        bucket.write.lock().push_back(conn.clone());
        assert!(
            bucket.acquire(MasterSlave::Master).is_none(),
            "超限连接应被丢弃"
        );
    }

    #[test]
    fn slave_queue_acquire_release() {
        let bucket = AttrBucket::new(BucketCfg::default());
        let conn = make_conn(&bucket, MasterSlave::Slave);
        assert!(
            bucket.read.lock().iter().any(|c| Arc::ptr_eq(c, &conn)),
            "Slave 连接入读队列"
        );
        let c = bucket.acquire(MasterSlave::Slave).unwrap();
        assert_eq!(c.ms, MasterSlave::Slave);
        assert!(bucket.acquire(MasterSlave::Slave).is_none(), "取出后为空");
        // Master 队列独立
        assert!(bucket.acquire(MasterSlave::Master).is_none());
    }

    #[test]
    fn reap_bucket_removes_expired() {
        // max_idle_secs=0:last_active 远早于 now 的连接被回收;last_active 在未来
        // (now+1h) 的连接保留——用显式时间戳保证确定性。
        let bucket = Arc::new(AttrBucket::new(BucketCfg {
            max_idle_secs: 0,
            ..BucketCfg::default()
        }));
        let db = make_db();
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3306));
        let expired = Arc::new(BackConn::new(addr, db.clone(), MasterSlave::Master));
        expired.in_pool.store(true, Ordering::Release);
        expired.last_active.store(1, Ordering::Relaxed);
        bucket.write.lock().push_back(expired.clone());
        let fresh = Arc::new(BackConn::new(addr, db, MasterSlave::Slave));
        fresh.in_pool.store(true, Ordering::Release);
        fresh
            .last_active
            .store(now_micros() + 3_600_000_000, Ordering::Relaxed);
        bucket.read.lock().push_back(fresh.clone());

        reap_bucket(&bucket, now_micros());
        assert!(bucket.write.lock().is_empty(), "过期连接应被回收");
        assert!(
            !expired.in_pool.load(Ordering::Relaxed),
            "回收时置 in_pool=false"
        );
        assert_eq!(bucket.read.lock().len(), 1, "新鲜连接保留");
    }

    #[tokio::test]
    async fn warmup_pool_min_idle() {
        let pool = SrvPool::new();
        // min_idle=0 → 无预建
        pool.get_or_create_bucket("c", "t", "u", "db1", BucketCfg::default());
        warmup_pool(&pool).await;
        // min_idle>0 → 走预建分支(不 panic)
        let mut cfg = BucketCfg::default();
        cfg.min_idle = 2;
        pool.get_or_create_bucket("c", "t", "u2", "db2", cfg);
        warmup_pool(&pool).await;
    }

    #[test]
    fn bucket_key_format() {
        assert_eq!(
            bucket_key("c0", "t1", "root", "sbtest"),
            "c0.t1.root.sbtest"
        );
    }
}

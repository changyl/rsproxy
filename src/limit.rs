// 限流 + IP 黑白名单
// T4.5 实现:token bucket 限流器 + IP 认证检查

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::config::AppConfig;

// ─── Token Bucket 限流器 ───

/// Token bucket 限流器(线程安全,lock-free)
///
/// 原理:桶以恒定速率填充 token,每个请求消耗一个 token
pub struct TokenBucket {
    /// 当前 token 数(原子操作)
    tokens: AtomicU64,
    /// 最大 token 容量
    capacity: u64,
    /// 填充速率(tokens/秒)
    rate: u64,
    /// 上次填充时间(微秒)
    last_refill: AtomicU64,
}

impl TokenBucket {
    /// 创建 token bucket
    pub fn new(capacity: u64, rate: u64) -> Self {
        Self {
            tokens: AtomicU64::new(capacity),
            capacity,
            rate,
            last_refill: AtomicU64::new(now_micros()),
        }
    }

    /// 尝试获取一个 token。成功返回 true,失败(限流)返回 false
    pub fn try_acquire(&self) -> bool {
        self.refill();
        loop {
            let current = self.tokens.load(Ordering::Relaxed);
            if current == 0 {
                return false;
            }
            if self
                .tokens
                .compare_exchange(current, current - 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// 填充 token(按时间比例)
    fn refill(&self) {
        let now = now_micros();
        let last = self.last_refill.load(Ordering::Relaxed);
        let elapsed = now.saturating_sub(last);

        // 按速率计算新增 token
        let new_tokens = (elapsed * self.rate) / 1_000_000;
        if new_tokens == 0 {
            return;
        }

        // CAS 更新时间戳
        if self
            .last_refill
            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            // 填充 token(不超过容量)
            loop {
                let current = self.tokens.load(Ordering::Relaxed);
                let target = (current + new_tokens).min(self.capacity);
                if self
                    .tokens
                    .compare_exchange(current, target, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            }
        }
    }
}

// ─── IP 认证 ───

/// IP 认证检查器
pub struct IpChecker;

impl IpChecker {
    /// 检查 IP 是否在黑名单中
    pub fn is_blocked(config: &AppConfig, ip: Ipv4Addr) -> bool {
        config
            .ignore_ips
            .iter()
            .any(|(net_ip, mask)| ip_matches(ip, *net_ip, *mask))
    }

    /// 检查 IP 是否在白名单中,返回允许的用户列表(空=所有人)
    pub fn allowed_users(config: &AppConfig, ip: Ipv4Addr) -> Option<&[String]> {
        for auth_ip in &config.auth_ips {
            if ip_matches(ip, auth_ip.ip, auth_ip.mask) {
                return Some(&auth_ip.users);
            }
        }
        // 无匹配规则:允许所有用户
        None
    }

    /// 验证 IP 是否允许访问指定用户
    pub fn check_access(config: &AppConfig, ip: Ipv4Addr, username: &str) -> bool {
        // 黑名单优先
        if Self::is_blocked(config, ip) {
            return false;
        }

        // 白名单检查
        match Self::allowed_users(config, ip) {
            None => true,     // 无规则:允许所有人
            Some([]) => true, // 空列表:允许所有人
            Some(users) => users.iter().any(|u| u == username),
        }
    }
}

/// 检查 IP 是否匹配网段
fn ip_matches(ip: Ipv4Addr, net: Ipv4Addr, mask: u32) -> bool {
    if mask == 0 {
        return true;
    }
    if mask >= 32 {
        return ip == net;
    }
    let mask_bits = u32::MAX << (32 - mask);
    let ip_u32 = u32::from(ip);
    let net_u32 = u32::from(net);
    (ip_u32 & mask_bits) == (net_u32 & mask_bits)
}

fn now_micros() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_acquire() {
        let bucket = TokenBucket::new(10, 100); // 容量 10, 速率 100/s
                                                // 初始应能获取 token
        for _ in 0..10 {
            assert!(bucket.try_acquire());
        }
        // 桶空,不能再获取
        assert!(!bucket.try_acquire());
    }

    #[test]
    fn ip_matches_exact() {
        let ip = Ipv4Addr::new(10, 0, 0, 5);
        let net = Ipv4Addr::new(10, 0, 0, 5);
        assert!(ip_matches(ip, net, 32));
        assert!(!ip_matches(ip, Ipv4Addr::new(10, 0, 0, 6), 32));
    }

    #[test]
    fn ip_matches_subnet() {
        let ip = Ipv4Addr::new(10, 0, 0, 5);
        let net = Ipv4Addr::new(10, 0, 0, 0);
        assert!(ip_matches(ip, net, 24));
        assert!(!ip_matches(ip, Ipv4Addr::new(10, 0, 1, 0), 24));
    }

    #[test]
    fn ip_matches_any() {
        assert!(ip_matches(
            Ipv4Addr::new(192, 168, 1, 1),
            Ipv4Addr::new(0, 0, 0, 0),
            0
        ));
    }

    #[test]
    fn token_bucket_refills() {
        // 容量 10、速率 100/s:取空后等 300ms 应恢复 30 个(上限 10)
        let bucket = TokenBucket::new(10, 100);
        for _ in 0..10 {
            assert!(bucket.try_acquire());
        }
        assert!(!bucket.try_acquire(), "桶空");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut got = 0;
        while bucket.try_acquire() {
            got += 1;
        }
        assert_eq!(got, 10, "300ms 恢复 30 个但容量上限 10,应恰好取 10 个");
        assert!(!bucket.try_acquire(), "恢复后再次取空");
    }

    #[test]
    fn token_bucket_rate_limits() {
        // 容量 1、速率 1/s:立即取 1 个后为空
        let bucket = TokenBucket::new(1, 1);
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire());
        // 容量 0:恒失败
        let empty = TokenBucket::new(0, 100);
        assert!(!empty.try_acquire());
    }

    fn cfg_with_ips(auth: &[(Ipv4Addr, u32, Vec<String>)], ignore: &[(Ipv4Addr, u32)]) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.auth_ips = auth
            .iter()
            .map(|(ip, mask, users)| crate::config::model::AuthIp {
                ip: *ip,
                mask: *mask,
                users: users.clone(),
            })
            .collect();
        cfg.ignore_ips = ignore.to_vec();
        cfg
    }

    #[test]
    fn ip_checker_allow_deny() {
        // 黑名单优先:命中 ignore_ips 直接拒绝
        let cfg = cfg_with_ips(&[], &[(Ipv4Addr::new(10, 0, 0, 0), 24)]);
        assert!(IpChecker::is_blocked(&cfg, Ipv4Addr::new(10, 0, 0, 9)));
        assert!(!IpChecker::is_blocked(&cfg, Ipv4Addr::new(10, 0, 1, 9)), "网段外不拦截");
        assert!(!IpChecker::check_access(&cfg, Ipv4Addr::new(10, 0, 0, 9), "root"));

        // 无白名单规则:放行所有人
        let cfg = cfg_with_ips(&[], &[]);
        assert!(IpChecker::check_access(&cfg, Ipv4Addr::new(1, 2, 3, 4), "anyone"));
        assert!(IpChecker::allowed_users(&cfg, Ipv4Addr::new(1, 2, 3, 4)).is_none());

        // 白名单命中:仅允许列表内用户
        let cfg = cfg_with_ips(&[(Ipv4Addr::new(10, 0, 0, 0), 24, vec!["app".into()])], &[]);
        assert_eq!(
            IpChecker::allowed_users(&cfg, Ipv4Addr::new(10, 0, 0, 5)).unwrap(),
            &["app".to_string()]
        );
        assert!(IpChecker::check_access(&cfg, Ipv4Addr::new(10, 0, 0, 5), "app"));
        assert!(!IpChecker::check_access(&cfg, Ipv4Addr::new(10, 0, 0, 5), "other"));
        // 未命中白名单的 IP → 无规则 → 放行
        assert!(IpChecker::check_access(&cfg, Ipv4Addr::new(8, 8, 8, 8), "other"));

        // 空用户列表 → 放行所有人(仅限命中网段)
        let cfg = cfg_with_ips(&[(Ipv4Addr::new(10, 0, 0, 0), 24, vec![])], &[]);
        assert!(IpChecker::check_access(&cfg, Ipv4Addr::new(10, 0, 0, 1), "anyone"));
    }
}

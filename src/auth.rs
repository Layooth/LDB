//! 认证失败限速。
//!
//! 常量时间比较只消除时序侧信道，防不住「猜很多次」。这里按来源 IP 累计
//! **连续**失败：超过阈值后指数退避（30s 起、每次翻倍、上限 15 分钟），
//! 成功认证一次立刻清零。HTTP 返回 429 + `Retry-After`，LDBP 返回
//! `RATE_LIMITED` 错误码。
//!
//! 局限（写清楚免得误解）：这是单进程内存计数，重启即清零，也不跨进程共享；
//! 按 IP 计数会被 NAT/代理共享，但足以挡住脚本化的暴力枚举。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const BASE_BACKOFF_SECS: u64 = 30;
const MAX_BACKOFF_SECS: u64 = 15 * 60;
/// 最多跟踪的来源数，防止「换 IP 打爆内存」
const MAX_TRACKED: usize = 4096;
/// 多久没动静就丢弃该来源的记录
const IDLE_FORGET: Duration = Duration::from_secs(3600);

struct Entry {
    failures: u32,
    blocked_until: Option<Instant>,
    last_seen: Instant,
}

pub struct AuthLimiter {
    /// 允许的连续失败次数；0 = 关闭限速
    limit: u32,
    entries: Mutex<HashMap<IpAddr, Entry>>,
}

impl AuthLimiter {
    pub fn new(limit: u32) -> AuthLimiter {
        AuthLimiter {
            limit,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.limit > 0
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// 这次尝试是否放行；`Err(秒)` 表示还在退避窗口内
    pub fn check(&self, ip: IpAddr) -> Result<(), u64> {
        if !self.enabled() {
            return Ok(());
        }
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&ip) else {
            return Ok(());
        };
        entry.last_seen = Instant::now();
        match entry.blocked_until {
            Some(until) if until > Instant::now() => {
                let remain = until.duration_since(Instant::now());
                // 向上取整：剩 0.2 秒也报 1 秒，避免客户端立刻重试
                Err(remain.as_secs() + u64::from(remain.subsec_nanos() > 0))
            }
            Some(_) => {
                entry.blocked_until = None;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// 记一次失败；返回刚刚触发的退避秒数（若本次跨过阈值）
    pub fn record_failure(&self, ip: IpAddr) -> Option<u64> {
        if !self.enabled() {
            return None;
        }
        let now = Instant::now();
        let mut entries = self.lock();
        if entries.len() >= MAX_TRACKED && !entries.contains_key(&ip) {
            entries.retain(|_, entry| {
                entry.last_seen.elapsed() < IDLE_FORGET
                    || entry.blocked_until.map(|u| u > now).unwrap_or(false)
            });
        }
        let entry = entries.entry(ip).or_insert_with(|| Entry {
            failures: 0,
            blocked_until: None,
            last_seen: now,
        });
        entry.failures = entry.failures.saturating_add(1);
        entry.last_seen = now;
        if entry.failures <= self.limit {
            return None;
        }
        // 第 limit+1 次失败开始退避：30s、60s、120s ……
        let excess = (entry.failures - self.limit - 1).min(10);
        let secs = BASE_BACKOFF_SECS
            .saturating_mul(1u64 << excess)
            .min(MAX_BACKOFF_SECS);
        entry.blocked_until = Some(now + Duration::from_secs(secs));
        Some(secs)
    }

    /// 认证成功：清零该来源的失败计数
    pub fn record_success(&self, ip: IpAddr) {
        if !self.enabled() {
            return;
        }
        let mut entries = self.lock();
        if let Some(entry) = entries.get_mut(&ip) {
            entry.failures = 0;
            entry.blocked_until = None;
            entry.last_seen = Instant::now();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn allows_up_to_limit_then_backs_off() {
        let limiter = AuthLimiter::new(3);
        // 前 3 次失败不锁
        for _ in 0..3 {
            assert!(limiter.check(ip(1)).is_ok());
            assert_eq!(limiter.record_failure(ip(1)), None);
        }
        // 第 4 次开始退避 30 秒
        assert_eq!(limiter.record_failure(ip(1)), Some(30));
        let wait = limiter.check(ip(1)).unwrap_err();
        assert!((29..=30).contains(&wait), "退避窗口应为约 30 秒，实际 {}", wait);

        // 继续失败 → 翻倍
        assert_eq!(limiter.record_failure(ip(1)), Some(60));
        assert_eq!(limiter.record_failure(ip(1)), Some(120));
    }

    #[test]
    fn backoff_is_capped() {
        let limiter = AuthLimiter::new(1);
        let mut last = 0;
        for _ in 0..20 {
            if let Some(secs) = limiter.record_failure(ip(2)) {
                last = secs;
            }
        }
        assert_eq!(last, MAX_BACKOFF_SECS);
    }

    #[test]
    fn success_resets_counter() {
        let limiter = AuthLimiter::new(2);
        limiter.record_failure(ip(3));
        limiter.record_failure(ip(3));
        limiter.record_success(ip(3));
        assert!(limiter.check(ip(3)).is_ok());
        // 清零后可以再失败 2 次而不被锁
        assert_eq!(limiter.record_failure(ip(3)), None);
        assert_eq!(limiter.record_failure(ip(3)), None);
        assert_eq!(limiter.record_failure(ip(3)), Some(30));
    }

    #[test]
    fn sources_are_independent() {
        let limiter = AuthLimiter::new(1);
        limiter.record_failure(ip(4));
        assert_eq!(limiter.record_failure(ip(4)), Some(30));
        assert!(limiter.check(ip(4)).is_err());
        assert!(limiter.check(ip(5)).is_ok(), "其它来源不应受影响");
    }

    #[test]
    fn zero_disables_limiting() {
        let limiter = AuthLimiter::new(0);
        for _ in 0..1000 {
            assert_eq!(limiter.record_failure(ip(6)), None);
        }
        assert!(limiter.check(ip(6)).is_ok());
        assert!(!limiter.enabled());
    }
}

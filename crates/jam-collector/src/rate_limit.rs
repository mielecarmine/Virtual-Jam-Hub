//! Per-IP fixed-window rate limiter with bounded memory.

use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant},
};

const MAX_TRACKED_IPS: usize = 8192;

struct Window {
    started: Instant,
    count: u32,
}

/// Limits accepted requests per source IP within a rolling window.
pub struct RateLimiter {
    max_requests: u32,
    window: Duration,
    entries: HashMap<IpAddr, Window>,
}

impl RateLimiter {
    #[must_use]
    pub fn new(max_requests: u32, window: Duration) -> Self {
        Self {
            max_requests,
            window,
            entries: HashMap::new(),
        }
    }

    /// Returns `true` if `ip` is allowed at `now`, `false` if rate-limited.
    pub fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        if self.max_requests == 0 {
            return false;
        }
        if self.entries.len() >= MAX_TRACKED_IPS && !self.entries.contains_key(&ip) {
            self.entries
                .retain(|_, entry| now.duration_since(entry.started) < self.window);
            if self.entries.len() >= MAX_TRACKED_IPS {
                return false;
            }
        }
        let entry = self.entries.entry(ip).or_insert(Window {
            started: now,
            count: 0,
        });
        if now.duration_since(entry.started) >= self.window {
            entry.started = now;
            entry.count = 0;
        }
        if entry.count >= self.max_requests {
            return false;
        }
        entry.count += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn enforces_limit_per_ip_and_resets_after_window() {
        let mut limiter = RateLimiter::new(2, Duration::from_secs(60));
        let ip_a = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let ip_b = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
        let start = Instant::now();

        assert!(limiter.allow(ip_a, start));
        assert!(limiter.allow(ip_a, start + Duration::from_secs(10)));
        assert!(!limiter.allow(ip_a, start + Duration::from_secs(20)));
        assert!(limiter.allow(ip_b, start + Duration::from_secs(20)));
        assert!(limiter.allow(ip_a, start + Duration::from_secs(61)));
    }
}

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Simple fixed-window per-principal rate limiter.
///
/// Each principal gets a fresh window every `window_secs`. Within the window
/// the principal is allowed at most `max_requests` requests. This is
/// intentionally lightweight — no eviction background task, no persistence.
pub struct RateLimiter {
    inner: Mutex<HashMap<String, WindowEntry>>,
    max_requests: u32,
    window: Duration,
}

struct WindowEntry {
    count: u32,
    window_start: Instant,
}

impl RateLimiter {
    pub fn new(max_requests: u32, window_secs: u64) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_requests,
            window: Duration::from_secs(window_secs),
        }
    }

    /// Check whether `principal` is within its rate limit.
    ///
    /// Returns `Ok(())` on success (and increments the counter).
    /// Returns `Err(RateLimitExceeded)` when the window budget is exhausted.
    pub fn check(&self, principal: &str) -> Result<(), RateLimitExceeded> {
        let mut map = self.inner.lock().unwrap();
        let now = Instant::now();
        let entry = map
            .entry(principal.to_string())
            .or_insert_with(|| WindowEntry {
                count: 0,
                window_start: now,
            });

        if now.duration_since(entry.window_start) >= self.window {
            entry.count = 0;
            entry.window_start = now;
        }

        if entry.count >= self.max_requests {
            return Err(RateLimitExceeded);
        }
        entry.count += 1;
        Ok(())
    }
}

#[derive(Debug)]
pub struct RateLimitExceeded;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_limit() {
        let rl = RateLimiter::new(3, 60);
        assert!(rl.check("alice").is_ok());
        assert!(rl.check("alice").is_ok());
        assert!(rl.check("alice").is_ok());
        assert!(rl.check("alice").is_err());
    }

    #[test]
    fn different_principals_are_independent() {
        let rl = RateLimiter::new(1, 60);
        assert!(rl.check("alice").is_ok());
        assert!(rl.check("bob").is_ok());
        assert!(rl.check("alice").is_err());
        assert!(rl.check("bob").is_err());
    }
}

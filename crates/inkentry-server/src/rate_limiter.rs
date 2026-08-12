use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Ceiling on how many buckets the limiter will track at once.
///
/// Sized far above any plausible team (one bucket per principal × client
/// address), so a deployment never reaches it in normal use; it exists so the
/// map has a bound at all rather than growing with whatever traffic arrives.
pub const DEFAULT_MAX_BUCKETS: usize = 10_000;

/// Simple fixed-window rate limiter, keyed on caller identity.
///
/// Each key gets a fresh window every `window_secs`, and within the window at
/// most `max_requests` requests. Deliberately lightweight — in-process, no
/// persistence — but not unbounded: entries whose window has expired are swept
/// (at most once per window, and on insert pressure), and the map is hard-capped
/// at `max_buckets` so a burst of distinct callers cannot pin memory for the
/// lifetime of the process.
pub struct RateLimiter {
    inner: Mutex<Buckets>,
    max_requests: u32,
    window: Duration,
    max_buckets: usize,
}

struct Buckets {
    entries: HashMap<String, WindowEntry>,
    /// When the last full expiry sweep ran, so the sweep costs O(n) once per
    /// window instead of once per request.
    last_sweep: Instant,
}

struct WindowEntry {
    count: u32,
    window_start: Instant,
}

impl RateLimiter {
    pub fn new(max_requests: u32, window_secs: u64) -> Self {
        Self::with_window(max_requests, Duration::from_secs(window_secs))
    }

    pub fn with_window(max_requests: u32, window: Duration) -> Self {
        Self {
            inner: Mutex::new(Buckets {
                entries: HashMap::new(),
                last_sweep: Instant::now(),
            }),
            max_requests,
            window,
            max_buckets: DEFAULT_MAX_BUCKETS,
        }
    }

    pub fn with_max_buckets(mut self, max_buckets: usize) -> Self {
        self.max_buckets = max_buckets.max(1);
        self
    }

    /// How many buckets are currently held. Exposed so tests can assert the
    /// map stays bounded.
    pub fn tracked_buckets(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    /// Check whether `key` is within its rate limit.
    ///
    /// Returns `Ok(())` on success (and increments the counter).
    /// Returns `Err(RateLimitExceeded)` when the window budget is exhausted.
    pub fn check(&self, key: &str) -> Result<(), RateLimitExceeded> {
        let mut buckets = self.inner.lock().unwrap();
        let now = Instant::now();

        if now.duration_since(buckets.last_sweep) >= self.window {
            self.sweep_expired(&mut buckets, now);
        }

        if !buckets.entries.contains_key(key) && buckets.entries.len() >= self.max_buckets {
            self.sweep_expired(&mut buckets, now);
            // Still full: every tracked window is live, so make room by
            // dropping the one closest to expiring anyway. Serving a new
            // caller matters more than the last moments of the oldest window.
            while buckets.entries.len() >= self.max_buckets {
                let Some(oldest) = buckets
                    .entries
                    .iter()
                    .min_by_key(|(_, e)| e.window_start)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                buckets.entries.remove(&oldest);
            }
        }

        let entry = buckets
            .entries
            .entry(key.to_string())
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

    /// Drop every bucket whose window has already elapsed. Such a bucket is
    /// indistinguishable from a bucket that was never created: the next request
    /// on that key would reset it anyway.
    fn sweep_expired(&self, buckets: &mut Buckets, now: Instant) {
        buckets
            .entries
            .retain(|_, e| now.duration_since(e.window_start) < self.window);
        buckets.last_sweep = now;
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

    #[test]
    fn expired_windows_are_swept_instead_of_retained_forever() {
        let rl = RateLimiter::with_window(1, Duration::from_millis(20));
        for i in 0..50 {
            assert!(rl.check(&format!("client-{i}")).is_ok());
        }
        assert_eq!(rl.tracked_buckets(), 50);

        std::thread::sleep(Duration::from_millis(40));

        // Any request after the window elapses triggers the sweep, which
        // reclaims the 50 dead buckets and leaves only this caller's.
        assert!(rl.check("late-arrival").is_ok());
        assert_eq!(
            rl.tracked_buckets(),
            1,
            "windows that have elapsed must not be retained for the process lifetime"
        );
    }

    #[test]
    fn map_stays_bounded_under_a_flood_of_distinct_keys() {
        let rl = RateLimiter::new(1, 3600).with_max_buckets(16);
        for i in 0..5_000 {
            let _ = rl.check(&format!("client-{i}"));
        }
        assert!(
            rl.tracked_buckets() <= 16,
            "bucket map must stay within its cap, got {}",
            rl.tracked_buckets()
        );
    }

    #[test]
    fn a_flood_of_new_keys_still_gets_served() {
        let rl = RateLimiter::new(1, 3600).with_max_buckets(4);
        for i in 0..100 {
            assert!(
                rl.check(&format!("client-{i}")).is_ok(),
                "eviction must make room for a new caller, not refuse it"
            );
        }
    }
}

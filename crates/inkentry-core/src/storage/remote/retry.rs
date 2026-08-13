//! Client-side handling for a server that sheds a write with `429`.
//!
//! Every memory write that makes the server embed — `POST /memory` and
//! `POST /memory/batch` — runs under the server's bounded embed admission
//! queue, so a write can come back `429 Too Many Requests` with a
//! `Retry-After` instead of being queued. That condition is transient and
//! self-clearing: the permit is released the moment the in-flight embed
//! finishes. Failing the caller on the first one would abort a whole
//! `inkentry sync` over a few hundred milliseconds of server contention.
//!
//! Mirrors the `429` handling `inkentry index`'s embed phase already applies
//! to `POST /index/embed`, with a far smaller retry budget: a sync is one
//! interactive command a user is waiting on, not a long background pass, so it
//! must give up and report rather than sit indefinitely on a saturated server.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};

/// Wait before retrying a `429` that carries no usable `Retry-After`. Matches
/// the server's own `EMBED_BUSY_RETRY_AFTER_SECS`, so a server that omits the
/// header is retried on the cadence it would have asked for anyway.
const DEFAULT_SATURATION_RETRY: Duration = Duration::from_secs(5);

/// Total sends per request, the first attempt included — so three retries.
/// Deliberately small: the admission queue drains in embed-time, so a write
/// that is still shed on the fourth try is contending with something that will
/// not clear in the seconds an interactive `sync` may spend waiting.
const SATURATION_ATTEMPTS: usize = 4;

/// Ceiling on the cumulative sleeping one request may do across its retries.
/// [`SATURATION_ATTEMPTS`] alone bounds nothing in wall-clock terms, since the
/// wait comes from a header the server controls; this bounds the case of a
/// server asking for a wait far longer than a user would sit through.
const SATURATION_WAIT_BUDGET: Duration = Duration::from_secs(60);

/// How hard a request retries a `429` before it becomes a caller-visible error.
#[derive(Debug, Clone, Copy)]
pub(super) struct RetryPolicy {
    attempts: usize,
    fallback: Duration,
    wait_budget: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: SATURATION_ATTEMPTS,
            fallback: DEFAULT_SATURATION_RETRY,
            wait_budget: SATURATION_WAIT_BUDGET,
        }
    }
}

impl RetryPolicy {
    /// A policy with the waits collapsed, so a test can exercise the retry
    /// path without spending the production cadence in real sleeps.
    #[cfg(test)]
    pub(super) fn immediate(attempts: usize) -> Self {
        Self {
            attempts,
            fallback: Duration::from_millis(1),
            wait_budget: SATURATION_WAIT_BUDGET,
        }
    }
}

/// Send `request`, retrying while the server answers `429`.
///
/// `send` is called afresh per attempt because a `reqwest` request is consumed
/// by sending it; the closure rebuilds and re-serialises the same body.
/// Anything other than a `429` — success, or any other error status — is
/// returned to the caller untouched, so status handling stays where it was.
///
/// `label` names the request in both the retry notice and the give-up error
/// (`"POST /memory/batch"`).
pub(super) async fn send_retrying_while_shed<F, Fut>(
    policy: &RetryPolicy,
    label: &str,
    send: F,
) -> Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: Future<Output = reqwest::Result<reqwest::Response>>,
{
    let mut slept = Duration::ZERO;
    let mut attempt = 1usize;
    loop {
        let resp = send().await.with_context(|| label.to_string())?;
        if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(resp);
        }
        let wait = retry_after(&resp).unwrap_or(policy.fallback);
        if attempt >= policy.attempts || slept + wait > policy.wait_budget {
            anyhow::bail!(
                "{label} was shed by the server (429: its embed queue is full) and was still \
                 being shed after {attempt} attempt(s) — re-run once the server has caught up"
            );
        }
        // The user is sitting in front of an `inkentry sync`; without this the
        // wait is indistinguishable from a hang.
        eprintln!(
            "{label}: server busy (429), retrying in {:.1}s (attempt {}/{})…",
            wait.as_secs_f32(),
            attempt + 1,
            policy.attempts,
        );
        slept += wait;
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

/// The response's `Retry-After` as whole seconds. `None` when the header is
/// absent or is not a plain integer — inkentry servers only ever send
/// delta-seconds, never the HTTP-date form the RFC also allows.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The production policy is the one every non-test caller gets, and its
    // whole point is being bounded: a regression to an unbounded or
    // minutes-long budget would turn a saturated server into a hung `sync`.
    #[test]
    fn production_policy_is_bounded_and_modest() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.attempts, 4);
        assert_eq!(policy.fallback, Duration::from_secs(5));
        assert!(
            policy.wait_budget <= Duration::from_secs(60),
            "an interactive sync must not sit on a saturated server for minutes"
        );
    }
}

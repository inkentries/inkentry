//! Origins whose connection attempt recently failed.
//!
//! One command can ask several independent subsystems to reach the same
//! server: a capability probe, an embed, a dialect probe, then the request the
//! user actually asked for. Each one has its own client and its own fallback,
//! so against an absent server each spends a full connect timeout rediscovering
//! the same fact, and the user waits for the sum.
//!
//! This records that a connect to an origin failed, so the attempts after the
//! first can skip straight to the conclusion the first one reached.
//!
//! # This is a latency memo, never a routing input
//!
//! Consult it **only** to skip a redundant connection attempt. Never let it
//! decide where memory is read or written, which backend is opened, or what
//! mode is in effect. Under `cloud_first` the store of record is chosen from
//! the resolved mode and `server_url` alone, and that independence from any
//! notion of "we think we are offline" is what makes the no-silent-fallback
//! guarantee true by construction rather than by care. A caller that skips an
//! attempt must still fail exactly as it would have failed had it attempted,
//! never quietly serve something else instead.
//!
//! # Why entries expire
//!
//! Expiry is what makes the memo safe, and it is load-bearing rather than
//! housekeeping. A recorded miss is a claim about one moment, and the process
//! it lives in is not always short: the detached index worker polls a server's
//! readiness for as long as a model download takes, precisely so it can watch a
//! server that is not up yet come up. A memo that never expired would let one
//! refused poll stand in for every later one, and the worker would abandon
//! durable queued work for a server that came back seconds later.
//!
//! So an entry is worth only [`MEMO_TTL`]: long enough that the remaining
//! attempts of one short command all land inside it, which is the whole point,
//! and short enough that a poller re-attempts on a later iteration. Nothing
//! refreshes an entry on a skipped attempt, so a run of skips cannot extend the
//! window indefinitely.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a recorded miss is worth acting on.
///
/// Twice the connect bound: the attempts of a single command follow each other
/// within one connect timeout or so, while anything that waits longer than this
/// between attempts is a poller, which must be allowed to see the server come
/// back.
pub const MEMO_TTL: Duration =
    Duration::from_secs(crate::config::REMOTE_CONNECT_TIMEOUT.as_secs() * 2);

fn memo() -> &'static Mutex<HashMap<String, Instant>> {
    static MEMO: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Every caller keys off the same configured server URL, so normalising the
/// trailing slash is all that is needed to make the entries line up.
fn key(base_url: &str) -> &str {
    base_url.trim_end_matches('/')
}

/// Record that a connection attempt to `base_url` failed because the server
/// could not be reached. Only ever called for a genuine connect-stage failure,
/// never for a server that answered.
pub fn record_connect_failure(base_url: &str) {
    record_at(base_url, Instant::now());
}

fn record_at(base_url: &str, at: Instant) {
    if let Ok(mut memo) = memo().lock() {
        memo.insert(key(base_url).to_string(), at);
    }
}

/// Whether a connection to `base_url` failed recently enough to act on.
///
/// A `true` answer licenses skipping another attempt and reporting the same
/// failure the first attempt produced. It licenses nothing else: see the
/// module docs.
pub fn connect_already_failed(base_url: &str) -> bool {
    memo().lock().is_ok_and(|memo| {
        memo.get(key(base_url))
            .is_some_and(|at| at.elapsed() < MEMO_TTL)
    })
}

/// Record a miss as though it had happened `age` ago, so a test can exercise
/// expiry without spending the wall-clock time.
#[cfg(any(test, feature = "test-support"))]
pub fn record_connect_failure_aged(base_url: &str, age: Duration) {
    let at = Instant::now()
        .checked_sub(age)
        .expect("an age that predates the process start");
    record_at(base_url, at);
}

/// Drop every recorded origin, so one test's failure cannot leak into another
/// through the process-wide memo.
#[cfg(any(test, feature = "test-support"))]
pub fn clear_for_test() {
    if let Ok(mut memo) = memo().lock() {
        memo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // The memo is process-wide, so these run one at a time and start clean.

    #[test]
    #[serial(reachability_memo)]
    fn an_origin_is_unknown_until_a_connect_to_it_fails() {
        clear_for_test();
        assert!(!connect_already_failed("https://server.example:4655"));
        record_connect_failure("https://server.example:4655");
        assert!(connect_already_failed("https://server.example:4655"));
    }

    #[test]
    #[serial(reachability_memo)]
    fn a_failure_against_one_origin_says_nothing_about_another() {
        clear_for_test();
        record_connect_failure("https://a.example:4655");
        assert!(
            !connect_already_failed("https://b.example:4655"),
            "one server being absent must never imply anything about a different one"
        );
    }

    #[test]
    #[serial(reachability_memo)]
    fn a_trailing_slash_addresses_the_same_origin() {
        // Callers read the same configured URL from different places, and some
        // trim it while others do not; a miss here would silently cost the
        // extra connect attempt this exists to skip.
        clear_for_test();
        record_connect_failure("https://server.example:4655/");
        assert!(connect_already_failed("https://server.example:4655"));
    }

    #[test]
    #[serial(reachability_memo)]
    fn an_entry_stops_counting_once_it_is_older_than_the_ttl() {
        // What keeps a long-lived poller able to see a server come back.
        clear_for_test();
        let url = "https://server.example:4655";
        // A literal, not an offset from the constant under test: the latter
        // would be expired for any value of MEMO_TTL and so could not see its
        // magnitude at all.
        record_connect_failure_aged(url, Duration::from_secs(5));
        assert!(
            !connect_already_failed(url),
            "a stale miss must not stand in for an attempt that was never made"
        );
    }

    #[test]
    #[serial(reachability_memo)]
    fn an_entry_still_counts_just_inside_the_ttl() {
        // The other half: expiry must not be so eager that the attempts of one
        // command stop skipping, which is the whole reason the memo exists.
        clear_for_test();
        let url = "https://server.example:4655";
        record_connect_failure_aged(url, Duration::from_secs(1));
        assert!(connect_already_failed(url));
    }

    #[test]
    #[serial(reachability_memo)]
    fn the_ttl_outlives_a_single_connect_attempt() {
        assert!(
            MEMO_TTL > crate::config::REMOTE_CONNECT_TIMEOUT,
            "an entry must outlive the attempt that recorded it, or it buys nothing"
        );
    }
}

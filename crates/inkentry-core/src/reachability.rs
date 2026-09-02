//! Origins whose connection attempt already failed in this process.
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
//! Deliberately process-lifetime and never cleared: a single command is the
//! whole window, so an entry cannot go stale enough to matter, and a later
//! command starts with an empty memo and re-attempts normally.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

fn memo() -> &'static Mutex<HashSet<String>> {
    static MEMO: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashSet::new()))
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
    if let Ok(mut memo) = memo().lock() {
        memo.insert(key(base_url).to_string());
    }
}

/// Whether a connection to `base_url` already failed in this process.
///
/// A `true` answer licenses skipping another attempt and reporting the same
/// failure the first attempt produced. It licenses nothing else: see the
/// module docs.
pub fn connect_already_failed(base_url: &str) -> bool {
    memo().lock().is_ok_and(|memo| memo.contains(key(base_url)))
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
}

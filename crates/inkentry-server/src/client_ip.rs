//! Resolving the address a request actually came from.
//!
//! The answer feeds the rate limiter's bucket key, so it decides how much of
//! the operator's LLM budget one caller may burn (ADR-002 makes that limit a
//! binding requirement). Anything a client can set at will is therefore
//! unusable as an identity: a caller who can choose its own key can mint a
//! fresh budget per request and the limit stops existing.
//!
//! `X-Forwarded-For` is exactly that — a request header, writable by whoever
//! opened the connection. ADR-066 gave the server in-process TLS precisely so
//! that a team-reachable deployment is first-party and proxy-free, so in the
//! ratified shape there is no proxy in front and nothing legitimate to trust.
//! The header is therefore ignored unless the operator has named the peer as a
//! trusted proxy, and even then only a syntactically valid IP is accepted.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::http::HeaderMap;

/// Peers whose `X-Forwarded-For` header this server will believe.
///
/// Empty by default, which is the ADR-066 deployment: no proxy in front, so
/// the forwarded header carries no authority and the TCP peer is the client.
#[derive(Clone, Default)]
pub struct TrustedProxies(Arc<[IpAddr]>);

impl TrustedProxies {
    /// Entries are canonicalised on the way in, so a configured `10.0.0.5`
    /// still matches a proxy that reaches a dual-stack bind (`--host ::`) over
    /// IPv4 and therefore presents as `::ffff:10.0.0.5`. Without it the two
    /// spellings never compare equal and the proxy silently loses its trust.
    pub fn new(addrs: impl IntoIterator<Item = IpAddr>) -> Self {
        Self(addrs.into_iter().map(|a| a.to_canonical()).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn addrs(&self) -> &[IpAddr] {
        &self.0
    }

    fn trusts(&self, peer: IpAddr) -> bool {
        self.0.contains(&peer.to_canonical())
    }
}

/// The client address to attribute this request to, as a rate-limiter key
/// fragment.
///
/// The TCP peer wins unless the peer is a configured trusted proxy *and* sent a
/// parseable `X-Forwarded-For` entry. Requests with no peer at all (in-process
/// test routers) collapse onto one shared bucket rather than escaping the limit.
///
/// The address is canonicalised, so one client reaching a dual-stack bind over
/// both stacks gets one budget rather than two: `::ffff:10.0.0.5` *is*
/// `10.0.0.5`, and a v4-mapped address is a socket-API spelling that cannot
/// appear as a source address on the wire, so folding the two cannot merge two
/// distinct callers.
pub fn client_ip_key(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted: &TrustedProxies,
) -> String {
    let peer_ip = peer.map(|addr| addr.ip());

    if let Some(peer_ip) = peer_ip
        && trusted.trusts(peer_ip)
        && let Some(forwarded) = forwarded_client_ip(headers)
    {
        return forwarded.to_canonical().to_string();
    }

    match peer_ip {
        Some(ip) => ip.to_canonical().to_string(),
        None => "unknown".to_string(),
    }
}

/// The *trailing* `X-Forwarded-For` entry, if it parses as an IP address.
///
/// Rightmost rather than leftmost, because it is the only choice that is
/// correct under both ways a proxy can be configured to set the header:
///
/// - Appending (nginx's common `$proxy_add_x_forwarded_for`) keeps whatever the
///   client sent and adds the address the proxy actually saw. A client sending
///   `9.9.9.9` arrives as `9.9.9.9, <real client>`, so everything left of the
///   last entry is attacker-chosen and only the last entry is observed fact.
/// - Overwriting (`$remote_addr`) leaves exactly one entry, where rightmost and
///   leftmost are the same value.
///
/// Taking the leftmost entry would therefore reopen the spoofing bug for any
/// operator whose proxy appends — the majority default.
///
/// The trust config names the immediate peer, so a chain of two or more trusted
/// proxies would resolve to the inner proxy rather than the originating client.
/// That is out of scope here, and it fails safe: the key is a proxy address, not
/// one the caller chose.
///
/// Rejecting non-IP text is load-bearing beyond tidiness: the returned value
/// becomes a rate-limiter map key, and an unparsed header would let a caller
/// choose keys of arbitrary length and cardinality.
fn forwarded_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())?
        .split(',')
        .next_back()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(s: &str) -> Option<SocketAddr> {
        Some(s.parse().expect("test peer address"))
    }

    fn headers_with_xff(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", value.parse().expect("header value"));
        headers
    }

    #[test]
    fn untrusted_peer_forwarded_header_is_ignored() {
        let trusted = TrustedProxies::default();
        let key = client_ip_key(
            &headers_with_xff("203.0.113.7"),
            peer("198.51.100.4:5555"),
            &trusted,
        );
        assert_eq!(
            key, "198.51.100.4",
            "with no trusted proxy configured the TCP peer is the client, not the header"
        );
    }

    #[test]
    fn forged_header_values_all_collapse_onto_the_peer() {
        let trusted = TrustedProxies::default();
        let a = client_ip_key(
            &headers_with_xff("10.0.0.1"),
            peer("198.51.100.4:5555"),
            &trusted,
        );
        let b = client_ip_key(
            &headers_with_xff("10.0.0.2"),
            peer("198.51.100.4:5556"),
            &trusted,
        );
        assert_eq!(
            a, b,
            "varying the header must not move a caller to a different bucket"
        );
    }

    #[test]
    fn trusted_peer_forwarded_header_is_honoured() {
        let trusted = TrustedProxies::new(["198.51.100.4".parse().unwrap()]);
        let key = client_ip_key(
            &headers_with_xff("203.0.113.7"),
            peer("198.51.100.4:5555"),
            &trusted,
        );
        assert_eq!(key, "203.0.113.7");
    }

    #[test]
    fn trusted_peer_takes_the_trailing_entry() {
        let trusted = TrustedProxies::new(["198.51.100.4".parse().unwrap()]);
        let key = client_ip_key(
            &headers_with_xff("203.0.113.7, 198.51.100.9"),
            peer("198.51.100.4:5555"),
            &trusted,
        );
        assert_eq!(key, "198.51.100.9");
    }

    // nginx's `$proxy_add_x_forwarded_for` appends rather than overwrites, so a
    // client that sends its own header keeps the leading entry. Reading the
    // leftmost value would hand that client the bucket key.
    #[test]
    fn an_appending_proxy_keys_on_the_address_the_proxy_saw() {
        let trusted = TrustedProxies::new(["198.51.100.4".parse().unwrap()]);
        let key = client_ip_key(
            &headers_with_xff("9.9.9.9, 10.0.0.5"),
            peer("198.51.100.4:5555"),
            &trusted,
        );
        assert_eq!(
            key, "10.0.0.5",
            "the entry the proxy appended is the client; the one before it is attacker-supplied"
        );
    }

    #[test]
    fn a_client_prefixed_chain_cannot_move_itself_between_buckets() {
        let trusted = TrustedProxies::new(["198.51.100.4".parse().unwrap()]);
        let a = client_ip_key(
            &headers_with_xff("1.1.1.1, 10.0.0.5"),
            peer("198.51.100.4:5555"),
            &trusted,
        );
        let b = client_ip_key(
            &headers_with_xff("2.2.2.2, 3.3.3.3, 10.0.0.5"),
            peer("198.51.100.4:5556"),
            &trusted,
        );
        assert_eq!(a, b, "only the proxy-appended tail may decide the bucket");
    }

    #[test]
    fn a_v4_mapped_peer_matches_a_v4_configured_proxy() {
        let trusted = TrustedProxies::new(["10.0.0.5".parse().unwrap()]);
        let key = client_ip_key(
            &headers_with_xff("203.0.113.7"),
            peer("[::ffff:10.0.0.5]:5555"),
            &trusted,
        );
        assert_eq!(
            key, "203.0.113.7",
            "a proxy reaching a dual-stack bind over IPv4 must keep the trust it was configured with"
        );
    }

    #[test]
    fn a_v4_mapped_peer_shares_one_bucket_with_its_v4_spelling() {
        let trusted = TrustedProxies::default();
        let mapped = client_ip_key(&HeaderMap::new(), peer("[::ffff:10.0.0.5]:5555"), &trusted);
        let plain = client_ip_key(&HeaderMap::new(), peer("10.0.0.5:5555"), &trusted);
        assert_eq!(
            mapped, plain,
            "one client reaching a dual-stack server over both stacks must not get two budgets"
        );
    }

    #[test]
    fn a_trusted_proxy_entry_that_is_not_an_ip_falls_back_to_the_peer() {
        let trusted = TrustedProxies::new(["198.51.100.4".parse().unwrap()]);
        let junk = "A".repeat(200);
        let key = client_ip_key(
            &headers_with_xff(&junk),
            peer("198.51.100.4:5555"),
            &trusted,
        );
        assert_eq!(
            key, "198.51.100.4",
            "an unparseable forwarded value must not become a map key"
        );
    }

    #[test]
    fn forwarded_ip_is_canonicalised_not_echoed() {
        let trusted = TrustedProxies::new(["198.51.100.4".parse().unwrap()]);
        let spaced = client_ip_key(
            &headers_with_xff("  2001:db8:0:0:0:0:0:1  "),
            peer("198.51.100.4:1"),
            &trusted,
        );
        let compact = client_ip_key(
            &headers_with_xff("2001:db8::1"),
            peer("198.51.100.4:1"),
            &trusted,
        );
        assert_eq!(
            spaced, compact,
            "two spellings of one address must share a bucket"
        );
    }

    #[test]
    fn missing_peer_and_header_share_one_bucket() {
        let trusted = TrustedProxies::default();
        assert_eq!(client_ip_key(&HeaderMap::new(), None, &trusted), "unknown");
        assert_eq!(
            client_ip_key(&headers_with_xff("10.0.0.1"), None, &trusted),
            "unknown",
            "a peerless request must not be able to name itself"
        );
    }
}

// Every outbound call in this workspace goes through reqwest (no raw
// std::net/socket use, no `Client::builder().no_proxy()` call site), so
// pointing HTTP_PROXY/HTTPS_PROXY/ALL_PROXY at a sink we control, with
// NO_PROXY carving out loopback, turns every call into an observable event
// without touching production code. Pure userspace, so it behaves the same on
// every platform.
//
// NO_PROXY matches by hostname only, not host:port (verified: a
// NO_PROXY=127.0.0.1:<port> entry does not stop proxying to a different
// 127.0.0.1:<other_port>). So this proves nothing left the loopback interface,
// not that nothing hit an unintended loopback port — each test must keep its
// loopback surface to exactly the mock server(s) it starts, so a wrong port
// fails outright instead of passing silently.

use assert_cmd::Command;
use wiremock::MockServer;

// The Host/CONNECT-authority header on a proxied Request survives even though
// wiremock never completes the CONNECT tunnel; it's the only reliable way to
// name the destination for both plain HTTP and HTTPS-via-CONNECT requests.
fn destination(r: &wiremock::Request) -> String {
    r.headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} {}", r.method, r.url))
}

// Every non-loopback HTTP(S) call a wired `Command` makes is funneled here
// instead of reaching the real network.
pub struct EgressTrap {
    sink: MockServer,
}

impl EgressTrap {
    pub async fn start() -> Self {
        Self {
            sink: MockServer::start().await,
        }
    }

    // http:// URL of the trap's sink, so a caller can apply the same proxy env
    // vars `wire()` sets to the current process instead.
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.sink.address())
    }

    // Routes everything except loopback through this trap. Sets both upper-
    // and lower-case proxy vars since libraries disagree on which case they
    // read.
    pub fn wire(&self, cmd: &mut Command) {
        let proxy = self.proxy_url();
        for var in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            cmd.env(var, &proxy);
        }
        // Hostnames only, not host:port (see module doc), so tests must keep
        // their loopback surface to exactly the mock server(s) they start.
        for var in ["NO_PROXY", "no_proxy"] {
            cmd.env(var, "127.0.0.1,localhost,::1");
        }
    }

    // Panics naming every destination seen, not just "test failed".
    pub async fn assert_clean(&self) {
        let seen = self.sink.received_requests().await.expect(
            "wiremock request journaling must stay enabled (default for MockServer::start())",
        );
        assert!(
            seen.is_empty(),
            "egress trap caught {} unexpected non-loopback connection attempt(s): [{}]",
            seen.len(),
            seen.iter().map(destination).collect::<Vec<_>>().join(", "),
        );
    }

    // Same as assert_clean, but returns the destinations instead of panicking,
    // for the self-test that proves a rogue call is actually caught.
    pub async fn destinations_seen(&self) -> Vec<String> {
        self.sink
            .received_requests()
            .await
            .expect(
                "wiremock request journaling must stay enabled (default for MockServer::start())",
            )
            .iter()
            .map(destination)
            .collect()
    }
}

// Feeds loopback auto-discovery's fixed-port fallback (step 3b) via
// `INKENTRY_TEST_DISCOVERY_PORT`, so a mock on `url` stands in for the
// auto-discovered inference server — distinct from an explicit `server_url`.
//
// Not step 3a's `server.port` file: that step only responds when the pid
// beside the port is a live `inkentry-server` reporting the recorded instance
// id, which a wiremock stand-in cannot fake, and fabricating the file here
// would leave the command with no server at all.
pub fn loopback_discovery_port(url: &str) -> String {
    url.rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .to_string()
}

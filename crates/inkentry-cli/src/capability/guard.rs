use super::tier::Tier;

// Fires only with server_url unset (client construction succeeds whenever it is set),
// so it never advises server_url.
pub fn inference_server_required_message(feature: &str) -> String {
    format!(
        "'inkentry {feature}' requires inkentry-server.\n\
         Run `inkentry server start` to enable this feature."
    )
}

pub fn require_tier1(feature: &str, tier: &Tier, server_url: Option<&str>) -> anyhow::Result<()> {
    if tier.is_server() {
        return Ok(());
    }
    match server_url {
        Some(url) => anyhow::bail!(
            "'inkentry {feature}' requires inkentry-server.\n\
             The configured server_url ({url}) did not respond to the health probe.\n\
             Check that server and your network; for TLS trust failures see \
             server_ca / INKENTRY_SERVER_CA."
        ),
        None => anyhow::bail!(
            "'inkentry {feature}' requires inkentry-server.\n\
             Set server_url in ~/.config/inkentry/config.toml to enable this feature."
        ),
    }
}

// Tier::Server alone cannot tell an auto-discovered loopback server (never a memory store)
// from a configured one, so callers run require_tier1 first and this confirms the config.
// Never probes reachability.
pub fn require_explicit_server_url(
    feature: &str,
    cfg: &crate::config::Config,
) -> anyhow::Result<String> {
    cfg.server_url.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "'inkentry {feature}' requires a server. Set `server_url` in your inkentry config \
             (e.g. ~/.config/inkentry/config.toml or .inkentry/config.toml)."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::diagnostics::OfflineReason;
    use super::super::state::{Capabilities, EmbedderState};
    use super::*;
    use crate::config::Config;

    #[test]
    fn inference_msg_no_server_url_points_at_local_start_only() {
        let msg = inference_server_required_message("memory search");
        assert!(msg.contains("'inkentry memory search' requires inkentry-server"));
        assert!(
            msg.contains("inkentry server start"),
            "must point at the local auto-server: {msg}"
        );
        assert!(
            !msg.contains("server_url"),
            "must NOT mention server_url when none is configured: {msg}"
        );
    }

    #[test]
    fn inference_msg_interpolates_feature_and_keeps_harvest_substring() {
        let msg = inference_server_required_message("harvest");
        assert!(msg.contains("'inkentry harvest' requires inkentry-server"));
    }

    #[test]
    fn require_tier1_ok_for_server() {
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(require_tier1("memory search", &tier, Some("http://example.com")).is_ok());
    }

    #[test]
    fn require_tier1_err_for_offline_no_url() {
        let tier = Tier::Offline(OfflineReason::NoLocalServer);
        let err = require_tier1("memory search", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'inkentry memory search'"));
        assert!(msg.contains("requires inkentry-server"));
        assert!(msg.contains("Set server_url"));
    }

    #[test]
    fn require_tier1_err_for_offline_with_url_names_that_server() {
        let tier = Tier::Offline(OfflineReason::NoLocalServer);
        let err = require_tier1("plan", &tier, Some("https://bad:4655")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'inkentry plan'"));
        assert!(msg.contains("requires inkentry-server"));
        assert!(msg.contains("https://bad:4655"));
        assert!(
            !msg.contains("Set server_url"),
            "must not suggest setting an already-set server_url: {msg}"
        );
        assert!(
            msg.contains("server_ca"),
            "must point at the TLS-trust knob for untrusted-cert failures: {msg}"
        );
    }

    #[test]
    fn require_tier1_uses_feature_name_in_message() {
        let tier = Tier::Offline(OfflineReason::NoLocalServer);
        let err = require_tier1("plumbing push", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'inkentry plumbing push'"));
    }

    #[test]
    fn require_explicit_server_url_errs_when_unset() {
        let cfg = Config {
            server_url: None,
            ..Default::default()
        };
        let err = require_explicit_server_url("sync", &cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server_url"));
    }

    #[test]
    fn require_explicit_server_url_ok_regardless_of_reachability() {
        let cfg = Config {
            server_url: Some("https://unreachable.invalid:1".to_string()),
            ..Default::default()
        };
        assert_eq!(
            require_explicit_server_url("sync", &cfg).unwrap(),
            "https://unreachable.invalid:1"
        );
    }

    #[test]
    fn require_explicit_server_url_message_is_identical_in_shape_across_features() {
        let cfg = Config {
            server_url: None,
            ..Default::default()
        };
        let push_msg = require_explicit_server_url("plumbing push", &cfg)
            .unwrap_err()
            .to_string();
        let sync_msg = require_explicit_server_url("sync", &cfg)
            .unwrap_err()
            .to_string();
        assert_eq!(
            push_msg.replace("plumbing push", "sync"),
            sync_msg,
            "push and sync messages must differ only in the feature name: \
             push={push_msg:?} sync={sync_msg:?}"
        );
    }
}

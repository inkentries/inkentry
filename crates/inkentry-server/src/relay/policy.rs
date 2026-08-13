//! Where the local relay is allowed to connect, and where that answer comes
//! from.
//!
//! The relay is the one part of this daemon that opens *outbound* connections
//! on the caller's behalf. Its destination therefore may not come from the
//! request: a `server_url` taken out of a request body turns the daemon into an
//! egress proxy for any process that can reach loopback — an attacker-chosen
//! host, reached from the daemon's network position, carrying an
//! attacker-chosen bearer, retried for as long as the daemon lives.
//!
//! So the destination is resolved instead from local configuration
//! ([`inkentry_core::config::declared_team_targets`]): the request may only
//! *select* among the (team server, project) pairs this machine already
//! declares, and anything else is refused. Choosing the destination stops being
//! a request-level capability.

use std::path::PathBuf;
use std::sync::Arc;

use inkentry_core::config::{TeamTarget, declared_team_targets};

type TargetSource = Arc<dyn Fn() -> Vec<TeamTarget> + Send + Sync>;

/// The set of team targets this daemon may relay to.
#[derive(Clone)]
pub struct RelayPolicy {
    source: TargetSource,
}

impl RelayPolicy {
    /// Production policy: whatever this machine's own configuration declares —
    /// the `INKENTRY_SERVER_URL`/`INKENTRY_PROJECT_ID` environment pair the
    /// daemon was spawned with, the project containing its working directory,
    /// and every project in the local registry.
    pub fn from_local_config() -> Self {
        Self::from_fn(local_config_targets)
    }

    /// A fixed allowlist, for a caller that has already resolved its targets.
    pub fn allowing(targets: Vec<TeamTarget>) -> Self {
        Self::from_fn(move || targets.clone())
    }

    /// A policy that resolves its targets by calling `source`. The closure
    /// takes no arguments by design: nothing about a request can reach it, so
    /// no policy can be built that lets the request choose its own target.
    pub fn from_fn(source: impl Fn() -> Vec<TeamTarget> + Send + Sync + 'static) -> Self {
        Self {
            source: Arc::new(source),
        }
    }

    /// The declared target `(server_url, project_id)` names, or `None` when no
    /// local configuration declares that pair.
    ///
    /// Resolved fresh each time rather than snapshotted at startup: a project
    /// configured after the daemon started must start relaying without needing
    /// the daemon restarted. The read is a few small config files and one
    /// registry query, on a path that is about to make a network round trip
    /// anyway.
    pub fn resolve(&self, server_url: &str, project_id: &str) -> Option<TeamTarget> {
        (self.source)()
            .into_iter()
            .find(|t| t.matches(server_url, project_id))
    }
}

/// Roots to look for a declaring `.inkentry/config.toml` under: the daemon's
/// own working directory (it is spawned from the project the CLI ran in) plus
/// every project in the local registry, so a machine-wide daemon still relays
/// for the other projects on the machine and not only the one that started it.
fn local_config_targets() -> Vec<TeamTarget> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    match inkentry_core::registry::Registry::open() {
        Ok(registry) => match registry.all_projects() {
            Ok(projects) => roots.extend(projects.into_iter().map(|p| p.root_path)),
            Err(e) => tracing::debug!("listing registered projects for the relay policy: {e}"),
        },
        Err(e) => tracing::debug!("opening the registry for the relay policy: {e}"),
    }
    declared_team_targets(&roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn target(url: &str, project: &str) -> TeamTarget {
        TeamTarget {
            server_url: url.to_string(),
            project_id: project.to_string(),
            server_ca: None,
        }
    }

    #[test]
    fn only_a_declared_pair_resolves() {
        let policy = RelayPolicy::allowing(vec![target("https://team.example", "acme/app")]);

        assert!(policy.resolve("https://team.example", "acme/app").is_some());
        assert!(
            policy
                .resolve("https://team.example/", "acme/app")
                .is_some()
        );
        assert!(
            policy
                .resolve("http://attacker.example", "acme/app")
                .is_none(),
            "a server_url no local config declares must not resolve"
        );
        assert!(
            policy
                .resolve("https://team.example", "other/app")
                .is_none(),
            "an undeclared project on a declared server must not resolve either"
        );
    }

    #[test]
    fn an_empty_allowlist_resolves_nothing() {
        let policy = RelayPolicy::allowing(vec![]);
        assert!(policy.resolve("https://team.example", "acme/app").is_none());
    }

    #[test]
    fn the_resolved_target_carries_its_configured_ca() {
        let policy = RelayPolicy::allowing(vec![TeamTarget {
            server_url: "https://team.example".into(),
            project_id: "acme/app".into(),
            server_ca: Some(PathBuf::from("/etc/inkentry/ca.pem")),
        }]);

        let resolved = policy.resolve("https://team.example", "acme/app").unwrap();

        assert_eq!(
            resolved.server_ca.as_deref(),
            Some(std::path::Path::new("/etc/inkentry/ca.pem"))
        );
    }

    // A project configured while the daemon is already running must be
    // relayable without restarting it, so the allowlist is not snapshotted.
    #[test]
    fn a_newly_declared_target_resolves_without_a_restart() {
        let declared: Arc<Mutex<Vec<TeamTarget>>> = Arc::new(Mutex::new(vec![]));
        let source = declared.clone();
        let policy = RelayPolicy::from_fn(move || source.lock().unwrap().clone());

        assert!(policy.resolve("https://team.example", "acme/app").is_none());

        declared
            .lock()
            .unwrap()
            .push(target("https://team.example", "acme/app"));

        assert!(policy.resolve("https://team.example", "acme/app").is_some());
    }
}

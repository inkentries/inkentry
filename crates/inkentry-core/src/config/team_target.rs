use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::ProjectConfig;
use super::paths::{find_project_config, inkentry_config_dir};

/// One (team server, project) pair declared by a local, on-disk configuration,
/// with the custom CA that connection must trust.
///
/// The point of the type is provenance: a `TeamTarget` can only be produced by
/// reading local configuration, never by deserializing a request body. A
/// process that opens outbound connections on a user's behalf can then take its
/// destination from this and refuse anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamTarget {
    pub server_url: String,
    pub project_id: String,
    pub server_ca: Option<PathBuf>,
}

impl TeamTarget {
    /// Whether this target is the one `(server_url, project_id)` names.
    /// Trailing slashes are not part of a server's identity — every consumer
    /// trims them before use, so a request that keeps one still matches.
    pub fn matches(&self, server_url: &str, project_id: &str) -> bool {
        self.server_url.trim_end_matches('/') == server_url.trim_end_matches('/')
            && self.project_id == project_id
    }
}

/// The one field of the personal global config a team target can draw from.
/// `server_url` is deliberately absent: [`super::Config::load_with_store`]
/// discards a global `server_url` too, since a team server is a project-wide
/// choice.
#[derive(Debug, Default, Deserialize)]
struct GlobalCa {
    server_ca: Option<String>,
}

fn global_server_ca() -> Option<PathBuf> {
    let raw = std::fs::read_to_string(inkentry_config_dir().join("config.toml")).ok()?;
    toml::from_str::<GlobalCa>(&raw)
        .ok()?
        .server_ca
        .map(PathBuf::from)
}

fn non_blank(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Every team target local configuration declares: the `INKENTRY_SERVER_URL` +
/// `INKENTRY_PROJECT_ID` environment pair, plus the `.inkentry/config.toml`
/// found by walking up from each path in `roots`.
///
/// Deliberately does not go through [`super::Config::load`]: that resolves the
/// secret store, which is an unanswerable keychain prompt in a detached daemon
/// with no user session. Reading only the fields a target needs avoids it.
///
/// A malformed or unreadable config contributes nothing rather than failing the
/// whole resolution: one broken project must not disable the relay for every
/// other project on the machine.
pub fn declared_team_targets(roots: &[PathBuf]) -> Vec<TeamTarget> {
    let env_ca = std::env::var("INKENTRY_SERVER_CA").ok().map(PathBuf::from);
    let global_ca = global_server_ca();

    let mut out: Vec<TeamTarget> = Vec::new();
    let mut add = |server_url: Option<String>, project_id: Option<String>, ca: Option<PathBuf>| {
        let (Some(server_url), Some(project_id)) = (non_blank(server_url), non_blank(project_id))
        else {
            return;
        };
        let target = TeamTarget {
            server_url,
            project_id,
            server_ca: ca,
        };
        if !out.contains(&target) {
            out.push(target);
        }
    };

    add(
        std::env::var("INKENTRY_SERVER_URL").ok(),
        std::env::var("INKENTRY_PROJECT_ID").ok(),
        env_ca.clone().or_else(|| global_ca.clone()),
    );

    for root in roots {
        let Some(target) = read_project_target(root, env_ca.as_deref(), global_ca.as_deref())
        else {
            continue;
        };
        add(
            Some(target.server_url),
            Some(target.project_id),
            target.server_ca,
        );
    }
    out
}

fn read_project_target(
    root: &Path,
    env_ca: Option<&Path>,
    global_ca: Option<&Path>,
) -> Option<TeamTarget> {
    let path = find_project_config(root)?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let proj: ProjectConfig = toml::from_str(&raw).ok()?;
    Some(TeamTarget {
        server_url: non_blank(proj.server_url)?,
        project_id: non_blank(proj.project_id)?,
        // `INKENTRY_SERVER_CA` outranks either config file, mirroring the
        // precedence `Config::load_with_store` applies.
        server_ca: env_ca
            .map(Path::to_path_buf)
            .or_else(|| non_blank(proj.server_ca).map(PathBuf::from))
            .or_else(|| global_ca.map(Path::to_path_buf)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn project_with(dir: &TempDir, body: &str) -> PathBuf {
        let proj = dir.path().join(".inkentry");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("config.toml"), body).unwrap();
        dir.path().to_path_buf()
    }

    // The env pair is process-global; every test here scrubs it so an
    // ambient INKENTRY_SERVER_URL cannot add a target the assertions don't
    // expect. Grouped with the config-dir guard because `global_server_ca`
    // reads INKENTRY_CONFIG_DIR too.
    struct EnvGuard;

    impl EnvGuard {
        fn scrubbed() -> Self {
            for k in [
                "INKENTRY_SERVER_URL",
                "INKENTRY_PROJECT_ID",
                "INKENTRY_SERVER_CA",
            ] {
                // SAFETY: every test using this is pinned to the
                // `team_target_env` serial group.
                unsafe { std::env::remove_var(k) };
            }
            Self
        }
    }

    #[test]
    #[serial_test::serial(team_target_env)]
    fn a_project_config_declaring_a_server_and_project_yields_one_target() {
        let _env = EnvGuard::scrubbed();
        let dir = TempDir::new().unwrap();
        let root = project_with(
            &dir,
            "server_url = \"https://team.example\"\nproject_id = \"acme/app\"\n",
        );

        let targets = declared_team_targets(&[root]);

        assert_eq!(targets.len(), 1);
        assert!(targets[0].matches("https://team.example", "acme/app"));
        assert!(targets[0].matches("https://team.example/", "acme/app"));
        assert!(!targets[0].matches("https://attacker.example", "acme/app"));
        assert!(!targets[0].matches("https://team.example", "other/app"));
    }

    #[test]
    #[serial_test::serial(team_target_env)]
    fn a_project_config_without_a_server_url_declares_nothing() {
        let _env = EnvGuard::scrubbed();
        let dir = TempDir::new().unwrap();
        let root = project_with(&dir, "project_id = \"acme/app\"\n");
        assert!(declared_team_targets(&[root]).is_empty());
    }

    #[test]
    #[serial_test::serial(team_target_env)]
    fn a_malformed_project_config_is_skipped_not_fatal() {
        let _env = EnvGuard::scrubbed();
        let broken = TempDir::new().unwrap();
        let broken_root = project_with(&broken, "server_url = [[[not toml");
        let good = TempDir::new().unwrap();
        let good_root = project_with(
            &good,
            "server_url = \"https://team.example\"\nproject_id = \"acme/app\"\n",
        );

        let targets = declared_team_targets(&[broken_root, good_root]);

        assert_eq!(
            targets.len(),
            1,
            "the readable project still declares: {targets:?}"
        );
    }

    #[test]
    #[serial_test::serial(team_target_env)]
    fn the_projects_own_ca_is_carried_on_its_target() {
        let _env = EnvGuard::scrubbed();
        let dir = TempDir::new().unwrap();
        let root = project_with(
            &dir,
            "server_url = \"https://team.example\"\nproject_id = \"acme/app\"\n\
             server_ca = \"/etc/inkentry/internal-ca.pem\"\n",
        );

        let targets = declared_team_targets(&[root]);

        assert_eq!(
            targets[0].server_ca.as_deref(),
            Some(Path::new("/etc/inkentry/internal-ca.pem"))
        );
    }

    #[test]
    #[serial_test::serial(team_target_env)]
    fn the_ca_environment_variable_outranks_the_project_file() {
        let _env = EnvGuard::scrubbed();
        // SAFETY: pinned to the `team_target_env` serial group.
        unsafe { std::env::set_var("INKENTRY_SERVER_CA", "/etc/inkentry/env-ca.pem") };
        let dir = TempDir::new().unwrap();
        let root = project_with(
            &dir,
            "server_url = \"https://team.example\"\nproject_id = \"acme/app\"\n\
             server_ca = \"/etc/inkentry/file-ca.pem\"\n",
        );

        let targets = declared_team_targets(&[root]);
        unsafe { std::env::remove_var("INKENTRY_SERVER_CA") };

        assert_eq!(
            targets[0].server_ca.as_deref(),
            Some(Path::new("/etc/inkentry/env-ca.pem"))
        );
    }

    #[test]
    #[serial_test::serial(team_target_env)]
    fn the_environment_pair_declares_a_target_of_its_own() {
        let _env = EnvGuard::scrubbed();
        // SAFETY: pinned to the `team_target_env` serial group.
        unsafe {
            std::env::set_var("INKENTRY_SERVER_URL", "https://env.example");
            std::env::set_var("INKENTRY_PROJECT_ID", "acme/env");
        }

        let targets = declared_team_targets(&[]);
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_URL");
            std::env::remove_var("INKENTRY_PROJECT_ID");
        }

        assert_eq!(targets.len(), 1);
        assert!(targets[0].matches("https://env.example", "acme/env"));
    }

    #[test]
    #[serial_test::serial(team_target_env)]
    fn the_same_pair_declared_twice_is_one_target() {
        let _env = EnvGuard::scrubbed();
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let body = "server_url = \"https://team.example\"\nproject_id = \"acme/app\"\n";
        let roots = vec![project_with(&a, body), project_with(&b, body)];

        assert_eq!(declared_team_targets(&roots).len(), 1);
    }

    #[test]
    #[serial_test::serial(team_target_env)]
    fn no_local_configuration_declares_no_targets() {
        let _env = EnvGuard::scrubbed();
        let empty = TempDir::new().unwrap();
        assert!(declared_team_targets(&[empty.path().to_path_buf()]).is_empty());
    }
}

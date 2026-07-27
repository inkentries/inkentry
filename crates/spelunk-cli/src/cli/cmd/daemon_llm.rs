//! LLM configuration handed to the auto-spawned `spelunk-server` daemon.
//!
//! Resolution happens here, in the user's own session, and splits into two
//! channels:
//!
//! * **argv** carries the endpoint URL and model. Neither is secret, and `ps`
//!   showing which endpoint a daemon serves is a diagnostic feature.
//! * **the child environment** carries the credential, and nothing else does.
//!
//! The daemon is detached and long-lived, so it must never open the OS
//! keychain itself: on macOS a keychain read from a background process with no
//! session is an authorization prompt the user cannot see or answer. The CLI
//! reads the credential once, here, and passes it out of band. `--llm-key` is
//! therefore never emitted into the child's argv for any input, since argv is
//! world-readable through the process table.

use anyhow::Result;
use std::ffi::OsString;

use spelunk_core::config::{Config, llm_key, secret_store::SecretStore};

/// The LLM values a spawned daemon is configured with.
///
/// `key` is `Some` only when a credential actually resolved; it is never
/// rendered into [`LlmSpawn::args`].
#[derive(Default)]
pub(super) struct LlmSpawn {
    pub url: Option<String>,
    pub model: Option<String>,
    pub key: Option<String>,
}

/// Hand-written so the credential cannot be leaked by a `{:?}` somewhere down
/// the line: a derived `Debug` would print it verbatim.
impl std::fmt::Debug for LlmSpawn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmSpawn")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Trim `raw` and treat a blank result as unset.
fn normalize(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

impl LlmSpawn {
    /// Resolve from a loaded [`Config`] plus optional per-spawn CLI overrides,
    /// using an injected secret store.
    ///
    /// `Config` has already folded `SPELUNK_LLM_URL` / `SPELUNK_LLM_MODEL`
    /// over the personal config file, so an override here is the top of the
    /// precedence chain.
    pub(super) fn resolve_with_store(
        cfg: &Config,
        url_override: Option<&str>,
        model_override: Option<&str>,
        store: &dyn SecretStore,
    ) -> Result<Self> {
        let url = normalize(url_override).or_else(|| normalize(cfg.llm_url.as_deref()));
        let model = normalize(model_override).or_else(|| normalize(cfg.llm_model.as_deref()));
        // Resolving the credential is the only secret-store read on this path,
        // and it happens only here (never in `Config::load`).
        let key = llm_key::resolve_with_store(store)?;
        Ok(Self { url, model, key })
    }

    /// Resolve using the host's default secret store.
    pub(super) fn resolve(
        cfg: &Config,
        url_override: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Self> {
        let store = spelunk_core::config::default_secret_store()?;
        Self::resolve_with_store(cfg, url_override, model_override, store.as_ref())
    }

    /// The daemon arguments carrying the non-secret LLM values.
    ///
    /// Empty unless an endpoint URL resolved: a model without an endpoint is
    /// not a configuration, and emitting nothing keeps the daemon arg list
    /// byte-identical to an unconfigured spawn.
    pub(super) fn args(&self) -> Vec<OsString> {
        let Some(url) = &self.url else {
            return Vec::new();
        };
        let mut args: Vec<OsString> = vec!["--llm-url".into(), url.into()];
        if let Some(model) = &self.model {
            args.push("--llm-model".into());
            args.push(model.into());
        }
        args
    }

    /// The environment entries to set on the child, which is where the
    /// credential travels. An explicit entry also pins the value against
    /// whatever the child would otherwise inherit from this process.
    pub(super) fn child_env(&self) -> Vec<(&'static str, String)> {
        match &self.key {
            Some(key) => vec![(llm_key::ENV_LLM_KEY, key.clone())],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelunk_core::config::secret_store::MemoryStore;

    fn clear_env() {
        unsafe {
            std::env::remove_var("SPELUNK_LLM_KEY");
            std::env::remove_var("SPELUNK_LLM_URL");
            std::env::remove_var("SPELUNK_LLM_MODEL");
        }
    }

    fn cfg_with(url: Option<&str>, model: Option<&str>) -> Config {
        Config {
            llm_url: url.map(str::to_string),
            llm_model: model.map(str::to_string),
            ..Config::default()
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    #[serial_test::serial]
    fn no_url_emits_no_llm_args() {
        clear_env();
        let store = MemoryStore::default();
        let spawn =
            LlmSpawn::resolve_with_store(&cfg_with(None, None), None, None, &store).unwrap();

        assert!(spawn.args().is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn url_without_model_emits_only_the_url() {
        clear_env();
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        assert_eq!(
            strings(&spawn.args()),
            vec!["--llm-url".to_string(), "http://127.0.0.1:1234".to_string()]
        );
    }

    #[test]
    #[serial_test::serial]
    fn url_and_model_emit_both_flags() {
        clear_env();
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), Some("gpt-oss")),
            None,
            None,
            &store,
        )
        .unwrap();

        assert_eq!(
            strings(&spawn.args()),
            vec![
                "--llm-url".to_string(),
                "http://127.0.0.1:1234".to_string(),
                "--llm-model".to_string(),
                "gpt-oss".to_string(),
            ]
        );
    }

    // A model with no endpoint to send it to is not a configuration.
    #[test]
    #[serial_test::serial]
    fn model_without_url_emits_nothing() {
        clear_env();
        let store = MemoryStore::default();
        let spawn =
            LlmSpawn::resolve_with_store(&cfg_with(None, Some("gpt-oss")), None, None, &store)
                .unwrap();

        assert!(spawn.args().is_empty(), "got {:?}", strings(&spawn.args()));
    }

    #[test]
    #[serial_test::serial]
    fn the_key_never_reaches_argv() {
        clear_env();
        let secret = "sk-llm-secret";
        for (url, model) in [
            (None, None),
            (Some("http://127.0.0.1:1234"), None),
            (None, Some("gpt-oss")),
            (Some("http://127.0.0.1:1234"), Some("gpt-oss")),
        ] {
            let store = MemoryStore::default();
            llm_key::set_with_store(secret, &store).unwrap();
            let spawn =
                LlmSpawn::resolve_with_store(&cfg_with(url, model), None, None, &store).unwrap();

            assert_eq!(spawn.key.as_deref(), Some(secret));
            let rendered = strings(&spawn.args());
            assert!(
                rendered.iter().all(|a| !a.contains(secret)),
                "credential leaked into argv for ({url:?}, {model:?}): {rendered:?}"
            );
            assert!(
                rendered
                    .iter()
                    .all(|a| a != "--llm-key" && a != "--llm-key-file"),
                "a key flag was emitted for ({url:?}, {model:?}): {rendered:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_resolved_key_appears_once_in_the_child_env() {
        clear_env();
        let store = MemoryStore::default();
        llm_key::set_with_store("sk-llm-secret", &store).unwrap();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        let env = spawn.child_env();
        assert_eq!(env, vec![("SPELUNK_LLM_KEY", "sk-llm-secret".to_string())]);
    }

    #[test]
    #[serial_test::serial]
    fn no_key_means_no_child_env_entry() {
        clear_env();
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        assert!(spawn.child_env().is_empty());
    }

    // Config::load has already folded SPELUNK_LLM_URL over the config file, so
    // the value the CLI resolved is authoritative and must be spelled out in
    // argv rather than left to the child's inherited environment.
    #[test]
    #[serial_test::serial]
    fn an_explicit_url_override_beats_the_inherited_env() {
        clear_env();
        unsafe { std::env::set_var("SPELUNK_LLM_URL", "http://from-env:1234") };
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://from-env:1234"), None),
            Some("http://from-arg:1234"),
            None,
            &store,
        );
        clear_env();

        assert_eq!(
            strings(&spawn.unwrap().args()),
            vec!["--llm-url".to_string(), "http://from-arg:1234".to_string()]
        );
    }

    #[test]
    #[serial_test::serial]
    fn an_explicit_model_override_beats_the_inherited_env() {
        clear_env();
        unsafe { std::env::set_var("SPELUNK_LLM_MODEL", "from-env") };
        let store = MemoryStore::default();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), Some("from-env")),
            None,
            Some("from-arg"),
            &store,
        );
        clear_env();

        assert_eq!(
            strings(&spawn.unwrap().args()),
            vec![
                "--llm-url".to_string(),
                "http://127.0.0.1:1234".to_string(),
                "--llm-model".to_string(),
                "from-arg".to_string(),
            ]
        );
    }

    // A `{:?}` on this struct must never be the thing that leaks the key.
    #[test]
    #[serial_test::serial]
    fn debug_output_redacts_the_key() {
        clear_env();
        let store = MemoryStore::default();
        llm_key::set_with_store("sk-llm-secret", &store).unwrap();
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        let rendered = format!("{spawn:?}");
        assert!(!rendered.contains("sk-llm-secret"), "got {rendered}");
        assert!(rendered.contains("redacted"), "got {rendered}");
    }

    // The spawn path must resolve the credential from whichever backend
    // SPELUNK_SECRET_STORE selects, never by reaching past it to the keychain.
    #[test]
    #[serial_test::serial]
    fn resolves_from_a_file_backed_store_without_a_keychain() {
        clear_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let store =
            spelunk_core::config::secret_store::FileStore::new(tmp.path().join("secrets.toml"));
        llm_key::set_with_store("sk-llm-secret", &store).unwrap();

        assert_eq!(store.kind(), "file");
        let spawn = LlmSpawn::resolve_with_store(
            &cfg_with(Some("http://127.0.0.1:1234"), None),
            None,
            None,
            &store,
        )
        .unwrap();

        assert_eq!(spawn.key.as_deref(), Some("sk-llm-secret"));
    }
}

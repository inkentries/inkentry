//! Per-origin server-key map (ADR-071 D1/D2).
//!
//! The client's bearer credential used to be a single flat `server_key`,
//! which cannot represent a developer who holds keys for two different
//! self-hosted `server_url`s (ADR-056's recommended multi-server topology).
//! This module gives the credential a home keyed by the server it belongs to:
//! a single secret-store entry (`KEY_SERVER_KEYS_MAP`) whose payload is a JSON
//! object mapping normalized origin to key. One entry, not one per host, so
//! granting keychain access once covers every server (see the module-level
//! rationale in ADR-071 D1).
//!
//! [`bearer_for`] is the resolution entry point: it decides the credential
//! *kind* (cloud vs. self-hosted) from the target `server_url`'s origin
//! before touching any store, so a given request only ever consults the tier
//! its own kind uses (ADR-071 D2). The flat entry that predated the map is
//! gone, along with the migrate-on-read it was kept alive for (ADR-088 D2/D3):
//! `inkentry auth set-key --server <url>` is the one way a key gets into the
//! map.

use anyhow::{Context, Result};
use std::collections::HashMap;

use super::AuthTokens;
use super::secret_store::SecretStore;

/// Key name for the per-origin server-key map: one secret-store entry holding
/// every origin's key (ADR-071 D1).
pub const KEY_SERVER_KEYS_MAP: &str = "server_keys";

/// Default inkentry cloud API origin. Overridable via `INKENTRY_CLOUD_URL`,
/// which is read directly here (and by every cloud-api call site) so bearer
/// resolution, `/v1/me`, and WorkOS client-id selection all agree on the same
/// value. Single source of truth for the constant: `inkentry-cli`'s
/// `auth_api` module re-exports this rather than defining its own copy.
pub const DEFAULT_CLOUD_URL: &str = "https://api.inkentry.com";

/// Normalize `url` to its origin: scheme, lowercased host, and explicit
/// port (default port applied for comparison, omitted from the canonical
/// form when it matches the scheme default). This is the WHATWG origin
/// concept. Path, query, trailing slash, and host case do not participate
/// (ADR-071 D1).
pub fn normalize_origin(url: &str) -> Result<String> {
    let parsed =
        reqwest::Url::parse(url.trim()).with_context(|| format!("{url:?} is not a valid URL"))?;
    let origin = parsed.origin();
    if !origin.is_tuple() {
        anyhow::bail!("{url:?} has no origin (must be an http(s) URL)");
    }
    Ok(origin.ascii_serialization())
}

/// The cloud origin bearer resolution branches against (D2).
fn cloud_origin() -> Result<String> {
    let raw = std::env::var("INKENTRY_CLOUD_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLOUD_URL.to_string());
    normalize_origin(&raw)
}

fn read_map(store: &dyn SecretStore) -> Result<HashMap<String, String>> {
    match store.get(KEY_SERVER_KEYS_MAP)? {
        Some(raw) if !raw.trim().is_empty() => {
            serde_json::from_str(&raw).context("parsing the server_keys map from the secret store")
        }
        _ => Ok(HashMap::new()),
    }
}

/// Persist `map`, or delete the entry outright when `map` is empty (ADR-090
/// D5).
///
/// The empty case is handled here rather than in each caller so no stored
/// key can ever leave an entry holding `{}` behind: in a keychain UI, which
/// shows names and not values, that reads as a credential that was never
/// removed.
fn write_map(store: &dyn SecretStore, map: &HashMap<String, String>) -> Result<()> {
    if map.is_empty() {
        return store.delete(KEY_SERVER_KEYS_MAP);
    }
    let raw = serde_json::to_string(map).context("serialising the server_keys map")?;
    store.set(KEY_SERVER_KEYS_MAP, &raw)
}

/// The server-key-kind credential for `origin` (D2's second tier, non-cloud):
/// whatever the map holds for it, and nothing else.
fn server_key_for_origin(origin: &str, store: &dyn SecretStore) -> Result<Option<String>> {
    Ok(read_map(store)?.remove(origin))
}

/// Resolve the effective bearer for a request to `server_url` (ADR-071 D2).
///
/// Branches on credential kind by origin before touching any store:
/// * **Cloud kind** (origin matches [`DEFAULT_CLOUD_URL`] /
///   `INKENTRY_CLOUD_URL`): `INKENTRY_SERVER_KEY` env, then `[auth]`'s access
///   token. The map is never consulted.
/// * **Server-key kind** (any other origin): `INKENTRY_SERVER_KEY` env, then
///   the per-origin map. `[auth]` is never consulted.
///
/// An origin the map has no entry for resolves to no bearer; the fix is
/// `inkentry auth set-key --server <url>` (ADR-088 D3).
pub fn bearer_for(
    auth: Option<&AuthTokens>,
    server_url: &str,
    store: &dyn SecretStore,
) -> Result<Option<String>> {
    if let Ok(v) = std::env::var("INKENTRY_SERVER_KEY") {
        return Ok(Some(v));
    }
    let origin = normalize_origin(server_url)?;
    if origin == cloud_origin()? {
        // An empty (or absent) access token means "not logged in": resolve to
        // no bearer rather than an empty-string `Some("")`.
        return Ok(auth
            .map(|a| a.access_token.clone())
            .filter(|token| !token.is_empty()));
    }
    server_key_for_origin(&origin, store)
}

/// `inkentry auth set-key --server <url>`: store `key` for `url`'s origin.
/// Returns the normalized origin it was stored under.
pub fn set_key_for_origin(server_url: &str, key: &str, store: &dyn SecretStore) -> Result<String> {
    let origin = normalize_origin(server_url)?;
    let mut map = read_map(store)?;
    map.insert(origin.clone(), key.to_string());
    write_map(store, &map)?;
    Ok(origin)
}

/// `inkentry auth list-servers`: origins with a stored key, sorted. Never
/// returns key material.
pub fn list_origins(store: &dyn SecretStore) -> Result<Vec<String>> {
    let mut origins: Vec<String> = read_map(store)?.into_keys().collect();
    origins.sort();
    Ok(origins)
}

/// Count of stored server-key credentials (used by bare `inkentry logout` to
/// report what it left untouched, ADR-071 D3).
pub fn count(store: &dyn SecretStore) -> Result<usize> {
    Ok(list_origins(store)?.len())
}

/// `inkentry auth remove-key --all-servers`: clear the per-origin map.
/// Returns how many origins had a stored key.
pub fn clear_all(store: &dyn SecretStore) -> Result<usize> {
    let removed = list_origins(store)?.len();
    store
        .delete(KEY_SERVER_KEYS_MAP)
        .context("clearing the server_keys map")?;
    Ok(removed)
}

/// What a single-origin removal did (ADR-090 D4).
///
/// `removed` is the difference between a real revocation and a mistyped URL,
/// which look identical from the caller's side otherwise: both leave the
/// origin unmapped. Reporting only `origin` is what let a typo print the same
/// success sentence as a removal, so the caller stopped looking while the
/// credential stayed live.
pub struct OriginRemoval {
    /// The origin `server_url` normalized to, whether or not it was mapped.
    pub origin: String,
    /// Whether the map actually held a key for `origin`.
    pub removed: bool,
}

/// `inkentry auth remove-key --server <url>`: clear only that origin's
/// credential.
///
/// Normalizes through [`normalize_origin`], the same function
/// [`set_key_for_origin`] stores under, so the two cannot drift on what counts
/// as the same server (ADR-090 D3).
pub fn clear_origin(server_url: &str, store: &dyn SecretStore) -> Result<OriginRemoval> {
    let origin = normalize_origin(server_url)?;
    let mut map = read_map(store)?;
    let removed = map.remove(&origin).is_some();
    if removed {
        write_map(store, &map)?;
    }
    Ok(OriginRemoval { origin, removed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::secret_store::MemoryStore;

    fn tokens(access_token: &str) -> AuthTokens {
        AuthTokens {
            access_token: access_token.to_string(),
            refresh_token: "rt".to_string(),
            expires_at: 4_000_000_000,
            org_id: "org_1".to_string(),
        }
    }

    fn clear_env() {
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_KEY");
            std::env::remove_var("INKENTRY_CLOUD_URL");
        }
    }

    // ── normalize_origin ──────────────────────────────────────────────────

    #[test]
    fn normalize_origin_omits_default_port_and_lowercases_host() {
        assert_eq!(
            normalize_origin("https://Inkentry.Internal.Example.Com/foo?x=1#y").unwrap(),
            "https://inkentry.internal.example.com"
        );
    }

    #[test]
    fn normalize_origin_keeps_explicit_non_default_port() {
        assert_eq!(
            normalize_origin("https://other.example.net:8443/").unwrap(),
            "https://other.example.net:8443"
        );
    }

    #[test]
    fn normalize_origin_ignores_path_query_and_trailing_slash() {
        let a = normalize_origin("http://team.example:4655/a/b?x=1").unwrap();
        let b = normalize_origin("http://team.example:4655/").unwrap();
        let c = normalize_origin("http://team.example:4655").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn normalize_origin_rejects_invalid_url() {
        assert!(normalize_origin("not a url").is_err());
    }

    // ── bearer_for: env always wins, no store touch ─────────────────────────

    #[test]
    #[serial_test::serial]
    fn bearer_for_env_wins_over_everything_and_skips_store() {
        clear_env();
        let store = MemoryStore::default();
        set_key_for_origin("https://team.example:4655", "sk-team", &store).unwrap();
        let auth = tokens("at-cloud");

        unsafe { std::env::set_var("INKENTRY_SERVER_KEY", "sk-from-env") };
        let cloud = bearer_for(Some(&auth), DEFAULT_CLOUD_URL, &store).unwrap();
        let team = bearer_for(Some(&auth), "https://team.example:4655", &store).unwrap();
        unsafe { std::env::remove_var("INKENTRY_SERVER_KEY") };

        assert_eq!(cloud.as_deref(), Some("sk-from-env"));
        assert_eq!(team.as_deref(), Some("sk-from-env"));
        assert_eq!(
            list_origins(&store).unwrap(),
            vec!["https://team.example:4655"]
        );
    }

    // ── bearer_for: cloud kind ───────────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn bearer_for_cloud_origin_uses_auth_token() {
        clear_env();
        let store = MemoryStore::default();
        let auth = tokens("at-cloud");

        let result = bearer_for(Some(&auth), DEFAULT_CLOUD_URL, &store).unwrap();
        assert_eq!(result.as_deref(), Some("at-cloud"));
    }

    #[test]
    #[serial_test::serial]
    fn bearer_for_cloud_origin_without_auth_is_none() {
        clear_env();
        let store = MemoryStore::default();
        assert_eq!(bearer_for(None, DEFAULT_CLOUD_URL, &store).unwrap(), None);
    }

    #[test]
    #[serial_test::serial]
    fn bearer_for_cloud_origin_never_touches_the_map() {
        clear_env();
        let store = MemoryStore::default();
        set_key_for_origin("https://team.example:4655", "sk-team", &store).unwrap();
        let auth = tokens("at-cloud");

        let result = bearer_for(Some(&auth), DEFAULT_CLOUD_URL, &store).unwrap();
        assert_eq!(result.as_deref(), Some("at-cloud"));
        assert_eq!(
            list_origins(&store).unwrap(),
            vec!["https://team.example:4655"]
        );
    }

    // ── bearer_for: server-key kind ──────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn bearer_for_non_cloud_origin_uses_map_entry_ignoring_auth() {
        clear_env();
        let store = MemoryStore::default();
        set_key_for_origin("https://team.example:4655", "sk-team", &store).unwrap();
        let auth = tokens("at-cloud");

        // A cloud [auth] token must never leak to a self-hosted origin.
        let result = bearer_for(Some(&auth), "https://team.example:4655", &store).unwrap();
        assert_eq!(result.as_deref(), Some("sk-team"));
    }

    // ADR-088 D2/D3: the flat entry a pre-ADR-071 client left behind is not a
    // tier and is not migrated on the way out. It resolves to nothing, and it
    // is not consumed either: the store is the user's own, so the entry stays
    // where it is until they clear it.
    #[test]
    #[serial_test::serial]
    fn bearer_for_ignores_a_flat_key_left_by_an_older_client() {
        clear_env();
        let store = MemoryStore::default();
        store.set("server_key", "sk-legacy").unwrap();

        assert_eq!(
            bearer_for(None, "https://team.example:4655", &store).unwrap(),
            None
        );
        assert!(list_origins(&store).unwrap().is_empty());
        assert_eq!(
            store.get("server_key").unwrap().as_deref(),
            Some("sk-legacy")
        );
    }

    #[test]
    #[serial_test::serial]
    fn bearer_for_non_cloud_origin_no_credential_anywhere_is_none() {
        clear_env();
        let store = MemoryStore::default();
        assert_eq!(
            bearer_for(None, "https://team.example:4655", &store).unwrap(),
            None
        );
    }

    #[test]
    #[serial_test::serial]
    fn one_origins_key_does_not_leak_to_another() {
        clear_env();
        let store = MemoryStore::default();
        set_key_for_origin("https://a.example:4655", "sk-a", &store).unwrap();

        assert_eq!(
            bearer_for(None, "https://a.example:4655", &store)
                .unwrap()
                .as_deref(),
            Some("sk-a")
        );
        // An unmapped origin must fail closed, never reuse another's key.
        assert_eq!(
            bearer_for(None, "https://b.example:4655", &store).unwrap(),
            None
        );
    }

    // ── D1: the map's on-the-wire JSON shape ─────────────────────────────────

    /// D1: the payload behind the single `KEY_SERVER_KEYS_MAP` entry is a
    /// flat JSON object of `origin -> key`, nothing more (no envelope, no
    /// metadata). Verified against the raw string the store holds, not just
    /// through the read helpers that would mask a shape drift.
    #[test]
    fn map_entry_payload_is_a_flat_json_object_of_origin_to_key() {
        let store = MemoryStore::default();
        set_key_for_origin("https://a.example:4655", "sk-a", &store).unwrap();
        set_key_for_origin("https://b.example", "sk-b", &store).unwrap();

        let raw = store.get(KEY_SERVER_KEYS_MAP).unwrap().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({
                "https://a.example:4655": "sk-a",
                "https://b.example": "sk-b",
            })
        );
    }

    #[test]
    fn empty_map_leaves_no_entry_rather_than_an_empty_json_object() {
        // Before any set_key_for_origin call, the entry must not exist at all
        // (read_map treats "absent" and "{}" the same, but nothing should
        // write an empty object pre-emptively).
        let store = MemoryStore::default();
        assert!(list_origins(&store).unwrap().is_empty());
        assert_eq!(store.get(KEY_SERVER_KEYS_MAP).unwrap(), None);
    }

    // ── set_key_for_origin / list_origins ────────────────────────────────────

    #[test]
    fn set_key_for_origin_normalizes_and_overwrites() {
        let store = MemoryStore::default();
        set_key_for_origin("https://Team.Example:4655/ignored/path", "sk-1", &store).unwrap();
        set_key_for_origin("https://team.example:4655", "sk-2", &store).unwrap();

        assert_eq!(
            list_origins(&store).unwrap(),
            vec!["https://team.example:4655".to_string()]
        );
        assert_eq!(
            bearer_for(None, "https://team.example:4655", &store)
                .unwrap()
                .as_deref(),
            Some("sk-2")
        );
    }

    // ── clear_all / clear_origin / count ─────────────────────────────────────

    #[test]
    fn clear_all_removes_every_origin_and_counts_what_it_removed() {
        let store = MemoryStore::default();
        set_key_for_origin("https://a.example", "sk-a", &store).unwrap();
        set_key_for_origin("https://b.example", "sk-b", &store).unwrap();

        assert_eq!(clear_all(&store).unwrap(), 2);

        assert!(list_origins(&store).unwrap().is_empty());
        assert_eq!(clear_all(&store).unwrap(), 0);
    }

    #[test]
    fn clear_origin_removes_only_that_origin() {
        let store = MemoryStore::default();
        set_key_for_origin("https://a.example", "sk-a", &store).unwrap();
        set_key_for_origin("https://b.example", "sk-b", &store).unwrap();

        let outcome = clear_origin("https://a.example", &store).unwrap();
        assert_eq!(outcome.origin, "https://a.example");
        assert!(outcome.removed);

        assert_eq!(
            list_origins(&store).unwrap(),
            vec!["https://b.example".to_string()]
        );
        assert_eq!(
            bearer_for(None, "https://b.example", &store)
                .unwrap()
                .as_deref(),
            Some("sk-b")
        );
    }

    // ADR-090 D4: absence exits normally, and says so rather than reporting a
    // removal that did not happen.
    #[test]
    fn clear_origin_is_a_no_op_when_nothing_is_stored() {
        let store = MemoryStore::default();
        let outcome = clear_origin("https://nothing.example", &store).unwrap();
        assert_eq!(outcome.origin, "https://nothing.example");
        assert!(!outcome.removed);
        assert!(list_origins(&store).unwrap().is_empty());
    }

    #[test]
    fn clear_origin_reports_no_removal_for_an_origin_the_map_does_not_hold() {
        let store = MemoryStore::default();
        set_key_for_origin("https://a.example", "sk-a", &store).unwrap();

        let outcome = clear_origin("https://typo.example", &store).unwrap();
        assert!(!outcome.removed);
        assert_eq!(
            list_origins(&store).unwrap(),
            vec!["https://a.example".to_string()]
        );
    }

    // ADR-090 D3: a URL form that `set_key_for_origin` accepted must match on
    // the way back out. A remove that misses the entry reports the honest
    // "nothing stored" while the credential stays live, so this asserts the
    // pairing directly rather than trusting two call sites to agree.
    #[test]
    fn every_url_form_set_key_accepts_is_matched_by_clear_origin() {
        let forms = [
            "https://team.example:4655",
            "https://team.example:4655/",
            "https://team.example:4655/a/b?x=1",
            "https://TEAM.Example:4655",
        ];
        for set_form in forms {
            for remove_form in forms {
                let store = MemoryStore::default();
                set_key_for_origin(set_form, "sk-team", &store).unwrap();

                let outcome = clear_origin(remove_form, &store).unwrap();
                assert!(
                    outcome.removed,
                    "set as {set_form:?} then removed as {remove_form:?} matched nothing"
                );
                assert!(list_origins(&store).unwrap().is_empty());
            }
        }
    }

    // The default port is part of the same pairing: `https://x.example` and
    // `https://x.example:443` are one origin, and `:8443` is a different one.
    #[test]
    fn default_port_forms_pair_up_and_a_different_port_is_a_different_origin() {
        let store = MemoryStore::default();
        set_key_for_origin("https://x.example:443", "sk-x", &store).unwrap();

        assert!(
            !clear_origin("https://x.example:8443", &store)
                .unwrap()
                .removed
        );
        assert!(clear_origin("https://x.example", &store).unwrap().removed);
    }

    // ADR-090 D5: emptying the map deletes the entry rather than storing "{}".
    // A user auditing their keychain after removing their last key must not
    // find an inkentry credential still sitting in it.
    #[test]
    fn removing_the_last_origin_deletes_the_map_entry_rather_than_storing_an_empty_object() {
        let store = MemoryStore::default();
        set_key_for_origin("https://only.example", "sk-only", &store).unwrap();

        clear_origin("https://only.example", &store).unwrap();

        assert_eq!(store.get(KEY_SERVER_KEYS_MAP).unwrap(), None);
    }

    #[test]
    fn clear_all_and_clear_origin_converge_on_the_same_empty_end_state() {
        let via_origin = MemoryStore::default();
        set_key_for_origin("https://only.example", "sk-only", &via_origin).unwrap();
        clear_origin("https://only.example", &via_origin).unwrap();

        let via_all = MemoryStore::default();
        set_key_for_origin("https://only.example", "sk-only", &via_all).unwrap();
        clear_all(&via_all).unwrap();

        assert_eq!(via_origin.get(KEY_SERVER_KEYS_MAP).unwrap(), None);
        assert_eq!(via_all.get(KEY_SERVER_KEYS_MAP).unwrap(), None);
    }

    #[test]
    fn count_reflects_map_size() {
        let store = MemoryStore::default();
        assert_eq!(count(&store).unwrap(), 0);
        set_key_for_origin("https://a.example", "sk-a", &store).unwrap();
        assert_eq!(count(&store).unwrap(), 1);
        set_key_for_origin("https://b.example", "sk-b", &store).unwrap();
        assert_eq!(count(&store).unwrap(), 2);
    }

    // ── adversarial: independent test-engineer coverage ──────────────────────
    //
    // The tests above are the Engineer's own suite. These probe a case their
    // own tests don't: a corrupted store payload.

    // A corrupted `server_keys` payload must fail resolution loudly (`Err`),
    // never silently fall through to "no credential". A silent fallthrough
    // here would be the dangerous case: an operator could believe a server is
    // unauthenticated-safe (loopback, firewalled) when in fact resolution
    // swallowed a real error and just returned `None`, instead of a clear
    // "your credential store is broken" signal.
    #[test]
    #[serial_test::serial]
    fn corrupted_map_json_fails_resolution_loudly_not_silently() {
        clear_env();
        let store = MemoryStore::default();
        // Not valid JSON at all.
        store.set(KEY_SERVER_KEYS_MAP, "{not valid json").unwrap();

        let result = bearer_for(None, "https://team.example:4655", &store);
        assert!(
            result.is_err(),
            "a corrupted map must fail loudly, not resolve to Some/None silently; got {result:?}"
        );

        // Same for the JSON tools directly: a valid JSON value of the wrong
        // shape (an array, not an object) must also fail, not deserialize
        // into an empty/default map.
        store
            .set(KEY_SERVER_KEYS_MAP, "[\"not\", \"a\", \"map\"]")
            .unwrap();
        let result2 = bearer_for(None, "https://team.example:4655", &store);
        assert!(
            result2.is_err(),
            "a wrong-shaped-but-valid-JSON map must also fail loudly; got {result2:?}"
        );
    }

    // `set_key_for_origin` / `list_origins` must likewise surface a corrupted
    // map rather than silently treating it as empty and overwriting it (which
    // would quietly discard whatever the corrupted payload's other origins
    // were, destroying credentials for servers the corruption didn't touch).
    #[test]
    fn corrupted_map_json_fails_set_and_list_loudly() {
        let store = MemoryStore::default();
        store.set(KEY_SERVER_KEYS_MAP, "{not valid json").unwrap();

        assert!(set_key_for_origin("https://a.example", "sk-a", &store).is_err());
        assert!(list_origins(&store).is_err());
    }

    /// The two-server, two-key motivating case (task's acceptance sketch),
    /// driven purely through the public resolution/storage API with no env
    /// var involved at any point: both origins resolve to their own key on
    /// repeated, interleaved lookups, with no state that could cause a
    /// second read to see the first origin's key.
    #[test]
    #[serial_test::serial]
    fn two_projects_two_origins_two_keys_resolve_independently_interleaved() {
        clear_env();
        let store = MemoryStore::default();
        set_key_for_origin("https://proj-a.example:4655", "sk-proj-a", &store).unwrap();
        set_key_for_origin("https://proj-b.example:9443", "sk-proj-b", &store).unwrap();

        // Interleave lookups (A, B, A, B) to catch any accidental
        // last-write-wins / shared-mutable-state bug a naive cache could
        // introduce.
        for _ in 0..3 {
            assert_eq!(
                bearer_for(None, "https://proj-a.example:4655", &store)
                    .unwrap()
                    .as_deref(),
                Some("sk-proj-a")
            );
            assert_eq!(
                bearer_for(None, "https://proj-b.example:9443", &store)
                    .unwrap()
                    .as_deref(),
                Some("sk-proj-b")
            );
        }
    }
}

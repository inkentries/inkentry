//! Per-organization WorkOS session cache (ADR-074 D1/D3/D4).
//!
//! `inkentry login` and `inkentry org switch` used to write a single WorkOS
//! session — the short-lived access token and the long-lived, rotating refresh
//! token — as plaintext in the `[auth]` table of
//! `~/.config/inkentry/config.toml`. Any process running as the user could read
//! it, and the common leak is a `~/.config` synced into a dotfiles repo. This
//! module moves the session into the secret store, the WorkOS analogue of
//! ADR-071's `server_keys` move: one secret-store entry ([`KEY_ORG_TOKENS`])
//! whose payload is a JSON object holding one session per organization, keyed by
//! WorkOS org id, plus an `active` pointer naming the org an invocation uses when
//! nothing else selects one (ADR-074 D2's lowest-precedence tier). One entry,
//! not one per org, so granting keychain access once covers every organization.
//!
//! The [`SecretStore`] stays an opaque get/set/delete; the JSON shape and the
//! org-keying live here in the config layer (ADR-074 D1).
//!
//! Each cached session records the `cloud_origin` it was issued for, so the
//! access token is released only to that origin (ADR-095). The ADR-074 example
//! predates ADR-095 and omits that field; it is stored here because dropping it
//! would let a cloud-URL override redirect a stored token to a host it was never
//! issued for.
//!
//! Resolution and refresh never leave one org's entry (ADR-074 D3): an expired
//! session is refreshed with its own refresh token, scoped to its own org, and
//! written back to its own slot, so a long-running agent scoped to org A is
//! never disturbed by a switch to org B elsewhere on the same machine.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::AuthTokens;
use super::secret_store::SecretStore;

/// Secret-store entry name holding every organization's WorkOS session
/// (ADR-074 D1). One entry, not one per org, for the same reason ADR-071 D1
/// gives: a distinct keychain item per org would re-prompt for access on every
/// new client, and one item, once granted, covers all of them.
pub const KEY_ORG_TOKENS: &str = "org_tokens";

/// Environment variable that pins the organization for one invocation, above the
/// project `org` and below an explicit `--org` flag (ADR-074 D2).
pub const ENV_ORG: &str = "INKENTRY_ORG";

/// One organization's cached WorkOS session — the value in the D1 map, keyed by
/// WorkOS org id (so the org id itself is not stored inside the value).
///
/// `slug` records the human identifier the session was last onboarded or
/// switched under, so a repo can pin `org = "<slug>"` (ADR-074 D2) and be
/// resolved offline, and so `org list` (D4) can print it. It is optional: a
/// session onboarded by a path that never saw a slug has none.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
struct OrgSession {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_at: i64,
    #[serde(default)]
    cloud_origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    slug: Option<String>,
}

/// The whole cache payload behind [`KEY_ORG_TOKENS`] (ADR-074 D1).
///
/// `active` names the org an invocation with no higher-precedence pin resolves
/// to (D2's fourth tier). `orgs` maps WorkOS org id to that org's session. A
/// `BTreeMap` keeps the on-disk JSON key order stable across writes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Cache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<String>,
    #[serde(default)]
    orgs: BTreeMap<String, OrgSession>,
}

/// One cached organization, for `org list` (ADR-074 D4). Never carries token
/// material.
pub struct CachedOrg {
    /// WorkOS org id (the cache key).
    pub org_id: String,
    /// Human slug where the session recorded one, else `None`.
    pub slug: Option<String>,
    /// Whether this org is the cache's `active` pointer.
    pub is_active: bool,
}

fn read_cache(store: &dyn SecretStore) -> Result<Cache> {
    match store.get(KEY_ORG_TOKENS)? {
        Some(raw) if !raw.trim().is_empty() => {
            serde_json::from_str(&raw).context("parsing the org-token cache from the secret store")
        }
        _ => Ok(Cache::default()),
    }
}

/// Persist `cache`, or delete the entry outright when it holds nothing (no
/// active pointer and no orgs).
///
/// The empty case is handled here rather than in each caller so a fully
/// logged-out user never leaves an entry holding `{}` behind: in a keychain UI,
/// which shows names and not values, that reads as a credential that was never
/// removed (mirrors ADR-090 D5 for `server_keys`).
fn write_cache(store: &dyn SecretStore, cache: &Cache) -> Result<()> {
    if cache.active.is_none() && cache.orgs.is_empty() {
        return store.delete(KEY_ORG_TOKENS);
    }
    let raw = serde_json::to_string(cache).context("serialising the org-token cache")?;
    store.set(KEY_ORG_TOKENS, &raw)
}

fn to_auth_tokens(org_id: &str, session: &OrgSession) -> AuthTokens {
    AuthTokens {
        access_token: session.access_token.clone(),
        refresh_token: session.refresh_token.clone(),
        expires_at: session.expires_at,
        org_id: org_id.to_string(),
        cloud_origin: session.cloud_origin.clone(),
    }
}

fn session_from(tokens: &AuthTokens, slug: Option<String>) -> OrgSession {
    OrgSession {
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        expires_at: tokens.expires_at,
        cloud_origin: tokens.cloud_origin.clone(),
        slug,
    }
}

/// The cache key an identifier names, if the cache holds one: a WorkOS org id
/// used directly as a key, or a stored slug matched to its key. A local org
/// UUID has no offline mapping (slugs are stored, UUIDs are not), so it resolves
/// only when it happens to equal a key.
fn key_for(cache: &Cache, ident: &str) -> Option<String> {
    if cache.orgs.contains_key(ident) {
        return Some(ident.to_string());
    }
    cache
        .orgs
        .iter()
        .find(|(_, session)| session.slug.as_deref() == Some(ident))
        .map(|(key, _)| key.clone())
}

/// The org id this invocation resolves to: the `pin` (a WorkOS org id, an exact
/// cache key, or a stored slug) when given, else the cache's `active`
/// (ADR-074 D2). A pin that resolves to no cached entry returns `None` and never
/// falls back to `active`, so a config-pinned org is never silently served
/// another org's token.
fn resolve_org_id_in(cache: &Cache, pin: Option<&str>) -> Option<String> {
    match pin {
        Some(ident) => key_for(cache, ident),
        None => cache.active.clone(),
    }
}

/// The cached WorkOS session this invocation resolves to (its pinned or active
/// org), or `None` when not logged in for that org (ADR-074 D3.1).
pub fn resolve_session(store: &dyn SecretStore, pin: Option<&str>) -> Result<Option<AuthTokens>> {
    let cache = read_cache(store)?;
    Ok(resolve_org_id_in(&cache, pin).and_then(|id| {
        cache
            .orgs
            .get(&id)
            .map(|session| to_auth_tokens(&id, session))
    }))
}

/// Store `tokens` under their org id and point `active` at it — what `login` and
/// `org switch` do once a session is minted (ADR-074 D4). `slug` records the
/// human identifier; when `None`, any slug already stored for this org is kept.
pub fn set_active(store: &dyn SecretStore, tokens: &AuthTokens, slug: Option<&str>) -> Result<()> {
    let mut cache = read_cache(store)?;
    let slug = slug
        .map(str::to_string)
        .or_else(|| cache.orgs.get(&tokens.org_id).and_then(|s| s.slug.clone()));
    cache
        .orgs
        .insert(tokens.org_id.clone(), session_from(tokens, slug));
    cache.active = Some(tokens.org_id.clone());
    write_cache(store, &cache)
}

/// Update `tokens.org_id`'s slot in place, preserving `active` and the stored
/// slug — the refresh write-back (ADR-074 D3.3). It never reads or writes a
/// sibling org's slot and never moves the active pointer, so rotating org A's
/// single-use token leaves org B untouched.
pub fn update_in_place(store: &dyn SecretStore, tokens: &AuthTokens) -> Result<()> {
    let mut cache = read_cache(store)?;
    let slug = cache.orgs.get(&tokens.org_id).and_then(|s| s.slug.clone());
    cache
        .orgs
        .insert(tokens.org_id.clone(), session_from(tokens, slug));
    write_cache(store, &cache)
}

/// Whether `ident` (a WorkOS org id or a stored slug) already has a cached
/// entry. `org switch` uses this to stay local when the target is cached
/// (ADR-074 D4), calling WorkOS only to onboard an org with no entry.
pub fn contains(store: &dyn SecretStore, ident: &str) -> Result<bool> {
    Ok(key_for(&read_cache(store)?, ident).is_some())
}

/// Point `active` at `ident`'s already-cached entry with no token change — the
/// local `org switch` (ADR-074 D4). Returns `false` when the org is not cached,
/// so the caller falls through to the network onboarding path.
pub fn set_active_local(store: &dyn SecretStore, ident: &str) -> Result<bool> {
    let mut cache = read_cache(store)?;
    let Some(key) = key_for(&cache, ident) else {
        return Ok(false);
    };
    cache.active = Some(key);
    write_cache(store, &cache)?;
    Ok(true)
}

/// The active org id, if any.
pub fn active(store: &dyn SecretStore) -> Result<Option<String>> {
    Ok(read_cache(store)?.active)
}

/// Every cached org for `org list` (ADR-074 D4), sorted by org id, with the
/// active one flagged. Never returns token material.
pub fn list(store: &dyn SecretStore) -> Result<Vec<CachedOrg>> {
    let cache = read_cache(store)?;
    let active = cache.active.clone();
    Ok(cache
        .orgs
        .into_iter()
        .map(|(org_id, session)| CachedOrg {
            is_active: active.as_deref() == Some(org_id.as_str()),
            slug: session.slug,
            org_id,
        })
        .collect())
}

/// Count of cached orgs (used by bare `inkentry logout` to report what it
/// cleared).
pub fn count(store: &dyn SecretStore) -> Result<usize> {
    Ok(read_cache(store)?.orgs.len())
}

/// Clear every cached org (bare `inkentry logout`, ADR-074 D4). Returns how many
/// orgs were cleared.
pub fn clear_all(store: &dyn SecretStore) -> Result<usize> {
    let removed = read_cache(store)?.orgs.len();
    store
        .delete(KEY_ORG_TOKENS)
        .context("clearing the org-token cache")?;
    Ok(removed)
}

/// Clear only `ident`'s entry (`inkentry logout --org`, ADR-074 D4), leaving
/// every other cached org's session intact. Clears `active` when it named the
/// removed org, so nothing points at a session that no longer exists. Returns
/// whether an entry was removed.
pub fn clear_org(store: &dyn SecretStore, ident: &str) -> Result<bool> {
    let mut cache = read_cache(store)?;
    let Some(key) = key_for(&cache, ident) else {
        return Ok(false);
    };
    cache.orgs.remove(&key);
    if cache.active.as_deref() == Some(key.as_str()) {
        cache.active = None;
    }
    write_cache(store, &cache)?;
    Ok(true)
}

/// Lift a legacy plaintext `[auth]` session into the cache the first time a
/// config with one is loaded (ADR-074 migration): store it under its own org id
/// and, when the cache has no active pointer yet, make it active. An org already
/// present in the cache is left as-is — the cache is authoritative once
/// populated, so a re-run after a partial migration never clobbers a fresher
/// session.
pub fn migrate_legacy(store: &dyn SecretStore, legacy: &AuthTokens) -> Result<()> {
    let mut cache = read_cache(store)?;
    cache
        .orgs
        .entry(legacy.org_id.clone())
        .or_insert_with(|| session_from(legacy, None));
    if cache.active.is_none() {
        cache.active = Some(legacy.org_id.clone());
    }
    write_cache(store, &cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::secret_store::MemoryStore;

    fn tokens(org_id: &str, access: &str, refresh: &str) -> AuthTokens {
        AuthTokens {
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            expires_at: 4_000_000_000,
            org_id: org_id.to_string(),
            cloud_origin: "https://api.inkentry.com".to_string(),
        }
    }

    // ── set_active / resolve_session ────────────────────────────────────────

    #[test]
    fn set_active_stores_session_and_points_active_at_it() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), Some("acme")).unwrap();

        assert_eq!(active(&store).unwrap().as_deref(), Some("org_a"));
        let resolved = resolve_session(&store, None).unwrap().unwrap();
        assert_eq!(resolved.access_token, "at-a");
        assert_eq!(resolved.org_id, "org_a");
    }

    #[test]
    fn resolve_session_uses_active_when_no_pin() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), None).unwrap();
        set_active(&store, &tokens("org_b", "at-b", "rt-b"), None).unwrap();
        // The last set_active made org_b active.
        assert_eq!(
            resolve_session(&store, None).unwrap().unwrap().access_token,
            "at-b"
        );
    }

    #[test]
    fn resolve_session_pin_selects_that_org_over_active() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), Some("acme")).unwrap();
        set_active(&store, &tokens("org_b", "at-b", "rt-b"), Some("beta")).unwrap();
        // active is org_b, but a pin selects org_a.
        assert_eq!(
            resolve_session(&store, Some("org_a"))
                .unwrap()
                .unwrap()
                .access_token,
            "at-a"
        );
    }

    #[test]
    fn resolve_session_pin_matches_a_stored_slug() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), Some("acme")).unwrap();
        set_active(&store, &tokens("org_b", "at-b", "rt-b"), Some("beta")).unwrap();
        assert_eq!(
            resolve_session(&store, Some("acme"))
                .unwrap()
                .unwrap()
                .org_id,
            "org_a"
        );
    }

    #[test]
    fn resolve_session_pin_with_no_cached_entry_is_none_and_never_falls_back_to_active() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), None).unwrap();
        // A pin the cache does not hold must fail closed, not serve org_a's token.
        assert!(
            resolve_session(&store, Some("org_unknown"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_session_on_empty_cache_is_none() {
        let store = MemoryStore::default();
        assert!(resolve_session(&store, None).unwrap().is_none());
        assert!(resolve_session(&store, Some("org_a")).unwrap().is_none());
    }

    // ── per-org isolation on refresh (ADR-074 D3) ───────────────────────────

    #[test]
    fn update_in_place_rotates_one_org_and_leaves_the_sibling_and_active_untouched() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), Some("acme")).unwrap();
        set_active(&store, &tokens("org_b", "at-b", "rt-b"), Some("beta")).unwrap();
        // active is now org_b.

        update_in_place(&store, &tokens("org_a", "at-a2", "rt-a2")).unwrap();

        // org_a rotated.
        let a = resolve_session(&store, Some("org_a")).unwrap().unwrap();
        assert_eq!(a.access_token, "at-a2");
        assert_eq!(a.refresh_token, "rt-a2");
        // org_b untouched.
        let b = resolve_session(&store, Some("org_b")).unwrap().unwrap();
        assert_eq!(b.access_token, "at-b");
        assert_eq!(b.refresh_token, "rt-b");
        // active pointer unchanged by a refresh.
        assert_eq!(active(&store).unwrap().as_deref(), Some("org_b"));
    }

    #[test]
    fn update_in_place_preserves_a_stored_slug() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), Some("acme")).unwrap();
        update_in_place(&store, &tokens("org_a", "at-a2", "rt-a2")).unwrap();
        // The slug survives so a later pin by slug still resolves.
        assert_eq!(
            resolve_session(&store, Some("acme"))
                .unwrap()
                .unwrap()
                .access_token,
            "at-a2"
        );
    }

    // ── org switch: local when cached (ADR-074 D4) ──────────────────────────

    #[test]
    fn set_active_local_points_at_a_cached_org_without_a_token_change() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), Some("acme")).unwrap();
        set_active(&store, &tokens("org_b", "at-b", "rt-b"), Some("beta")).unwrap();
        // active is org_b; switch back to acme locally.
        assert!(set_active_local(&store, "acme").unwrap());
        assert_eq!(active(&store).unwrap().as_deref(), Some("org_a"));
        // No token changed.
        assert_eq!(
            resolve_session(&store, Some("org_a"))
                .unwrap()
                .unwrap()
                .access_token,
            "at-a"
        );
    }

    #[test]
    fn set_active_local_reports_false_for_an_uncached_org() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), None).unwrap();
        assert!(!set_active_local(&store, "org_never_seen").unwrap());
        // active is unchanged.
        assert_eq!(active(&store).unwrap().as_deref(), Some("org_a"));
    }

    // ── logout: all and one (ADR-074 D4) ────────────────────────────────────

    #[test]
    fn clear_all_removes_every_org_and_reports_the_count() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), None).unwrap();
        set_active(&store, &tokens("org_b", "at-b", "rt-b"), None).unwrap();

        assert_eq!(clear_all(&store).unwrap(), 2);
        assert!(resolve_session(&store, None).unwrap().is_none());
        // Emptying deletes the entry rather than leaving a "{}" behind.
        assert_eq!(store.get(KEY_ORG_TOKENS).unwrap(), None);
        assert_eq!(clear_all(&store).unwrap(), 0);
    }

    #[test]
    fn clear_org_removes_one_and_leaves_the_rest() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), Some("acme")).unwrap();
        set_active(&store, &tokens("org_b", "at-b", "rt-b"), Some("beta")).unwrap();
        // active is org_b.

        // Remove the non-active org by slug.
        assert!(clear_org(&store, "acme").unwrap());
        assert!(resolve_session(&store, Some("org_a")).unwrap().is_none());
        // The sibling and the active pointer are untouched.
        assert_eq!(active(&store).unwrap().as_deref(), Some("org_b"));
        assert_eq!(
            resolve_session(&store, Some("org_b"))
                .unwrap()
                .unwrap()
                .access_token,
            "at-b"
        );
    }

    #[test]
    fn clear_org_clears_active_when_it_removes_the_active_org() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), None).unwrap();
        set_active(&store, &tokens("org_b", "at-b", "rt-b"), None).unwrap();
        // active is org_b; remove it.
        assert!(clear_org(&store, "org_b").unwrap());
        assert_eq!(active(&store).unwrap(), None);
        // A no-pin resolution now finds nothing, never a stale sibling.
        assert!(resolve_session(&store, None).unwrap().is_none());
    }

    #[test]
    fn clear_org_reports_false_for_an_org_the_cache_does_not_hold() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), None).unwrap();
        assert!(!clear_org(&store, "org_absent").unwrap());
        assert_eq!(count(&store).unwrap(), 1);
    }

    // ── org list prints no token material (ADR-074 D4) ──────────────────────

    #[test]
    fn list_returns_orgs_with_active_flagged_and_no_token_material() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), Some("acme")).unwrap();
        set_active(&store, &tokens("org_b", "at-b", "rt-b"), Some("beta")).unwrap();
        // active is org_b.

        let listed = list(&store).unwrap();
        assert_eq!(listed.len(), 2);
        // Sorted by org id.
        assert_eq!(listed[0].org_id, "org_a");
        assert!(!listed[0].is_active);
        assert_eq!(listed[0].slug.as_deref(), Some("acme"));
        assert_eq!(listed[1].org_id, "org_b");
        assert!(listed[1].is_active);
        // CachedOrg carries no token field at all; assert the render a command
        // would produce leaks no secret either.
        let rendered = listed
            .iter()
            .map(|o| format!("{} {:?} {}", o.org_id, o.slug, o.is_active))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("at-a"));
        assert!(!rendered.contains("rt-a"));
        assert!(!rendered.contains("at-b"));
        assert!(!rendered.contains("rt-b"));
    }

    // ── migration (ADR-074) ─────────────────────────────────────────────────

    #[test]
    fn migrate_legacy_stores_the_session_and_makes_it_active() {
        let store = MemoryStore::default();
        migrate_legacy(&store, &tokens("org_a", "at-legacy", "rt-legacy")).unwrap();

        assert_eq!(active(&store).unwrap().as_deref(), Some("org_a"));
        let resolved = resolve_session(&store, None).unwrap().unwrap();
        assert_eq!(resolved.access_token, "at-legacy");
        assert_eq!(resolved.refresh_token, "rt-legacy");
        assert_eq!(resolved.cloud_origin, "https://api.inkentry.com");
    }

    #[test]
    fn migrate_legacy_does_not_clobber_an_already_cached_org_or_move_active() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-fresh", "rt-fresh"), None).unwrap();
        set_active(&store, &tokens("org_b", "at-b", "rt-b"), None).unwrap();
        // active is org_b; a stale legacy [auth] for org_a must not win.
        migrate_legacy(&store, &tokens("org_a", "at-stale", "rt-stale")).unwrap();

        assert_eq!(
            resolve_session(&store, Some("org_a"))
                .unwrap()
                .unwrap()
                .access_token,
            "at-fresh"
        );
        assert_eq!(active(&store).unwrap().as_deref(), Some("org_b"));
    }

    // ── D1 on-the-wire JSON shape ───────────────────────────────────────────

    #[test]
    fn cache_payload_shape_is_active_plus_per_org_sessions() {
        let store = MemoryStore::default();
        set_active(&store, &tokens("org_a", "at-a", "rt-a"), Some("acme")).unwrap();
        update_in_place(&store, &tokens("org_b", "at-b", "rt-b")).unwrap();

        let raw = store.get(KEY_ORG_TOKENS).unwrap().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({
                "active": "org_a",
                "orgs": {
                    "org_a": {
                        "access_token": "at-a",
                        "refresh_token": "rt-a",
                        "expires_at": 4_000_000_000_i64,
                        "cloud_origin": "https://api.inkentry.com",
                        "slug": "acme",
                    },
                    "org_b": {
                        "access_token": "at-b",
                        "refresh_token": "rt-b",
                        "expires_at": 4_000_000_000_i64,
                        "cloud_origin": "https://api.inkentry.com",
                    },
                }
            })
        );
    }

    #[test]
    fn a_corrupted_cache_fails_resolution_loudly_not_silently() {
        let store = MemoryStore::default();
        store.set(KEY_ORG_TOKENS, "{not valid json").unwrap();
        assert!(resolve_session(&store, None).is_err());

        // A valid JSON value of the wrong shape must also fail, not deserialize
        // into an empty/default cache that reads as "not logged in".
        store
            .set(KEY_ORG_TOKENS, "[\"not\", \"a\", \"cache\"]")
            .unwrap();
        assert!(resolve_session(&store, None).is_err());
    }
}

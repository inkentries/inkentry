use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use inkentry_core::config::{self, AuthTokens};

use super::auth_api::{self, DEFAULT_CLOUD_URL, MeOrg};

#[derive(Args, Debug)]
pub struct OrgArgs {
    /// Development override: points `org` at a development cloud instead of the
    /// fixed hosted URL (default: https://api.inkentry.com). Not a user setting
    /// for choosing a cloud (use `cloud = true` in `.inkentry/config.toml`), and
    /// it does not change which origin a stored access token is released to.
    #[arg(long, env = "INKENTRY_CLOUD_URL", global = true)]
    pub cloud_url: Option<String>,

    #[command(subcommand)]
    pub command: OrgCommand,
}

#[derive(Subcommand, Debug)]
pub enum OrgCommand {
    /// Switch the active organization. Local (no WorkOS call) when the target is
    /// already cached; otherwise onboards it with the current session.
    Switch(OrgSwitchArgs),
    /// List the organizations with a cached session and mark the active one.
    List,
}

#[derive(Args, Debug)]
pub struct OrgSwitchArgs {
    /// Organization to switch to. Accepts a WorkOS org id (`org_…`), an org
    /// slug, or a local org UUID. A slug or local UUID is resolved to its
    /// WorkOS org id via `GET /v1/me` before switching.
    pub org: String,
}

pub async fn org(args: OrgArgs) -> Result<()> {
    let cloud_url = args
        .cloud_url
        .as_deref()
        .unwrap_or(DEFAULT_CLOUD_URL)
        .trim_end_matches('/')
        .to_string();

    let workos_url = auth_api::workos_url();
    let client_id = auth_api::workos_client_id(&cloud_url);
    match args.command {
        OrgCommand::Switch(switch_args) => {
            org_switch(&workos_url, &cloud_url, &client_id, &switch_args.org).await
        }
        OrgCommand::List => org_list(),
    }
}

async fn org_switch(workos_url: &str, cloud_url: &str, client_id: &str, org: &str) -> Result<()> {
    let store = config::default_secret_store()?;

    // A cached target only moves the active pointer: no WorkOS call, and no
    // other org's session is touched.
    if config::org_tokens::set_active_local(store.as_ref(), org)? {
        println!("Switched to organization '{org}'.");
        return Ok(());
    }

    // Not cached: onboard it from the current session's refresh token.
    let cfg = config::Config::load(None).context("loading config")?;
    let auth = cfg.cloud_session()?.ok_or_else(|| {
        anyhow::anyhow!("Not logged in. Run `inkentry login` before switching organizations.")
    })?;

    let client = auth_api::build_client()?;
    let tokens = switch_org(&client, workos_url, cloud_url, client_id, &auth, org).await?;
    config::store_active_session(&tokens, slug_hint(org))?;
    println!("Switched to organization '{org}'.");
    Ok(())
}

fn org_list() -> Result<()> {
    let store = config::default_secret_store()?;
    let orgs = config::org_tokens::list(store.as_ref())?;
    if orgs.is_empty() {
        println!("No organizations cached. Run `inkentry login` to sign in.");
        return Ok(());
    }
    for org in orgs {
        let marker = if org.is_active { "* " } else { "  " };
        let label = match &org.slug {
            Some(slug) => format!("{} ({})", slug, org.org_id),
            None => org.org_id.clone(),
        };
        println!("{marker}{label}");
    }
    Ok(())
}

pub async fn switch_org(
    client: &reqwest::Client,
    workos_url: &str,
    cloud_url: &str,
    client_id: &str,
    auth: &AuthTokens,
    org: &str,
) -> Result<AuthTokens> {
    let workos_org_id =
        resolve_workos_org_id(client, workos_url, cloud_url, client_id, auth, org).await?;
    let success = auth_api::refresh_token(
        client,
        workos_url,
        client_id,
        &auth.refresh_token,
        Some(&workos_org_id),
    )
    .await?;
    Ok(
        success.into_auth_tokens(inkentry_core::config::server_keys::normalize_origin(
            cloud_url,
        )?),
    )
}

// WorkOS's refresh grant wants the WorkOS org id, not the cloud-api local UUID.
async fn resolve_workos_org_id(
    client: &reqwest::Client,
    workos_url: &str,
    cloud_url: &str,
    client_id: &str,
    auth: &AuthTokens,
    arg: &str,
) -> Result<String> {
    if is_workos_org_id(arg) {
        return Ok(arg.to_string());
    }

    // WorkOS access tokens are short-lived; a paused session would 401 on `/v1/me`.
    let fresh = auth_api::ensure_fresh_token(
        client,
        workos_url,
        client_id,
        auth,
        config::update_org_session,
    )
    .await?;
    let me = auth_api::fetch_me(client, cloud_url, &fresh.access_token).await?;
    resolve_arg_to_workos_org_id(&me.orgs, arg)
}

fn is_workos_org_id(arg: &str) -> bool {
    arg.starts_with("org_")
}

fn resolve_arg_to_workos_org_id(orgs: &[MeOrg], arg: &str) -> Result<String> {
    let entry = orgs
        .iter()
        .find(|o| o.slug == arg || o.id == arg)
        .ok_or_else(|| anyhow::anyhow!("not a member of org '{arg}', or unknown org"))?;
    entry.workos_org_id.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "org '{arg}' has no WorkOS org id on record; cannot switch \
             (re-run `inkentry login` or contact support)"
        )
    })
}

// Only a slug has display value; recording it lets a repo pin `org = "<slug>"`.
pub fn slug_hint(arg: &str) -> Option<&str> {
    (!is_workos_org_id(arg) && !looks_like_uuid(arg)).then_some(arg)
}

fn looks_like_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(len, part)| part.len() == *len && part.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me_orgs() -> Vec<MeOrg> {
        vec![
            MeOrg {
                id: "11111111-1111-1111-1111-111111111111".into(),
                name: "Acme".into(),
                slug: "acme".into(),
                workos_org_id: Some("org_acme".into()),
            },
            MeOrg {
                id: "22222222-2222-2222-2222-222222222222".into(),
                name: "Beta".into(),
                slug: "beta".into(),
                workos_org_id: Some("org_beta".into()),
            },
        ]
    }

    #[test]
    fn is_workos_org_id_recognises_provider_ids() {
        assert!(is_workos_org_id("org_01ABCDEF"));
        assert!(!is_workos_org_id("beta"));
        assert!(!is_workos_org_id("22222222-2222-2222-2222-222222222222"));
    }

    #[test]
    fn resolve_arg_to_workos_org_id_matches_slug() {
        let id = resolve_arg_to_workos_org_id(&me_orgs(), "beta").unwrap();
        assert_eq!(id, "org_beta");
    }

    #[test]
    fn resolve_arg_to_workos_org_id_matches_local_uuid() {
        let id = resolve_arg_to_workos_org_id(&me_orgs(), "11111111-1111-1111-1111-111111111111")
            .unwrap();
        assert_eq!(id, "org_acme");
    }

    #[test]
    fn resolve_arg_to_workos_org_id_unknown_errors() {
        let err = resolve_arg_to_workos_org_id(&me_orgs(), "gamma").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gamma"), "error should name the arg: {msg}");
    }

    #[test]
    fn resolve_arg_to_workos_org_id_missing_workos_id_errors() {
        let orgs = vec![MeOrg {
            id: "33333333-3333-3333-3333-333333333333".into(),
            name: "Gamma".into(),
            slug: "gamma".into(),
            workos_org_id: None,
        }];
        let err = resolve_arg_to_workos_org_id(&orgs, "gamma").unwrap_err();
        assert!(
            err.to_string().contains("WorkOS org id"),
            "error should explain the missing WorkOS org id: {err}"
        );
    }

    #[test]
    fn slug_hint_recorded_only_for_a_slug() {
        assert_eq!(slug_hint("acme"), Some("acme"));
        assert_eq!(slug_hint("org_01ABC"), None);
        assert_eq!(slug_hint("11111111-1111-1111-1111-111111111111"), None);
    }

    // Needs INKENTRY_TEST_WORKOS_CLIENT_ID and a single-use refresh token per org
    // in INKENTRY_TEST_WORKOS_RT_A / _RT_B, each from an independent `login --org`.
    #[tokio::test]
    #[ignore = "requires live WorkOS credentials; run manually"]
    async fn live_two_orgs_refresh_on_independent_lineages() {
        let (Ok(client_id), Ok(rt_a), Ok(rt_b)) = (
            std::env::var("INKENTRY_TEST_WORKOS_CLIENT_ID"),
            std::env::var("INKENTRY_TEST_WORKOS_RT_A"),
            std::env::var("INKENTRY_TEST_WORKOS_RT_B"),
        ) else {
            return; // no creds provisioned; nothing to exercise
        };
        let client = auth_api::build_client().unwrap();
        let workos = auth_api::workos_url();
        let a = auth_api::refresh_token(&client, &workos, &client_id, &rt_a, None)
            .await
            .expect("org A refresh should succeed");
        let b = auth_api::refresh_token(&client, &workos, &client_id, &rt_b, None)
            .await
            .expect("org B refresh should succeed on its own lineage");
        assert!(!a.refresh_token.is_empty() && !b.refresh_token.is_empty());
        assert_ne!(
            a.refresh_token, b.refresh_token,
            "each org must rotate its own refresh lineage"
        );
    }

    // One mock server serves as both `cloud_url` and `workos_url`.
    mod switch_org_wire {
        use serde_json::Value;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        use crate::cli::cmd::auth_api::DEFAULT_CLOUD_URL;
        use crate::cli::cmd::org::switch_org;
        use inkentry_core::config::AuthTokens;

        const BETA_UUID: &str = "22222222-2222-2222-2222-222222222222";
        const BETA_WORKOS: &str = "org_beta01";
        const CLIENT_ID: &str = "client_test";

        fn auth() -> AuthTokens {
            AuthTokens {
                access_token: "at-current".into(),
                refresh_token: "rt-current".into(),
                expires_at: 4_000_000_000,
                org_id: "00000000-0000-0000-0000-000000000000".into(),
                cloud_origin: DEFAULT_CLOUD_URL.to_string(),
            }
        }

        fn jwt(org_id: &str, exp: i64) -> String {
            fn b64url(bytes: &[u8]) -> String {
                const A: &[u8] =
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
                let mut out = String::new();
                for chunk in bytes.chunks(3) {
                    let b = [
                        chunk[0],
                        *chunk.get(1).unwrap_or(&0),
                        *chunk.get(2).unwrap_or(&0),
                    ];
                    let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                    out.push(A[((n >> 18) & 0x3f) as usize] as char);
                    out.push(A[((n >> 12) & 0x3f) as usize] as char);
                    if chunk.len() > 1 {
                        out.push(A[((n >> 6) & 0x3f) as usize] as char);
                    }
                    if chunk.len() > 2 {
                        out.push(A[(n & 0x3f) as usize] as char);
                    }
                }
                out
            }
            let payload = serde_json::json!({ "exp": exp, "org_id": org_id }).to_string();
            format!("{}.{}.sig", b64url(b"{}"), b64url(payload.as_bytes()))
        }

        fn workos_success(org_id: &str) -> Value {
            serde_json::json!({
                "access_token": jwt(org_id, 4_000_000_100),
                "refresh_token": "rt-new",
                "organization_id": org_id,
            })
        }

        struct AssertForm(&'static str);
        impl Respond for AssertForm {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body = String::from_utf8_lossy(&request.body);
                assert!(
                    body.contains(&format!("organization_id={}", self.0)),
                    "organization_id must be the WorkOS org id, got body: {body}"
                );
                assert!(
                    body.contains("grant_type=refresh_token"),
                    "must use the refresh-token grant, got body: {body}"
                );
                ResponseTemplate::new(200).set_body_json(workos_success(self.0))
            }
        }

        #[tokio::test]
        async fn slug_resolves_via_me_then_switches() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/v1/me"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "orgs": [
                        { "id": "11111111-1111-1111-1111-111111111111",
                          "name": "Acme", "slug": "acme",
                          "workos_org_id": "org_acme01", "role": "member" },
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta",
                          "workos_org_id": BETA_WORKOS, "role": "admin" }
                    ]
                })))
                .expect(1)
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(AssertForm(BETA_WORKOS))
                .expect(1)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let tokens = switch_org(
                &client,
                &server.uri(),
                &server.uri(),
                CLIENT_ID,
                &auth(),
                "beta",
            )
            .await
            .expect("slug switch should succeed");
            assert_eq!(tokens.refresh_token, "rt-new");
            assert_eq!(tokens.org_id, BETA_WORKOS);
        }

        #[tokio::test]
        async fn local_uuid_resolves_to_workos_org_id() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/v1/me"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "orgs": [
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta",
                          "workos_org_id": BETA_WORKOS, "role": "admin" }
                    ]
                })))
                .expect(1)
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(AssertForm(BETA_WORKOS))
                .expect(1)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let tokens = switch_org(
                &client,
                &server.uri(),
                &server.uri(),
                CLIENT_ID,
                &auth(),
                BETA_UUID,
            )
            .await
            .expect("local-uuid switch should succeed");
            assert_eq!(tokens.org_id, BETA_WORKOS);
        }

        #[tokio::test]
        async fn workos_org_id_passed_directly_skips_me() {
            let server = MockServer::start().await;

            // No /v1/me mock mounted: if it is hit, the request 404s.
            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(AssertForm(BETA_WORKOS))
                .expect(1)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let tokens = switch_org(
                &client,
                &server.uri(),
                &server.uri(),
                CLIENT_ID,
                &auth(),
                BETA_WORKOS,
            )
            .await
            .expect("workos-org-id switch should succeed");
            assert_eq!(tokens.org_id, BETA_WORKOS);
        }

        #[tokio::test]
        async fn unknown_slug_errors_before_token_call() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/v1/me"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "orgs": [
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta",
                          "workos_org_id": BETA_WORKOS, "role": "admin" }
                    ]
                })))
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(ResponseTemplate::new(200).set_body_json(workos_success(BETA_WORKOS)))
                .expect(0)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let err = switch_org(
                &client,
                &server.uri(),
                &server.uri(),
                CLIENT_ID,
                &auth(),
                "gamma",
            )
            .await
            .expect_err("unknown slug should error");
            assert!(
                err.to_string().contains("gamma"),
                "error should name the slug: {err}"
            );
        }

        #[tokio::test]
        async fn organization_not_found_maps_to_membership_error() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/v1/me"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "orgs": [
                        { "id": BETA_UUID, "name": "Beta", "slug": "beta",
                          "workos_org_id": BETA_WORKOS, "role": "admin" }
                    ]
                })))
                .mount(&server)
                .await;

            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": "organization_not_found"
                })))
                .expect(1)
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let err = switch_org(
                &client,
                &server.uri(),
                &server.uri(),
                CLIENT_ID,
                &auth(),
                "beta",
            )
            .await
            .expect_err("an organization_not_found must error");
            assert!(
                err.to_string().contains("not a member"),
                "expected a clear membership error, got: {err}"
            );
        }
    }
}

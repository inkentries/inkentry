//! `spelunk login` — WorkOS device-authorization grant, direct (ADR-047).
//!
//! Flow
//! ----
//! 1. POST WorkOS `/authorize/device` (client_id only) → device_code, user_code,
//!    verification_uri.
//! 2. Print the verification URL and user code for the operator.
//! 3. Poll POST WorkOS `/authenticate` (device-code grant) every `interval`
//!    seconds (RFC 8628):
//!    - success                         → persist tokens, done
//!    - authorization_pending           → keep polling
//!    - slow_down                       → increase interval by 5 s
//!    - expired_token / access_denied   → exit 1
//!    - MFA / step-up challenge         → print "complete in browser", keep polling
//!
//! Org selection happens browser-side on WorkOS's hosted approval page, so the
//! CLI never sees an org-selection step and the returned token is already
//! org-scoped.
//!
//! `--org <slug>` is login-then-switch: a device login always yields a token
//! first; if `--org` is given, the session is then silently re-scoped to that
//! org via the refresh grant. When the operator is already logged in with a
//! valid refresh token and passes `--org`, login short-circuits straight to the
//! silent org-switch (no device re-entry).

use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;

use spelunk_core::config::{self, AuthTokens};

use super::auth_api::{self, DEFAULT_CLOUD_URL, PollOutcome};
use super::org::{persist_tokens, switch_org};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Override the spelunk cloud API URL (default: https://api.spelunk.cloud).
    /// Also selects the WorkOS environment (prod host → prod client_id; any
    /// other host → dev client_id) unless `SPELUNK_WORKOS_CLIENT_ID` is set.
    #[arg(long, env = "SPELUNK_CLOUD_URL")]
    pub cloud_url: Option<String>,

    /// Organization to log into (slug). After the device login yields a token,
    /// the session is silently re-scoped to this org; when already logged in it
    /// re-scopes without a new device login.
    #[arg(long)]
    pub org: Option<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn login(args: LoginArgs) -> Result<()> {
    let cloud_url = args
        .cloud_url
        .as_deref()
        .unwrap_or(DEFAULT_CLOUD_URL)
        .trim_end_matches('/')
        .to_string();
    let workos_url = auth_api::workos_url();
    let client_id = auth_api::workos_client_id(&cloud_url);

    let client = auth_api::build_client()?;

    // Already logged in with a valid refresh token + `--org`: silent re-scope.
    if let Some(org_slug) = &args.org {
        let cfg = config::Config::load(None).context("loading config")?;
        if let Some(auth) = cfg.auth.as_ref() {
            let tokens =
                switch_org(&client, &workos_url, &cloud_url, &client_id, auth, org_slug).await?;
            return finish_login(&cloud_url, tokens, Some(org_slug)).await;
        }
    }

    // ── Step 1: initiate device authorization ─────────────────────────────────
    let device = auth_api::initiate_device(&client, &workos_url, &client_id).await?;

    // ── Step 2: prompt the user ───────────────────────────────────────────────
    println!();
    println!("Open the following URL in your browser:");
    println!();
    println!("  {}", device.verification_uri);
    println!();
    println!("Enter the code: {}", device.user_code);
    println!();

    if let Some(ref complete_url) = device.verification_uri_complete
        && complete_url != &device.verification_uri
    {
        println!("Or open this direct link (code pre-filled):\n  {complete_url}");
        println!();
    }

    println!(
        "Waiting for authorization (expires in {} s)...",
        device.expires_in
    );

    // ── Step 3: polling loop ──────────────────────────────────────────────────
    let mut interval_secs = device.interval.max(5);
    let mut consecutive_errors: u32 = 0;
    let mut challenge_announced = false;

    let tokens = loop {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        match auth_api::poll_token(&client, &workos_url, &client_id, &device.device_code).await {
            PollOutcome::Success(token) => break token.into_auth_tokens(),
            PollOutcome::Pending => {
                print!(".");
                let _ = std::io::stdout().flush();
                consecutive_errors = 0;
            }
            PollOutcome::SlowDown => {
                interval_secs += 5;
                consecutive_errors = 0;
            }
            PollOutcome::RateLimit => {
                interval_secs *= 2;
                consecutive_errors = 0;
            }
            PollOutcome::Challenge(url) => {
                if !challenge_announced {
                    match url {
                        Some(u) => eprintln!(
                            "\nAdditional verification required — complete it in your browser:\n  {u}"
                        ),
                        None => eprintln!(
                            "\nAdditional verification required — complete it in your browser."
                        ),
                    }
                    challenge_announced = true;
                }
                consecutive_errors = 0;
            }
            PollOutcome::Expired => {
                eprintln!("\nLogin timed out. Run `spelunk login` again.");
                std::process::exit(1);
            }
            PollOutcome::Denied => {
                eprintln!("\nLogin was denied.");
                std::process::exit(1);
            }
            PollOutcome::InvalidGrant(msg) => {
                eprintln!("\nLogin failed: {msg}");
                std::process::exit(1);
            }
            PollOutcome::Error(err) => {
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    return Err(err.context("polling for token failed 3 times in a row"));
                }
                tracing::warn!("token poll error (attempt {consecutive_errors}/3): {err:#}");
            }
        }
    };

    // A device login always yields a token first; honour `--org` as a
    // login-then-switch by re-scoping the freshly-issued session.
    let (tokens, entered_org) = match &args.org {
        Some(org_slug) => {
            let switched = switch_org(
                &client,
                &workos_url,
                &cloud_url,
                &client_id,
                &tokens,
                org_slug,
            )
            .await?;
            (switched, Some(org_slug.clone()))
        }
        None => (tokens, None),
    };

    finish_login(&cloud_url, tokens, entered_org.as_deref()).await
}

/// Persist tokens and print the success message naming the org entered.
///
/// Attempts a best-effort `GET /v1/me` lookup to resolve the WorkOS org id to
/// a human-readable `"<name> (<slug>)"` string. The lookup is never fatal: any
/// error, timeout, or missing entry falls back to the `--org` slug hint (when
/// provided) or the raw `org_id` from the token.
async fn finish_login(
    cloud_url: &str,
    tokens: AuthTokens,
    org_slug_hint: Option<&str>,
) -> Result<()> {
    // Write before printing so a write error surfaces before the user believes
    // they are logged in.
    persist_tokens(&tokens)?;
    println!();
    // Best-effort: resolve the WorkOS org id to a display name.
    let display =
        auth_api::lookup_org_display_name(cloud_url, &tokens.access_token, &tokens.org_id).await;
    // Fall back chain: resolved name → slug hint → raw org_id.
    let label = display
        .as_deref()
        .or(org_slug_hint)
        .unwrap_or(&tokens.org_id);
    print_logged_in(label);
    Ok(())
}

/// Print the "logged in to <org>" confirmation with a switch hint.
fn print_logged_in(org: &str) {
    println!("Logged in to {org}. Use `spelunk org switch` to change.");
}

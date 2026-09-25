use std::io::{IsTerminal as _, Write as _};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;

use inkentry_core::config::{self, AuthTokens};

use super::auth_api::{self, DEFAULT_CLOUD_URL, MeOrg, PollOutcome};
use super::org::switch_org;

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Development override: points `login` at a development cloud instead of
    /// the fixed hosted URL (default: https://api.inkentry.com). Not a user
    /// setting for choosing a cloud (use `cloud = true` in
    /// `.inkentry/config.toml`), and it does not change which origin a stored
    /// access token is released to. Also selects the WorkOS environment (prod
    /// host → prod client_id; any other host → dev client_id) unless
    /// `INKENTRY_WORKOS_CLIENT_ID` is set.
    #[arg(long, env = "INKENTRY_CLOUD_URL")]
    pub cloud_url: Option<String>,

    /// Organization to log into (slug). After the device login yields a token,
    /// the session is silently re-scoped to this org; when already logged in it
    /// re-scopes without a new device login.
    #[arg(long)]
    pub org: Option<String>,
}

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

    // Already logged in: re-scope silently, skipping the device flow.
    if let Some(org_slug) = &args.org {
        let cfg = config::Config::load(None).context("loading config")?;
        if let Some(auth) = cfg.cloud_session()? {
            let tokens = switch_org(
                &client,
                &workos_url,
                &cloud_url,
                &client_id,
                &auth,
                org_slug,
            )
            .await?;
            return finish_login(&cloud_url, tokens, Some(org_slug)).await;
        }
    }

    let device = auth_api::initiate_device(&client, &workos_url, &client_id).await?;

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

    let mut interval_secs = device.interval.max(5);
    let mut consecutive_errors: u32 = 0;
    let mut challenge_announced = false;

    let tokens = loop {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        match auth_api::poll_token(&client, &workos_url, &client_id, &device.device_code).await {
            PollOutcome::Success(token) => {
                break token.into_auth_tokens(config::server_keys::normalize_origin(&cloud_url)?);
            }
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
                eprintln!("\nLogin timed out. Run `inkentry login` again.");
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

    match &args.org {
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
            finish_login(&cloud_url, switched, Some(org_slug.as_str())).await
        }
        // WorkOS never auto-selects an org, so a plain login can yield an empty
        // `org_id`; resolve one here so the first run needs no `org switch`.
        None if !tokens.org_id.is_empty() => finish_login(&cloud_url, tokens, None).await,
        None => resolve_org_after_login(&client, &workos_url, &cloud_url, &client_id, tokens).await,
    }
}

async fn resolve_org_after_login(
    client: &reqwest::Client,
    workos_url: &str,
    cloud_url: &str,
    client_id: &str,
    tokens: AuthTokens,
) -> Result<()> {
    let me = auth_api::fetch_me(client, cloud_url, &tokens.access_token)
        .await
        .context("fetching your organizations after login")?;

    let interactive = std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && !inkentry_core::utils::is_agent_mode();

    match choose_org(&me.orgs, interactive)? {
        OrgChoice::Switch(org) => {
            // The WorkOS org id spares `switch_org` a redundant /v1/me.
            let target = org
                .workos_org_id
                .clone()
                .unwrap_or_else(|| org.slug.clone());
            let switched =
                switch_org(client, workos_url, cloud_url, client_id, &tokens, &target).await?;
            finish_login(cloud_url, switched, Some(&org.slug)).await
        }
    }
}

#[derive(Debug)]
enum OrgChoice {
    Switch(MeOrg),
}

fn choose_org(orgs: &[MeOrg], interactive: bool) -> Result<OrgChoice> {
    match orgs.len() {
        0 => anyhow::bail!(
            "Your account is not a member of any organization yet.\n\
             Create one at https://app.inkentry.com/onboarding, then run `inkentry login` again."
        ),
        1 => Ok(OrgChoice::Switch(orgs[0].clone())),
        _ if interactive => {
            let idx = prompt_org_selection(orgs)?;
            Ok(OrgChoice::Switch(orgs[idx].clone()))
        }
        _ => {
            let slugs = orgs
                .iter()
                .map(|o| o.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "You are a member of multiple organizations; a non-interactive shell \
                 cannot prompt.\n\
                 Re-run with `inkentry login --org <slug>` (one of: {slugs})."
            )
        }
    }
}

fn prompt_org_selection(orgs: &[MeOrg]) -> Result<usize> {
    eprintln!();
    eprintln!("You are a member of multiple organizations. Select one:");
    for (i, o) in orgs.iter().enumerate() {
        eprintln!("  {}. {} ({})", i + 1, o.name, o.slug);
    }
    eprintln!();

    let stdin = std::io::stdin();
    loop {
        eprint!("Enter a number [1-{}]: ", orgs.len());
        std::io::stderr().flush().ok();

        let mut line = String::new();
        let n = stdin
            .read_line(&mut line)
            .context("reading org selection")?;
        if n == 0 {
            // EOF: bail rather than re-prompt forever.
            anyhow::bail!("no organization selected (input closed)");
        }
        match line.trim().parse::<usize>() {
            Ok(choice) if (1..=orgs.len()).contains(&choice) => return Ok(choice - 1),
            _ => eprintln!("Please enter a number between 1 and {}.", orgs.len()),
        }
    }
}

async fn finish_login(
    cloud_url: &str,
    tokens: AuthTokens,
    org_slug_hint: Option<&str>,
) -> Result<()> {
    // Write before printing so a write error surfaces before the user believes
    // they are logged in. Records the slug so a repo can later pin `org = "<slug>"`.
    config::store_active_session(&tokens, org_slug_hint)?;
    println!();
    let display =
        auth_api::lookup_org_display_name(cloud_url, &tokens.access_token, &tokens.org_id).await;
    let label = display
        .as_deref()
        .or(org_slug_hint)
        .unwrap_or(&tokens.org_id);
    print_logged_in(label);
    Ok(())
}

fn print_logged_in(org: &str) {
    println!("Logged in to {org}. Use `inkentry org switch` to change.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org(id: &str, name: &str, slug: &str, workos: Option<&str>) -> MeOrg {
        MeOrg {
            id: id.into(),
            name: name.into(),
            slug: slug.into(),
            workos_org_id: workos.map(str::to_string),
        }
    }

    fn one_org() -> Vec<MeOrg> {
        vec![org(
            "11111111-1111-1111-1111-111111111111",
            "Acme",
            "acme",
            Some("org_acme"),
        )]
    }

    fn two_orgs() -> Vec<MeOrg> {
        vec![
            org(
                "11111111-1111-1111-1111-111111111111",
                "Acme",
                "acme",
                Some("org_acme"),
            ),
            org(
                "22222222-2222-2222-2222-222222222222",
                "Beta Corp",
                "beta",
                Some("org_beta"),
            ),
        ]
    }

    #[test]
    fn choose_org_single_org_auto_selects() {
        let OrgChoice::Switch(picked) = choose_org(&one_org(), false).unwrap();
        assert_eq!(picked.slug, "acme");
        let OrgChoice::Switch(picked) = choose_org(&one_org(), true).unwrap();
        assert_eq!(picked.slug, "acme");
    }

    #[test]
    fn choose_org_zero_orgs_points_at_onboarding() {
        let err = choose_org(&[], true).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("onboarding"),
            "0-org error should point at onboarding, got: {msg}"
        );
        assert!(
            msg.contains("not a member of any organization"),
            "0-org error should explain the cause, got: {msg}"
        );
    }

    #[test]
    fn choose_org_multi_non_interactive_requires_org_flag() {
        let err = choose_org(&two_orgs(), false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--org"),
            "multi/non-TTY error should tell the user to pass --org, got: {msg}"
        );
        assert!(
            msg.contains("acme") && msg.contains("beta"),
            "error should list the candidate slugs, got: {msg}"
        );
    }

    #[test]
    fn choose_org_multi_non_interactive_does_not_prompt() {
        // Must return Err without reading stdin.
        assert!(choose_org(&two_orgs(), false).is_err());
    }
}

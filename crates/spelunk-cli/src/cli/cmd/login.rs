//! `spelunk login` — OAuth 2.0 Device Authorization Grant (RFC 8628).
//!
//! Flow
//! ----
//! 1. POST /v1/auth/device → `{ device_code, user_code, verification_uri,
//!    verification_uri_complete?, expires_in, interval }`
//! 2. Print the verification URL and user code for the operator.
//! 3. Poll POST /v1/auth/device/token every `interval` seconds until:
//!    - 200 OK  →  parse { api_key }  →  write to config  →  print "Login successful."
//!    - 400 authorization_pending  →  keep polling (show progress dot)
//!    - 400 slow_down              →  increase interval by 5 s (RFC 8628 §3.5)
//!    - 400 expired_token          →  exit 1 with timeout message
//!    - 400 access_denied          →  exit 1
//!    - 400 invalid_grant          →  exit 1 with error body
//!    - 429                        →  double interval, retry
//!    - network/parse error        →  retry up to 3 times then exit 1

use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;

use spelunk_core::config;

// ── Default cloud API base URL ────────────────────────────────────────────────

const DEFAULT_CLOUD_URL: &str = "https://api.spelunk.cloud";

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Override the spelunk cloud API URL (default: https://api.spelunk.cloud)
    #[arg(long, env = "SPELUNK_CLOUD_URL")]
    pub cloud_url: Option<String>,
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    api_key: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn login(args: LoginArgs) -> Result<()> {
    let cloud_url = args
        .cloud_url
        .as_deref()
        .unwrap_or(DEFAULT_CLOUD_URL)
        .trim_end_matches('/');

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    // ── Step 1: initiate device authorization ─────────────────────────────────
    let device_resp = client
        .post(format!("{cloud_url}/v1/auth/device"))
        .send()
        .await
        .context("POST /v1/auth/device failed")?;

    if !device_resp.status().is_success() {
        let status = device_resp.status();
        let body = device_resp.text().await.unwrap_or_default();
        anyhow::bail!("Device authorization request failed ({status}): {body}");
    }

    let device: DeviceCodeResponse = device_resp
        .json()
        .await
        .context("parsing device authorization response")?;

    // ── Step 2: prompt the user ───────────────────────────────────────────────
    println!();
    println!("Open the following URL in your browser:");
    println!();
    println!("  {}", device.verification_uri);
    println!();
    println!("Enter the code: {}", device.user_code);
    println!();

    // If the server provides a one-click URL, print it as a convenience.
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

    loop {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        let result = poll_token(&client, cloud_url, &device.device_code).await;

        match result {
            PollOutcome::Success(api_key) => {
                // Write to config before printing the success message so that a
                // write error surfaces before the user thinks they are logged in.
                config::save_api_key(&api_key)
                    .context("saving api_key to ~/.config/spelunk/config.toml")?;
                println!("\nLogin successful.");
                return Ok(());
            }
            PollOutcome::Pending => {
                // Print a progress dot without a newline so the user sees activity.
                print!(".");
                let _ = std::io::stdout().flush();
                consecutive_errors = 0;
            }
            PollOutcome::SlowDown => {
                // RFC 8628 §3.5: increase interval by 5 s on slow_down.
                interval_secs += 5;
                consecutive_errors = 0;
            }
            PollOutcome::RateLimit => {
                // 429: double the interval as a back-off.
                interval_secs *= 2;
                consecutive_errors = 0;
            }
            PollOutcome::Expired => {
                eprintln!("\nLogin timed out. Run `spelunk login` again.");
                eprintln!(
                    "Hint: your account may not be part of an organization yet — \
                     contact your admin."
                );
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
    }
}

// ── Poll outcome ──────────────────────────────────────────────────────────────

enum PollOutcome {
    /// Token issued — contains the api_key.
    Success(String),
    /// User has not yet approved.
    Pending,
    /// Server requests slower polling (RFC 8628 §3.5).
    SlowDown,
    /// HTTP 429 — back off.
    RateLimit,
    /// Device code expired.
    Expired,
    /// User explicitly denied.
    Denied,
    /// invalid_grant (e.g. code already used or revoked).
    InvalidGrant(String),
    /// Transient network / parse error.
    Error(anyhow::Error),
}

async fn poll_token(client: &reqwest::Client, cloud_url: &str, device_code: &str) -> PollOutcome {
    let body = serde_json::json!({ "device_code": device_code });

    let resp = match client
        .post(format!("{cloud_url}/v1/auth/device/token"))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return PollOutcome::Error(anyhow::anyhow!(e).context("network error")),
    };

    let status = resp.status();

    if status.is_success() {
        match resp.json::<TokenResponse>().await {
            Ok(t) => return PollOutcome::Success(t.api_key),
            Err(e) => {
                return PollOutcome::Error(anyhow::anyhow!(e).context("parsing token response"));
            }
        }
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return PollOutcome::RateLimit;
    }

    // Parse the error body.
    let err: ErrorResponse = match resp.json().await {
        Ok(e) => e,
        Err(e) => {
            return PollOutcome::Error(anyhow::anyhow!(e).context("parsing error response"));
        }
    };

    match err.error.as_str() {
        "authorization_pending" => PollOutcome::Pending,
        "slow_down" => PollOutcome::SlowDown,
        "expired_token" => PollOutcome::Expired,
        "access_denied" => PollOutcome::Denied,
        "invalid_grant" => {
            let msg = err
                .error_description
                .unwrap_or_else(|| "invalid_grant".to_string());
            PollOutcome::InvalidGrant(msg)
        }
        other => PollOutcome::Error(anyhow::anyhow!(
            "unexpected error from token endpoint: {other}"
        )),
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use spelunk_core::config;

    /// save_api_key_to creates the file and writes the key.
    #[test]
    fn save_api_key_creates_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        config::save_api_key_to("sk-sp-test", &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("api_key"), "should contain api_key");
        assert!(
            contents.contains("sk-sp-test"),
            "should contain the key value"
        );
    }

    /// save_api_key_to preserves existing keys.
    #[test]
    fn save_api_key_preserves_other_keys() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_url = \"http://localhost:7777\"\n").unwrap();
        config::save_api_key_to("sk-sp-test", &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("server_url"),
            "should still contain server_url"
        );
        assert!(
            contents.contains("sk-sp-test"),
            "should contain the new key"
        );
    }

    /// save_api_key_to replaces an existing api_key entry.
    #[test]
    fn save_api_key_replaces_existing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "api_key = \"sk-sp-old\"\n").unwrap();
        config::save_api_key_to("sk-sp-new", &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("sk-sp-old"), "old key should be gone");
        assert!(contents.contains("sk-sp-new"), "new key should be present");
    }

    /// remove_api_key_from removes the api_key line.
    #[test]
    fn remove_api_key_removes_line() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "server_url = \"http://localhost:7777\"\napi_key = \"sk-sp-test\"\n",
        )
        .unwrap();
        config::remove_api_key_from(&path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("api_key"), "api_key should be removed");
        assert!(
            contents.contains("server_url"),
            "server_url should still be present"
        );
    }

    /// remove_api_key_from is a no-op when the file does not exist.
    #[test]
    fn remove_api_key_no_op_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        // Must not error even when file is absent.
        config::remove_api_key_from(&path).unwrap();
    }
}

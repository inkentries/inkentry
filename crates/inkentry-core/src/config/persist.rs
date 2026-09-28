use anyhow::{Context, Result};
use std::path::Path;

use super::paths::inkentry_config_dir;

// The WorkOS session lives in the secret store, keyed per organization (see
// `super::org_tokens`), never here. These remove functions exist only to
// strip a legacy plaintext `[auth]` table.

/// Remove the legacy `[auth]` table from `~/.config/inkentry/config.toml`.
///
/// Used by the migration off plaintext and by `inkentry logout` to clear a
/// legacy remnant. Does not touch self-hosted server keys as a side effect;
/// removing those is the explicit `inkentry auth remove-key` (see
/// [`super::server_keys::clear_origin`] and
/// [`super::server_keys::clear_all`]). No-op if the file or the table is
/// absent. Other keys are preserved.
pub fn remove_auth_tokens() -> Result<()> {
    remove_auth_tokens_from(&inkentry_config_dir().join("config.toml"))
}

/// Same as [`remove_auth_tokens`] but operates on an explicit path (tests).
pub fn remove_auth_tokens_from(config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }
    let mut doc = read_config_table(config_path)?;
    if doc.remove("auth").is_none() {
        return Ok(());
    }
    let serialised = toml::to_string_pretty(&doc).context("serialising config.toml")?;
    write_config_secure(config_path, &serialised)
}

/// Write `slug` as `project_id` to a project-level `.inkentry/config.toml`,
/// creating the file (and parent dir) if absent. Other keys are preserved.
///
/// Returns `(effective_slug, wrote)`: if the file already sets `project_id`,
/// it is left untouched — `wrote` is `false` and the existing value is returned
/// (no retroactive rename). This is a committed, shared file, so it is written
/// with normal permissions (unlike the secret-bearing personal config).
pub fn write_project_slug(config_path: &Path, slug: &str) -> Result<(String, bool)> {
    let mut doc = read_config_table(config_path)?;
    if let Some(existing) = doc.get("project_id").and_then(|v| v.as_str()) {
        return Ok((existing.to_string(), false));
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    doc.insert(
        "project_id".to_string(),
        toml::Value::String(slug.to_string()),
    );
    let serialised = toml::to_string_pretty(&doc).context("serialising config.toml")?;
    std::fs::write(config_path, serialised)
        .with_context(|| format!("writing {}", config_path.display()))?;
    Ok((slug.to_string(), true))
}

// Parse the config file into a toml::Table, returning an empty table when the
// file does not exist.
fn read_config_table(config_path: &Path) -> Result<toml::Table> {
    if !config_path.exists() {
        return Ok(toml::Table::new());
    }
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    raw.parse::<toml::Table>()
        .with_context(|| format!("parsing {}", config_path.display()))
}

// Write `contents` to `config_path` and tighten permissions to 0600 on Unix so
// secrets in the file are owner-only.
fn write_config_secure(config_path: &Path, contents: &str) -> Result<()> {
    std::fs::write(config_path, contents)
        .with_context(|| format!("writing {}", config_path.display()))?;
    set_owner_only_permissions(config_path)?;
    Ok(())
}

// Set 0600 permissions on Unix; a no-op on other platforms.
#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("setting 0600 permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_project_slug_creates_file_and_reports_written() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join(".inkentry").join("config.toml");

        let (slug, wrote) = write_project_slug(&cfg, "github.com/acme/app").unwrap();
        assert_eq!(slug, "github.com/acme/app");
        assert!(wrote);

        let raw = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            raw.contains("project_id = \"github.com/acme/app\""),
            "{raw}"
        );
    }

    #[test]
    fn write_project_slug_preserves_existing_and_does_not_rename() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            "server_url = \"http://team:4655\"\nproject_id = \"old/slug\"\n",
        )
        .unwrap();

        let (slug, wrote) = write_project_slug(&cfg, "local/deadbeef").unwrap();
        assert_eq!(slug, "old/slug");
        assert!(!wrote);

        let raw = std::fs::read_to_string(&cfg).unwrap();
        assert!(raw.contains("project_id = \"old/slug\""), "{raw}");
        assert!(raw.contains("server_url"), "other keys preserved: {raw}");
    }

    #[test]
    fn write_project_slug_adds_key_preserving_other_keys() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(&cfg, "server_url = \"http://team:4655\"\n").unwrap();

        let (_, wrote) = write_project_slug(&cfg, "github.com/acme/app").unwrap();
        assert!(wrote);

        let raw = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            raw.contains("project_id = \"github.com/acme/app\""),
            "{raw}"
        );
        assert!(raw.contains("server_url"), "existing key preserved: {raw}");
    }

    #[test]
    fn remove_auth_tokens_no_op_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        remove_auth_tokens_from(&path).unwrap();
    }
}

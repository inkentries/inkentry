# Releasing inkentry

This document describes how to cut a release of inkentry.

## Overview

Releases are fully automated via GitHub Actions. Pushing a version tag triggers
`.github/workflows/release.yml`, which:

1. Builds `inkentry` and `inkentry-server` release binaries for all supported platforms.
2. Strips binaries where possible to reduce download size.
3. Packages each platform's binaries into a `.tar.gz` archive.
4. Builds an `amd64` Debian package (`inkentry_<version>_amd64.deb`).
5. Creates a GitHub Release and attaches all `.tar.gz` archives and the `.deb` as downloadable assets.
6. Auto-generates release notes from merged pull requests and commits.

Three install paths live outside this workflow:

- **`install.sh` and `install.ps1`** are not in this repository. They live in
  `inkentries/get.inkentry.com`, served over GitHub Pages at
  `https://get.inkentry.com/install.sh` and `.../install.ps1`, which are the
  commands `README.md` and `docs/getting-started.md` publish. Neither needs a
  per-release edit: each resolves the newest release tag through the GitHub
  API, falling back to the newest release of any kind because
  `/releases/latest` excludes pre-releases and so 404s for the whole
  `v1.0.0-rc` cycle.

  They do need an edit when asset naming changes, and nothing checks it. The
  scripts rebuild the archive name from the git tag
  (`inkentry-<tag>-<target>.tar.gz`) to match `.github/workflows/release.yml`,
  and that agreement is held by hand: the tap and the bucket below at least
  have a generator job here, while nothing in this repository writes or
  verifies the installer. The scripts shipped with the wrong shape once
  already, and a person running the command caught it, not CI. **If you change
  what the release workflow names its assets, change the scripts in
  `inkentries/get.inkentry.com` in the same round of work.**
- **Homebrew tap** lives in the separate `inkentries/homebrew-inkentry`
  repo, seeded at v0.9.8. The `update-homebrew-formula` job in
  `.github/workflows/release.yml` regenerates `Formula/inkentry.rb` with the new
  `url`/`sha256`/`version` and pushes it to that repo's `main` branch directly,
  using the `HOMEBREW_TAP_TOKEN` secret (a token with `contents: write` on
  `homebrew-inkentry` — `GITHUB_TOKEN` only has access to this repo). The
  workflow points at the new tap rather than the old organisation's, which
  serves the pre-rename binary names.
- **Scoop bucket** lives in the separate `inkentries/scoop-inkentry` repo,
  seeded at v0.9.8. `update-scoop-manifest` regenerates `bucket/inkentry.json`
  there and pushes to its `main` directly, using a `SCOOP_BUCKET_TOKEN` secret
  with `contents: write` on the bucket — `HOMEBREW_TAP_TOKEN` is scoped to the
  tap and cannot be reused. It lived in this repo until the bucket was split
  out; a pull request opened with `GITHUB_TOKEN` does not trigger workflows, so
  the merge waited on someone approving a workflow run by hand, and the manifest
  commit landed after the tag was cut and so opened the *next* release's
  changelog carrying the *previous* version.

  Both packaging jobs publish every stable tag, plus pre-release tags on the
  `v1.0.0` line only — the release candidates for v1 are installable through
  brew and scoop, and the condition expires on its own once v1.0.0 ships, since
  a later `v1.1.0-rc0` no longer matches `v1.0.0-`. Without that, a `1.1.0-rc`
  would sort *above* `1.0.0` in both package managers and be served to everyone
  tracking stable.

  Neither the tap nor the bucket keeps more than one file, so publishing order,
  not version order, decides what users get. Both generator scripts read the
  version already published and refuse to write one that does not sort above it
  (`version-order.js`, tested in CI): re-running an older tag's release workflow
  leaves the tap alone rather than downgrading every installed user. There is no
  override — roll a bad release forward with a new tag.

## Supported platforms

| Target | Runner | Archive format | Notes |
|--------|--------|---------------|-------|
| `x86_64-unknown-linux-gnu` | ubuntu-latest | `.tar.gz` | Built in a `debian:11` container; binaries stripped. Ships the llama.cpp Vulkan engine: the archive also carries `lib*.so*` (core engine libs + ggml backend modules) that must stay next to the binaries |
| `aarch64-unknown-linux-gnu` | ubuntu-24.04-arm | `.tar.gz` | Native arm64 runner, built in a `debian:11` container. candle engine only (LunarG ships no arm64-linux Vulkan SDK) |
| `aarch64-apple-darwin` | macos-latest | `.tar.gz` | Native build (Apple Silicon), candle Metal engine |
| `x86_64-pc-windows-msvc` | windows-latest | `.zip` | Native build; produces `.exe` binaries plus the llama.cpp engine DLLs (`ggml*.dll`, `llama*.dll`), which must stay next to the `.exe`s |

> **Note:** `x86_64-apple-darwin` (Intel Mac) prebuilt binaries were dropped —
> Apple deprecated the architecture and Apple Silicon replaced it on new
> hardware six years ago. Intel Mac users build from source (see
> `docs/building.md`).

## Local dry run before tagging

The release workflow only triggers on a pushed `v*.*.*` tag: there is no
`workflow_dispatch`, so the packaging pipeline (glibc-floor container build,
the `.deb`'s `dpkg-shlibdeps`-derived `Depends`, and the floor install/smoke
test) otherwise gets exercised for the first time at real tag-push, after
which a passing run cascades straight into a GitHub Release and the
Homebrew/Scoop publish steps.

`scripts/release-dry-run.sh` reproduces the Linux x86_64 leg of that
pipeline locally, with Docker as the only prerequisite:

```bash
scripts/release-dry-run.sh
```

It builds `inkentry` + `inkentry-server` inside `debian:11` (the glibc 2.31
floor), runs the same glibc-ceiling check as CI, assembles and builds the
`.deb` (with `Depends` derived inside `debian:11`, matching the workflow),
and installs + smoke-tests the result in fresh `debian:11` / `ubuntu:20.04`
/ `ubuntu:24.04` containers.

**What it proves:** the Linux x86_64 build links against the glibc floor,
the `.deb` installs and its shipped binaries actually run (not just link)
on the support floor.

**What it does not prove:** macOS/Windows builds, the arm64 Linux leg, the
real GitHub Release, or the Homebrew/Scoop publish steps. Those are only
exercised by `.github/workflows/release.yml` at real tag-push time. The
script has no code path that can create a GitHub release, push to the
`homebrew-inkentry` tap, or write the Scoop bucket's `bucket/inkentry.json`.

Run it before tagging; a failure here is cheaper to fix than one discovered
after a tag is already pushed.

### 1. Bump the version in the four crate manifests

**The root `Cargo.toml` has no version to bump.** It is a virtual workspace
manifest — `[workspace]`, `members`, `[workspace.dependencies]` — with no
`[package]` section, no `version` field, and no `[workspace.package]` for the
crates to inherit from. Editing it changes nothing, and a tag pushed on that
basis ships binaries still reporting the previous version.

The version lives in each crate's own manifest, and all four move together:

- `crates/inkentry-cli/Cargo.toml`
- `crates/inkentry-core/Cargo.toml`
- `crates/inkentry-embed/Cargo.toml`
- `crates/inkentry-server/Cargo.toml`

```toml
[package]
name = "inkentry-cli"
version = "1.0.0"   # <-- update this, in all four manifests
```

Then regenerate `Cargo.lock` with cargo — never hand-edit it. The workspace
members are path dependencies, so their recorded versions have to move too:

```bash
INKENTRY_SECRET_STORE=file cargo update --workspace --offline
```

That should touch exactly the four `inkentry-*` entries and nothing else.

Promote the changelog next: move the accumulated `## [Unreleased]` notes in
`CHANGELOG.md` into a new `## [<version>] — <date>` section, leaving
`## [Unreleased]` in place and empty. Anything user-facing that landed without
an entry gets written now, based on `git log` since the previous bump.

Finally, confirm the bump actually reached the binary rather than assuming it
did:

```bash
INKENTRY_SECRET_STORE=file cargo build -p inkentry-cli
./target/debug/inkentry --version
```

### 1a. Check for hardcoded version references in docs

The install docs were rewritten to avoid hardcoding the version: `docs/getting-started.md`
points at `install.sh` / Homebrew and uses a `<version>` placeholder for manual
tarball and `.deb` downloads, so it normally needs no per-release edit. Still,
sweep for stray hardcoded versions before tagging:

```bash
grep -rn "inkentry-v[0-9]\|inkentry_[0-9]" docs/ README.md
```

Fix anything that pins a specific old version (use `<version>` or point at
`install.sh`). Commit everything together:

```bash
git add crates/*/Cargo.toml Cargo.lock CHANGELOG.md docs/
git commit -m "chore: bump version to 1.0.0"
git push origin main
```

### 2. Tag and push

```bash
git tag v1.0.0
git push origin v1.0.0
```

That's it. The release workflow triggers automatically on the pushed tag.

### 3. Monitor the workflow

Watch progress at:
`https://github.com/inkentries/inkentry/actions/workflows/release.yml`

Once all jobs pass, the release appears at:
`https://github.com/inkentries/inkentry/releases/tag/v1.0.0`

## Pre-releases

Append a pre-release suffix to the tag. The workflow automatically marks the
GitHub Release as a pre-release when the tag contains `-rc`, `-beta`, or
`-alpha`:

```bash
git tag v1.0.0-rc.1
git push origin v1.0.0-rc.1
```

## Download URLs

After a release is published, assets follow these patterns (the `<version>`
segment is the full tag, e.g. `v1.0.0`):

```
# Unix tarballs
https://github.com/inkentries/inkentry/releases/download/<version>/inkentry-<version>-<target>.tar.gz

# Windows zip
https://github.com/inkentries/inkentry/releases/download/<version>/inkentry-<version>-x86_64-pc-windows-msvc.zip

# Debian package (amd64)
https://github.com/inkentries/inkentry/releases/download/<version>/inkentry_<version-no-v>_amd64.deb

# Checksums for every archive and package above
https://github.com/inkentries/inkentry/releases/download/<version>/SHA256SUMS
```

The release job also signs a build-provenance attestation for every archive and
package, so a download can be traced back to this repository and the commit it
was built from:

```bash
gh attestation verify inkentry-<version>-<target>.tar.gz --repo inkentries/inkentry
```

Worth running once by hand after a release, alongside checking the assets are
all present — it is the only step of the release that fails silently from a
user's point of view, because nothing downstream reads it.

Examples for `v1.0.0`:

```bash
# macOS Apple Silicon
https://github.com/inkentries/inkentry/releases/download/v1.0.0/inkentry-v1.0.0-aarch64-apple-darwin.tar.gz

# Linux x86_64
https://github.com/inkentries/inkentry/releases/download/v1.0.0/inkentry-v1.0.0-x86_64-unknown-linux-gnu.tar.gz

# Linux ARM64
https://github.com/inkentries/inkentry/releases/download/v1.0.0/inkentry-v1.0.0-aarch64-unknown-linux-gnu.tar.gz

# Windows x86_64
https://github.com/inkentries/inkentry/releases/download/v1.0.0/inkentry-v1.0.0-x86_64-pc-windows-msvc.zip

# Debian (amd64)
https://github.com/inkentries/inkentry/releases/download/v1.0.0/inkentry_1.0.0_amd64.deb
```

> `releases/latest/download/<asset>` also works when the asset name is exact,
> but the tag-pinned `releases/download/<version>/<asset>` form is unambiguous
> and avoids the 404s you get when an asset filename changes between releases
> and a `latest` URL still names the old one.

## Deleting a bad release

If a release needs to be pulled:

```bash
# Delete the tag locally and on remote
git tag -d v1.0.0
git push origin :refs/tags/v1.0.0

# Delete the GitHub Release (requires gh CLI)
gh release delete v1.0.0 --yes
```

Then fix the issue, re-commit, and re-tag.

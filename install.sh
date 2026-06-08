#!/bin/sh
# spelunk installer — https://spelunk.cloud
# Usage: curl -fsSL https://spelunk.cloud/install.sh | sh
#        curl -fsSL https://spelunk.cloud/install.sh | sh -s -- --dry-run
set -e

REPO="spelunk-cloud/spelunk"
DRY_RUN=0

for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    *)
      printf 'Unknown flag: %s\n' "$arg" >&2
      exit 1
      ;;
  esac
done

# ── OS detection ──────────────────────────────────────────────────────────────
OS="$(uname -s)"
case "$OS" in
  Linux)  OS_NAME="linux" ;;
  Darwin) OS_NAME="macos" ;;
  *)
    printf 'Unsupported OS: %s\n' "$OS" >&2
    printf 'spelunk supports Linux and macOS. Please install from source:\n' >&2
    printf '  https://github.com/%s\n' "$REPO" >&2
    exit 1
    ;;
esac

# ── Arch detection ────────────────────────────────────────────────────────────
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64)          ARCH_NAME="x86_64" ;;
  aarch64|arm64)   ARCH_NAME="aarch64" ;;
  *)
    printf 'Unsupported architecture: %s\n' "$ARCH" >&2
    printf 'spelunk supports x86_64 and aarch64/arm64.\n' >&2
    exit 1
    ;;
esac

# ── Map to release tarball name ───────────────────────────────────────────────
case "${OS_NAME}-${ARCH_NAME}" in
  linux-x86_64)   TARGET="x86_64-unknown-linux-gnu" ;;
  linux-aarch64)  TARGET="aarch64-unknown-linux-gnu" ;;
  macos-x86_64)
    # Apple deprecated this architecture (Apple Silicon replaced it on new
    # hardware six years ago) — we no longer publish a prebuilt binary for it.
    printf 'spelunk no longer ships a prebuilt binary for Intel Macs (x86_64-apple-darwin).\n' >&2
    printf 'Please build from source instead — see:\n' >&2
    printf '  https://github.com/%s/blob/main/docs/getting-started.md\n' "$REPO" >&2
    exit 1
    ;;
  macos-aarch64)  TARGET="aarch64-apple-darwin" ;;
esac

# ── Resolve latest version tag ───────────────────────────────────────────────
VERSION=$(curl -sf "https://api.github.com/repos/${REPO}/releases/latest" \
  | grep '"tag_name"' \
  | sed 's/.*"tag_name": *"\(.*\)".*/\1/')

if [ -z "$VERSION" ]; then
  printf 'Error: could not determine latest spelunk version\n' >&2
  exit 1
fi

TARBALL="spelunk-${VERSION}-${TARGET}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}"

# ── Install directory ─────────────────────────────────────────────────────────
if [ -w /usr/local/bin ]; then
  INSTALL_DIR="/usr/local/bin"
elif [ "$(id -u)" -eq 0 ]; then
  # Running as root but /usr/local/bin not writable? Unusual but handle it.
  INSTALL_DIR="/usr/local/bin"
  mkdir -p "$INSTALL_DIR"
else
  INSTALL_DIR="${HOME}/.local/bin"
  mkdir -p "$INSTALL_DIR"
fi

# ── Dry-run summary ───────────────────────────────────────────────────────────
printf '\n'
printf '  spelunk installer\n'
printf '  ─────────────────────────────────────────\n'
printf '  OS/arch  : %s / %s\n' "$OS" "$ARCH"
printf '  Version  : %s\n' "$VERSION"
printf '  Target   : %s\n' "$TARGET"
printf '  Download : %s\n' "$DOWNLOAD_URL"
printf '  Install  : %s/spelunk\n' "$INSTALL_DIR"
printf '             %s/spelunk-server\n' "$INSTALL_DIR"
printf '\n'

if [ "$DRY_RUN" -eq 1 ]; then
  printf '  Dry-run mode — nothing was installed.\n\n'
  exit 0
fi

# ── Check for required tools ──────────────────────────────────────────────────
for cmd in curl tar; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    printf 'Required tool not found: %s\n' "$cmd" >&2
    exit 1
  fi
done

# ── Download and extract ──────────────────────────────────────────────────────
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

printf 'Downloading %s ...\n' "$TARBALL"
curl --fail --silent --show-error --location \
  --output "${TMP_DIR}/${TARBALL}" \
  "$DOWNLOAD_URL"

printf 'Extracting ...\n'
tar -xzf "${TMP_DIR}/${TARBALL}" -C "$TMP_DIR"

# ── Install binaries ──────────────────────────────────────────────────────────
for bin in spelunk spelunk-server; do
  if [ -f "${TMP_DIR}/${bin}" ]; then
    install -m 755 "${TMP_DIR}/${bin}" "${INSTALL_DIR}/${bin}"
    printf 'Installed %s -> %s/%s\n' "$bin" "$INSTALL_DIR" "$bin"
  fi
done

# ── PATH hint ─────────────────────────────────────────────────────────────────
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) : ;;  # already in PATH
  *)
    printf '\n'
    printf '  Note: %s is not in your PATH.\n' "$INSTALL_DIR"
    printf '  Add the following to your shell profile:\n'
    printf '    export PATH="%s:$PATH"\n' "$INSTALL_DIR"
    printf '\n'
    ;;
esac

# ── Verify ────────────────────────────────────────────────────────────────────
printf '\n'
if command -v spelunk >/dev/null 2>&1; then
  spelunk --version
elif [ -x "${INSTALL_DIR}/spelunk" ]; then
  "${INSTALL_DIR}/spelunk" --version
fi
printf '\nspelunk installed successfully. Run `spelunk init` to get started.\n\n'

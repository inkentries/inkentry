#!/bin/sh
# Create the spelunk-cloud/homebrew-spelunk repo and push the tap formula.
# Run this once after merging the install-paths PR.
#
# Prerequisites: gh CLI authenticated as a spelunk-cloud org admin.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TAP_SRC="${REPO_ROOT}/homebrew-tap"
REMOTE="spelunk-cloud/homebrew-spelunk"
TMP_DIR="$(mktemp -d)"

trap 'rm -rf "$TMP_DIR"' EXIT

echo "Creating ${REMOTE} ..."
gh repo create "${REMOTE}" \
  --public \
  --description "Homebrew tap for spelunk" \
  --confirm 2>/dev/null || echo "(repo may already exist — continuing)"

echo "Setting up local clone in ${TMP_DIR} ..."
cp -r "${TAP_SRC}/." "${TMP_DIR}/"

git -C "$TMP_DIR" init
git -C "$TMP_DIR" checkout -b main
git -C "$TMP_DIR" add .
git -C "$TMP_DIR" \
  -c user.name="spelunk-cloud" \
  -c user.email="hello@spelunk.cloud" \
  commit -m "chore: initial homebrew tap for spelunk v0.8.0"

PUSH_URL="https://github.com/${REMOTE}.git"
echo "Pushing to ${PUSH_URL} ..."
git -C "$TMP_DIR" remote add origin "$PUSH_URL"
git -C "$TMP_DIR" push -u origin main

echo ""
echo "Done. Homebrew tap is live at https://github.com/${REMOTE}"
echo ""
echo "Install with:"
echo "  brew tap spelunk-cloud/spelunk"
echo "  brew install spelunk"

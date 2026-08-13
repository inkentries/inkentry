#!/usr/bin/env node
//
// Ordering for release versions, shared by update-homebrew-formula.js and
// update-scoop-manifest.js.
//
// Both generators overwrite a single file in a downstream repo, so whichever
// tag runs last wins — publishing order, not version order, decides what users
// get. That is fine while only stable tags publish, but once pre-releases
// publish too a re-run of an old tag (or a `1.1.0-rc0` cut after `1.0.0`) would
// hand every installed user a downgrade. The generators compare against the
// version already published and refuse to go backwards.
//
// The ordering matches what the two package managers themselves do with these
// tags, so the guard never disagrees with the client: Homebrew's
// `Version::RCToken` sorts `1.0.0-rc0` below `1.0.0`, and Scoop's
// `Compare-Version` treats a trailing `alpha|beta|rc|pre` block the same way.
//
// Only Node.js builtins are used — no extra npm dependencies.

"use strict";

// alpha < beta < rc/pre < (no pre-release at all).
const PRERELEASE_RANK = { alpha: 0, beta: 1, pre: 2, rc: 2 };

const VERSION_RE =
  /^v?(\d+(?:\.\d+)*)(?:-(alpha|beta|pre|rc)\.?(\d+)?)?$/i;

function parseVersion(version) {
  const match = VERSION_RE.exec(String(version).trim());
  if (!match) {
    throw new Error(
      `Unrecognised version: "${version}" (expected e.g. 1.0.0 or 1.0.0-rc0)`
    );
  }
  const [, release, prerelease, prereleaseNumber] = match;
  return {
    release: release.split(".").map(Number),
    prereleaseRank:
      prerelease === undefined
        ? Infinity
        : PRERELEASE_RANK[prerelease.toLowerCase()],
    prereleaseNumber:
      prereleaseNumber === undefined ? 0 : Number(prereleaseNumber),
  };
}

// -1 if a sorts below b, 0 if equal, 1 if a sorts above b.
function compareVersions(a, b) {
  const left = parseVersion(a);
  const right = parseVersion(b);

  const parts = Math.max(left.release.length, right.release.length);
  for (let i = 0; i < parts; i++) {
    // 1.0 and 1.0.0 are the same release; a missing part is zero, not absent.
    const diff = (left.release[i] ?? 0) - (right.release[i] ?? 0);
    if (diff !== 0) return Math.sign(diff);
  }

  if (left.prereleaseRank !== right.prereleaseRank) {
    return Math.sign(left.prereleaseRank - right.prereleaseRank);
  }
  return Math.sign(left.prereleaseNumber - right.prereleaseNumber);
}

module.exports = { compareVersions, parseVersion };

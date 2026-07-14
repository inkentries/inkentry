#!/usr/bin/env node
//
// Guard rails for write-deb-control.js.
// Run: node --test .github/scripts/write-deb-control.test.js
// (--test won't discover inside a dot-directory, so name the file.)
//
// node:test is a Node builtin, so this needs no package.json and no install.
// Black-box over the subprocess because that is the contract release.yml uses.
//
// Every case here is one way `Depends:` could go stale, blank, or truncated –
// the failure mode that shipped a .deb missing libdbus-1-3.

"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const SCRIPT = path.join(__dirname, "write-deb-control.js");

function run({ args = [], env = {} } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "deb-control-"));
  const out = path.join(dir, "control");
  const res = spawnSync(process.execPath, [SCRIPT, ...args, "--out", out], {
    encoding: "utf8",
    // Blank the inherited DEB_* so an ambient value can't mask a missing input.
    env: { ...process.env, DEB_VERSION: "", DEB_DEPENDS: "", ...env },
  });
  const control = fs.existsSync(out) ? fs.readFileSync(out, "utf8") : null;
  fs.rmSync(dir, { recursive: true, force: true });
  return { status: res.status, stderr: res.stderr, control };
}

const dependsLine = (control) =>
  control.split("\n").find((l) => l.startsWith("Depends:"));

test("--depends has no default: refuses rather than emitting a stale list", () => {
  const r = run({ args: ["--deb-version", "0.9.3"] });
  assert.equal(r.status, 1);
  assert.equal(r.control, null, "must not write a control file");
  assert.match(r.stderr, /--depends/);
});

test("an empty derived list fails instead of writing a blank Depends", () => {
  for (const depends of ["   ", "\n", "shlibs:Depends="]) {
    const r = run({ args: ["--deb-version", "0.9.3", "--depends", depends] });
    assert.equal(r.status, 1, `expected failure for ${JSON.stringify(depends)}`);
    assert.equal(r.control, null);
  }
});

test("folds dpkg-shlibdeps -O output onto one line, prefix stripped", () => {
  // A newline would end the field and silently drop the rest of the deps.
  const r = run({
    env: {
      DEB_VERSION: "0.9.3",
      DEB_DEPENDS:
        "shlibs:Depends=libc6 (>= 2.39),\nlibdbus-1-3 (>= 1.9.14),\nlibgcc-s1 (>= 4.2)",
    },
  });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(
    dependsLine(r.control),
    "Depends: libc6 (>= 2.39), libdbus-1-3 (>= 1.9.14), libgcc-s1 (>= 4.2)"
  );
});

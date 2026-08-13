#!/usr/bin/env node
//
// Guard rails for version-order.js.
// Run: node --test .github/scripts/version-order.test.js
// (--test won't discover inside a dot-directory, so name the file.)
//
// node:test is a Node builtin, so this needs no package.json and no install.
//
// Every case here is a way the tap or bucket could be handed a downgrade:
// a pre-release outranking the stable it precedes, a re-run of an old tag, or
// a version string the comparator quietly mis-reads instead of rejecting.

"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");

const { compareVersions } = require("./version-order.js");

const ASCENDING = [
  "0.9.8",
  "1.0.0-alpha1",
  "1.0.0-beta1",
  "1.0.0-rc0",
  "1.0.0-rc1",
  "1.0.0-rc10",
  "1.0.0",
  "1.0.1",
  "1.1.0-rc0",
  "1.1.0",
  "2.0.0",
];

test("the release tags we cut sort in the order they ship", () => {
  for (let i = 0; i < ASCENDING.length - 1; i++) {
    const lower = ASCENDING[i];
    const higher = ASCENDING[i + 1];
    assert.equal(compareVersions(lower, higher), -1, `${lower} < ${higher}`);
    assert.equal(compareVersions(higher, lower), 1, `${higher} > ${lower}`);
  }
});

test("a pre-release never outranks the stable release it precedes", () => {
  // The whole reason the guard exists: 1.1.0-rc0 lands after 1.0.0 in time,
  // and must not be published over it.
  assert.equal(compareVersions("1.0.0-rc0", "1.0.0"), -1);
  assert.equal(compareVersions("1.1.0-rc0", "1.0.0"), 1);
  assert.equal(compareVersions("1.0.0-rc10", "1.0.0-rc9"), 1);
});

test("equal versions compare equal, with or without the tag's leading v", () => {
  assert.equal(compareVersions("1.0.0", "1.0.0"), 0);
  assert.equal(compareVersions("v1.0.0-rc0", "1.0.0-rc0"), 0);
  assert.equal(compareVersions("1.0", "1.0.0"), 0);
});

test("an unreadable version is rejected, not guessed at", () => {
  for (const bad of ["", "latest", "1.0.0-nightly", "1.0.0rc0", "1..0"]) {
    assert.throws(
      () => compareVersions(bad, "1.0.0"),
      /Unrecognised version/,
      `expected ${JSON.stringify(bad)} to be rejected`
    );
  }
});

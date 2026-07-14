#!/usr/bin/env node
//
// Writes BUILD/DEBIAN/control from a string-literal template.
//
// dpkg is strict about DEBIAN/control formatting — no leading whitespace on
// field lines, exactly one blank line between the short description and the
// long description, and a trailing newline.
//
// Usage:
//   node write-deb-control.js \
//     --deb-version 0.8.1 \
//     --depends 'libc6 (>= 2.39), libdbus-1-3 (>= 1.9.14)' \
//     [--out BUILD/DEBIAN/control]
//
// DEB_VERSION / DEB_DEPENDS may also come from the environment (flag wins).
//
// --depends is required and deliberately has no default: it must be derived
// from the shipped binaries with dpkg-shlibdeps, never hardcoded here.
//
// Only Node.js builtins are used (fs, path) — no extra npm dependencies.

"use strict";

const fs = require("fs");
const path = require("path");

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (!arg.startsWith("--")) continue;
    const key = arg.slice(2);
    const value = argv[i + 1];
    if (value === undefined || value.startsWith("--")) {
      throw new Error(`Missing value for --${key}`);
    }
    args[key] = value;
    i++;
  }
  return args;
}

function requireValue(name, ...sources) {
  for (const source of sources) {
    if (source !== undefined && source !== null && source !== "") {
      return source;
    }
  }
  throw new Error(
    `Missing required value: ${name} (pass it as a flag or environment variable)`
  );
}

// Accepts raw `dpkg-shlibdeps -O` output ("shlibs:Depends=…") or a bare list.
// Must fold to one line: a newline would truncate the field and silently drop
// the remaining dependencies.
function normaliseDepends(raw) {
  const depends = raw
    .replace(/^shlibs:Depends=/, "")
    .replace(/\s+/g, " ")
    .trim();
  if (depends === "") {
    throw new Error("--depends resolved to an empty dependency list");
  }
  return depends;
}

function buildControl(debVersion, depends) {
  // Leading whitespace matters for dpkg: field names must start at column 0.
  // The long-description line must be preceded by a single space and exactly
  // one blank line after the short description.
  return `Package: spelunk
Version: ${debVersion}
Architecture: amd64
Maintainer: spelunk-cloud <hello@spelunk.cloud>
Depends: ${depends}
Description: Code intelligence for AI agents
 spelunk provides persistent memory, a code graph, and semantic search
 for AI coding agents. Includes the spelunk CLI and spelunk-server.
Homepage: https://spelunk.cloud
`;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const env = process.env;

  const debVersion = requireValue(
    "--deb-version",
    args["deb-version"],
    env.DEB_VERSION
  );

  const depends = normaliseDepends(
    requireValue("--depends", args.depends, env.DEB_DEPENDS)
  );

  const outPath = path.resolve(
    requireValue("--out", args.out, "BUILD/DEBIAN/control")
  );

  // Ensure parent directory exists
  fs.mkdirSync(path.dirname(outPath), { recursive: true });

  const control = buildControl(debVersion, depends);

  fs.writeFileSync(outPath, control, "utf8");
  process.stdout.write(`Wrote ${outPath}\n`);
}

main();

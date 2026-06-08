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
//     [--out BUILD/DEBIAN/control]
//
// DEB_VERSION may also come from the DEB_VERSION env var (flag wins).
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

function buildControl(debVersion) {
  // Leading whitespace matters for dpkg: field names must start at column 0.
  // The long-description line must be preceded by a single space and exactly
  // one blank line after the short description.
  return `Package: spelunk
Version: ${debVersion}
Architecture: amd64
Maintainer: spelunk-cloud <hello@spelunk.cloud>
Depends: libc6 (>= 2.17)
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

  const outPath = path.resolve(
    requireValue("--out", args.out, "BUILD/DEBIAN/control")
  );

  // Ensure parent directory exists
  fs.mkdirSync(path.dirname(outPath), { recursive: true });

  const control = buildControl(debVersion);

  fs.writeFileSync(outPath, control, "utf8");
  process.stdout.write(`Wrote ${outPath}\n`);
}

main();

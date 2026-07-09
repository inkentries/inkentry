#!/usr/bin/env node
//
// Regenerates bucket/spelunk.json from a template.
//
// The manifest is always written from scratch. Given a version and the Windows
// x86_64 release-asset sha256 digest, this script produces the exact file
// contents and writes them out.
//
// Invoked by the update-scoop-manifest job in .github/workflows/release.yml on
// every stable tag push, which then commits the result back to the bucket in
// this repo.
//
// Usage:
//   node update-scoop-manifest.js \
//     --version 0.8.1 \
//     --sha-x86_64-windows <hex> \
//     [--out bucket/spelunk.json]
//
// --sha-x86_64-windows may also be supplied via SHA_X86_64_WINDOWS, and
// --version via VERSION. Flags win when both are present.
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

function assertSha256(name, value) {
  if (!/^[0-9a-f]{64}$/i.test(value)) {
    throw new Error(
      `Invalid sha256 for ${name}: "${value}" (expected 64 hex characters)`
    );
  }
  return value.toLowerCase();
}

function buildManifest({ version, shaX86_64Windows }) {
  const base = "https://github.com/spelunk-cloud/spelunk";
  const manifest = {
    version,
    description:
      "Code intelligence for AI agents - persistent memory, code graph, search",
    homepage: base,
    license: "MIT",
    architecture: {
      "64bit": {
        url: `${base}/releases/download/v${version}/spelunk-v${version}-x86_64-pc-windows-msvc.zip`,
        hash: shaX86_64Windows,
      },
    },
    bin: ["spelunk.exe", "spelunk-server.exe"],
    checkver: {
      github: base,
    },
    autoupdate: {
      architecture: {
        "64bit": {
          url: `${base}/releases/download/v$version/spelunk-v$version-x86_64-pc-windows-msvc.zip`,
        },
      },
    },
  };
  return JSON.stringify(manifest, null, 2) + "\n";
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const env = process.env;

  const rawVersion = requireValue("--version", args.version, env.VERSION);
  // The manifest `version` field never carries the leading "v"; the URL
  // templates add it back ("v${version}" / "v$version").
  const version = rawVersion.replace(/^v/, "");

  const shaX86_64Windows = assertSha256(
    "x86_64-pc-windows-msvc",
    requireValue(
      "--sha-x86_64-windows",
      args["sha-x86_64-windows"],
      env.SHA_X86_64_WINDOWS
    )
  );

  const outPath = path.resolve(
    requireValue("--out", args.out, "bucket/spelunk.json")
  );

  const manifest = buildManifest({ version, shaX86_64Windows });

  fs.mkdirSync(path.dirname(outPath), { recursive: true });
  fs.writeFileSync(outPath, manifest, "utf8");
  process.stdout.write(`Wrote ${outPath}\n`);
  process.stdout.write(manifest);
}

main();

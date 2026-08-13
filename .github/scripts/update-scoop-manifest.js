#!/usr/bin/env node
//
// Regenerates bucket/inkentry.json from a template.
//
// The manifest is always written from scratch. Given a version and the Windows
// x86_64 release-asset sha256 digest, this script produces the exact file
// contents and writes them out.
//
// Invoked by the update-scoop-manifest job in .github/workflows/release.yml,
// which then commits the result back to the inkentries/scoop-inkentry bucket.
//
// The bucket holds one manifest, so publishing order — not version order —
// decides what users get. The manifest already in the bucket is read first and
// a version that does not sort above it is refused, leaving the file untouched:
// re-running an older tag's release workflow would otherwise hand every
// installed user a downgrade. Roll a bad release forward with a new tag; there
// is no flag to republish an older one.
//
// Usage:
//   node update-scoop-manifest.js \
//     --version 0.8.1 \
//     --sha-x86_64-windows <hex> \
//     [--out bucket/inkentry.json]
//
// --sha-x86_64-windows may also be supplied via SHA_X86_64_WINDOWS, and
// --version via VERSION. Flags win when both are present.
//
// Only Node.js builtins are used (fs, path) — no extra npm dependencies.

"use strict";

const fs = require("fs");
const path = require("path");
const { compareVersions } = require("./version-order.js");

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

// null means "nothing to protect": an absent or unreadable manifest is
// overwritten rather than treated as a floor no release can clear.
function publishedVersion(outPath) {
  if (!fs.existsSync(outPath)) return null;
  try {
    const version = JSON.parse(fs.readFileSync(outPath, "utf8")).version;
    return typeof version === "string" ? version : null;
  } catch {
    return null;
  }
}

function buildManifest({ version, shaX86_64Windows }) {
  const base = "https://github.com/inkentries/inkentry";
  const manifest = {
    version,
    description:
      "Code intelligence for AI agents - persistent memory, code graph, search",
    homepage: base,
    license: "MIT",
    architecture: {
      "64bit": {
        url: `${base}/releases/download/v${version}/inkentry-v${version}-x86_64-pc-windows-msvc.zip`,
        hash: shaX86_64Windows,
      },
    },
    bin: ["inkentry.exe", "inkentry-server.exe"],
    checkver: {
      github: base,
    },
    autoupdate: {
      architecture: {
        "64bit": {
          url: `${base}/releases/download/v$version/inkentry-v$version-x86_64-pc-windows-msvc.zip`,
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
    requireValue("--out", args.out, "bucket/inkentry.json")
  );

  const published = publishedVersion(outPath);
  if (published !== null && compareVersions(version, published) <= 0) {
    process.stderr.write(
      `Refusing to publish ${version} over ${published}: it does not sort ` +
        `above what the bucket already serves. Leaving ${outPath} untouched.\n`
    );
    return;
  }

  const manifest = buildManifest({ version, shaX86_64Windows });

  fs.mkdirSync(path.dirname(outPath), { recursive: true });
  fs.writeFileSync(outPath, manifest, "utf8");
  process.stdout.write(`Wrote ${outPath}\n`);
  process.stdout.write(manifest);
}

main();

#!/usr/bin/env node
// Find the binary npm installed for this platform, then become it.
//
// The binary arrives as an optional dependency, not as a postinstall download,
// so the only host an install talks to is the npm registry the machine is
// already pointed at. That is what makes this work behind a proxy, under
// `npm ci --ignore-scripts`, and off a private mirror.
//
// This file is duplicated in npm/mcp/bin/krowk-mcp.js. Two published packages
// each have to stand alone, and a shared module would mean a third package or a
// codegen step to save sixty lines. npm/build.mjs checks both copies name the
// same platforms, which is the drift that would actually bite.
"use strict";

const { spawnSync } = require("node:child_process");
const path = require("node:path");

// Keyed by `${process.platform}-${process.arch}`, so the lookup is the identity.
const PACKAGES = {
  "darwin-arm64": "@krowk/cli-darwin-arm64",
  "darwin-x64": "@krowk/cli-darwin-x64",
  "linux-arm64": "@krowk/cli-linux-arm64",
  "linux-x64": "@krowk/cli-linux-x64",
  "win32-x64": "@krowk/cli-win32-x64",
};

const BINARY = "krowk";

// fail prints the reason and the next action, then stops. Every exit from here
// is a person or an agent who cannot upload, so neither is left guessing.
function fail(lines) {
  process.stderr.write(BINARY + ": " + lines.join("\n") + "\n");
  process.exit(1);
}

function locate() {
  const host = process.platform + "-" + process.arch;
  const pkg = PACKAGES[host];

  if (!pkg) {
    fail([
      "no prebuilt binary for " + host + ".",
      "",
      "Built for: " + Object.keys(PACKAGES).sort().join(", ") + ".",
      "Everywhere else, build it yourself with Rust (rustup.rs):",
      "",
      "  cargo install --locked --git https://github.com/krowkcom/krowk --features harness krowk",
    ]);
  }

  let manifest;
  try {
    manifest = require.resolve(pkg + "/package.json");
  } catch {
    fail([
      pkg + " is not installed, so there is no " + BINARY + " binary to run.",
      "",
      "It is an optional dependency, and installers skip those under",
      "--no-optional, --omit=optional and --ignore-optional. A lockfile",
      "written on another platform does the same thing.",
      "",
      "  npm install @krowk/cli --include=optional",
      "",
      "Or take the binary straight from the release, no Node involved:",
      "",
      "  https://github.com/krowkcom/krowk/releases/latest",
    ]);
  }

  return path.join(
    path.dirname(manifest),
    "bin",
    BINARY + (process.platform === "win32" ? ".exe" : ""),
  );
}

const binary = locate();

// stdio: "inherit" hands the child the real file descriptors, so nothing here
// sits between the binary and the terminal — or between it and a pipe.
const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  fail([
    "could not run " + binary + ": " + result.error.message,
    "",
    "The file is there but would not start. On a shared volume or an",
    "extracted archive that usually means the execute bit was lost:",
    "",
    "  chmod +x " + binary,
  ]);
}

// Re-raise the signal that killed the child, so `krowk` dies the way the binary
// did and a caller waiting on the exit status reads the same thing either way.
if (result.signal) {
  process.kill(process.pid, result.signal);
}

process.exit(result.status === null ? 1 : result.status);

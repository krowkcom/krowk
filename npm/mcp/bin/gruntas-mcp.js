#!/usr/bin/env node
// Find the binary npm installed for this platform, then become it.
//
// The binary arrives as an optional dependency, not as a postinstall download,
// so the only host an install talks to is the npm registry the machine is
// already pointed at. That is what makes this work behind a proxy, under
// `npm ci --ignore-scripts`, and off a private mirror.
//
// MCP speaks over stdio, so the passthrough below has to be exactly that:
// inherited file descriptors, nothing in this process reading or writing them.
//
// This file is duplicated in npm/cli/bin/gruntas.js. Two published packages each
// have to stand alone, and a shared module would mean a third package or a
// codegen step to save sixty lines. npm/build.mjs checks both copies name the
// same platforms, which is the drift that would actually bite.
"use strict";

const { spawnSync } = require("node:child_process");
const path = require("node:path");

// Keyed by `${process.platform}-${process.arch}`, so the lookup is the identity.
const PACKAGES = {
  "darwin-arm64": "@gruntas/cli-darwin-arm64",
  "darwin-x64": "@gruntas/cli-darwin-x64",
  "linux-arm64": "@gruntas/cli-linux-arm64",
  "linux-x64": "@gruntas/cli-linux-x64",
  "win32-x64": "@gruntas/cli-win32-x64",
};

const BINARY = "gruntas-mcp";

// fail prints the reason and the next action, then stops. Nothing readable
// reaches an MCP client from here — stdout is the protocol — so the whole
// explanation goes to stderr, where the client logs it.
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
      "  cargo install --locked --git https://github.com/gruntas/gruntas gruntas --bin gruntas-mcp",
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
      "  npm install @gruntas/mcp --include=optional",
      "",
      "Or take the binary straight from the release, no Node involved:",
      "",
      "  https://github.com/gruntas/gruntas/releases/latest",
    ]);
  }

  return path.join(
    path.dirname(manifest),
    "bin",
    BINARY + (process.platform === "win32" ? ".exe" : ""),
  );
}

const binary = locate();

// stdio: "inherit" hands the child the real file descriptors, so the MCP client
// on the other end is talking to the Go server directly.
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

// Re-raise the signal that killed the child, so `gruntas-mcp` dies the way the
// binary did and a client waiting on the exit status reads the same thing.
if (result.signal) {
  process.kill(process.pid, result.signal);
}

process.exit(result.status === null ? 1 : result.status);

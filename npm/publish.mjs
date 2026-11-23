#!/usr/bin/env node
// Publish what npm/build.mjs staged, in the order the manifest lists it.
//
//   node npm/publish.mjs [--dir dist/npm] [--dry-run]
//
// Two things this handles that a `for` loop in the workflow would not:
//
//   Credentials — with a token (NODE_AUTH_TOKEN or NPM_TOKEN), publish with it.
//   Without one, publish through trusted publishing: npm 11.5.1 or later trades
//   the workflow's OIDC token for a short-lived one, for packages whose
//   trusted publisher names this workflow. Either way the OIDC token also signs
//   provenance. Neither available fails the step: a release without its npm
//   packages is not a release to skip past quietly. Only --dry-run, which
//   publishes nothing, runs without credentials.
//
//   Already published — npm versions are immutable, so a re-run of a release
//   that got halfway must be able to finish rather than die on the first
//   package it already sent.

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function flag(name, fallback) {
  const at = process.argv.indexOf("--" + name);
  return at === -1 ? fallback : process.argv[at + 1];
}

const stagedIn = path.resolve(repo, flag("dir", "dist/npm"));
const dryRun = process.argv.includes("--dry-run");

function die(message) {
  process.stderr.write("npm/publish.mjs: " + message + "\n");
  process.exit(1);
}

// GitHub renders these in the run's annotations, so the reason a release has no
// npm packages is visible without opening the log.
function notice(message) {
  process.stdout.write(
    (process.env.GITHUB_ACTIONS ? "::notice::" : "npm/publish.mjs: ") + message + "\n",
  );
}

let manifest;
try {
  manifest = JSON.parse(fs.readFileSync(path.join(stagedIn, "manifest.json"), "utf8"));
} catch (err) {
  die("nothing staged at " + path.relative(repo, stagedIn) + ": " + err.message +
    "\n  Run `node npm/build.mjs` first.");
}

// setup-node writes an .npmrc that reads NODE_AUTH_TOKEN. NPM_TOKEN is what the
// repo secret is called, so accept either and let a human running this by hand
// use whichever they have exported.
const token = process.env.NODE_AUTH_TOKEN || process.env.NPM_TOKEN;
// What GitHub Actions sets when the job has `id-token: write`: npm reads these
// both for trusted publishing and for provenance.
const oidc = Boolean(
  process.env.ACTIONS_ID_TOKEN_REQUEST_URL && process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN,
);

// The first npm that publishes through OIDC.
const TRUSTED_NPM = [11, 5, 1];

function npmVersion() {
  const ran = spawnSync("npm", ["--version"], { encoding: "utf8" });
  if (ran.status !== 0) die("could not run `npm --version`");
  return ran.stdout.trim();
}

function atLeast(version, floor) {
  const parts = version.split(/[.-]/).slice(0, 3).map(Number);
  for (let i = 0; i < floor.length; i++) {
    if (parts[i] !== floor[i]) return parts[i] > floor[i];
  }
  return true;
}

let auth;
if (token) {
  // npm still tries OIDC first and uses it for a package with a trusted
  // publisher, so the token only covers the ones without.
  auth = oidc ? "token (or trusted publishing, where a package has one)" : "token";
} else if (oidc) {
  const version = npmVersion();
  if (!atLeast(version, TRUSTED_NPM)) {
    die("no npm token, and npm " + version + " cannot publish through OIDC" +
      "\n  (trusted publishing needs npm " + TRUSTED_NPM.join(".") + " or later).");
  }
  auth = "trusted publishing (OIDC)";
} else if (dryRun) {
  auth = "none (dry run)";
} else {
  die("no way to publish " + manifest.version + ": NODE_AUTH_TOKEN and NPM_TOKEN are unset," +
    "\n  and there is no OIDC token for trusted publishing (the job needs" +
    "\n  `permissions: id-token: write`). Set the NPM_TOKEN repo secret or configure" +
    "\n  a trusted publisher for each package, then re-run this workflow.");
}
process.stdout.write("npm/publish.mjs: publishing with " + auth + "\n");

// A prerelease under `latest` would hand every `npx @gruntas/cli` an rc. Park it on
// `next` and leave `latest` where it was.
const tag = manifest.prerelease ? "next" : "latest";

function alreadyPublished(name, version) {
  const seen = spawnSync("npm", ["view", name + "@" + version, "version"], {
    encoding: "utf8",
  });
  // Anything but a clean hit — a 404, an unclaimed name, a registry that is
  // having a day — means "publish and find out", which fails loudly rather than
  // skipping something that needed to go out.
  return seen.status === 0 && seen.stdout.trim() === version;
}

for (const pkg of manifest.packages) {
  const dir = path.join(stagedIn, pkg.dir);

  if (alreadyPublished(pkg.name, manifest.version)) {
    process.stdout.write(pkg.name + "@" + manifest.version + " is already published, skipping\n");
    continue;
  }

  const args = ["publish", "--access", "public", "--tag", tag];
  // Provenance needs the OIDC token; a human publishing by hand has none.
  if (oidc) args.push("--provenance");
  if (dryRun) args.push("--dry-run");

  process.stdout.write("publishing " + pkg.name + "@" + manifest.version + " (" + tag + ")\n");
  const published = spawnSync("npm", args, { cwd: dir, stdio: "inherit" });
  if (published.status !== 0) {
    die("npm publish failed for " + pkg.name + "@" + manifest.version +
      "\n  The packages before it in the manifest are already out; re-running this" +
      "\n  workflow will skip those and pick up here." +
      (auth === "token" ? "" :
        "\n  Without a token, npm needs a trusted publisher on " + pkg.name +
        "\n  naming this repository and .github/workflows/release.yml."));
  }
}

notice("published " + manifest.packages.length + " npm packages at " + manifest.version + " (" + tag + ")");

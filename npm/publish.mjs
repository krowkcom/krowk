#!/usr/bin/env node
// Publish what npm/build.mjs staged, in the order the manifest lists it.
//
//   node npm/publish.mjs [--dir dist/npm] [--dry-run]
//
// Two things this handles that a `for` loop in the workflow would not:
//
//   No token — the npm names may not be claimed yet, and a release that cannot
//   publish should still be a release. Missing credentials skip the publish and
//   say so; they do not fail the tag.
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
if (!token && !dryRun) {
  notice(
    "NPM_TOKEN is not set — skipping npm publish for " + manifest.version + ". " +
      "The GitHub release is unaffected. Set the NPM_TOKEN repo secret with " +
      "publish rights on the @gruntas scope, then re-run this workflow.",
  );
  process.exit(0);
}

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
  if (dryRun) args.push("--dry-run");

  process.stdout.write("publishing " + pkg.name + "@" + manifest.version + " (" + tag + ")\n");
  const published = spawnSync("npm", args, { cwd: dir, stdio: "inherit" });
  if (published.status !== 0) {
    die("npm publish failed for " + pkg.name + "@" + manifest.version +
      "\n  The packages before it in the manifest are already out; re-running this" +
      "\n  workflow will skip those and pick up here.");
  }
}

notice("published " + manifest.packages.length + " npm packages at " + manifest.version + " (" + tag + ")");

// Tests for npm/publish.mjs: which credentials it publishes with, and that it
// refuses to publish with none. A fake `npm` on PATH records each call, so
// nothing reaches the registry.
//
//   node --test npm/publish_test.mjs

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const script = path.join(path.dirname(fileURLToPath(import.meta.url)), "publish.mjs");

// `npm --version` answers FAKE_NPM_VERSION, `npm view` finds nothing, and
// `npm publish` exits FAKE_NPM_PUBLISH_STATUS. Every call is logged.
const FAKE_NPM = `#!/bin/sh
echo "$*" >> "$FAKE_NPM_LOG"
case "$1" in
  --version) echo "$FAKE_NPM_VERSION" ;;
  view) exit 1 ;;
  publish) exit "\${FAKE_NPM_PUBLISH_STATUS:-0}" ;;
esac
`;

const CLI_ONLY = [{ name: "@gruntas/cli", dir: "cli" }];

function run({ env = {}, args = [], packages = CLI_ONLY } = {}) {
  const work = fs.mkdtempSync(path.join(os.tmpdir(), "publish-test-"));
  const bin = path.join(work, "bin");
  const staged = path.join(work, "npm");
  fs.mkdirSync(bin);
  for (const p of packages) fs.mkdirSync(path.join(staged, p.dir), { recursive: true });
  fs.writeFileSync(path.join(bin, "npm"), FAKE_NPM, { mode: 0o755 });
  fs.writeFileSync(
    path.join(staged, "manifest.json"),
    JSON.stringify({ version: "2026.11.0", prerelease: false, packages }),
  );
  const log = path.join(work, "npm.log");

  const base = { ...process.env };
  for (const name of [
    "NODE_AUTH_TOKEN", "NPM_TOKEN", "ACTIONS_ID_TOKEN_REQUEST_URL", "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    "GITHUB_ACTIONS", "NPM_CONFIG_PROVENANCE",
  ]) delete base[name];

  const ran = spawnSync(process.execPath, [script, "--dir", staged, ...args], {
    encoding: "utf8",
    env: {
      ...base,
      PATH: bin + path.delimiter + process.env.PATH,
      FAKE_NPM_LOG: log,
      FAKE_NPM_VERSION: "11.21.3",
      ...env,
    },
  });
  const calls = fs.existsSync(log) ? fs.readFileSync(log, "utf8").trim().split("\n") : [];
  fs.rmSync(work, { recursive: true, force: true });
  return { ...ran, publishes: calls.filter((c) => c.startsWith("publish")) };
}

const OIDC = {
  ACTIONS_ID_TOKEN_REQUEST_URL: "https://token.actions.example/request",
  ACTIONS_ID_TOKEN_REQUEST_TOKEN: "request-token",
};

test("a token publishes with it, with provenance when there is an OIDC token", () => {
  const ran = run({ env: { NODE_AUTH_TOKEN: "npm_secret", ...OIDC } });
  assert.equal(ran.status, 0, ran.stderr);
  assert.match(ran.stdout, /publishing with token/);
  assert.deepEqual(ran.publishes, ["publish --access public --tag latest --provenance"]);
});

test("NPM_TOKEN counts as a token too", () => {
  const ran = run({ env: { NPM_TOKEN: "npm_secret" } });
  assert.equal(ran.status, 0, ran.stderr);
  assert.match(ran.stdout, /publishing with token/);
  assert.deepEqual(ran.publishes, ["publish --access public --tag latest"]);
});

test("without a token it publishes through OIDC, with provenance", () => {
  const ran = run({ env: OIDC });
  assert.equal(ran.status, 0, ran.stderr);
  assert.match(ran.stdout, /publishing with trusted publishing \(OIDC\)/);
  assert.deepEqual(ran.publishes, ["publish --access public --tag latest --provenance"]);
});

test("OIDC with an npm too old for trusted publishing fails before publishing", () => {
  const ran = run({ env: { ...OIDC, FAKE_NPM_VERSION: "10.9.2" } });
  assert.equal(ran.status, 1);
  assert.match(ran.stderr, /npm 10\.9\.2 cannot publish through OIDC/);
  assert.deepEqual(ran.publishes, []);
});

test("an OIDC publish npm rejects fails and names the trusted publisher", () => {
  const ran = run({ env: { ...OIDC, FAKE_NPM_PUBLISH_STATUS: "1" } });
  assert.equal(ran.status, 1);
  assert.match(ran.stderr, /npm publish failed for @gruntas\/cli@2026\.11\.0/);
  assert.match(ran.stderr, /trusted publisher on @gruntas\/cli/);
});

test("neither a token nor OIDC fails loudly instead of skipping", () => {
  const ran = run();
  assert.equal(ran.status, 1);
  assert.match(ran.stderr, /no way to publish 2026\.11\.0/);
  assert.deepEqual(ran.publishes, []);
});

test("a dry run needs no credentials", () => {
  const ran = run({ args: ["--dry-run"] });
  assert.equal(ran.status, 0, ran.stderr);
  assert.deepEqual(ran.publishes, ["publish --access public --tag latest --dry-run"]);
});

test("a package the build holds back is not published, and the run says why", () => {
  const packages = [...CLI_ONLY, { name: "@gruntas/pi", dir: "pi", held: "no trusted publisher yet" }];
  const ran = run({ env: OIDC, packages });
  assert.equal(ran.status, 0, ran.stderr);
  assert.deepEqual(ran.publishes, ["publish --access public --tag latest --provenance"]);
  assert.match(ran.stdout, /@gruntas\/pi@2026\.11\.0 is held back: no trusted publisher yet/);
  assert.match(ran.stdout, /published 1 npm packages/);
});

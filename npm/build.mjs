#!/usr/bin/env node
// Assemble the npm packages from the binaries scripts/dist.sh just built.
//
// Eight packages come out of one release: five that carry nothing but the two
// binaries for one platform, and the two launchers people actually name —
// `@gruntas/cli` and `@gruntas/mcp` — which depend on all five as optionalDependencies
// and use whichever npm decided applies. That is the esbuild pattern. The
// eighth is `@gruntas/pi`, the pi package, which carries no binary: it runs
// the pi extension the `gruntas` already on the PATH carries.
//
// The bytes are the release's bytes, read out of dist/, not a second `cargo build`
// that could differ from what the checksums cover.
//
//   node npm/build.mjs [--dist dist] [--out dist/npm]
//
// Node's standard library only, matching the Go module's own rule.

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");

// The one platform table. scripts/dist.sh's targets and the launchers' own
// tables are checked against it below, so adding a platform in one place and
// forgetting the others fails the build instead of the release.
const PLATFORMS = [
  { npm: "@gruntas/cli-darwin-arm64", goos: "darwin", goarch: "arm64", os: "darwin", cpu: "arm64" },
  { npm: "@gruntas/cli-darwin-x64", goos: "darwin", goarch: "amd64", os: "darwin", cpu: "x64" },
  { npm: "@gruntas/cli-linux-arm64", goos: "linux", goarch: "arm64", os: "linux", cpu: "arm64" },
  { npm: "@gruntas/cli-linux-x64", goos: "linux", goarch: "amd64", os: "linux", cpu: "x64" },
  { npm: "@gruntas/cli-win32-x64", goos: "windows", goarch: "amd64", os: "win32", cpu: "x64" },
];

// The binary names, which are also the artifact ids scripts/dist.sh records.
const BINARIES = ["gruntas", "gruntas-mcp"];

// The launchers, in the repo. Their versions and optionalDependencies are
// stamped on the way out; what is checked in stays at a version that cannot be
// mistaken for a real one.
const LAUNCHERS = [
  { dir: "npm/cli", name: "@gruntas/cli", bin: "bin/gruntas.js" },
  { dir: "npm/mcp", name: "@gruntas/mcp", bin: "bin/gruntas-mcp.js" },
];

function die(message) {
  process.stderr.write("npm/build.mjs: " + message + "\n");
  process.exit(1);
}

function flag(name, fallback) {
  const at = process.argv.indexOf("--" + name);
  return at === -1 ? fallback : process.argv[at + 1];
}

const distDir = path.resolve(repo, flag("dist", "dist"));
const outDir = path.resolve(repo, flag("out", path.join(distDir, "npm")));

function readJSON(file) {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (err) {
    die("cannot read " + path.relative(repo, file) + ": " + err.message +
      "\n  Run `make dist` first, or point --dist at a finished build.");
  }
}

// scripts/dist.sh records what it built and at which version. Taking both from there
// means the npm version is the released version by construction — there is no
// second place to get it wrong.
const metadata = readJSON(path.join(distDir, "metadata.json"));
const artifacts = readJSON(path.join(distDir, "artifacts.json"));
const version = metadata.version;

if (!version) die("no version in " + path.join(distDir, "metadata.json"));

// npm has no opinion about a leading v; a tag does. scripts/dist.sh strips it, and
// this is the assertion that it stayed stripped. No number has a leading zero
// (2026.03.0 is not semver), as scripts/release_tag.sh holds a release tag to.
if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$/.test(version)) {
  die("version " + JSON.stringify(version) + " is not a version npm will accept");
}

const prerelease = version.includes("-");

// --- Check the three copies of the platform list agree ----------------------

const built = artifacts.filter((a) => a.type === "Binary");
const wanted = new Set(PLATFORMS.map((p) => p.goos + "/" + p.goarch));
const extra = new Set(
  built.map((a) => a.goos + "/" + a.goarch).filter((k) => !wanted.has(k)),
);
if (extra.size > 0) {
  die("scripts/dist.sh built " + [...extra].sort().join(", ") +
    ", which no npm package covers.\n  Add it to PLATFORMS here and to PACKAGES in both launchers, or drop it from scripts/dist.sh.");
}

for (const launcher of LAUNCHERS) {
  const source = fs.readFileSync(path.join(repo, launcher.dir, launcher.bin), "utf8");
  const named = new Set(source.match(/@gruntas\/[a-z0-9-]+/g) || []);
  for (const p of PLATFORMS) {
    if (!named.has(p.npm)) {
      die(launcher.dir + " never mentions " + p.npm + ", so it cannot run there.");
    }
  }
  for (const name of named) {
    // A launcher naming itself is the install hint in its own error message.
    if (name === launcher.name) continue;
    if (!PLATFORMS.some((p) => p.npm === name)) {
      die(launcher.dir + " mentions " + name + ", which nothing publishes.");
    }
  }
}

// --- Write the platform packages --------------------------------------------

fs.rmSync(outDir, { recursive: true, force: true });
fs.mkdirSync(outDir, { recursive: true });

// Directory names are for us, not for npm — it reads the name out of
// package.json — so they stay flat and shell-safe.
function stage(name) {
  const dir = path.join(outDir, name.replace("@", "").replace("/", "-"));
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
  return dir;
}

const published = [];

for (const platform of PLATFORMS) {
  const dir = stage(platform.npm);

  for (const binary of BINARIES) {
    const artifact = built.find(
      (a) => a.extra?.ID === binary && a.goos === platform.goos && a.goarch === platform.goarch,
    );
    if (!artifact) {
      die("scripts/dist.sh built no " + binary + " for " + platform.goos + "/" + platform.goarch);
    }
    const ext = platform.os === "win32" ? ".exe" : "";
    const to = path.join(dir, "bin", binary + ext);
    fs.copyFileSync(path.resolve(repo, artifact.path), to);
    // npm preserves the mode it finds in the tarball, so set it here rather
    // than discover on some other machine that it did not survive.
    fs.chmodSync(to, 0o755);
  }

  // `os` and `cpu` make npm skip the four that do not apply instead of
  // downloading five binaries onto every machine. `preferUnplugged` keeps Yarn
  // PnP from zipping the binary, which would leave nothing to exec.
  fs.writeFileSync(
    path.join(dir, "package.json"),
    JSON.stringify(
      {
        name: platform.npm,
        version,
        description: "gruntas CLI + MCP binaries for " + platform.os + " " + platform.cpu + ".",
        homepage: "https://gruntas.com",
        repository: { type: "git", url: "git+https://github.com/gruntas/gruntas.git" },
        license: "MIT",
        os: [platform.os],
        cpu: [platform.cpu],
        preferUnplugged: true,
        files: ["bin"],
      },
      null,
      2,
    ) + "\n",
  );

  fs.writeFileSync(
    path.join(dir, "README.md"),
    "# " + platform.npm + "\n\n" +
      "The `gruntas` and `gruntas-mcp` binaries for " + platform.os + " " + platform.cpu +
      ", and nothing else.\n\n" +
      "Do not install this directly. Install [`@gruntas/cli`](https://www.npmjs.com/package/@gruntas/cli)\n" +
      "or [`@gruntas/mcp`](https://www.npmjs.com/package/@gruntas/mcp) and npm will pick the\n" +
      "one that matches the machine.\n\n" +
      "MIT.\n",
  );

  published.push({ name: platform.npm, dir });
}

// --- Write the launchers -----------------------------------------------------

for (const launcher of LAUNCHERS) {
  const from = path.join(repo, launcher.dir);
  const dir = stage(launcher.name);
  fs.cpSync(from, dir, { recursive: true });

  const manifest = readJSON(path.join(dir, "package.json"));
  if (manifest.name !== launcher.name) {
    die(launcher.dir + " is named " + manifest.name + ", not " + launcher.name);
  }
  if (!Object.values(manifest.bin ?? {}).includes(launcher.bin)) {
    die(launcher.dir + " has no bin entry for " + launcher.bin + ", so npx has nothing to run");
  }

  manifest.version = version;
  // Pinned exactly. A launcher and its binaries are one release cut in two, and
  // a range would let npm pair a new launcher with an old binary.
  manifest.optionalDependencies = Object.fromEntries(
    PLATFORMS.map((p) => [p.npm, version]),
  );
  fs.writeFileSync(
    path.join(dir, "package.json"),
    JSON.stringify(manifest, null, 2) + "\n",
  );

  // The launcher is the one file that runs on a user's machine before anything
  // of ours does. A syntax error in it is a broken release, so parse it here.
  const check = spawnSync(process.execPath, ["--check", path.join(dir, launcher.bin)], {
    stdio: "inherit",
  });
  if (check.status !== 0) die(launcher.dir + " does not parse");

  published.push({ name: launcher.name, dir });
}

// --- Write the pi package -----------------------------------------------------

// @gruntas/pi is a loader: one file that runs the pi extension the gruntas on
// the PATH carries (npm/pi/carried/gruntas.js, embedded in gruntas). What the
// extension is built from, and its SDKs, stay in the repository as npm/pi's
// devDependencies, so the package depends on nothing but the pi that loads it.

// Versioned with the CLI it runs. Held back from publishing (manifest.json's
// `held`, which npm/publish.mjs skips) until @gruntas/pi has a trusted
// publisher on npmjs.com naming this repository and release.yml (release.yml
// says how): then delete PI_HELD and the next release publishes it.
const PI_HELD = "no trusted publisher is set up for @gruntas/pi on npmjs.com yet";
{
  const from = path.join(repo, "npm/pi");
  const dir = stage("@gruntas/pi");
  const manifest = readJSON(path.join(from, "package.json"));
  if (manifest.name !== "@gruntas/pi") die("npm/pi is named " + manifest.name + ", not @gruntas/pi");
  if (manifest.dependencies) die("npm/pi has dependencies, which the loader must not: the extension's are in the bundle gruntas carries");
  for (const entry of manifest.pi?.extensions ?? []) {
    if (!manifest.files.includes(path.normalize(entry))) die("npm/pi's pi manifest names " + entry + ", which it does not ship");
  }
  for (const file of ["package.json", "README.md", ...manifest.files]) {
    if (!fs.existsSync(path.join(from, file))) die("npm/pi ships " + file + ", which is not there");
    fs.cpSync(path.join(from, file), path.join(dir, file), { recursive: true });
  }
  // stage() made a bin/ the extension has no use for.
  fs.rmSync(path.join(dir, "bin"), { recursive: true, force: true });
  manifest.version = version;
  delete manifest.devDependencies;
  fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(manifest, null, 2) + "\n");
  published.push({ name: manifest.name, dir, held: PI_HELD });
}

// Order is the publish order: a launcher whose optionalDependencies do not
// exist yet installs "successfully" with no binary, which is the one failure
// mode this whole arrangement is meant to avoid.
fs.writeFileSync(
  path.join(outDir, "manifest.json"),
  JSON.stringify(
    {
      version,
      prerelease,
      packages: published.map((p) => ({ name: p.name, dir: path.relative(outDir, p.dir), ...(p.held ? { held: p.held } : {}) })),
    },
    null,
    2,
  ) + "\n",
);

process.stdout.write(
  "npm/build.mjs: staged " + published.length + " packages at version " + version +
    (prerelease ? " (prerelease)" : "") + "\n",
);
for (const p of published) {
  process.stdout.write("  " + p.name + " → " + path.relative(repo, p.dir) + (p.held ? " (held: " + p.held + ")" : "") + "\n");
}

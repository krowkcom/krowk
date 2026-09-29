# Security

krowk runs commands with your own credentials, and it carries encrypted
transcripts of your sessions between your machines. Both have to hold up to
anyone reading this source. This file says how to report a problem, what
krowk sends without being asked, and how to check that a release is the one
this repository built.

## Reporting a vulnerability

Email **security@krowk.com**. Please don't open a public issue, pull request
or discussion for a vulnerability.

Include what you found, the version (`krowk --version`) and platform, and
the steps or a proof of concept that shows it. If you want credit, say how
you would like to be named.

What happens next:

- We acknowledge the report within **3 working days**, and give a first
  assessment within **10 working days**.
- We keep you told as the fix goes, and agree a disclosure date with you. The
  default is **90 days** from the report, sooner once a fixed release is out,
  and later only if you agree.
- The fix ships as a release, with a GitHub security advisory and a CVE where
  one applies, crediting you unless you ask us not to.

We won't pursue or support legal action against research done in good
faith: that stays within your own accounts and data, stops at the first
proof, doesn't degrade the service for others, and gives us the time above
before going public.

## What is in scope

This repository — the `krowk` and `krowk-mcp` binaries, both builds, the
installer (`scripts/install.sh`), the npm packages and the GitHub Action —
and the services it talks to: the registry at `api.krowk.com`, the relay,
and the permalinks it serves. The sharpest edges, and the ones we most want
reports about:

- **End-to-end encryption** (`crates/krowk-client`): anything that lets the
  registry, the relay or storage read, change, reorder or replay session
  content, or learn more than the threat model says it can.
- **The relay and the registry's sync endpoints**: joining a session, or
  holding its lease, as a device that shouldn't; reading another
  workspace's data.
- **The agent's permissions and fences**: a tool call that runs, reads or
  writes something the permission rules or the sandbox should have stopped.
- **Uploads**: `krowk push` sending a file it should have refused
  (credentials, a hard link, something outside the root).

Out of scope: problems in the model providers themselves, attacks that need
an attacker already running code as your user (the threat model says why),
and missing hardening headers on marketing pages.

The threat model, with the server side assumed hostile, and the crypto
design are `engineering/threat-model.md` and `engineering/crypto.md` in
[krowkcom/canon](https://github.com/krowkcom/canon), the repository that
holds krowk's design.

## Supported versions

The latest release. A fix is a new release; older versions are not patched.
`krowk upgrade` moves to it.

## Telemetry

krowk has **no telemetry and no crash reporting**. It records nothing about
how you use it for anyone but you, and a panic prints to your terminal and
nowhere else. There is nothing to opt out of, so there is no schema to
document; if that ever changes, it will be opt-in, off by default, and its
schema will be written here first.

The network calls krowk makes on its own, without a command that asks for
the network:

| When | Where | What it sends | How to stop it |
|---|---|---|---|
| after a command, at most once a day, on a release build | `api.github.com` (`/repos/krowkcom/krowk/releases/latest`) | a GET with `User-Agent: krowk-cli/<version>`, nothing else | `KROWK_NO_UPDATE_CHECK=1`; never under `CI` or GitHub Actions |
| `krowk sessions sync`, when prices are a day old | `models.dev/api.json` | a GET, nothing else | `krowk sessions sync --no-network` |

Everything else goes where you point it: your model providers, your krowk
registry and relay, with your keys. When krowk drives Claude Code for you, it
starts it with `DISABLE_TELEMETRY=1`, which an instance can undo in its
`env`.

## Verifying a release

Every release is built from its tag by `.github/workflows/release.yml`,
nowhere else, and carries:

- `checksums.txt`, the SHA-256 of every archive, and
  `checksums.txt.sigstore.json`, its signature. The signature is keyless
  ([Sigstore](https://www.sigstore.dev/)): it is bound to that workflow at
  that tag and logged in the public Rekor transparency log, so no signing
  key exists to steal.
- `krowk_<version>.cdx.json` and `krowk-lean_<version>.cdx.json`, CycloneDX
  SBOMs of the full and lean builds, each with its own `.sigstore.json`.

To check a download, with [cosign](https://docs.sigstore.dev/cosign/system_config/installation/)
3 or later:

```bash
tag=v0.1.0   # the release you downloaded
cosign verify-blob checksums.txt --bundle checksums.txt.sigstore.json \
  --certificate-identity "https://github.com/krowkcom/krowk/.github/workflows/release.yml@refs/tags/$tag" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
sha256sum --check --ignore-missing checksums.txt
```

The first command proves the checksums came from this repository's release
workflow at that tag; the second that your archive is one of them. An SBOM
verifies the same way, with its own bundle. The npm packages are published
with npm provenance (npm 9.5 or later), which `npm audit signatures`
checks.

`krowk upgrade` and the installer check the archive against `checksums.txt`
over HTTPS; they do not verify the signature yet. For a machine where that
matters, verify as above and install the archive yourself.

### Rebuilding a release

The binaries are reproducible: the same tag, built with the same toolchain,
gives the same bytes wherever it is checked out. The release proves it on
every tag — its `reproduce` job builds Linux x86-64 a second time, on
another runner from another path, and refuses to publish unless every
binary matches. To do it yourself on Linux, with Rust 1.97, zig 0.16.0 and
cargo-zigbuild 0.23.4 (the versions in `.github/workflows/dist.yml`):

```bash
git clone https://github.com/krowkcom/krowk && cd krowk && git checkout "$tag"
rustup target add x86_64-unknown-linux-musl
scripts/dist.sh build x86_64-unknown-linux-musl "${tag#v}"
curl -fsSLO "https://github.com/krowkcom/krowk/releases/download/$tag/krowk_${tag#v}_linux_amd64.tar.gz"
tar -xzOf "krowk_${tag#v}_linux_amd64.tar.gz" krowk | sha256sum   # the release's
sha256sum dist/x86_64-unknown-linux-musl/krowk                   # yours
```

The release's archive lands in the checkout's root, beside `dist/`, which
holds your own build and its archive under the same name.

The first release built with this in place is a release candidate
(`v0.x.y-rc1`): its `reproduce` job is the first proof that the full,
size-tuned release build is byte-identical across runners. Until one has
run, reproducibility is shown for the fast profile only.

Every dependency is checked by [`cargo deny`](deny.toml) on every pull
request and again at the tag: a crate with a RustSec advisory, a licence
outside the list, or a source other than crates.io stops the build.

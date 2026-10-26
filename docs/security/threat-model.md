# Threat model

> Published from Canon, krowk's private design repository (`engineering/threat-model.md` at 2129ba8). `crypto.md` is in this directory. The other design files it cites, such as `relay.md`, `harness.md` and `devices.md`, are not published.

What krowk protects, from whom, where each protection lives, and what it does not protect against. This is the public threat model spec R-OSS-1 asks for before sync ships; `crypto.md` is the crypto design it rests on, and `relay.md` the relay's contract. It states each defence once and links the file that holds it, so that the two cannot drift.

**The server side is assumed hostile.** The registry, the relay and R2 are krowk's own, and they are still modelled as an attacker who holds every byte they have ever been sent, can change or replay any of it, and can answer any request with anything. A defence that works only if krowk's servers behave is not counted as one. They can deny service; nothing else below depends on them.

## What is protected

In the order it would hurt to lose them:

1. **Session content.** Prompts, answers, tool calls and their output, file contents, diffs, the session's title, repository and cost. A transcript holds source code and whatever a person pasted into a prompt, credentials included.
2. **Keys.** The user key and its generations, the 12-word recovery kit (which derives a device of its own), each device's X25519 and Ed25519 private keys, session keys; and, outside sync, the provider keys and logins in `credentials.json` and the registry API keys.
3. **The person's machine.** The agent runs commands with the person's own credentials. What it runs, reads, writes and publishes is governed by the permission rules and fences in `harness.md` → Permissions and Trust and `architecture.md` → The security posture.
4. **Who may drive a session.** Holding a session's lease, joining it on the relay as host or viewer, approving a tool call in it.
5. **The release itself.** That the binary a person runs is the one this source builds.

## Who the attackers are

### A hostile registry, relay or storage

Holds ciphertext and metadata; can substitute, replay, reorder, drop and delay anything; can hand a client any public key, ticket or record.

- **Cannot read content.** Everything a session writes is sealed under its session key with XChaCha20-Poly1305 before it leaves the device; session keys are wrapped under the person's user key, the user key only ever to a device on the verified device list, with HPKE. No server holds a key that opens any of it (`crypto.md` → The key hierarchy).
- **Cannot change, move or replay content unnoticed.** Each sealed message is bound to its session, direction, the receiver's epoch and a strictly increasing counter; chunks at rest form a chain that binds each one's predecessor and its lease fence; a truncated stream is caught by the missing final sealed message and a truncated log by the head in the sealed index (`crypto.md` → Blob formats, Nonces).
- **Cannot add its own device.** The device list is signed and chained by the person's devices, and a device adopts a user key only when its id is the one the verified list names; pairing is by a short SPAKE2 code, so the registry gets one online guess per side (`crypto.md` → The device list, Adding a device, Threats to the device list).
- **Cannot act as a device.** Every registry call that acts as a device is signed by that device's Ed25519 key, which never leaves it, with a five-minute window and a replay memory; a relay join is a signature over the dialled origin and a fresh challenge (`crypto.md` → Signed registry requests, `relay.md` → Joining).
- **Can drop, delay or fake a generic alert.** It sends the push notifications, so it can make a phone buzz for nothing, late, or not at all. It cannot make the alert show content, since it has none to put there, nor answer for it: an approval is still a signed command over the relay.
- **Can** see the metadata `crypto.md` → What the server side can and cannot see lists: how many devices and sessions an account has, when each is active, every message's size and timing, device host names, the addresses devices connect from. And it can deny service.

### Someone on the network

Between any two of: a device, the registry, the relay, R2, a direct Tailscale or LAN path.

Sees at most what the servers see, because every leg carries the same sealed envelopes; TLS to the registry and relay adds to that rather than being relied on. A direct path admits a viewer only on the same ticket, challenge and handshake as the relay (`relay.md`), checked with the registry's ticket public keys, which the host fetches from the registry over the same TLS (`harness.md` → Direct paths); a registry named over plain `http://` lets someone between swap those keys and reach the direct listener's join, which still opens nothing sealed. Any peer on the tailnet — or on the LAN, when the person offers it — can reach that listener's pre-join surface, held to the reference relay's limits, as anyone on the internet can reach the relay's. On the LAN the host also advertises the listener over mDNS, so the network learns that a krowk session is hosted at that address, never which (its name is a hash of the session id); anyone there may answer a viewer's query for it, and an answer is dialed only at a LAN address and joined only by the ticket and challenge, which name the address dialed and open on no other. The reference relay serves plain `ws://` and is meant to sit behind TLS off a machine; nothing it carries is readable without it either.

### Another workspace

A tenant of the same registry or relay. Session ids are unique across workspaces, a relay channel holds to the workspace of its first admitted join, a ticket names its workspace and is signed by the registry, and another workspace's session answers as unknown rather than forbidden (`relay.md` → Tickets). Per-workspace fairness in the relay's pools keeps one tenant from locking another's devices out.

### A lost, stolen or removed device

It holds the user key, so until it is removed it can read every session sealed under that key's current and older generations. Removing it is an entry on the signed device list and rotates the user key, so nothing sealed afterwards opens on it; it also refuses its signed requests and so its relay tickets. A ticket already issued lasts up to five minutes, and a device already joined stays joined until it reconnects (`relay.md` → Not yet). **It keeps what it could already read**: session keys are not rewrapped on rotation (`crypto.md` → Out of scope, `devices.md` → Removing a device).

**An iPhone in someone else's hand.** Approvals are the phone's most valuable action: an allow runs code on the person's computer. Before it allows any tool not known to only read, the phone asks for its owner's Face ID or passcode (R-IOS-11, `harness.md` → The iPhone app), so someone holding an unlocked phone — picked up from a desk, or handed back after a call — cannot approve a command, and a tap that lands as a request rises cannot either. It does not stop the owner being talked into allowing something (the sheet shows the command, and the request's words are drawn as hostile text, but the choice is theirs), someone who knows the passcode, code running inside the unlocked app, or reading and denying: deny and the read-only tools ask nothing, and the transcript is readable to whoever holds the unlocked phone. Removing the device is still what ends its access. The phone's cache of session lists and recent transcripts (`harness.md` → The iPhone app → Sessions) is the same content as the transcript on screen, and as guarded: under `FileProtectionType.complete` it is readable only while the phone is unlocked, never in a backup, and it holds no key; a locked phone gives none of it up.

### Content the agent reads

A web page, an issue body, a file in a repository: text written by someone else that a model reads and may follow. This is prompt injection, and krowk does not claim to stop a model from being persuaded. It limits what a persuaded model can do: the permission modes and rules, the fence around krowk's home, the refusal of credential files and hard links on publish, and git judged fail-closed (`harness.md` → Permissions and Trust, `architecture.md` → The security posture). The boundary is the tool, not the model. What the model writes back is hostile too, to whoever reads it: on the iPhone (R-IOS-10, `harness.md` → The iPhone app) agent output is drawn with control and bidi-override characters stripped, no remote image loads and there is no web view, and a link shows its full host and opens only after a tap on a sheet naming the whole URL — so injected text cannot fetch on its own, pose a lookalike link as another site, or make a path read backwards.

### A compromised build or dependency

A binary that is not the one this source builds, or a dependency that turned hostile.

- Releases are built only by `release.yml` from the tag, and `checksums.txt` is signed keylessly with Sigstore, bound to that workflow at that tag and logged in Rekor; each release's CycloneDX SBOMs are signed the same way. `SECURITY.md` in `krowkcom/krowk` gives the verification commands.
- The build is reproducible: the release rebuilds Linux x86-64 on a second runner from another checkout path and refuses to publish unless every binary is byte-identical. Anyone can repeat it with the pinned toolchain.
- `cargo deny` runs on every pull request and at every tag: a RustSec advisory, a licence outside the list, or a source other than crates.io fails it. Crypto crates may be named only by `krowk-client`, which a test enforces (R-E2E-2), and the lean build's dependency list is pinned (`principles.md`).

### Local malware

A process running as the person. **Out of scope**: it can read `device.json`, `credentials.json` and the terminal, and anything the client can. Every defence above assumes the device itself is the person's. Platform key stores narrow this; they do not close it. On the iPhone they are built (`crypto.md` → R-IOS-8): every key slot and the krowk API key are Keychain items readable only while the phone is unlocked, never synced to iCloud, restorable from a backup only to the same phone, and sealed under a Secure Enclave key that cannot leave it; no key is a file in the app. A backup, a copied container or a forensic image of a locked phone holds no key that opens anywhere but in that phone's enclave; code running inside the unlocked app — an exploited app, a debugger, a jailbreak's injection — can still have the enclave open every slot, as the app does. The simulator's software wrapping key is for tests only. The macOS keychain and Android's keystore come with those clients.

## Accepted risks

Each is known, named here rather than left for a reader to find, and either accepted or owned by a later ticket.

- **Lengths leak, and compression makes them leak more.** Nothing is padded, and content is compressed before it is sealed. An attacker who can put chosen text into a session (a page a tool fetched) and watch the sizes of the batches it lands in could in principle learn something about other text compressed with it, as in CRIME. Accepted for now: exploiting it needs both the injection and the server-side view, and padding costs bandwidth on every turn. Revisit before sessions are shared between people.
- **Timing and shape are visible.** When a session is active, how fast tokens arrive, which devices attach and from where. Inherent to a relay that forwards in real time.
- **A device's name is its host name,** sent in the clear so a person can tell devices apart. `--name` or `KROWK_DEVICE_NAME` sends another.
- **No forward secrecy.** A session is sealed under one long-lived session key, not a ratchet; a leaked session key opens that session, past and future.
- **Removal doesn't reach the past.** Removal rotates the user key, but a removed device keeps what it could read before (above), and what a session sealed before a rotation held stays under its old key. Resumed after the rotation, it moves to a new key epoch before anything more is written (`crypto.md` → Key epochs); a host already running across the removal keeps its key until it next takes the session up.
- **Revocation lags** by a relay ticket's lifetime (five minutes), and a joined device stays joined until it reconnects.
- **Keys are `0600` files,** readable by anything running as the person, until the platform stores.
- **Upgrades verify the release signature without SCT or RFC 3161 checks.** `krowk upgrade` verifies `checksums.txt`'s Sigstore bundle against an embedded Sigstore trusted root (Fulcio chain at Rekor's logged time, workflow identity: the release workflow at the installed tag, in krowkcom/krowk, triggered by push; Rekor SET and inclusion proof). It does not check the certificate's SCT or the RFC 3161 timestamp, and a krowk built before a Sigstore key rotation refuses newer releases until reinstalled. The installer verifies with cosign only when cosign is installed; otherwise it checks the checksum over HTTPS and prints the cosign command, unless `KROWK_REQUIRE_SIGNATURE=1`, which refuses without cosign; releases before 0.12.0 are unsigned. `releases/latest` is unsigned (no TUF metadata of krowk's own): an attacker who controls it can hold a user on an intermediate signed version, not downgrade or substitute one. Releases are signed with a repository-pinned Sigstore signing config (Rekor v1), so Sigstore moving its default log cannot make a release unverifiable by the krowks already installed, and the release workflow checks the new bundle with the upgrader's own code before publishing. Signing runs in a job that runs only cosign; no Rust build runs where the release's OIDC token or write access is available.
- **Published artifacts carry plaintext metadata** — the session id, engine, model and provider — by design, so a card can link back to its session (`metadata.md`). Publishing is a per-file choice.
- **The OS sandbox's `workspace` profile shares the host's network namespace**, so a command can connect to an abstract Unix socket a host program listens on (X11's `@/tmp/.X11-unix/X0`, others). `strict` and `read-only` have their own namespace and reach none (`harness.md` → The OS sandbox).
- **Workspace files a host tool runs later** — `.cargo/config.toml`, `.envrc`, `.vscode/tasks.json`, a `Makefile`, `package.json` scripts, husky's `.husky/<hook>` scripts — are written freely under `workspace`: writing the workspace is that profile's point, and what runs them is the person, later, outside the sandbox. What git itself runs (`.git`, every nested repository's, the directory `core.hooksPath` names, husky's `.husky/_` included) stays read-only.
- **A background job is swept when it exits, not after each call.** A foreground call's sweep (`harness.md` → The OS sandbox) runs once the call's pid namespace is empty, so nothing the call started can plant a link after it. A background job (`harness.md` → Tools, R-STEER-2) runs beside later calls, so until it exits, a `.git` or a link it plants can outlive the sweep of every call that ran meanwhile, and a git that the person or a later call runs in that window can follow it. The sweep at the job's exit, once its namespace is empty, removes what it left as a call's sweep does: the `.git` and fenced directories it created, anything but a regular file or directory in a krowk worktree's writable git directories, and access it locked away. It does not undo what ran in the window, such as a hook git ran or a commit that followed a link. And it cannot tell who made what appeared: a repository the person cloned into the workspace while the job ran is removed with the rest, as one made during a long foreground call would be, so the job's note names what the sweep removed. The window is the job's lifetime, which has no timeout; `kill_bash` closes it early, and the job ends with the session's host.
- **A trusted repository's MCP config reads the person's environment.** `${VAR}` and `${VAR:-default}` in a `.mcp.json` server's `url`, `headers`, `env`, `args` and `command` are expanded from krowk's environment, as Claude Code expands them, so a repository the person trusted can send an environment secret — a token in their shell — to a host it chose. Untrusted, a repository's servers are not started at all (`harness.md` → MCP servers). Accepted because trust is the line: a trusted repository may already run commands with the person's whole environment, through its hooks and its servers.
- **The audit has not happened** (below).

## The independent audit

Spec R-OSS-2: an independent audit of the crypto and the relay before Pro sync reaches general availability. A beta may ship before it. Booking it is a person's job, recorded on the spec message; performing it is Harness P2 ticket 37. This is what it covers.

**In scope**

1. **`krowk-client`** in `krowkcom/krowk`, the one crate that implements crypto: the key hierarchy and its files, HPKE wrapping and the pinned suite, the blob formats and their version and suite bytes, sealing and opening (`Sealer`, `Opener`) and the counters and epochs they bind, chunks at rest and their chain, the sealed session index, the user key and its generations, the recovery kit, the signed device list and its verifier, pairing by a short SPAKE2 code, Ed25519 request and relay-join signing, relay tickets, zeroization. Against `crypto.md`, with the server assumed hostile.
2. **The relay**: the contract in `relay.md`, the reference relay (`krowk relay serve`, in `krowk-harness`), and the hosted Durable Object in `krowk-registry/cloudflare/` — joining, tickets and their checks, the lease fence, routing between host and viewers, the ring buffer and resume, limits and the refusals, and cross-tenant isolation.
3. **The registry's sync endpoints**, in `krowk-registry`: the device list and pairing (the legacy `POST /v1/devices` and `/v1/device_approvals` now answer 410), sessions and the sealed index (`PUT /v1/sessions/:id`), chunks (`POST /v1/sessions/:id/chunks`, finalization), the lease (`POST`, `PUT`, `DELETE /v1/sessions/:id/lease`), relay tickets (`GET /v1/sessions/:id/relay_ticket`), and the request-signature verification and replay memory in front of all of them.
4. **The direct path** (Tailscale and LAN) as far as it admits a viewer: that it takes exactly the relay's ticket, challenge and handshake.
5. **The release pipeline**, lightly: that signing, the SBOM and the reproducible rebuild do what `SECURITY.md` says.

**Out of scope**: the agent engine and its providers, the permission model (reviewed separately), the TUI, publishing and the artifact pages, the website, and the underlying primitives' own implementations (already audited or reviewed as `crypto.md` → Audit status says).

**What the auditor gets**: this file, `crypto.md`, `relay.md`, both repositories at a pinned commit, the conformance suite and its fixtures, and a staging registry and relay.

## Change log

- **2026-10-25** — Harness P4 ticket 129: An iPhone in someone else's hand — the on-device cache of session lists and recent transcripts is readable only while the phone is unlocked (`.complete`), like the transcript on screen, and kept out of backups.
- **2026-10-23** — Harness P4 ticket 91: A lost, stolen or removed device — what Face ID before an allow on the iPhone stops (an unlocked phone in someone else's hand approving a command) and what it doesn't (a persuaded owner, a known passcode, code inside the unlocked app, reading and denying) (R-IOS-11).
- **2026-10-22** — Harness P4 ticket 88: Content the agent reads — what it writes back is hostile to its reader: the iPhone's rules for drawing agent output (R-IOS-10).
- **2026-10-20** — Harness P4 ticket 82: Local malware — on the iPhone the platform key store is built (R-IOS-8): what the Keychain and the Secure Enclave keep out, and what they don't (code running inside the unlocked app).
- **2026-10-12** — P2 follow-ups: `krowk upgrade` verifies the release's Sigstore signature; the accepted risk narrows to the checks it leaves out.
- **2026-10-11** — P2 follow-ups: a trusted repository's `${VAR}` expansion in its MCP config named as an accepted risk.
- **2026-10-10** — Steering ST2: a background job's sweep comes at its exit, so a job leaves a wider window than a call; what the sweep at exit covers, and what it doesn't.

- **2026-10-09** — Harness P2 ticket 40: the OS sandbox's two accepted risks recorded here, not only in `harness.md` — abstract Unix sockets and X11 under `workspace`, and workspace files a host tool runs later.
- **2026-10-04** — Brought in line with devices you own (merged to main while this was on the P2 branch): the user key and its generations and the recovery kit for the account key and phrase, the signed device list and SPAKE2 pairing for the typed device id, removal with rotation, and the audit's scope to match.
- **2026-09-29** — Written with Harness P2 ticket 28: what is protected, the attackers and where each defence lives, the accepted risks (the length leak `crypto.md` handed here, timing, host names, no forward secrecy or rotation, revocation lag, key files, upgrades that checked checksums but not signatures (since verified, below), published metadata), the supply chain (signed, reproducible releases with SBOMs and `cargo deny`), and the audit's scope.

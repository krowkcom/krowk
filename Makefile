# The leading v comes off, because the release drops it and npm will not take it.
# A checkout and a release should not disagree about what version this is.
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null | sed 's/^v//' || echo dev)

.PHONY: build test test-build test-run test-doc lint lint-lean lint-sessions lint-harness check check-build check-run quick-check signoff install dev mock clean dist release-check golden golden-build golden-device golden-full golden-run golden-update bin/devregistry schema ffi-api codex-schema codex-schema-update lean-deps bench

build: ## Build target/release/krowk (the full build) and krowk-mcp
	KROWK_VERSION=$(VERSION) cargo build --release -p krowk --features harness

# With `harness`, which implies `sessions`: every test either build runs.
# nextest runs no doctests, so cargo test runs those. CI sets
# NEXTEST_PROFILE=ci, which retries a flaky test instead of failing the job.
# nextest gives each test a scratch KROWK_HOME (.config/nextest.toml); the
# doctests get the same one here. `check` builds the tests with everything
# else and runs them after (`check-run`).
NEXTEST := cargo nextest run --workspace --exclude krowk-golden --features krowk/harness

test: test-build test-doc test-run ## The unit and integration tests

test-build:
	$(NEXTEST) --no-run

test-run:
	$(NEXTEST)

test-doc:
	KROWK_HOME=$(CURDIR)/target/krowk-test-home cargo test --doc --workspace --exclude krowk-golden --features krowk/harness

# Both builds: the agent build (no sessions) is the one a container compiles
# from source, and a cfg that only one of them sees is a lint only one catches.
# cognitive_complexity sits in clippy's nursery group, which is off by default,
# so it is named here; clippy.toml sets the ceiling it holds a function to.
# Each feature set has a target directory of its own, so `make -j` runs the
# three at once (one directory is one cargo at a time) and none undoes
# another's build.
CLIPPY := cargo clippy --workspace --all-targets
CLIPPY_DENY := -- -D warnings -D clippy::cognitive_complexity

lint: lint-lean lint-sessions lint-harness

lint-lean:
	CARGO_TARGET_DIR=$(CURDIR)/target/lint-lean $(CLIPPY) $(CLIPPY_DENY)

lint-sessions:
	CARGO_TARGET_DIR=$(CURDIR)/target/lint-sessions $(CLIPPY) --features krowk/sessions $(CLIPPY_DENY)

lint-harness:
	CARGO_TARGET_DIR=$(CURDIR)/target/lint-harness $(CLIPPY) --features krowk/harness $(CLIPPY_DENY)

# Two phases. Everything that compiles runs at once: the lints, the test
# builds and the doctests, the golden builds. The release builds end in one
# single-threaded LTO link each, which the rest fills the cores around. The
# tests then run on a machine doing nothing else, as they always have: the
# no-lag tests measure time (.config/nextest.toml). One -j for all of it:
# make hands its job slots to every cargo below (the jobserver), so the
# builds share the cores instead of each taking them all.
CHECK_JOBS ?= $(shell nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)
# Each build's output kept together, where make can: --output-sync is GNU
# make 4's, and macOS ships 3.81, which refuses it and runs nothing.
OUTPUT_SYNC := $(if $(filter 3.%,$(MAKE_VERSION)),,--output-sync=target)

check: ## Everything CI runs
	$(MAKE) -j$(CHECK_JOBS) $(OUTPUT_SYNC) check-build
	$(MAKE) check-run

check-build: lint lean-deps test-build test-doc golden-build

check-run: golden-run test-run

# The edit loop, not a gate: the harness build's lint, then the tests of the
# crates this branch changed and of the crates that depend on them
# (scripts/affected.sh), leaving out the five that take a minute or more
# (.config/nextest.toml). The lint and the build are `check`'s, so each
# reuses the other's. No golden cases: `make golden` runs them in about half
# a minute. `make signoff` is still what a pull request needs.
quick-check: lint-harness ## Lint and the affected tests, in a minute or two
	$(NEXTEST) --profile quick --no-tests=pass -E "$$(scripts/affected.sh)"

# gh-signoff (github.com/basecamp/gh-signoff): `make check` on this machine,
# then a green `signoff` status on the commit, which is what a pull request
# needs to merge instead of a queue for CI runners. The status names a commit,
# so the tree has to be clean, and the commit pushed by the time it is posted.
# The ci profile retries a flaky test the way CI would.
signoff: ## Run `make check` here and sign off on the commit
	@gh signoff --help >/dev/null 2>&1 || { echo "gh extension install basecamp/gh-signoff" >&2; exit 1; }
	@test -z "$$(git status --porcelain)" || { echo "signoff names a commit: commit or stash first" >&2; exit 1; }
	sha=$$(git rev-parse HEAD) && NEXTEST_PROFILE=ci $(MAKE) check && \
		{ gh signoff --commit "$$sha" || { echo "push, then: gh signoff --commit $$sha" >&2; exit 1; }; }

# R-PKG-2: the agent build links exactly the crates crates/krowk/lean-deps.txt lists, on every target.
lean-deps: ## Hold the agent build to its dependency list
	scripts/lean_deps_check.sh

# R-PERF-7: every number in crates/krowk-bench/budgets.toml, measured on the
# release profile and failed on the first one broken. The same table CI prints
# in the job summary. Both builds are stamped with one version so a size is
# the code's, not the length of `git describe`; each is copied out before the
# next overwrites target/release/krowk (rm first, for the macOS inode reason
# at bin/devregistry below).
BENCH_DIR := target/bench
BENCH_VERSION := 0.0.0-bench

bench: ## Hold the release builds to the performance and size budgets
	KROWK_VERSION=$(BENCH_VERSION) cargo build --release --locked -p krowk --bin krowk
	mkdir -p $(BENCH_DIR) && rm -f $(BENCH_DIR)/krowk-lean && cp target/release/krowk $(BENCH_DIR)/krowk-lean
	KROWK_VERSION=$(BENCH_VERSION) cargo build --release --locked -p krowk --bin krowk --features harness
	rm -f $(BENCH_DIR)/krowk-full && cp target/release/krowk $(BENCH_DIR)/krowk-full
	cargo run --profile bench-tool --locked -p krowk-bench -- --budgets crates/krowk-bench/budgets.toml \
		--lean $(BENCH_DIR)/krowk-lean --full $(BENCH_DIR)/krowk-full --work $(BENCH_DIR)/work $(BENCH_FLAGS)
	# R-LAG-2/3/9's load with the flood unpaced, which only an optimized
	# daemon is held to (tests/daemon_ws.rs says why).
	cargo test --release --locked -p krowk-harness --test daemon_ws r_lag_2_r_lag_3

schema: ## Regenerate the harness protocol's JSON Schema after a type change
	KROWK_SCHEMA_UPDATE=1 cargo test -p krowk-harness --test schema

# R-FFI-2: refused when an existing item changed and the file's first line,
# FFI_API_VERSION, is not above the committed one (the constant in
# src/lib.rs is held equal to it by another test).
ffi-api: ## Rewrite crates/krowk-ffi/public-api.txt after an intended change
	UPDATE_PUBLIC_API=1 cargo test -p krowk-ffi --test public_api

# R-BACK-3: the schema of the `codex app-server` protocol krowk drives, pinned
# for the Codex version in crates/krowk-harness/schema/codex/VERSION. Needs
# that Codex installed (npm install -g @openai/codex@<version>), so it is CI's
# own job rather than part of `check`; `codex-schema-update` moves the pin to
# the Codex installed.
codex-schema: ## Fail when the pinned codex app-server schema is stale against the pinned Codex
	scripts/codex_schema.sh --check

codex-schema-update: ## Re-pin the codex app-server schema from the Codex installed
	scripts/codex_schema.sh --update

install: ## Install krowk and krowk-mcp into ~/.cargo/bin
	KROWK_VERSION=$(VERSION) cargo install --locked --path crates/krowk --features harness

# `krowk` on the PATH as a link into this checkout's target/quick, so every
# later `make dev` (or `cargo build --profile quick -p krowk --features
# harness`) is live at once, with no copy. The links follow the checkout
# `make dev` last ran in; `make install` puts real copies back.
DEV_BIN_DIR ?= $(HOME)/.cargo/bin

dev: ## Fast build, linked into ~/.cargo/bin: the latest krowk from this checkout
	KROWK_VERSION=$(VERSION) cargo build --profile quick -p krowk --features harness
	mkdir -p "$(DEV_BIN_DIR)"
	for b in krowk krowk-mcp; do ln -sfn "$(CURDIR)/target/quick/$$b" "$(DEV_BIN_DIR)/$$b"; done
	@echo "$(DEV_BIN_DIR)/krowk -> $(CURDIR)/target/quick/krowk"

mock: ## Local stand-in for api.krowk.com on :8787
	cargo run --release -p krowk-devregistry --bin devregistry

release-check: ## Validate the release layout, the npm launchers and the installer, offline
	bash -n scripts/dist.sh
	node --check npm/krowk/bin/krowk.js
	node --check npm/mcp/bin/krowk-mcp.js
	node scripts/descriptions_check.mjs
	# The installer downloads what this file produces, so it belongs to the
	# release pipeline rather than to `check`: it needs python3,
	# which a plain test run has no business requiring.
	scripts/install_test.sh

dist: ## The whole release, locally: every binary, the archives, the npm packages (needs zig + cargo-zigbuild, on macOS)
	scripts/dist.sh all $(VERSION)
	node npm/build.mjs

clean:
	rm -rf bin dist
	cargo clean

# The profile the golden cases and their registry are built with. The golden
# profile (Cargo.toml) is the quick build, a link of seconds where a release
# one takes minutes; full.yml sets GOLDEN_PROFILE=release, so the build that
# ships is held to the cases on every platform after the merge.
GOLDEN_PROFILE ?= golden
GOLDEN_DIR := target/$(GOLDEN_PROFILE)

# The stand-in registry the golden cases run against, at the path they look
# for it. Built by no release.
bin/devregistry:
	cargo build --profile $(GOLDEN_PROFILE) -p krowk-devregistry --bin devregistry
	# rm first: on macOS a rebuilt binary copied over the old one keeps the old
	# inode's code signature, and the kernel kills it on launch.
	mkdir -p bin && rm -f bin/devregistry && cp $(GOLDEN_DIR)/devregistry bin/devregistry

# The cases compare the version like any other output, so the build they run is
# stamped with one no release will ever carry.
GOLDEN_VERSION := 0.0.0-golden

# Two builds. The device build runs every case. The full build, which opens
# the TUI when a person is at the terminal, runs tests/golden/cases-full:
# bare `krowk` with no terminal to open it on, recorded from the full build
# before the TUI existed, so any difference is a behaviour change. The full
# build has a target directory of its own, so the two build at once and
# $(GOLDEN_DIR) holds the device build `golden-update` expects. The cases'
# own test binary builds apart from `test-build`'s too, so neither waits on
# the other's lock. Both runs take krowk-mcp from the device build.
GOLDEN_TEST := CARGO_TARGET_DIR=$(CURDIR)/target/golden-tests cargo test -p krowk-golden
GOLDEN_FULL := target/golden-full/$(GOLDEN_PROFILE)/krowk

golden: golden-build golden-run ## Hold the build to tests/golden/cases

golden-build: golden-device golden-full
	$(GOLDEN_TEST) --no-run

golden-device: bin/devregistry
	KROWK_VERSION=$(GOLDEN_VERSION) cargo build --profile $(GOLDEN_PROFILE) -p krowk --features sessions

golden-full:
	KROWK_VERSION=$(GOLDEN_VERSION) CARGO_TARGET_DIR=$(CURDIR)/target/golden-full cargo build --profile $(GOLDEN_PROFILE) -p krowk --features harness

golden-run:
	KROWK_BIN=$(GOLDEN_DIR)/krowk KROWK_MCP_BIN=$(GOLDEN_DIR)/krowk-mcp $(GOLDEN_TEST)
	GOLDEN_CASES=cases-full KROWK_BIN=$(GOLDEN_FULL) KROWK_MCP_BIN=$(GOLDEN_DIR)/krowk-mcp $(GOLDEN_TEST)

golden-update: bin/devregistry ## Re-record tests/golden/cases after an intended output change
	KROWK_VERSION=$(GOLDEN_VERSION) cargo build --profile $(GOLDEN_PROFILE) -p krowk --features sessions
	GOLDEN_UPDATE=1 KROWK_BIN=$(GOLDEN_DIR)/krowk KROWK_MCP_BIN=$(GOLDEN_DIR)/krowk-mcp cargo test -p krowk-golden

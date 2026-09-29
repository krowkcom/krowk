# The leading v comes off, because the release drops it and npm will not take it.
# A checkout and a release should not disagree about what version this is.
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null | sed 's/^v//' || echo dev)

.PHONY: build test lint check install dev mock clean dist release-check golden golden-update bin/devregistry schema codex-schema codex-schema-update lean-deps bench

build: ## Build target/release/krowk (the full build) and krowk-mcp
	KROWK_VERSION=$(VERSION) cargo build --release -p krowk --features harness

# With `harness`, which implies `sessions`: every test either build runs.
# nextest runs no doctests, so cargo test runs those. CI sets
# NEXTEST_PROFILE=ci, which retries a flaky test instead of failing the job.
test: ## The unit and integration tests
	cargo nextest run --workspace --exclude krowk-golden --features krowk/harness
	cargo test --doc --workspace --exclude krowk-golden --features krowk/harness

# Both builds: the agent build (no sessions) is the one a container compiles
# from source, and a cfg that only one of them sees is a lint only one catches.
lint:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy --workspace --all-targets --features krowk/sessions -- -D warnings
	cargo clippy --workspace --all-targets --features krowk/harness -- -D warnings

check: lint lean-deps test golden ## Everything CI runs

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

# The stand-in registry the golden cases and the integration tests run
# against, at the path they look for it. Built by no release.
bin/devregistry:
	cargo build --release -p krowk-devregistry --bin devregistry
	# rm first: on macOS a rebuilt binary copied over the old one keeps the old
	# inode's code signature, and the kernel kills it on launch.
	mkdir -p bin && rm -f bin/devregistry && cp target/release/devregistry bin/devregistry

# The cases compare the version like any other output, so the build they run is
# stamped with one no release will ever carry.
GOLDEN_VERSION := 0.0.0-golden

# Two builds. The device build runs every case. The full build, which opens
# the TUI when a person is at the terminal, runs tests/golden/cases-full:
# bare `krowk` with no terminal to open it on, recorded from the full build
# before the TUI existed, so any difference is a behaviour change. The full
# build goes first and is copied out, so target/release ends as the device
# build `golden-update` and the default KROWK_BIN expect.
golden: bin/devregistry ## Hold the build to tests/golden/cases
	KROWK_VERSION=$(GOLDEN_VERSION) cargo build --release -p krowk --features harness
	mkdir -p target/golden && rm -f target/golden/krowk-full && cp target/release/krowk target/golden/krowk-full
	KROWK_VERSION=$(GOLDEN_VERSION) cargo build --release -p krowk --features sessions
	cargo test -p krowk-golden
	GOLDEN_CASES=cases-full KROWK_BIN=target/golden/krowk-full cargo test -p krowk-golden

golden-update: bin/devregistry ## Re-record tests/golden/cases after an intended output change
	KROWK_VERSION=$(GOLDEN_VERSION) cargo build --release -p krowk --features sessions
	GOLDEN_UPDATE=1 cargo test -p krowk-golden

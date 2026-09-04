# Torque — one entry point per task.
#
# Operational rules live here as targets, not as prose someone has to remember:
# TORQUE_BUILD is exported once below, `check` is exactly what CI runs, and the
# commands that are easy to get subtly wrong (restoring a plain build after PGO,
# reaching the vendored crate's own workspace, including the :perf tag) each
# have a name. CI calls these targets, so they cannot drift from what it runs.

# Forces the NIF to compile from Rust source instead of downloading a
# precompiled binary. Without it a `_build` tree keeps loading a *released*
# binary against local Rust changes and reports a green suite.
export TORQUE_BUILD := true

VENDORED := --manifest-path native/sonic-rs/Cargo.toml

.PHONY: help deps build strict test test-perf test-all lint fmt check bench pgo plain ab clean

help: ## Show this help
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[1m%-12s\033[0m %s\n", $$1, $$2}'

deps: ## Fetch dependencies
	mix deps.get

build: ## Compile Elixir and the Rust NIF
	mix compile

strict: ## Compile with warnings as errors, as the CI matrix does
	mix compile --warnings-as-errors

test: ## Functional suite (test_helper.exs excludes :perf)
	mix test

test-perf: ## Only the timing and scheduler regressions
	mix test --only perf

test-all: ## Everything, functional and :perf together
	mix test --include perf

fmt: ## Format Elixir and Rust, including the vendored crate
	mix format
	cargo fmt
	cargo fmt $(VENDORED)

# The vendored crate carries its own `[workspace]`, so the workspace commands
# above do not reach it. Its root disables every lint for upstream code, which
# is why its clippy run needs no `-D warnings`: `extract.rs` denies them itself
# and nothing else there would report anything.
lint: ## Every format and lint check CI runs
	mix format --check-formatted
	cargo fmt --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace
	cargo fmt $(VENDORED) --check
	cargo clippy $(VENDORED) --lib

check: lint strict test test-perf ## Everything CI runs, in CI's order

bench: ## Run the benchmark suite
	MIX_ENV=bench mix run bench/torque_bench.exs

pgo: ## Instrument, profile, and rebuild the NIF optimised
	./scripts/pgo-build.sh

# `--force` is the half that is easy to miss: nothing is stale after a PGO
# build, so a plain `mix compile` is a no-op that leaves the profiled artifact
# in place.
plain: ## Restore a plain -O3 build after `make pgo`
	mix compile --force

# Comparing plain -O3 wall clock across a refactor measures placement, not the
# change: fat LTO in one codegen unit reshuffles everything when any source
# moves. This builds both revisions with PGO and reports instructions retired
# alongside cycles, so a layout swing is distinguishable from real work.
ab: ## A/B a revision against HEAD under PGO (make ab REF=<rev>)
	@test -n "$(REF)" || { echo "usage: make ab REF=<git-rev> [WORKLOAD=path.exs]"; exit 2; }
	./scripts/ab.sh $(REF) $(WORKLOAD)

clean: ## Remove build artifacts
	mix clean
	cargo clean

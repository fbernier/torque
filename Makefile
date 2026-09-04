# Torque task entry points; CI calls the same targets.
# Build the NIF from local Rust source rather than a released binary.
export TORQUE_BUILD := true

VENDORED := --manifest-path native/sonic-rs/Cargo.toml

.PHONY: help deps build strict test test-perf test-miri test-all lint fmt check bench sweeps ops fixtures pgo plain ab clean

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

test-miri: ## Check extraction arena lifetimes with nightly Miri and locked dependencies
	cargo +nightly miri test --locked --workspace decoder::extract_regressions

test-all: ## Everything, functional and :perf together
	mix test --include perf

fmt: ## Format Elixir and Rust, including the vendored crate
	mix format
	cargo fmt
	cargo fmt $(VENDORED)

# The vendored crate is a separate workspace; extract.rs enables its own lints.
lint: ## Every format and lint check CI runs
	mix format --check-formatted
	cargo fmt --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace
	cargo fmt $(VENDORED) --check
	cargo clippy $(VENDORED) --lib

check: lint strict test test-perf ## Everything CI runs, in CI's order

# Use `bench` for library comparisons and `ab` for revision comparisons.
bench: ## Compare against other JSON libraries (wall clock, CI trend data)
	MIX_ENV=bench mix run bench/torque_bench.exs

sweeps: ## Threshold sweeps: where a cutoff belongs (make sweeps NAME=members)
	MIX_ENV=bench mix run bench/sweeps.exs $(NAME)

ops: ## List the operations `make ab` measures, and the path each targets
	MIX_ENV=bench mix run bench/ops.exs describe

fixtures: ## List the benchmark payloads, their size, and the regime each pins
	MIX_ENV=bench mix run bench/ops.exs fixtures

pgo: ## Instrument, profile, and rebuild the NIF optimised
	./scripts/pgo-build.sh

# Force compilation so a profiled artifact is actually replaced.
plain: ## Restore a plain -O3 build after `make pgo`
	mix compile --force

# A/B uses PGO and per-operation instruction counts to separate work from code
# layout. Narrow with OPS or increase AB_TARGET_MS for a close result.

ab: ## A/B a revision against HEAD under PGO (make ab REF=<rev> [OPS="..."])
	@test -n "$(REF)" || { echo 'usage: make ab REF=<git-rev> [OPS="op op"]'; exit 2; }
	./scripts/ab.sh $(REF) $(OPS)

clean: ## Remove build artifacts
	mix clean
	cargo clean

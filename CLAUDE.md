# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
TORQUE_BUILD=true mix deps.get     # fetch deps + force local Rust build
TORQUE_BUILD=true mix compile      # build (includes Rust NIF compilation)
TORQUE_BUILD=true mix test         # run all tests
mix test test/pointer_test.exs:42  # run single test by line number
mix compile --warnings-as-errors   # build with strict warnings
mix format                         # format Elixir code
mix format --check-formatted       # check Elixir formatting
mix dialyzer                       # static type analysis
cargo fmt                          # format Rust code (run from repo root)
cargo fmt --check                  # check Rust formatting
cargo clippy -- -D warnings        # Rust linter
MIX_ENV=bench mix run bench/torque_bench.exs  # run benchmarks (requires simdjsone + jiffy)
```

`TORQUE_BUILD=true` is required for local development to force compilation from Rust source instead of downloading precompiled binaries. Without it, `RustlerPrecompiled` will try to fetch binaries from GitHub releases.

## Releasing

```bash
./scripts/release.sh  # tags, pushes, waits for CI, generates checksums
```

The script reads the version from `mix.exs`, creates a git tag, waits for the release workflow to build precompiled NIFs for all targets, then generates checksums. After it completes, commit the checksum file and run `mix hex.publish`.

## Architecture

Torque is a high-performance JSON library for Elixir using Rustler NIFs backed by sonic-rs (SIMD-accelerated JSON). sonic-rs is **vendored** under `native/sonic-rs/` with a minimal patch (see that crate's `Cargo.toml`): its native push-based `JsonVisitor` is made public, and its DOM parser is capped at 128 nesting levels so deeply nested input returns an error instead of overflowing the stack.

### Decoding Strategies

1. **Parse + Get** — `parse/1` returns an opaque reference to a parsed document (`sonic_rs::Value`). `get/2,3` extracts fields by JSON Pointer (RFC 6901) path via `value_to_term`. `get_many/2` extracts multiple fields in a single NIF call. Ideal when only a subset of fields is needed.

2. **Full decode** — `decode/1` builds Erlang terms directly during the SIMD parse by implementing sonic-rs's native `JsonVisitor` (`native_decode.rs`): single pass, no intermediate `Value`, zero-copy sub-binaries for unescaped strings.

### Encoding

`encode/1` walks Elixir terms directly (no intermediate representation) and writes JSON bytes to a buffer. Supports maps (atom/binary keys), lists, numbers, booleans, nil, and jiffy-style `{proplist}` tuples.

### Scheduler Awareness

Inputs larger than 20 KB are automatically dispatched to dirty CPU schedulers to avoid blocking normal BEAM schedulers. The `get/2` NIF always runs on a normal scheduler (sub-microsecond pointer traversal).

### Type Conversion

| JSON | Elixir |
|------|--------|
| object | map with binary keys |
| array | list |
| string | binary |
| integer | integer (i64/u64) |
| float | float |
| true/false | true/false |
| null | nil |

### Key Files

- `lib/torque.ex` — public API with `@doc`, typespecs, dirty scheduler dispatch
- `lib/torque/native.ex` — RustlerPrecompiled NIF stubs (set `TORQUE_BUILD=true` to compile from source)
- `native/torque_nif/src/lib.rs` — NIF registration, `ParsedDocument` resource
- `native/torque_nif/src/decoder.rs` — parse, get, get_many, decode NIFs
- `native/torque_nif/src/native_decode.rs` — fused decoder; builds terms during the SIMD parse via sonic-rs's `JsonVisitor`
- `native/torque_nif/src/encoder.rs` — direct term-walking JSON encoder
- `native/torque_nif/src/types.rs` — sonic_rs Value → Erlang term conversion (used by get/get_many)
- `native/torque_nif/src/atoms.rs` — cached atoms (ok, error, nil, no_such_field, nesting_too_deep, unsupported_type, non_finite_float, invalid_key, malformed_proplist, invalid_utf8)
- `native/sonic-rs/` — vendored, Torque-patched sonic-rs (native `JsonVisitor` exposed + DOM recursion-depth limit)

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
TORQUE_BUILD=true mix deps.get     # fetch deps + force local Rust build
TORQUE_BUILD=true mix compile      # build (includes Rust NIF compilation)
TORQUE_BUILD=true mix test         # functional tests
TORQUE_BUILD=true mix test --include perf   # full suite, including :perf
mix test test/pointer_test.exs:42  # run single test by line number
mix compile --warnings-as-errors   # build with strict warnings
mix format                         # format Elixir code
mix format --check-formatted       # check Elixir formatting
mix dialyzer                       # static type analysis
cargo fmt                          # format Rust code (run from repo root)
cargo fmt --check                  # check Rust formatting
cargo clippy --workspace --all-targets -- -D warnings   # Rust linter (as CI runs it)
cargo test --workspace             # Rust unit tests
# The vendored crate is a separate workspace, so none of the above reaches it.
# Its root disables every lint for upstream code; `extract.rs` denies them for
# itself, which is why no `-D warnings` is needed (and would do nothing).
cargo fmt --manifest-path native/sonic-rs/Cargo.toml --check
cargo clippy --manifest-path native/sonic-rs/Cargo.toml --lib
MIX_ENV=bench mix run bench/torque_bench.exs  # run benchmarks
```

`TORQUE_BUILD=true` is required for local development to force compilation from Rust source instead of downloading precompiled binaries. Without it, `RustlerPrecompiled` will try to fetch binaries from GitHub releases.

## Profile-Guided Optimisation (PGO)

```bash
./scripts/pgo-build.sh   # instrument -> run workload -> merge -> rebuild optimised
```

Produces an optimised `priv/native/torque_nif.so` (typically 5-15% faster on
JSON-heavy work than plain `-O3`). The script builds an instrumented NIF, runs
`bench/pgo_workload.exs` to collect branch/call-frequency data, merges the raw
`*.profraw` counters with `llvm-profdata`, then rebuilds with `-Cprofile-use`.
Like any `TORQUE_BUILD` build it overwrites `priv/native/torque_nif.so`, so
re-run `mix compile` (without PGO) to get back to a plain build.

Notes:
- rustc is LLVM-based, so PGO uses the same `llvm-profdata merge` step as a
  Clang PGO build. The merge tool's LLVM major version **must** match rustc's
  (`rustc -vV`); the script auto-detects a matching one (rustup
  `llvm-tools-preview`, Homebrew `llvm@<major>`, or PATH) — override with
  `LLVM_PROFDATA=...` if detection misses.
- Setting `RUSTFLAGS` replaces `native/torque_nif/.cargo/config.toml`'s
  rustflags rather than merging, so the script re-states `-C target-cpu=native`.
  Keep `BASE_RUSTFLAGS` in `scripts/pgo-build.sh` in sync with that config.
- Point the script at a different workload with `WORKLOAD=path/to.exs`.

The release workflow (`release.yml`) applies the same profile → rebuild step to
the targets it builds on a native runner (`aarch64-apple-darwin`,
`x86_64-unknown-linux-gnu`): it builds an instrumented NIF, runs
`bench/pgo_workload.exs` through the BEAM to collect a same-arch profile, then
rebuilds with `-Cprofile-use`. The cross-compiled targets (`x86_64-apple-darwin`
built on the arm runner, and `aarch64-unknown-linux-gnu` via `cross`) build
plain `-O3`, because PGO needs to *run* the instrumented binary and there's no
native runner for them. Trigger `release.yml` via `workflow_dispatch` to
build-and-profile every target without publishing (create/upload are gated on a
tag).

## Releasing

```bash
export HEX_API_KEY=...   # key with api:write permission
./scripts/release.sh     # tag, wait for CI, checksums, commit, publish
```

The script reads the version from `mix.exs`, creates a git tag, waits for the
release workflow to build precompiled NIFs for all targets, generates checksums,
commits and pushes them, then runs `mix hex.publish --yes`.

`HEX_API_KEY` is Hex's unencrypted write key: setting it skips the local-password
prompt, which is what makes the publish step non-interactive. Mint one with
`mix hex.user key generate --key-name torque-release --permission api:write`.
The script fails up front (before tagging) if it's missing. To stop after
checksums and publish by hand instead, run with `SKIP_PUBLISH=1`.

### Version bumping

The version lives in **three places**: `@version` in `mix.exs`, `version` in
`native/torque_nif/Cargo.toml` (these two must match — `release.sh` refuses
to tag on a mismatch), and the `{:torque, "~> x.y.z"}` install snippet in
`README.md`. To bump:

1. Edit `@version` in `mix.exs`, `version` in `native/torque_nif/Cargo.toml`,
   and the dep snippet in `README.md`.
2. Run `TORQUE_BUILD=true mix compile` once so `Cargo.lock` picks up the crate
   version.
3. Commit all four files (`mix.exs`, `Cargo.toml`, `Cargo.lock`, `README.md`)
   together as a single `Bump version to x.y.z` commit (see faaa403) before
   running `./scripts/release.sh`.

## Architecture

Torque is a high-performance JSON library for Elixir using Rustler NIFs backed by sonic-rs (SIMD-accelerated JSON). sonic-rs **0.5.8** is **vendored** under `native/sonic-rs/` with a minimal patch (see that crate's `Cargo.toml`): its native push-based `JsonVisitor` is made public, its parser is capped at 128 nesting levels so deeply nested input returns an error instead of overflowing the stack, and a document's objects/arrays are exposed as slices (`Value::as_pair_slice` / `as_value_slice`) so term building walks them without going through `Object`'s two-representation iterator.

`Read` caches the pinned input's `NonNull<[u8]>` in a field instead of resolving it per access. `slice()` backs every `peek`/`at`/`eat`/`remain`, and reaching it through `PinnedInput::as_ptr` cost a discriminant branch plus, on the `FastStr` arm, a second dispatch over that type's own representations — *per token*. The input is pinned and never reassigned, which is exactly the invariant that makes one resolved pointer valid for the whole parse; caching it cut validated one-pass extraction by 24% of its instructions and full decode by 6.5%.

Skipping sheds two costs of its own. `skip_one_at` is split so the slice it returns is built only where a caller wants it, since `skip_object`/`skip_array` and the extractor's descent all discard it. And container members take `skip_member`, which handles scalars inline: `skip_one_value_at` is self-recursive and so cannot inline into itself, making every member of every container pay a full frame for what is one inlined scan — worth 11% of validated extraction, 67.3 µs to 59.4 µs on a 135 KB feed.

### Decoding Strategies

1. **Parse + Get** — `parse/1` returns an opaque reference to a parsed document (`sonic_rs::Value`). `get/2,3` extracts fields by JSON Pointer (RFC 6901) path via `value_to_term`. `get_many/2` extracts multiple fields in a single NIF call. The handle is the point: `parse/1` builds the document so later lookups are cheap, which pays off when the same document is queried more than once. For one-shot extraction the compiled-pointer path below is ~2× faster, since it never builds a document it is about to discard.

2. **Compiled pointers** — for a *fixed* set of paths extracted from every document, `compile_pointers/2` pre-parses the pointer strings once into a `CompiledPaths` resource (`PathSeg::Key` / `PathSeg::Num{idx,key}`, with `~`-unescaping and array-index-vs-object-key resolution done up front) and prepares them into a `sonic_rs::extract::ExtractPlan`. `parse_get_many_nil/2` walks the document once through that plan (`native/sonic-rs/src/extract.rs`): values are built only where a path ends, everything else is skipped, and no `Value` DOM is materialized — so cost tracks document size, not content. Against the full-parse implementation it replaced: 2.3× on a 440 KB feed with three paths, 1.3× on a 500 B request; `validate: false` (SIMD skip instead of parsing the unselected regions) makes those 6.1× and 1.8×. The handle carries `unique_keys` and `validate`; `get_many_nil/2` still answers from a parsed document via `pointer_lookup_compiled`, and both resolve duplicate keys last-wins (first-wins under `unique_keys`). Validated extraction reports a fault anywhere in the document, including regions no path selects. sonic-rs's own lazy `get_many`/`PointerTree` is deliberately **not** used: it drops fields when a key repeats and stops validating after its last hit.

   `validate: true` applies full parser checks to skipped values, including Unicode escapes and finite numbers. `validate: false` keeps structural-only skipping. Parsing skipped numbers adds about 6% on a number-heavy 110 KB fixture (89.7 → 95.4 µs); string-heavy input was unchanged.

   Three invariants the extractor has to restore by hand, because it fills results as it walks rather than reading them out of a finished document. A repeated key overwrites, so re-entering one clears every result slot the plan holds beneath it (`Extractor::clear`) before reading the new value — otherwise `{"a":{"x":1},"a":{}}` answers `/a/x` with the dead `1`. Recognising that repeat is per object and has to work at any plan width: tracking it in one `u64` meant the 65th key at a node had no bit, so `unique_keys` silently became last-wins there and the unchecked early exit could never fire, because its found-count could not reach the node's width. Nodes up to `INLINE_SEEN` (64) keys still use the word; wider ones stamp a scratch vector indexed by child plan node, one allocation for the whole extraction rather than a bitset per object, and the stamp is bumped per object entered so a child's mark is only current inside its own object. And the 128-level nesting limit is one budget for the whole call: the plan walk, the skips and the parse of each selected subtree all count from the document's root (`parse_dom` and `skip_one_at` take the level they start at), otherwise a path 100 segments long buys another 128 levels below it and a 200-level document comes back extracted instead of refused. All three are pinned by tests in `test/pointer_test.exs` — the width ones sweep 1, 63, 64, 65 and 200 keys at a node — and the duplicate-key extraction property in `test/property_test.exs`.

   `ExtractPlan` indexes wide nodes during construction and drops those indexes in `finish`, since extraction scans the prepared child lists directly. The construction index uses `AHashMap`. `add_path` accepts borrowed `Seg<'_>` values, avoiding an intermediate vector and duplicate key ownership; the plan copies only keys added to its nodes.

   Extraction hands back `Extracted::Str` for a string with no escapes: a slice of the input, which `parse_get_many_nil` turns into a sub-binary of the caller's JSON rather than copying it into a `Value` and out again. That is what `decode/1` has always done with string values, and it is worth the same here — on a 1.9 KB OpenRTB request, 22 string fields went from 3.10 µs to 2.05 µs, and a string field now costs less than a number field (25 ns against 28 ns) because a sub-binary is cheaper than building an integer term. Escaped strings still copy: their bytes live in the parser's scratch buffer, which the next string overwrites.

   Whether to point at the input is decided per call, in `borrow_input`, not taken unconditionally. A sub-binary keeps the whole input alive behind it, and one-shot extraction is the one path whose purpose is to answer a few paths and drop the document: a 100-byte user agent taken from a 400 KB feed held all 400 KB, which `process_info(:binary)` reports and `test/pointer_test.exs` pins. So the input is borrowed from when it is at or under `BORROW_ANY_INPUT` (4 KB — a single allocation, about what one refc binary's bookkeeping costs, and where the request-shaped documents this path is tuned on sit), or when the strings being kept are at least a `BORROW_INPUT_FRACTION` of it and copying them out would save little. Nothing under 64 bytes can pin anything either way: ERTS copies a slice that small onto the process heap.

3. **Full decode** — `decode/1` builds Erlang terms directly during the SIMD parse by implementing sonic-rs's native `JsonVisitor` (`native_decode.rs`): single pass, no intermediate `Value`, zero-copy sub-binaries for unescaped strings, and a per-call key cache that decodes a key repeated across objects (the common array-of-records shape) to one shared term (median −3–4%, p99 −16%, decoded-term heap −37% on record-shaped payloads).

### Map Key Ordering

`enif_make_map_from_arrays` hands small maps to ERTS's
`erts_validate_and_sort_flatmap`, an insertion sort whose comparator is a full
generic term comparison. Keys arriving in Erlang term order cost it `n - 1`
comparisons; any other order costs O(n²). Only the BEAM iterates maps in term
order, so only BEAM-encoded JSON gets the cheap path — every other producer
emits its schema's field order, which on a record array was 68% of decode time.

`map_order.rs` orders each object's members first, comparing the raw key bytes
the parser already holds instead of built terms, and memoizes the permutation
per object shape so an array of records does not re-derive it per element.
Shared by the decoder (`native_decode.rs`) and `value_to_term` (`types.rs`). On
the 776 KB fixture: schema order −13.1%, reversed −30.6%, term-ordered JSON
+2-3%, key-order sensitivity 1.19×/1.52× → 1.01×/1.03×; the same records
through `get/2` −4.3% and −16.5%. That fixture's dominant object has 40 members,
making it an ERTS hashmap that none of this reaches; on the shape-variety
group's record array it is −68%.

The two callers cross over at different sizes — `value_to_term` holds `Value`s
and unpacks every key to reach the bytes the decoder already has, so its fixed
cost per object is about double — and a second, higher threshold for it was
measured and rejected: on 7-member records it bought 6% on term-ordered input
by giving up 12% on schema-ordered input, the order this exists to fix. What
`reorder_object` does instead is take the keys its caller has already unpacked,
which is free for every order. Term-ordered input still pays the scan and saves
nothing, and on this path that is 6-7% rather than the decoder's 2-3%.

This is correctness-neutral — ERTS sorts whatever it is handed — so the ways it
can silently switch itself off are pinned by regression tests in
`test/decode_order_perf_test.exs` and `map_order.rs`. Read those and the
module's doc comments before changing it.

Keep all four key-order benchmark groups. "Decode — object key order" varies
member order, since every other payload in that file is `Jason.encode!` output
and only exercises the free case. "Decode — object shape variety" varies how
many distinct shapes a document cycles through, which is what the memo is keyed
on. "Decode — member count sweep" and "Extract — member count sweep" are where
the threshold comes from, the second sweeping key style as well because keys
that share a prefix make ERTS's comparator work harder and shift the
crossover. "Extract subtree — object key order" runs the same orders through
`value_to_term`, the other caller, which no other group reaches.

### Encoding

`encode/1` walks Elixir terms directly (no intermediate representation) and writes JSON bytes to a buffer. Supports maps (atom/binary/integer keys — integer keys are stringified, since JSON object names must be strings), lists, numbers, booleans, nil, and jiffy-style `{proplist}` tuples.

### Scheduler Awareness

Decode/parse inputs larger than 20 KB are automatically dispatched to dirty CPU schedulers to avoid blocking normal BEAM schedulers. Encoding cannot cheaply predict output size, so dirty dispatch is opt-in via `dirty: true` on `encode/2`, `encode!/2`, and `encode_to_iodata/2`.

Batch lookups dispatch on **path count**, not bytes: they build one result per path, so a `pointers()` handle carrying 100k paths is 100k terms of work against a two-byte document, and 100k pointers on a normal scheduler measured 11.5 ms of wakeup latency for every other process on it. `compile_pointers/2` returns `{resource, path_count}` so the count is known without walking the caller's list — `length/1` on a large list is the cost being avoided — and `get_many/2`, `get_many_nil/2`, `get_many_defaults/2` and `parse_get_many_nil/2` go dirty past `@dirty_path_count` (2048). A raw path list has no stored count, so `many_paths?/1` drops the threshold's worth of cells rather than measuring the whole list. `compile_paths` runs dirty unconditionally: startup work, and a 100k-path set takes ~50 ms to prepare. The threshold is calibrated on `get_many_defaults/2`, the slowest per path (p99 258 µs at 2048, 667 µs at 4096, against a target of half a millisecond); below it the dispatch is unmeasurable, and at it dirty costs 0.5-5% throughput.

Two shapes this cannot see. A single huge result — `get/2` on a path holding a 1.3 MB array measured 1.6 ms against 1 µs for a scalar — has no cheap predictor at the call site, so `consume_timeslice_nodes` reports that work afterwards, adjusting the process's reduction budget without preempting a call already made (the deliberate choice of `7560c71`); `get/3`'s third argument is already `default`, so there is no room for encode's `dirty: true` shape without a new arity. And a wide document makes every lookup linear in its member count, which is the caller's document rather than the caller's path set. What the accounting does now cover is the result count itself: `value_to_term` charges only container children, so a batch of scalar, missing or default results used to report no work at all, and each emitted result is counted alongside them.

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
- `native/torque_nif/src/lib.rs` — NIF registration, `ParsedDocument` + `CompiledPaths` (`PathSeg`) resources
- `native/torque_nif/src/decoder.rs` — parse, get, get_many, get_many_nil, get_many_defaults, decode NIFs; compiled-pointer + one-pass `parse_get_many_nil` path
- `native/torque_nif/src/native_decode.rs` — fused decoder; builds terms during the SIMD parse via sonic-rs's `JsonVisitor`
- `native/torque_nif/src/map_order.rs` — Erlang term ordering for object keys, shared by the decoder and `value_to_term`
- `native/torque_nif/src/encoder.rs` — direct term-walking JSON encoder
- `native/torque_nif/src/types.rs` — sonic_rs Value → Erlang term conversion (used by get/get_many)
- `native/torque_nif/src/atoms.rs` — cached atoms (ok, error, nil, no_such_field, nesting_too_deep, unsupported_type, non_finite_float, invalid_key, malformed_proplist, invalid_utf8)
- `native/sonic-rs/` — vendored, Torque-patched sonic-rs 0.5.8 (native `JsonVisitor` exposed + parser recursion-depth limit + document slice accessors + `extract` one-pass path extraction)
- `native/sonic-rs/src/extract.rs` — Torque-owned: prepared `ExtractPlan` + one-pass extraction, validated or SIMD-skipped

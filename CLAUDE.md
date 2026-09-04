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

`TORQUE_BUILD=true` is required for local development to force compilation from Rust source instead of downloading precompiled binaries. Without it, `RustlerPrecompiled` will try to fetch binaries from GitHub releases. The flag is read when `Torque.Native` compiles, so `Torque.Build` (`lib/torque/build.ex`) makes it part of that module's staleness through `__mix_recompile__?/0`: without it a `_build` tree made without the variable keeps loading a downloaded NIF however later commands are invoked, which silently runs a *released* binary against local Rust changes and reports a green suite. The check cannot live in `Torque.Native`, because a module whose `on_load` fails is not loadable and Mix cannot ask it anything.

## Profile-Guided Optimisation (PGO)

```bash
./scripts/pgo-build.sh   # instrument -> run workload -> merge -> rebuild optimised
```

Produces an optimised `priv/native/torque_nif.so` (typically 5-15% faster on
JSON-heavy work than plain `-O3`). The script builds an instrumented NIF, runs
`bench/pgo_workload.exs` to collect branch/call-frequency data, merges the raw
`*.profraw` counters with `llvm-profdata`, then rebuilds with `-Cprofile-use`.
Like any `TORQUE_BUILD` build it overwrites `priv/native/torque_nif.so`. Run
`TORQUE_BUILD=true mix compile --force` to restore a plain build: the variable
selects the source build, and `--force` replaces the profiled artifact even when
the sources are unchanged.

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

`Read` caches the pinned input's `NonNull<[u8]>` in a field instead of resolving it per access. `slice()` backs every `peek`/`at`/`eat`/`remain`, and reaching it through `PinnedInput::as_ptr` cost a discriminant branch plus, on the `FastStr` arm, a second dispatch over that type's own representations — *per token*. The input is pinned and never reassigned, which is exactly the invariant that makes one resolved pointer valid for the whole parse; caching it cut validated one-pass extraction by 24% of its instructions and full decode by 6.5%. `as_pair_slice`/`as_value_slice` take the same node fast path as `as_node_str`: both read a pointer and a length straight out of the node, and `unpack_pair_slice` never looks at the `Shared` it is reached through, but resolving one walks backwards over the arena to its header and asserts a canary there *in release builds* — a cold load per lookup for a value then discarded. That is a cache miss, not an instruction: `get_many/2` on a 135 KB document dropped 6% of its cycles at unchanged instruction count.

Skipping sheds two costs of its own. `skip_one_at` is split so the slice it returns is built only where a caller wants it, since `skip_object`/`skip_array` and the extractor's descent all discard it. And container members take `skip_member`, which handles scalars inline: `skip_one_value_at` is self-recursive and so cannot inline into itself, making every member of every container pay a full frame for what is one inlined scan — worth 11% of validated extraction, 67.3 µs to 59.4 µs on a 135 KB feed.

### Decoding Strategies

1. **Parse + Get** — `parse/1` returns an opaque reference to a parsed document (`sonic_rs::Value`). `get/2,3` extracts fields by JSON Pointer (RFC 6901) path via `value_to_term`. `get_many/2` extracts multiple fields in a single NIF call. The handle is the point: `parse/1` builds the document so later lookups are cheap, which pays off when the same document is queried more than once. For one-shot extraction the compiled-pointer path below is ~2× faster, since it never builds a document it is about to discard.

   `pointer_lookup` uses byte splitting below `SEARCHER_SPLIT_BYTES` and `str::split('/')` above it. `/` and `~` are ASCII, so byte splitting preserves UTF-8 boundaries. The byte loop avoids searcher setup on short pointers; the standard searcher vectorizes long delimiter scans. Compiled pointers are split once by `compile_pointers/2` and do not use this path.

   Objects in a parsed document are flat `(key, value)` slices, so a lookup scans and a batch of paths against a wide object scans it once per path. Past `WIDE_OBJECT_MEMBERS` (128) a batch shares an `ObjectMemo` (`decoder.rs`) that indexes the objects its paths keep returning to; `get/2,3` and `length/2` instantiate the same code with `NoIndex`, which carries the duplicate-key rule and nothing else, and monomorphise back to the plain scan. Five things the memo has to get right, each pinned by a test — the shape ones in `decoder.rs`'s own unit tests, which drive `ObjectMemo` against a parsed document, and the cost ones in `test/pointer_test.exs`:

   - **Which duplicate wins.** The index answers what the scan answers: last occurrence, or first under `unique_keys`.
   - **More than one object.** A path walks a *chain*, so the object it ends at is not the one the next path starts from. Remembering one object evicts it before it is ever indexed — `/g1/f<i>` through a wide `g` and a wide `f` cost 18.7× the same lookups under a narrow parent, and two sibling dictionaries 17.6×. Slots are addressed by the object's address (4 sets × 2 ways) rather than searched and replaced, because every replacement policy over a searched set has a shape that defeats it: round-robin thrashes on one object more than there are slots, and evicting the unindexed slot thrashes on *two*, since the slot claimed one lookup ago is always the unindexed one. Two ways, because one way makes the same collision a silent cliff decided by where the parse happened to put the objects — measured 1025 µs against 130 µs on the same document.
   - **Who gets in.** Admission before eviction, not permanent incumbency. Protecting a built index for ever is its own cliff — twelve wide objects taken 128 paths at a time, each hot while the incumbents were finished with, scanned everything past the second and cost 2.6× the same paths split into calls that started fresh — and evicting on sight turns a conflict into repeated full-object hashes. So a set whose ways are both *occupied* - indexed or still earning one - puts newcomers on probation: one is scanned until it has paid the same scanning an incumbent paid, and only then takes a way. Two probation slots per set, matching the ways, because a phase change swaps *both* incumbents: with one, `C, D, C, D` against a set holding `A, B` had each newcomer resetting the other's credit and neither ever qualified — 623 µs against 194 µs for the same lookups grouped. The slot a newcomer takes — and the way an admitted newcomer takes, which is the same question — is the one furthest from its own index: never one whose next lookup would build it, then by progress against what that object's width owes rather than raw credit, since 200 members scanned is most of what a 128-member object owes and a rounding error to a 100k-member one, then by the key bytes still owed. Both fractions are clamped, so scanning long past a target already met is not progress that can be spent twice. An unindexed way goes before an indexed one, and only when both hold indexes does the one used longest ago go.
   - **What earns it.** The members an object has had *scanned*, not the lookups it has answered. A scan short-circuits, so a key at the end of a last-wins object is one comparison: counting lookups let 27 of those buy the same index a full scan does, measured 1 µs for 27 paths and 1719 µs for the 28th. `scan_budget` is the width times the break-even, and a lookup adds what it compared.
   - **When to build.** Per object, on its own scan credit, and never on the batch's length. An index costs a hash and an insert per member where a scan costs a compare, and the gap *widens* with width: the scan gets cheaper per member as the walk streams while the table stops fitting in cache. Measured break-even, marginal cost inside one batch: 3.7 lookups at 128 members, 7.4 at 512, 9.8 at 2000, 22.3 at 8000, 27.9 at 100k, which is the curve `index_after_visits` follows — a flat 8 charged a 100k-member object 1.6 ms to save seven 57 µs scans, taking an eight-path batch from 460 µs to 1830 µs. Batch length looks like evidence of reuse and is not: it bounds what an object *could* be asked for, so indexing on the second visit whenever the batch was long enough built an index for each of eight wide objects that got four lookups apiece — 7.7× what the same 32 paths cost split into calls that could not do it. Waiting each object's own break-even is the ski-rental answer: at most twice what knowing its share of the batch up front would cost. Member credit is not the only currency: an index hashes every key byte, so an object holding more key bytes than `INDEX_KEY_BYTES` (1 MiB) — profiled once, on the pass its scans already pay for, and remembered on the slot *and* on the candidate — has to earn it in bytes too, and only builds once its scans have read as many as the build would hash. 128 keys of 256 KiB answer a short needle by length alone, so three lookups measured 2 µs and a fourth that hashed 32 MB measured 2.3 ms; refusing outright is the opposite cliff, doubling a 1.2 MB object's same-length lookups from 0.51 to 1.06 ms, and same-length scans over that object read what the build would hash in one call. Those bytes have to be bytes a comparison *reached*: `memcmp` stops at the first difference, so `eq_counting` compares in chunks that double from 64 bytes — a cache line, the unit the machine actually reads — and counts the ones it visited. Crediting whole keys instead said a scan of 100k same-length keys differing at byte 0 had read 28 MB where it read 6.4 MB, which both overcharged the scheduler and let those scans buy the index. Counting costs about 21% on keys that really are compared to the end, which is why the uncounted scan stays for needles under `LONG_KEY_BYTES` (256) — unless the scan is wide enough that its bytes *are* its cost: past `SCAN_BYTES_ABOVE` (4 MiB of possible comparison) it counts anyway, since 100k keys of 256 bytes measured 498 µs and reported the 770 reductions a member count alone implies.
   - **What it reports.** Lookups are invisible to everything else the accounting counts, since eight paths return eight terms whatever the object's width, and a wide-object batch runs on a normal scheduler until 2048 paths. `Scanned::charge` and `Work::scanned`/`indexed`/`key_bytes` record them in the units they happen in, and `Work::nodes` converts once at the end of the call: comparisons actually made rather than the object's width (a last-wins scan stops at the last member, so `/k100000` on a 100k-member object is one compare and charges 10 reductions rather than 808), one node per 32 comparisons, one per 2 indexed members, one per 5 bytes of key hashed. Bytes matter because a member count cannot see them — 127 keys of 256 KiB that share a prefix are 32 MB of `memcmp`, measured 636 µs and reported as four reductions while the narrow path skipped accounting entirely, and 128 keys of 64 KiB are 8.4 MB of hashing. Narrow objects are charged the same way now. Past `LONG_KEY_BYTES` (256) a lookup takes `scan_long`, which counts same-length comparisons as it walks: counting them inside the ordinary scan cost a three-path batch 0.186 → 0.339 µs, and counting them afterwards walked the members twice, which a needle's length cannot justify — 65 → 150 µs across the boundary against short keys. Routing to it costs the ordinary lookup one comparison of the needle's length, 0.185 → 0.205 µs on that same batch. Pointer paths are charged too, since a path is scanned to split it and again to unescape a `~` segment: 32 MiB of path took 3.8 ms against `{}` and 23 ms with escapes, both reported as four reductions, and both now dispatch dirty on `byte_size(path)` like any other 20 KB of input. The memo itself is built on the first wide lookup rather than per call, for the same reason: zeroing its slots cost 0.187 → 0.278 µs on a batch that never met a wide object. Without any of this, a worker looping eight-path lookups over a 100k-member object starved every other process on its scheduler: measured against four such workers on one normal scheduler, a co-tenant waiting on a 1 ms timer woke twice in three seconds, p50 and p99 both 5.4 s. With it, 1501 wakeups, p50 2.00 ms, p99 2.005 ms. Steady-state per-call latency improves too — 784 µs against 1921 µs — and the first heavy batch on a document pays 1360 µs for the discovery before `heavy` latches. Throughput depends on whether normal schedulers are oversubscribed, and the earlier claim of a flat 15% cost was measured in the one corner where it is worst: with schedulers at or above the worker count it is a *gain* (+17% at 4 workers on 4 schedulers, +27% at 2 on 4, +10-14% single-worker), and only 4 workers crowded onto one scheduler pay, at -75%. That corner is precisely the one where the old behaviour bought its throughput by never yielding.

   The index is an `AHashMap`, not a `HashMap`: keys come from the document, so the hasher needs a runtime-random seed, but SipHash is not the only thing that provides one and it doubled the build. Switching halved break-even and made every batch size faster than scanning per path — 3 paths 28.2 → 4.2 µs, 64 paths 30.8 → 24.4, 2048 paths 122.8 → 113.5, 16384 paths 827.8 → 725.6.

2. **Compiled pointers** — for a *fixed* set of paths extracted from every document, `compile_pointers/2` pre-parses the pointer strings once into a `CompiledPaths` resource (`PathSeg::Key` / `PathSeg::Num{idx,key}`, with `~`-unescaping and array-index-vs-object-key resolution done up front) and prepares them into a `sonic_rs::extract::ExtractPlan`. `parse_get_many_nil/2` walks the document once through that plan (`native/sonic-rs/src/extract.rs`): values are built only where a path ends, everything else is skipped, and no `Value` DOM is materialized — so cost tracks document size, not content. Against the full-parse implementation it replaced: 2.3× on a 440 KB feed with three paths, 1.3× on a 500 B request; `validate: false` (SIMD skip instead of parsing the unselected regions) makes those 6.1× and 1.8×. The handle carries `unique_keys` and `validate`; `get_many_nil/2` still answers from a parsed document via `pointer_lookup_compiled`, and both resolve duplicate keys last-wins (first-wins under `unique_keys`). Validated extraction reports a fault anywhere in the document, including regions no path selects. sonic-rs's own lazy `get_many`/`PointerTree` is deliberately **not** used: it drops fields when a key repeats and stops validating after its last hit.

   For an already-parsed document, both `get_many/2` and `get_many_nil/2` accept the compiled handle and walk its `paths`; the former preserves tagged missing/depth errors, while the latter substitutes `nil`. Both take `unique_keys` from the handle and dispatch from its stored path count and path bytes.

   `validate: true` applies full parser checks to skipped values, including Unicode escapes and finite numbers. `validate: false` keeps structural-only skipping. Parsing skipped numbers adds about 6% on a number-heavy 110 KB fixture (89.7 → 95.4 µs); string-heavy input was unchanged.

   Three invariants the extractor has to restore by hand, because it fills results as it walks rather than reading them out of a finished document. A repeated key overwrites, so re-entering one clears every result slot the plan holds beneath it (`Extractor::clear`) before reading the new value — otherwise `{"a":{"x":1},"a":{}}` answers `/a/x` with the dead `1`. Recognising that repeat is per object and has to work at any plan width: tracking it in one `u64` meant the 65th key at a node had no bit, so `unique_keys` silently became last-wins there and the unchecked early exit could never fire, because its found-count could not reach the node's width. Nodes up to `INLINE_SEEN` (64) keys still use the word; wider ones stamp a scratch vector indexed by child plan node, one allocation for the whole extraction rather than a bitset per object, and the stamp is bumped per object entered so a child's mark is only current inside its own object. And the 128-level nesting limit is one budget for the whole call: the plan walk, the skips and the parse of each selected subtree all count from the document's root (`parse_dom` and `skip_one_at` take the level they start at), otherwise a path 100 segments long buys another 128 levels below it and a 200-level document comes back extracted instead of refused. All three are pinned by tests in `test/pointer_test.exs` — the width ones sweep 1, 63, 64, 65 and 200 keys at a node — and the duplicate-key extraction property in `test/property_test.exs`.

   `ExtractPlan` indexes wide nodes during construction and drops those indexes in `finish`, since extraction scans the prepared child lists directly. The construction index uses `AHashMap`. `add_path` accepts borrowed `Seg<'_>` values, avoiding an intermediate vector and duplicate key ownership; the plan copies only keys added to its nodes.

   Extraction hands back `Extracted::Str` for a string with no escapes: a slice of the input, which `parse_get_many_nil` turns into a sub-binary of the caller's JSON rather than copying it into a `Value` and out again. That is what `decode/1` has always done with string values, and it is worth the same here — on a 1.9 KB OpenRTB request, 22 string fields went from 3.10 µs to 2.05 µs, and a string field now costs less than a number field (25 ns against 28 ns) because a sub-binary is cheaper than building an integer term. Escaped strings still copy: their bytes live in the parser's scratch buffer, which the next string overwrites.

   Whether to point at the input is decided per call, in `borrow_input`, not taken unconditionally. A sub-binary keeps the whole input alive behind it, and one-shot extraction is the one path whose purpose is to answer a few paths and drop the document: a 100-byte user agent taken from a 400 KB feed held all 400 KB, which `:binary.referenced_byte_size/1` reports and `test/pointer_test.exs` pins in both directions — a small input is borrowed from, a large one is not. So the input is borrowed from when it is at or under `BORROW_ANY_INPUT` (4 KB — a single allocation, about what one refc binary's bookkeeping costs, and where the request-shaped documents this path is tuned on sit), or when the strings being kept are at least a `BORROW_INPUT_FRACTION` of it and copying them out would save little.

   The size that decides it is the **allocation**, not the binary handed in. A `binary_part/3` of a 400 KB refc binary is 630 bytes long and keeps all 400 KB alive, so measuring the slice borrowed the parent behind a 100-byte field — the exact case the policy exists to prevent. There is no NIF entry point for that size (`enif_inspect_binary` reports the slice), so `parse_get_many_nil/2` passes `:binary.referenced_byte_size/1` in and the NIF uses the logical length only for the offset check that decides a string is inside the input at all. On OTP 28+ ERTS copies a slice of 64 bytes or less onto the process heap, which the policy does not lean on: 26 and 27 build a real sub-binary at any size and CI runs all four.

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
Strings are written by `escape.rs`'s `write_json_string`, which reserves once for both quotes and the worst-case body so neither quote re-checks capacity, and runs an inlined SWAR pass over the string's clean leading run — eight bytes at a time, testing `< 0x20`, `"`, `\` and the high bit in one word — for anything under `SHORT_STRING` (32 bytes). Below 32 bytes the SIMD chain is pure overhead: AVX2 needs 32 bytes to enter its loop and SSE2 16, so a shorter string paid two `#[target_feature]` calls that cannot inline into each other, both with full register-save prologues, and a libc `memcpy` for the tail, only to run its whole body in the scalar epilogue. JSON is mostly short strings — object keys and record fields — and the profile put its time on those prologues and the `memcpy` PLT call rather than on any comparison. The AVX2 tails take the same pass for the same reason, and `escape_to_vec`, the atom-name path, shares it.

`escape_prefix` reports a **prefix**, not a verdict, and that distinction is the whole design. Returning "clean" or "not clean" and restarting the general path from zero made a string whose first escape is near its end pay for its clean run twice: 30 clean bytes followed by a quote regressed 16%, and a trailing non-ASCII character the same. As a resume point the bytes are already at their final offsets and the general path continues from `src + n` into `dst + n`. The bound is exact — the returned length ends at the *first* byte needing an escape or validation, so nothing before it is re-read and the pass cannot rescan more than the current 8-byte chunk. That is a property of this function, not a claim that the SIMD paths below it have no adverse shapes; the thresholds are tuned on x86-64 and NEON reaches them by a different route. Measured there against the pre-SWAR encoder: a 135 KB record feed 241 → 167 µs, a 576 B request 1.82 → 1.17 µs, escape-at-the-end 2.30 → 1.93 µs, trailing non-ASCII 2.17 → 1.82 µs, an all-non-ASCII short string 2.19 → 1.87 µs. The handoff offset is `unsafe` pointer arithmetic into uninitialised capacity, so five tests in `test/encode_test.exs` pin every length where its shape changes (7/8, 15/16, 23/24, 31/32/33, 63/64/65) against ten escape positions, in values, in keys, in atom names, and for invalid UTF-8 after a clean prefix of each length; each of those spellings is also checked against Jason's. They were mutation-verified: an off-by-one resume offset fails four of them, dropping the high-bit test from the SWAR word fails three, and dropping the escape table from the bytewise tail fails five.

The six SIMD kernels (`escape_*` / `validate_escape_*` × NEON/AVX2/SSE2) spell out the same three blocks by hand: the `QUOTE_TAB` escape emit at eleven sites, the copy-the-clean-run-then-escape-the-stopper block at six, and a 25-line bytewise chunk loop at three. **That duplication is load-bearing and must not be factored into helpers**, however obviously redundant it looks. Collapsing all of it into `#[inline(always)]` helpers costs 4.5% on an escape-heavy 776 KB encode and 6% on a UTF-8-heavy one; extracting only the chunk loop — which never executes on ASCII input at all — still costs 2.1%. The extracted code is not the problem: standalone, the helper version retires 7.8% *fewer* instructions on UTF-8 input and 1.3% fewer on escape-heavy input, and fewer cycles on both. The cost appears only inside the NIF, because `lto = "fat"` with `codegen-units = 1` puts the whole crate in one LLVM module and `write_json_string` is `#[inline]`: changing a kernel's shape changes how it inlines into `encode_binary` and `encode_map_key`. Wall-clock alone cannot show this — rebuilding an *unmodified* `escape.rs` with a few exported no-op functions moves the same benchmarks by up to 6%. Anything measured here has to be compared against that band, by building several such layout perturbations of both sides; the numbers above are medians of 4-6 builds each, where the two families do not overlap. Re-measured on x86-64 after an unrelated refactor moved encode by 3-6%, the extraction costs 7.4-9.7% on the escape-heavy fixture and 9.6-11.1% on the UTF-8-heavy one, over two independent rebuilds. An all-ASCII payload is the control that separates this from layout luck: it never enters the escape branches or the bytewise chunk loop, and it does not move (-0.2%, +1.7%). Cost that tracks whether the extracted code *runs* is the abstraction; layout would move all three.

Binary map keys are probed with `enif_inspect_binary` before the type is asked for, since that call already answers "is this a binary" and a type-first dispatch made every binary key pay two cross-DSO calls to learn what one of them returns. Atom keys reach the type switch behind a failed probe and are *still* faster than type-first — 73.3 against 74.8 µs on a 200-record atom-keyed map, next to 45.2 against 49.4 for the binary-keyed one — because `enif_term_type` is a full tag switch and skipping it on the common path buys more than the probe costs on the other. Integer keys pay the probe too and are rare by construction, existing only to be stringified.

Atom names are read Latin-1, because `ERL_NIF_UTF8` is a NIF 2.17 (OTP 26) addition and `nif_versions` still claims 2.15. `enif_get_atom_length` does not transcode: it *fails* for a name holding any character above U+00FF, which made `:"🚀"` and `:日本語` return `:unsupported_type` while `:café` encoded — atoms are Unicode by construction, so that was a wrong answer, not a limitation. Those names come from `enif_term_to_binary` instead: an atom that is not Latin-1 representable always serialises as `SMALL_ATOM_UTF8_EXT` or `ATOM_UTF8_EXT`, whose payload is the UTF-8 name. That costs a binary per atom, so only the names the Latin-1 read rejects take it, and a 200-record atom-keyed map is unchanged at 46-47 µs. Raising the floor to NIF 2.17 would replace both paths with one `ERL_NIF_UTF8` read and a 1020-byte buffer (255 characters × 4), at the cost of dropping OTP 22-25.


### Scheduler Awareness

Decode/parse inputs larger than 20 KB are automatically dispatched to dirty CPU schedulers to avoid blocking normal BEAM schedulers. Encoding cannot cheaply predict output size, so dirty dispatch is opt-in via `dirty: true` on `encode/2`, `encode!/2`, and `encode_to_iodata/2`.

A batch lookup depends on both the path set and the document shape. Parsed-document batch NIFs start on a normal scheduler under `NORMAL_BUDGET_NODES` and check `Work::nodes()` after each path. An overrun returns `dirty_required`; `retry_dirty` reruns the batch on its dirty twin with `UNBOUNDED_BUDGET`.

`Work` separates document-dependent counters from caller-dependent counters. Comparisons, indexed members, document key bytes and terms built by `value_to_term` contribute to `document_work()`. Emitted results and pointer bytes belong to the path set. `nodes()` includes both groups for scheduler accounting, but only a document-dependent overrun sets `ParsedDocument::heavy`. This keeps an oversized path set from permanently moving later cheap lookups on the same document to dirty schedulers.

Caller work that already exhausts the budget is dispatched dirty before entering the NIF. `@dirty_result_count` handles result construction, while the path-byte limit covers splitting, unescaping and lookup keys. Raw lists use bounded walks; compiled handles carry path count and bytes. `compile_pointers/2` and `parse_get_many_nil/2` keep the separate `@dirty_path_count` rule because their work cannot be retried partway.

A single large result remains indivisible: `value_to_term` completes on the current scheduler, then `consume_timeslice_nodes` reports the work to ERTS. Batch accounting is accumulated in native units and converted once so fractional work carries across paths.

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
- `lib/torque/build.ex` — captures `TORQUE_BUILD` and makes switching it recompile `Torque.Native`
- `native/torque_nif/src/lib.rs` — NIF registration, `ParsedDocument` + `CompiledPaths` (`PathSeg`) resources
- `native/torque_nif/src/decoder.rs` — parse, get, get_many, get_many_nil, get_many_defaults, decode NIFs; compiled-pointer + one-pass `parse_get_many_nil` path
- `native/torque_nif/src/native_decode.rs` — fused decoder; builds terms during the SIMD parse via sonic-rs's `JsonVisitor`
- `native/torque_nif/src/map_order.rs` — Erlang term ordering for object keys, shared by the decoder and `value_to_term`
- `native/torque_nif/src/encoder.rs` — direct term-walking JSON encoder
- `native/torque_nif/src/types.rs` — sonic_rs Value → Erlang term conversion (used by get/get_many)
- `native/torque_nif/src/atoms.rs` — cached atoms (ok, error, nil, no_such_field, nesting_too_deep, unsupported_type, non_finite_float, invalid_key, malformed_proplist, invalid_utf8)
- `native/sonic-rs/` — vendored, Torque-patched sonic-rs 0.5.8 (native `JsonVisitor` exposed + parser recursion-depth limit + document slice accessors + `extract` one-pass path extraction)
- `native/sonic-rs/src/extract.rs` — Torque-owned: prepared `ExtractPlan` + one-pass extraction, validated or SIMD-skipped

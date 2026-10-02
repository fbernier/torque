# Torque

High-performance JSON library for Elixir via [Rustler](https://github.com/rustler-magic/rustler) NIFs, powered by [sonic-rs](https://github.com/cloudwego/sonic-rs) (SIMD-accelerated).

Torque provides the fastest JSON encoding and decoding available in the BEAM ecosystem, with a selective field extraction API for workloads that only need a subset of fields from each document.

## Features

- SIMD-accelerated decoding (AVX2 on x86, NEON on ARM)
- Ultra-low memory encoder (64 B per encode vs ~4 KB for OTP `json`/jason)
- Parse-then-get API for selective field extraction via JSON Pointer (RFC 6901,
  with one documented deviation: `"/"` selects the root, not the empty key)
- Batch field extraction (`get_many/2`) with single NIF call
- Pre-compiled pointers with fused parse + extract (`parse_get_many_nil/2`)
- Automatic dirty CPU scheduler dispatch for decode/parse inputs larger than 20 KB (opt-in `dirty: true` for encode)
- jiffy-compatible `{proplist}` encoding
- Opt-in `Torque.Encoder` protocol for encoding structs, with `@derive`

## Installation

Add to your `mix.exs`:

```elixir
def deps do
  [
    {:torque, "~> 0.4.4"}
  ]
end
```

Precompiled binaries are available for common targets. To compile from source, install a stable Rust toolchain and set `TORQUE_BUILD=true`.

### CPU-optimized variants

On x86_64, precompiled binaries are available for three CPU feature levels:

| Variant | CPU features | `target-cpu` |
|---------|-------------|--------------|
| baseline | SSE2 | `x86-64` |
| v2 | SSE4.2, SSSE3, POPCNT | `x86-64-v2` |
| v3 | AVX2, AVX, BMI1, BMI2, FMA, PCLMULQDQ | `x86-64-v3` + `pclmulqdq` |

At compile time, Torque auto-detects the host CPU and downloads the best matching variant. To override detection (e.g., when cross-compiling for a different target):

```bash
TORQUE_CPU_VARIANT=v2 mix compile  # force SSE4.2 variant
TORQUE_CPU_VARIANT=v3 mix compile  # force AVX2 variant
TORQUE_CPU_VARIANT=base mix compile  # force baseline
```

## Usage

### Decoding

```elixir
{:ok, data} = Torque.decode(~s({"name":"Alice","age":30}))
# %{"name" => "Alice", "age" => 30}

data = Torque.decode!(json)
```

### Selective Field Extraction

Parse once, extract many fields without building the full Elixir term tree:

```elixir
{:ok, doc} = Torque.parse(json)

{:ok, "example.com"} = Torque.get(doc, "/site/domain")
nil = Torque.get(doc, "/missing/field", nil)

# Batch extraction (single NIF call, fastest path)
results = Torque.get_many(doc, ["/id", "/site/domain", "/device/ip"])
# [{:ok, "req-1"}, {:ok, "example.com"}, {:ok, "1.2.3.4"}]
```

When your JSON is known to have no duplicate object keys, pass `unique_keys: true`
for faster field lookups (uses sonic-rs internal indexing instead of linear scan):

```elixir
{:ok, doc} = Torque.parse(json, unique_keys: true)
```

### Compiled Pointers

When the same fixed set of paths is extracted from every document, compile the
pointers once and reuse the handle. `parse_get_many_nil/2` then reads the
document in a single pass, building values only where a path ends and skipping
everything else, without building an intermediate document. On a 1.2 KB bid
request with 26 fields that is ~1.35× the previous fused parse; with 3 paths
and `validate: false` (below) it is ~2.6×.

```elixir
# Once, at startup (e.g. into :persistent_term or application state; the
# handle is a NIF resource, so it cannot live in a module attribute):
pointers = Torque.compile_pointers(["/id", "/site/domain", "/imp/0/banner/w"], unique_keys: true)

# Per document — parse + extract in one call:
{:ok, ["req-1", "example.com", 300]} = Torque.parse_get_many_nil(json, pointers)
```

Missing fields and JSON `null` both become `nil`. The handle also works with an
already-parsed document via `Torque.get_many_nil(doc, pointers)`.

By default a malformed document is reported wherever the fault is, as `parse/2`
would report it, even in a region no path selects. `validate: false` skips
unselected regions with a structural bracket scan instead of tokenizing them,
but a malformed number, literal, or separator inside one of them goes
unreported, and so does anything after the document, which is therefore not
UTF-8 checked either. Truncated input, invalid UTF-8 in any byte the walk
consumed, and errors in selected values are still rejected. Use it only with
trusted input.

It is not a free speed-up. A bracket scan over 64-byte blocks beats tokenizing
a large subtree and loses to it on the few-byte scalars a dense path set leaves
behind, so the win tracks how little of the document the paths select. Three
paths out of a 2 KB request run ~3.6× faster unvalidated; 146 fields of the
same request run ~1.2× slower. Measure your own path set.

```elixir
pointers = Torque.compile_pointers(paths, unique_keys: true, validate: false)
```

### Encoding

```elixir
# Maps with atom or binary keys
{:ok, json} = Torque.encode(%{id: "abc", price: 1.5})
# "{\"id\":\"abc\",\"price\":1.5}"

# Integer keys are stringified — JSON object names must be strings
{:ok, json} = Torque.encode(%{0 => "a", 1 => "b"})
# "{\"0\":\"a\",\"1\":\"b\"}"

# Bang variant
json = Torque.encode!(%{id: "abc"})

# iodata variant (fastest, no {:ok, ...} tuple wrapping)
json = Torque.encode_to_iodata(%{id: "abc"})

# jiffy-compatible proplist format
{:ok, json} = Torque.encode({[{:id, "abc"}, {:price, 1.5}]})
```

Structs are rejected with `{:error, :unhandled_struct}` unless they implement
`Torque.Encoder`. Implement the protocol for custom types, or derive it to
encode a subset of fields:

```elixir
defimpl Torque.Encoder, for: Decimal do
  def encode(decimal), do: Decimal.to_string(decimal)
end

# or, on the struct itself:
@derive {Torque.Encoder, only: [:id, :name]}
defstruct [:id, :name, :secret]
```

`Date`, `Time`, `NaiveDateTime`, and `DateTime` ship with implementations and
encode as ISO 8601 strings.

> **Breaking change in 0.4.0.** Structs previously encoded as raw maps, leaking
> the struct marker into the output: `~D[2026-09-14]` produced
> `{"calendar":"Elixir.Calendar.ISO","month":9,"__struct__":"Elixir.Date",...}`.
> They now error unless the protocol is implemented.

Unlike decoding, encoding cannot cheaply predict its output size, so dirty
scheduler dispatch is opt-in. Pass `dirty: true` (accepted by `encode/2`,
`encode!/2`, `encode_to_iodata/2`, and `encode_to_iodata!/2`) when terms are
expected to encode to large output (more than roughly 20 KB):

```elixir
{:ok, json} = Torque.encode(big_term, dirty: true)
```

## API

| Function | Description |
|----------|-------------|
| `Torque.compile_pointers(paths, opts)` | Pre-compile a fixed path set into a reusable handle |
| `Torque.decode(binary)` | Decode JSON to Elixir terms |
| `Torque.decode!(binary)` | Decode JSON, raising on error |
| `Torque.encode(term, opts)` | Encode term to JSON binary |
| `Torque.encode!(term, opts)` | Encode term, raising on error |
| `Torque.encode_to_iodata(term, opts)` | Encode term, returns binary directly (fastest) |
| `Torque.encode_to_iodata!(term, opts)` | Alias for `encode_to_iodata/2` (Phoenix `:json_library`) |
| `Torque.get(doc, path)` | Extract field by JSON Pointer path |
| `Torque.get(doc, path, default)` | Extract field with default for missing paths |
| `Torque.get_many(doc, paths)` | Extract multiple fields in one NIF call |
| `Torque.get_many_nil(doc, paths)` | Extract multiple fields, `nil` for missing |
| `Torque.length(doc, path)` | Return length of array at path |
| `Torque.parse(binary, opts)` | Parse JSON into opaque document reference |
| `Torque.parse_get_many_nil(binary, pointers)` | Fused parse + extract of compiled pointers in one NIF call |

## Type Conversion

### JSON to Elixir

| JSON | Elixir |
|------|--------|
| object | map (binary keys) |
| array | list |
| string | binary |
| integer | integer |
| float | float |
| `true`, `false` | `true`, `false` |
| `null` | `nil` |

For objects with duplicate keys, the last value wins (unless `unique_keys: true` is passed to `parse/2`).

Integers outside the signed/unsigned 64-bit range decode as exact arbitrary-precision integers (Erlang bignums) via `decode/1`, rather than degrading to lossy floats. The `parse/2` + `get/2` path returns them as floats, since the parsed document cannot hold a bignum.

### Elixir to JSON

| Elixir | JSON |
|--------|------|
| map (atom/binary/integer keys) | object |
| list | array |
| binary | string |
| integer | number |
| float | number |
| `true`, `false` | `true`, `false` |
| `nil` | `null` |
| atom | string |
| `{keyword_list}` | object |
| struct implementing `Torque.Encoder` | whatever `encode/1` returns |

## Errors

Functions return `{:error, reason}` tuples (or raise `ArgumentError` for bang/iodata variants). Possible `reason` atoms:

### Decode / Parse

| Atom | Returned by | Meaning |
|------|-------------|---------|
| `:nesting_too_deep` | `decode/1`, `parse/1`, `get/2`, `get_many/2`, `parse_get_many_nil/2` | Document exceeds 128 nesting levels |

`parse/1`, `decode/1`, and `parse_get_many_nil/2` also return `{:error, binary}` with a message from sonic-rs for malformed JSON.

### Encode

| Atom | Returned by | Meaning |
|------|-------------|---------|
| `:unsupported_type` | `encode/1` | Term has no JSON representation (PID, reference, port, …) |
| `:invalid_utf8` | `encode/1` | Binary string or map key is not valid UTF-8 |
| `:invalid_key` | `encode/1` | Map key is not an atom, binary, or integer (e.g. float or tuple key) |
| `:malformed_proplist` | `encode/1` | `{proplist}` contains a non-`{key, value}` element |
| `:non_finite_float` | `encode/1` | Float is infinity or NaN (unreachable from normal BEAM code) |
| `:nesting_too_deep` | `encode/1` | Term exceeds 128 nesting levels |
| `:unhandled_struct` | `encode/1` | Struct has no `Torque.Encoder` implementation |
| `:encoder_expansion_too_deep` | `encode/1` | A `Torque.Encoder` implementation expands the same struct again, or structs nest past 128 levels |

## Benchmarks

Per-commit trends and the full cross-library comparison are published at
[lpgauth.github.io/torque/dev/bench](https://lpgauth.github.io/torque/dev/bench/).

Every table below comes from one run of `bench/torque_bench.exs`, against
glazer 1.1.5, on each of two machines:

- **arm64**: Apple M1 Pro, macOS, OTP 29, Elixir 1.20.3, Apple clang 21, rustc 1.98.1
- **x86_64**: Intel Xeon E5-2630 v3 (Haswell, 2.4 GHz), Ubuntu 22.04 container pinned to one core of a shared server, OTP 29, Elixir 1.20.2, GCC 13.4, rustc 1.99.0

Both libraries are profile-guided optimised (PGO) builds on both machines:
**Torque PGO** (via `scripts/pgo-build.sh`, which builds with
`-C target-cpu=native`) and **Glazer PGO** (via
`make -C deps/glazer/c_src PGO=generate`, the workload in
`bench/glazer_pgo_workload.exs`, then `PGO=use`). Glazer's Makefile writes that
flow for GCC; under clang the raw counters need an explicit
`llvm-profdata merge -o obj/pgo/default.profdata obj/pgo/*.profraw` between
those two steps.

glazer is benchmarked with UTF-8 validation enabled (`validate_utf8` on
decode, `force_utf8` on encode — both off by default in glazer) so every
library provides the same guarantee Torque always does: JSON strings are
valid UTF-8.

### Decode (1.2 KB OpenRTB)

**arm64**

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** | **400.0K** | **2.50 μs** | **2.38 μs** | **2.88 μs** | 1.56 KB |
| **glazer** | 349.1K | 2.86 μs | 2.75 μs | 4.04 μs | 1.56 KB |
| **jiffy** | 202.4K | 4.94 μs | 4.58 μs | 8.75 μs | **1.55 KB** |
| **otp json** | 137.8K | 7.25 μs | 7.00 μs | 12.17 μs | 7.73 KB |
| **jason** | 103.2K | 9.69 μs | 9.17 μs | 16.04 μs | 9.54 KB |

**x86_64**

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** | **173.0K** | **5.78 μs** | **5.54 μs** | **9.07 μs** | 1.56 KB |
| **glazer** | 170.3K | 5.87 μs | 5.62 μs | 9.38 μs | 1.56 KB |
| **jiffy** | 90.2K | 11.09 μs | 9.61 μs | 21.53 μs | **1.55 KB** |
| **otp json** | 60.5K | 16.53 μs | 16.12 μs | 25.60 μs | 7.73 KB |
| **jason** | 52.4K | 19.09 μs | 18.42 μs | 29.91 μs | 9.46 KB |

### Decode (750 KB Twitter)

**arm64**

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** | **685.2** | **1.46 ms** | **1.34 ms** | **1.91 ms** | **1.57 KB** |
| **glazer** | 577.3 | 1.73 ms | 1.65 ms | 2.15 ms | 1.58 KB |
| **jiffy** | 294.8 | 3.39 ms | 3.49 ms | 3.85 ms | 2.30 MB |
| **otp json** | 202.2 | 4.94 ms | 4.94 ms | 5.97 ms | 2.48 MB |
| **jason** | 122.1 | 8.19 ms | 8.17 ms | 8.74 ms | 3.52 MB |

**x86_64**

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** | **347.1** | **2.88 ms** | **2.93 ms** | 3.77 ms | **1.57 KB** |
| **glazer** | 337.8 | 2.96 ms | 3.00 ms | **3.76 ms** | 1.58 KB |
| **jiffy** | 132.8 | 7.53 ms | 8.29 ms | 8.81 ms | 2.30 MB |
| **otp json** | 88.3 | 11.32 ms | 11.40 ms | 14.63 ms | 2.48 MB |
| **jason** | 71.9 | 13.90 ms | 13.77 ms | 15.79 ms | 3.54 MB |

### Encode (1.2 KB OpenRTB)

**arm64**

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: binary()] | **1460K** | **0.68 μs** | **0.63 μs** | **0.75 μs** | 88 B |
| **torque** [proplist() :: iodata()] | 1430K | 0.70 μs | **0.63 μs** | 0.79 μs | **64 B** |
| **torque** [map() :: binary()] | 1390K | 0.72 μs | 0.67 μs | 0.79 μs | 88 B |
| **torque** [map() :: iodata()] | 1370K | 0.73 μs | 0.67 μs | **0.75 μs** | **64 B** |
| **otp json** [map() :: iodata()] | 1110K | 0.90 μs | 0.83 μs | 1.17 μs | 3.84 KB |
| **glazer** [map() :: binary()] | 1010K | 0.99 μs | 0.83 μs | 1.04 μs | **64 B** |
| **jiffy** [proplist() :: iodata()] | 830K | 1.20 μs | 1.04 μs | 1.29 μs | 120 B |
| **jiffy** [map() :: iodata()] | 670K | 1.50 μs | 1.33 μs | 1.63 μs | 632 B |
| **jason** [map() :: iodata()] | 580K | 1.72 μs | 1.63 μs | 2.67 μs | 3.76 KB |
| **jason** [map() :: binary()] | 380K | 2.65 μs | 2.50 μs | 4.50 μs | 3.82 KB |

**x86_64**

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: iodata()] | **709.8K** | **1.41 μs** | **1.26 μs** | **1.64 μs** | **64 B** |
| **torque** [proplist() :: binary()] | 708.0K | **1.41 μs** | **1.26 μs** | 1.67 μs | 88 B |
| **torque** [map() :: iodata()] | 624.9K | 1.60 μs | 1.46 μs | 1.76 μs | **64 B** |
| **torque** [map() :: binary()] | 615.4K | 1.63 μs | 1.49 μs | 1.77 μs | 88 B |
| **glazer** [map() :: binary()] | 479.1K | 2.09 μs | 1.92 μs | 2.63 μs | **64 B** |
| **otp json** [map() :: iodata()] | 429.8K | 2.33 μs | 2.05 μs | 3.32 μs | 3.84 KB |
| **jiffy** [proplist() :: iodata()] | 414.4K | 2.41 μs | 2.03 μs | 3.03 μs | 120 B |
| **jiffy** [map() :: iodata()] | 344.2K | 2.91 μs | 2.55 μs | 3.51 μs | 632 B |
| **jason** [map() :: iodata()] | 221.9K | 4.51 μs | 3.43 μs | 6.73 μs | 3.76 KB |
| **jason** [map() :: binary()] | 184.8K | 5.41 μs | 4.92 μs | 9.77 μs | 3.82 KB |

### Encode (750 KB Twitter)

**arm64**

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: iodata()] | **1640.2** | **0.61 ms** | **0.60 ms** | **0.72 ms** | **64 B** |
| **torque** [proplist() :: binary()] | 1629.3 | **0.61 ms** | **0.60 ms** | 0.75 ms | 88 B |
| **torque** [map() :: iodata()] | 1511.6 | 0.66 ms | 0.65 ms | 0.76 ms | **64 B** |
| **torque** [map() :: binary()] | 1509.7 | 0.66 ms | 0.65 ms | 0.78 ms | 88 B |
| **glazer** [map() :: binary()] | 857.7 | 1.17 ms | 1.16 ms | 1.36 ms | **64 B** |
| **jiffy** [proplist() :: iodata()] | 614.5 | 1.63 ms | 1.62 ms | 1.85 ms | 2.97 KB |
| **jiffy** [map() :: iodata()] | 504.0 | 1.98 ms | 1.97 ms | 2.15 ms | 803.19 KB |
| **otp json** [map() :: iodata()] | 263.6 | 3.79 ms | 3.81 ms | 5.04 ms | 5.40 MB |
| **jason** [map() :: iodata()] | 248.4 | 4.03 ms | 3.74 ms | 6.28 ms | 4.96 MB |
| **jason** [map() :: binary()] | 132.8 | 7.53 ms | 7.46 ms | 8.58 ms | 4.96 MB |

**x86_64**

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: binary()] | **772.9** | **1.29 ms** | **1.35 ms** | **2.32 ms** | 88 B |
| **torque** [proplist() :: iodata()] | 772.6 | **1.29 ms** | **1.35 ms** | 2.36 ms | **64 B** |
| **torque** [map() :: iodata()] | 641.6 | 1.56 ms | 1.61 ms | 2.60 ms | **64 B** |
| **torque** [map() :: binary()] | 606.8 | 1.65 ms | 1.70 ms | 2.72 ms | 88 B |
| **jiffy** [proplist() :: iodata()] | 377.8 | 2.65 ms | 2.54 ms | 3.33 ms | 2.97 KB |
| **glazer** [map() :: binary()] | 367.8 | 2.72 ms | 2.78 ms | 4.07 ms | **64 B** |
| **jiffy** [map() :: iodata()] | 257.0 | 3.89 ms | 3.70 ms | 5.37 ms | 803.19 KB |
| **otp json** [map() :: iodata()] | 107.6 | 9.30 ms | 9.59 ms | 11.95 ms | 5.40 MB |
| **jason** [map() :: iodata()] | 73.7 | 13.56 ms | 13.57 ms | 15.73 ms | 4.96 MB |
| **jason** [map() :: binary()] | 58.2 | 17.17 ms | 17.25 ms | 19.61 ms | 4.96 MB |

### Parse (1.2 KB OpenRTB)

**arm64**

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse | **633.7K** | **1.58 μs** | **1.33 μs** | **3.08 μs** |
| **torque** parse(unique_keys) | 593.1K | 1.69 μs | 1.38 μs | **3.08 μs** |

**x86_64**

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse(unique_keys) | **220.9K** | **4.53 μs** | 3.66 μs | **7.35 μs** |
| **torque** parse | 219.6K | 4.55 μs | **3.58 μs** | 13.27 μs |

### Extract 5 fields from raw JSON (1.2 KB OpenRTB)

End-to-end cost of pulling 5 fields out of a JSON blob: `parse` + `get`
(torque) vs `decode` + `find` (glazer has no lazy handle, so it must
fully decode first). This is the apples-to-apples version of "get" — torque's
selective extraction skips materializing the whole document.

`parse_get_many_nil` goes further. Given a handle compiled once at startup
(like glazer's compiled jq paths), it walks the document a single time and
builds a value only where a path ends, so no document is built at all.
`validate: false` also skips validating the regions no path selects, which on
a document this small is most of what is left.

**arm64**

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse_get_many_nil unique_keys validate: false | **1379K** | **0.73 μs** | **0.71 μs** | **0.83 μs** |
| **torque** parse_get_many_nil unique_keys | 723.8K | 1.38 μs | 1.33 μs | 1.50 μs |
| **torque** parse_get_many_nil | 694.5K | 1.44 μs | 1.33 μs | 1.67 μs |
| **torque** parse(unique_keys) + get_many | 504.8K | 1.98 μs | 1.75 μs | 3.58 μs |
| **torque** parse + get x5 | 482.6K | 2.07 μs | 1.83 μs | 4.00 μs |
| **torque** parse + get_many | 482.4K | 2.07 μs | 1.71 μs | 3.33 μs |
| **glazer** decode + find x5 | 315.7K | 3.17 μs | 3.08 μs | 4.33 μs |

**x86_64**

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse_get_many_nil unique_keys validate: false | **933.0K** | **1.07 μs** | **0.98 μs** | **1.63 μs** |
| **torque** parse_get_many_nil unique_keys | 533.3K | 1.87 μs | 1.77 μs | 2.44 μs |
| **torque** parse_get_many_nil | 533.1K | 1.88 μs | 1.78 μs | 2.44 μs |
| **torque** parse(unique_keys) + get_many | 187.3K | 5.34 μs | 4.05 μs | 14.88 μs |
| **torque** parse + get x5 | 181.1K | 5.52 μs | 4.85 μs | 12.84 μs |
| **torque** parse + get_many | 171.8K | 5.82 μs | 4.80 μs | 10.15 μs |
| **glazer** decode + find x5 | 152.3K | 6.57 μs | 6.31 μs | 11.41 μs |

Run benchmarks locally:

```bash
MIX_ENV=bench mix run bench/torque_bench.exs
```

## Limitations

- **Integer map keys are lossy**: JSON object names must be strings (RFC 8259 §4), so `encode/1` stringifies integer keys and `decode/1` gives them back as binaries — `%{1 => "a"}` round-trips to `%{"1" => "a"}`. A map mixing both forms, like `%{1 => "a", "1" => "b"}`, encodes to duplicate names (`{"1":"a","1":"b"}`); RFC 8259 says names *should* be unique, and decoders resolve the collision however they choose. Jason behaves identically.
- **Nesting depth**: JSON documents nested deeper than 128 levels return `{:error, :nesting_too_deep}` from `decode/1`, `parse/1`, `get/2`, `get_many/2`, and `encode/1` rather than crashing the VM. Real-world documents are never this deep; the limit exists to prevent stack overflow in the NIF (the dirty CPU scheduler, used for inputs over 20 KB, has a small stack).

## License

MIT

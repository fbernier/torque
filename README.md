# Torque

High-performance JSON library for Elixir via [Rustler](https://github.com/rustler-magic/rustler) NIFs, powered by [sonic-rs](https://github.com/cloudwego/sonic-rs) (SIMD-accelerated).

Torque provides the fastest JSON encoding and decoding available in the BEAM ecosystem, with a selective field extraction API for workloads that only need a subset of fields from each document.

## Features

- SIMD-accelerated decoding (AVX2 on x86, NEON on ARM)
- Ultra-low memory encoder (64 B per encode vs ~4 KB for OTP `json`/jason)
- Parse-then-get API for selective field extraction via JSON Pointer (RFC 6901)
- Batch field extraction (`get_many/2`) with single NIF call
- Pre-compiled pointers with fused parse + extract (`parse_get_many_nil/2`)
- Automatic dirty CPU scheduler dispatch for inputs larger than 20 KB
- jiffy-compatible `{proplist}` encoding

## Installation

Add to your `mix.exs`:

```elixir
def deps do
  [
    {:torque, "~> 0.2.4"}
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
| v3 | AVX2, AVX, BMI1, BMI2, FMA | `x86-64-v3` |

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
pointers once and reuse the handle. `parse_get_many_nil/2` then fuses the parse
and extraction into a single NIF call, skipping all per-request path parsing —
roughly 1.5× faster end-to-end than `parse/2` + `get_many_nil/2`.

```elixir
# Once, at startup (e.g. a module attribute or :persistent_term):
pointers = Torque.compile_pointers(["/id", "/site/domain", "/imp/0/banner/w"], unique_keys: true)

# Per document — parse + extract in one call:
{:ok, ["req-1", "example.com", 300]} = Torque.parse_get_many_nil(json, pointers)
```

Missing fields and JSON `null` both become `nil`. The handle also works with an
already-parsed document via `Torque.get_many_nil(doc, pointers)`.

### Encoding

```elixir
# Maps with atom or binary keys
{:ok, json} = Torque.encode(%{id: "abc", price: 1.5})
# "{\"id\":\"abc\",\"price\":1.5}"

# Bang variant
json = Torque.encode!(%{id: "abc"})

# iodata variant (fastest, no {:ok, ...} tuple wrapping)
json = Torque.encode_to_iodata(%{id: "abc"})

# jiffy-compatible proplist format
{:ok, json} = Torque.encode({[{:id, "abc"}, {:price, 1.5}]})
```

## API

| Function | Description |
|----------|-------------|
| `Torque.compile_pointers(paths, opts)` | Pre-compile a fixed path set into a reusable handle |
| `Torque.decode(binary)` | Decode JSON to Elixir terms |
| `Torque.decode!(binary)` | Decode JSON, raising on error |
| `Torque.encode(term)` | Encode term to JSON binary |
| `Torque.encode!(term)` | Encode term, raising on error |
| `Torque.encode_to_iodata(term)` | Encode term, returns binary directly (fastest) |
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
| map (atom/binary keys) | object |
| list | array |
| binary | string |
| integer | number |
| float | number |
| `true`, `false` | `true`, `false` |
| `nil` | `null` |
| atom | string |
| `{keyword_list}` | object |

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
| `:invalid_key` | `encode/1` | Map key is not an atom or binary (e.g. integer key) |
| `:malformed_proplist` | `encode/1` | `{proplist}` contains a non-`{key, value}` element |
| `:non_finite_float` | `encode/1` | Float is infinity or NaN (unreachable from normal BEAM code) |
| `:nesting_too_deep` | `encode/1` | Term exceeds 128 nesting levels |

## Benchmarks

Apple M2 Pro, OTP 29, Elixir 1.20. Both libraries are profile-guided
optimised (PGO) builds: **Torque PGO** (via `scripts/pgo-build.sh`) and
**Glazer PGO** (via `OPTIMIZE=1`).

### Decode (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** | **413.3K** | **2.42 μs** | **2.29 μs** | **4.04 μs** | 1.56 KB |
| **glazer** | 401.8K | 2.49 μs | 2.38 μs | 4.58 μs | 1.56 KB |
| **jiffy** | 202.5K | 4.94 μs | 4.50 μs | 9.75 μs | **1.55 KB** |
| **simdjsone** | 178.2K | 5.61 μs | 5.25 μs | 12.63 μs | 1.59 KB |
| **otp json** | 147.4K | 6.78 μs | 6.58 μs | 10.58 μs | 7.73 KB |
| **jason** | 113.2K | 8.83 μs | 8.46 μs | 13.96 μs | 9.54 KB |

### Decode (750 KB Twitter)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** | **660.7** | **1.51 ms** | **1.33 ms** | 2.21 ms | **1.57 KB** |
| **glazer** | 659.6 | 1.52 ms | 1.43 ms | **1.94 ms** | 1.58 KB |
| **simdjsone** | 413.6 | 2.42 ms | 1.93 ms | 3.75 ms | 1.59 KB |
| **jiffy** | 301.4 | 3.32 ms | 3.39 ms | 3.96 ms | 2.30 MB |
| **otp json** | 205.3 | 4.87 ms | 4.80 ms | 7.52 ms | 2.48 MB |
| **jason** | 151.7 | 6.59 ms | 6.58 ms | 6.97 ms | 3.52 MB |

### Encode (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **otp json** [map() :: iodata()] | **1180K** | **0.85 μs** | **0.79 μs** | **1.08 μs** | 3928 B |
| **torque** [proplist() :: binary()] | 1110K | 0.90 μs | 0.83 μs | **1.08 μs** | 88 B |
| **torque** [proplist() :: iodata()] | 1080K | 0.92 μs | 0.83 μs | 1.83 μs | **64 B** |
| **torque** [map() :: iodata()] | 1000K | 1.00 μs | 0.96 μs | 1.13 μs | **64 B** |
| **glazer** [map() :: binary()] | 990K | 1.01 μs | 0.92 μs | 1.29 μs | **64 B** |
| **torque** [map() :: binary()] | 980K | 1.02 μs | 0.96 μs | 1.17 μs | 88 B |
| **jiffy** [proplist() :: iodata()] | 640K | 1.57 μs | 1.33 μs | 1.83 μs | 120 B |
| **jason** [map() :: iodata()] | 620K | 1.62 μs | 1.54 μs | 2.63 μs | 3848 B |
| **jiffy** [map() :: iodata()] | 520K | 1.91 μs | 1.75 μs | 2.25 μs | 824 B |
| **simdjsone** [proplist() :: iodata()] | 410K | 2.42 μs | 2.29 μs | 3.00 μs | 184 B |
| **jason** [map() :: binary()] | 380K | 2.63 μs | 2.38 μs | 6.42 μs | 3912 B |
| **simdjsone** [map() :: iodata()] | 340K | 2.94 μs | 2.75 μs | 4.38 μs | 888 B |

### Encode (750 KB Twitter)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: iodata()] | **1156.0** | **0.87 ms** | **0.85 ms** | **1.06 ms** | **64 B** |
| **torque** [proplist() :: binary()] | 1154.1 | **0.87 ms** | **0.85 ms** | **1.06 ms** | 88 B |
| **torque** [map() :: binary()] | 1045.4 | 0.96 ms | 0.95 ms | 1.13 ms | 88 B |
| **torque** [map() :: iodata()] | 1040.4 | 0.96 ms | 0.96 ms | 1.13 ms | **64 B** |
| **glazer** [map() :: binary()] | 1033.3 | 0.97 ms | 0.96 ms | 1.08 ms | **64 B** |
| **jiffy** [proplist() :: iodata()] | 469.5 | 2.13 ms | 2.10 ms | 2.51 ms | 37.7 KB |
| **jiffy** [map() :: iodata()] | 347.6 | 2.88 ms | 2.96 ms | 3.90 ms | 1.06 MB |
| **simdjsone** [proplist() :: iodata()] | 254.7 | 3.93 ms | 3.87 ms | 5.44 ms | 37.7 KB |
| **otp json** [map() :: iodata()] | 251.8 | 3.97 ms | 4.13 ms | 6.44 ms | 5.40 MB |
| **jason** [map() :: iodata()] | 233.5 | 4.28 ms | 4.03 ms | 6.50 ms | 4.96 MB |
| **simdjsone** [map() :: iodata()] | 215.1 | 4.65 ms | 4.76 ms | 5.22 ms | 1.06 MB |
| **jason** [map() :: binary()] | 127.9 | 7.82 ms | 7.84 ms | 8.43 ms | 4.96 MB |

### Parse (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse(unique_keys) | **542.0K** | **1.84 μs** | 1.46 μs | **5.63 μs** |
| **torque** parse | 522.8K | 1.91 μs | 1.46 μs | 6.00 μs |
| **simdjsone** parse | 306.3K | 3.27 μs | **1.21 μs** | 5.83 μs |

### Extract 5 fields from raw JSON (1.2 KB OpenRTB)

End-to-end cost of pulling 5 fields out of a JSON blob: `parse` + `get`
(torque, simdjsone) vs `decode` + `find` (glazer has no lazy handle, so it must
fully decode first). This is the apples-to-apples version of "get" — torque's
selective extraction skips materializing the whole document.

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse + get_many | **436.6K** | **2.29 μs** | **1.79 μs** | **6.21 μs** |
| **torque** parse(unique_keys) + get_many | 429.5K | 2.33 μs | **1.79 μs** | 6.67 μs |
| **torque** parse + get x5 | 409.8K | 2.44 μs | 2.00 μs | 6.67 μs |
| **simdjsone** parse + get x5 | 374.9K | 2.67 μs | **1.79 μs** | 7.21 μs |
| **glazer** decode + find x5 | 344.9K | 2.90 μs | 2.75 μs | 6.71 μs |

Run benchmarks locally:

```bash
MIX_ENV=bench mix run bench/torque_bench.exs
```

## Limitations

- **Nesting depth**: JSON documents nested deeper than 128 levels return `{:error, :nesting_too_deep}` from `decode/1`, `parse/1`, `get/2`, `get_many/2`, and `encode/1` rather than crashing the VM. Real-world documents are never this deep; the limit exists to prevent stack overflow in the NIF (the dirty CPU scheduler, used for inputs over 20 KB, has a small stack).

## License

MIT

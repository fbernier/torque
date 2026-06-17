# Torque

High-performance JSON library for Elixir via [Rustler](https://github.com/rustler-magic/rustler) NIFs, powered by [sonic-rs](https://github.com/cloudwego/sonic-rs) (SIMD-accelerated).

Torque provides the fastest JSON encoding and decoding available in the BEAM ecosystem, with a selective field extraction API for workloads that only need a subset of fields from each document.

## Features

- SIMD-accelerated decoding (AVX2/SSE4.2 on x86, NEON on ARM)
- Ultra-low memory encoder (64 B per encode vs ~4 KB for OTP `json`/jason)
- Parse-then-get API for selective field extraction via JSON Pointer (RFC 6901)
- Batch field extraction (`get_many/2`) with single NIF call
- Automatic dirty CPU scheduler dispatch for inputs larger than 20 KB
- jiffy-compatible `{proplist}` encoding

## Installation

Add to your `mix.exs`:

```elixir
def deps do
  [
    {:torque, "~> 0.2.3"}
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
| `Torque.decode(binary)` | Decode JSON to Elixir terms |
| `Torque.decode!(binary)` | Decode JSON, raising on error |
| `Torque.parse(binary, opts)` | Parse JSON into opaque document reference |
| `Torque.get(doc, path)` | Extract field by JSON Pointer path |
| `Torque.get(doc, path, default)` | Extract field with default for missing paths |
| `Torque.get_many(doc, paths)` | Extract multiple fields in one NIF call |
| `Torque.get_many_nil(doc, paths)` | Extract multiple fields, `nil` for missing |
| `Torque.length(doc, path)` | Return length of array at path |
| `Torque.encode(term)` | Encode term to JSON binary |
| `Torque.encode!(term)` | Encode term, raising on error |
| `Torque.encode_to_iodata(term)` | Encode term, returns binary directly (fastest) |

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
| `:nesting_too_deep` | `decode/1`, `parse/1`, `get/2`, `get_many/2` | Document exceeds 128 nesting levels |

`parse/1` and `decode/1` also return `{:error, binary}` with a message from sonic-rs for malformed JSON.

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
| **torque** | **404.0K** | **2.48 μs** | **2.29 μs** | **5.75 μs** | 1.56 KB |
| **glazer** | 378.7K | 2.64 μs | 2.38 μs | 7.08 μs | 1.56 KB |
| **jiffy** | 201.3K | 4.97 μs | 4.25 μs | 13.21 μs | **1.55 KB** |
| **simdjsone** | 180.8K | 5.53 μs | 5.21 μs | 12.50 μs | 1.59 KB |
| **otp json** | 140.6K | 7.11 μs | 6.71 μs | 15.38 μs | 7.73 KB |
| **jason** | 109.4K | 9.14 μs | 8.50 μs | 18.83 μs | 9.54 KB |

### Decode (750 KB Twitter)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** | **648.3** | **1.54 ms** | **1.34 ms** | **2.12 ms** | **1.57 KB** |
| **glazer** | 587.5 | 1.70 ms | 1.56 ms | 2.24 ms | 1.58 KB |
| **simdjsone** | 437.9 | 2.28 ms | 1.95 ms | 3.37 ms | 1.59 KB |
| **jiffy** | 278.5 | 3.59 ms | 3.69 ms | 4.00 ms | 2.30 MB |
| **otp json** | 201.3 | 4.97 ms | 5.02 ms | 5.68 ms | 2.48 MB |
| **jason** | 140.3 | 7.13 ms | 7.13 ms | 7.65 ms | 3.54 MB |

### Encode (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: iodata()] | **1360K** | **0.74 μs** | **0.67 μs** | **0.92 μs** | **64 B** |
| **torque** [proplist() :: binary()] | 1340K | 0.75 μs | **0.67 μs** | **0.92 μs** | 88 B |
| **glazer** [map() :: binary()] | 1230K | 0.81 μs | 0.75 μs | 1.00 μs | **64 B** |
| **otp json** [map() :: iodata()] | 1190K | 0.84 μs | 0.79 μs | 1.21 μs | 3928 B |
| **torque** [map() :: iodata()] | 1160K | 0.86 μs | 0.79 μs | 1.04 μs | **64 B** |
| **torque** [map() :: binary()] | 1160K | 0.86 μs | 0.79 μs | 1.04 μs | 88 B |
| **jiffy** [proplist() :: iodata()] | 720K | 1.39 μs | 1.17 μs | 1.79 μs | 120 B |
| **jiffy** [map() :: iodata()] | 610K | 1.65 μs | 1.42 μs | 3.58 μs | 824 B |
| **jason** [map() :: iodata()] | 600K | 1.68 μs | 1.50 μs | 4.21 μs | 3848 B |
| **simdjsone** [proplist() :: iodata()] | 450K | 2.22 μs | 2.04 μs | 2.71 μs | 184 B |
| **jason** [map() :: binary()] | 390K | 2.59 μs | 2.38 μs | 6.42 μs | 3912 B |
| **simdjsone** [map() :: iodata()] | 380K | 2.62 μs | 2.42 μs | 3.33 μs | 888 B |

### Encode (750 KB Twitter)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: binary()] | **1424.6** | **0.70 ms** | **0.68 ms** | **0.84 ms** | 88 B |
| **torque** [proplist() :: iodata()] | 1416.7 | 0.71 ms | **0.68 ms** | 1.11 ms | **64 B** |
| **torque** [map() :: binary()] | 1211.6 | 0.83 ms | 0.81 ms | 1.00 ms | 88 B |
| **torque** [map() :: iodata()] | 1208.4 | 0.83 ms | 0.81 ms | 0.99 ms | **64 B** |
| **glazer** [map() :: binary()] | 1082.5 | 0.92 ms | 0.91 ms | 1.06 ms | **64 B** |
| **jiffy** [proplist() :: iodata()] | 537.1 | 1.86 ms | 1.84 ms | 2.11 ms | 37.7 KB |
| **jiffy** [map() :: iodata()] | 409.0 | 2.45 ms | 2.33 ms | 2.99 ms | 1.06 MB |
| **simdjsone** [proplist() :: iodata()] | 259.1 | 3.86 ms | 3.81 ms | 5.68 ms | 37.7 KB |
| **jason** [map() :: iodata()] | 258.9 | 3.86 ms | 3.62 ms | 5.88 ms | 4.96 MB |
| **otp json** [map() :: iodata()] | 252.7 | 3.96 ms | 4.14 ms | 6.63 ms | 5.40 MB |
| **simdjsone** [map() :: iodata()] | 229.7 | 4.35 ms | 4.34 ms | 4.67 ms | 1.06 MB |
| **jason** [map() :: binary()] | 130.4 | 7.67 ms | 7.70 ms | 8.35 ms | 4.96 MB |

### Parse (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse(unique_keys) | **572.6K** | **1.75 μs** | 1.46 μs | **4.83 μs** |
| **torque** parse | 510.3K | 1.96 μs | 1.46 μs | 6.29 μs |
| **simdjsone** parse | 304.0K | 3.29 μs | **1.21 μs** | 5.33 μs |

### Extract 5 fields from raw JSON (1.2 KB OpenRTB)

End-to-end cost of pulling 5 fields out of a JSON blob: `parse` + `get`
(torque, simdjsone) vs `decode` + `find` (glazer has no lazy handle, so it must
fully decode first). This is the apples-to-apples version of "get" — torque's
selective extraction skips materializing the whole document.

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse + get_many | **476.1K** | **2.10 μs** | 1.75 μs | **4.75 μs** |
| **torque** parse + get x5 | 434.6K | 2.30 μs | 1.92 μs | 5.96 μs |
| **torque** parse(unique_keys) + get_many | 417.8K | 2.39 μs | 1.75 μs | 8.08 μs |
| **simdjsone** parse + get x5 | 415.8K | 2.41 μs | **1.67 μs** | 6.50 μs |
| **glazer** decode + find x5 | 317.7K | 3.15 μs | 2.79 μs | 8.54 μs |

Run benchmarks locally:

```bash
MIX_ENV=bench mix run bench/torque_bench.exs
```

## Limitations

- **Nesting depth**: JSON documents nested deeper than 128 levels return `{:error, :nesting_too_deep}` from `decode/1`, `parse/1`, `get/2`, `get_many/2`, and `encode/1` rather than crashing the VM. Real-world documents are never this deep; the limit exists to prevent stack overflow in the NIF (the dirty CPU scheduler, used for inputs over 20 KB, has a small stack).

## License

MIT

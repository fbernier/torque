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
    {:torque, "~> 0.2.1"}
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

Apple M2 Pro, OTP 29, Elixir 1.20:

### Decode (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** | **392.8K** | **2.55 μs** | **2.46 μs** | **4.25 μs** | 1.56 KB |
| **glazer** | 298.9K | 3.35 μs | 3.00 μs | 8.92 μs | 1.56 KB |
| **jiffy** | 208.8K | 4.79 μs | 4.33 μs | 10.13 μs | **1.55 KB** |
| **simdjsone** | 181.6K | 5.51 μs | 5.29 μs | 9.88 μs | 1.59 KB |
| **otp json** | 125.2K | 7.99 μs | 7.71 μs | 13.08 μs | 7.73 KB |
| **jason** | 92.6K | 10.80 μs | 10.13 μs | 22.33 μs | 9.54 KB |

### Decode (750 KB Twitter)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** | **655.1** | **1.53 ms** | **1.32 ms** | **2.16 ms** | **1.57 KB** |
| **glazer** | 590.8 | 1.69 ms | 1.53 ms | 2.56 ms | 1.58 KB |
| **simdjsone** | 392.5 | 2.55 ms | 2.10 ms | 4.53 ms | 1.59 KB |
| **jiffy** | 286.3 | 3.49 ms | 3.59 ms | 3.92 ms | 2.30 MB |
| **otp json** | 165.2 | 6.05 ms | 5.42 ms | 16.57 ms | 2.48 MB |
| **jason** | 142.1 | 7.04 ms | 7.01 ms | 8.15 ms | 3.54 MB |

### Encode (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: iodata()] | **1280K** | **0.78 μs** | **0.71 μs** | **0.88 μs** | **64 B** |
| **torque** [proplist() :: binary()] | 1190K | 0.84 μs | 0.75 μs | 1.63 μs | 88 B |
| **glazer** [map() :: binary()] | 1090K | 0.92 μs | 0.83 μs | 1.13 μs | **64 B** |
| **otp json** [map() :: iodata()] | 1080K | 0.92 μs | 0.79 μs | 2.04 μs | 3928 B |
| **torque** [map() :: iodata()] | 1050K | 0.95 μs | 0.88 μs | 1.13 μs | **64 B** |
| **torque** [map() :: binary()] | 1040K | 0.96 μs | 0.88 μs | 1.13 μs | 88 B |
| **jiffy** [proplist() :: iodata()] | 730K | 1.37 μs | 1.13 μs | 2.33 μs | 120 B |
| **jiffy** [map() :: iodata()] | 620K | 1.62 μs | 1.42 μs | 2.04 μs | 824 B |
| **jason** [map() :: iodata()] | 610K | 1.63 μs | 1.50 μs | 2.88 μs | 3848 B |
| **simdjsone** [proplist() :: iodata()] | 440K | 2.25 μs | 2.04 μs | 2.88 μs | 184 B |
| **jason** [map() :: binary()] | 380K | 2.66 μs | 2.38 μs | 6.29 μs | 3912 B |
| **simdjsone** [map() :: iodata()] | 370K | 2.69 μs | 2.42 μs | 4.79 μs | 888 B |

### Encode (750 KB Twitter)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: iodata()] | **1228.7** | **0.81 ms** | **0.80 ms** | 1.00 ms | **64 B** |
| **torque** [proplist() :: binary()] | 1226.6 | 0.82 ms | **0.80 ms** | **0.99 ms** | 88 B |
| **torque** [map() :: binary()] | 1098.9 | 0.91 ms | 0.89 ms | 1.17 ms | 88 B |
| **torque** [map() :: iodata()] | 1077.4 | 0.93 ms | 0.91 ms | 1.12 ms | **64 B** |
| **glazer** [map() :: binary()] | 981.9 | 1.02 ms | 1.00 ms | 1.28 ms | **64 B** |
| **jiffy** [proplist() :: iodata()] | 536.5 | 1.86 ms | 1.82 ms | 2.98 ms | 37.7 KB |
| **jiffy** [map() :: iodata()] | 439.8 | 2.27 ms | 2.26 ms | 2.48 ms | 1.06 MB |
| **otp json** [map() :: iodata()] | 267.3 | 3.74 ms | 4.08 ms | 6.51 ms | 5.40 MB |
| **jason** [map() :: iodata()] | 260.6 | 3.84 ms | 3.58 ms | 6.07 ms | 4.96 MB |
| **simdjsone** [proplist() :: iodata()] | 255.6 | 3.91 ms | 3.78 ms | 6.60 ms | 37.7 KB |
| **simdjsone** [map() :: iodata()] | 214.7 | 4.66 ms | 4.64 ms | 6.61 ms | 1.06 MB |
| **jason** [map() :: binary()] | 136.6 | 7.32 ms | 7.10 ms | 9.02 ms | 4.96 MB |

### Parse (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse(unique_keys) | **572.1K** | **1.75 μs** | 1.33 μs | **5.75 μs** |
| **torque** parse | 549.5K | 1.82 μs | 1.33 μs | 6.08 μs |
| **simdjsone** parse | 320.4K | 3.12 μs | **1.21 μs** | 5.96 μs |

### Extract 5 fields from raw JSON (1.2 KB OpenRTB)

End-to-end cost of pulling 5 fields out of a JSON blob: `parse` + `get`
(torque, simdjsone) vs `decode` + `find` (glazer has no lazy handle, so it must
fully decode first). This is the apples-to-apples version of "get" — torque's
selective extraction skips materializing the whole document.

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse(unique_keys) + get_many | **443.3K** | **2.26 μs** | 1.71 μs | 6.67 μs |
| **torque** parse + get_many | 425.3K | 2.35 μs | 1.79 μs | **6.04 μs** |
| **torque** parse + get x5 | 419.9K | 2.38 μs | 2.00 μs | 6.92 μs |
| **simdjsone** parse + get x5 | 371.7K | 2.69 μs | **1.67 μs** | 7.00 μs |
| **glazer** decode + find x5 | 317.0K | 3.15 μs | 2.83 μs | 7.71 μs |

Run benchmarks locally:

```bash
MIX_ENV=bench mix run bench/torque_bench.exs
```

## Limitations

- **Nesting depth**: JSON documents nested deeper than 128 levels return `{:error, :nesting_too_deep}` from `decode/1`, `parse/1`, `get/2`, `get_many/2`, and `encode/1` rather than crashing the VM. Real-world documents are never this deep; the limit exists to prevent stack overflow in the NIF (the dirty CPU scheduler, used for inputs over 20 KB, has a small stack).

## License

MIT

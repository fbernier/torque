# Torque

High-performance JSON library for Elixir via [Rustler](https://github.com/rustler-magic/rustler) NIFs, powered by [sonic-rs](https://github.com/cloudwego/sonic-rs) (SIMD-accelerated).

Torque provides among the fastest JSON encoding and decoding available in the BEAM ecosystem, with a selective field extraction API for workloads that only need a subset of fields from each document.

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
    {:torque, "~> 0.1.10"}
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
| `:nesting_too_deep` | `decode/1`, `get/2`, `get_many/2` | Document exceeds 128 nesting levels |

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
| **glazer** | **371.2K** | **2.69 μs** | **2.50 μs** | 6.42 μs | 1.56 KB |
| **torque** | 284.3K | 3.52 μs | 3.42 μs | **5.71 μs** | 1.56 KB |
| **jiffy** | 215.7K | 4.64 μs | 4.21 μs | 8.71 μs | **1.55 KB** |
| **simdjsone** | 194.9K | 5.13 μs | 4.96 μs | 8.96 μs | 1.59 KB |
| **otp json** | 145.6K | 6.87 μs | 6.58 μs | 11.63 μs | 7.73 KB |
| **jason** | 113.4K | 8.82 μs | 8.42 μs | 16.08 μs | 9.54 KB |

### Decode (750 KB Twitter)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **glazer** | **592.9** | **1.69 ms** | **1.50 ms** | **2.21 ms** | 1.58 KB |
| **torque** | 543.6 | 1.84 ms | 1.66 ms | 2.36 ms | **1.57 KB** |
| **simdjsone** | 438.4 | 2.28 ms | 1.80 ms | 3.40 ms | 1.59 KB |
| **jiffy** | 311.5 | 3.21 ms | 3.31 ms | 3.71 ms | 2.30 MB |
| **otp json** | 213.1 | 4.69 ms | 4.73 ms | 5.68 ms | 2.48 MB |
| **jason** | 152.0 | 6.58 ms | 6.58 ms | 6.88 ms | 3.52 MB |

### Encode (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: iodata()] | **1260K** | **0.79 μs** | **0.71 μs** | **0.92 μs** | **64 B** |
| **torque** [proplist() :: binary()] | 1250K | 0.80 μs | 0.75 μs | **0.92 μs** | 88 B |
| **otp json** [map() :: iodata()] | 1180K | 0.85 μs | 0.79 μs | 1.25 μs | 3928 B |
| **torque** [map() :: binary()] | 1050K | 0.96 μs | 0.88 μs | 1.04 μs | 88 B |
| **glazer** [map() :: binary()] | 1020K | 0.98 μs | 0.83 μs | 1.63 μs | **64 B** |
| **torque** [map() :: iodata()] | 1010K | 0.99 μs | 0.88 μs | 1.96 μs | **64 B** |
| **jiffy** [proplist() :: iodata()] | 730K | 1.37 μs | 1.17 μs | 1.63 μs | 120 B |
| **jason** [map() :: iodata()] | 610K | 1.63 μs | 1.50 μs | 2.63 μs | 3848 B |
| **jiffy** [map() :: iodata()] | 590K | 1.71 μs | 1.42 μs | 3.71 μs | 824 B |
| **simdjsone** [proplist() :: iodata()] | 450K | 2.23 μs | 2.04 μs | 2.75 μs | 184 B |
| **simdjsone** [map() :: iodata()] | 370K | 2.67 μs | 2.42 μs | 4.67 μs | 888 B |
| **jason** [map() :: binary()] | 290K | 3.48 μs | 2.42 μs | 20.04 μs | 3912 B |

### Encode (750 KB Twitter)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **torque** [proplist() :: binary()] | **1245.8** | **0.80 ms** | **0.79 ms** | **0.98 ms** | 88 B |
| **torque** [proplist() :: iodata()] | 1241.5 | 0.81 ms | **0.79 ms** | **0.98 ms** | **64 B** |
| **torque** [map() :: binary()] | 1121.0 | 0.89 ms | 0.88 ms | 1.05 ms | 88 B |
| **torque** [map() :: iodata()] | 1096.7 | 0.91 ms | 0.90 ms | 1.08 ms | **64 B** |
| **glazer** [map() :: binary()] | 1001.0 | 1.00 ms | 0.99 ms | 1.10 ms | **64 B** |
| **jiffy** [proplist() :: iodata()] | 544.7 | 1.84 ms | 1.83 ms | 2.04 ms | 37.7 KB |
| **jiffy** [map() :: iodata()] | 402.6 | 2.48 ms | 2.63 ms | 2.97 ms | 1.06 MB |
| **otp json** [map() :: iodata()] | 277.6 | 3.60 ms | 3.40 ms | 4.71 ms | 5.40 MB |
| **simdjsone** [proplist() :: iodata()] | 258.6 | 3.87 ms | 3.81 ms | 5.82 ms | 37.7 KB |
| **jason** [map() :: iodata()] | 240.0 | 4.17 ms | 3.91 ms | 6.22 ms | 4.96 MB |
| **simdjsone** [map() :: iodata()] | 218.6 | 4.57 ms | 4.66 ms | 5.31 ms | 1.06 MB |
| **jason** [map() :: binary()] | 129.6 | 7.72 ms | 7.71 ms | 8.34 ms | 4.96 MB |

### Parse (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 |
|---|---|---|---|---|
| **torque** parse(unique_keys) | **570.0K** | **1.75 μs** | 1.33 μs | 5.79 μs |
| **torque** parse | 545.2K | 1.83 μs | 1.33 μs | 6.08 μs |
| **simdjsone** parse | 360.1K | 2.78 μs | **1.17 μs** | **5.63 μs** |

### Get (5 fields) (1.2 KB OpenRTB)

| Library | ips | mean | median | p99 | memory |
|---|---|---|---|---|---|
| **glazer** find (decoded) | **2.83M** | **353 ns** | **333 ns** | **459 ns** | 424 B |
| **torque** get_many_nil (unique_keys) | 2.43M | 411 ns | 375 ns | 500 ns | **240 B** |
| **torque** get_many (unique_keys) | 2.36M | 423 ns | 375 ns | 500 ns | 360 B |
| **torque** get_many_nil | 2.13M | 469 ns | 458 ns | 584 ns | **240 B** |
| **torque** get_many | 2.04M | 491 ns | 458 ns | 625 ns | 360 B |
| **simdjsone** get | 1.76M | 568 ns | 458 ns | 1000 ns | 384 B |
| **torque** get (unique_keys) | 1.56M | 641 ns | 584 ns | 792 ns | 384 B |
| **torque** get | 1.41M | 709 ns | 667 ns | 916 ns | 384 B |

`glazer find` runs over a fully decoded term (decode cost excluded, as parse
cost is excluded for `torque`/`simdjsone`); glazer has no parse-to-handle API,
so it is absent from the parse benchmark.

Run benchmarks locally:

```bash
MIX_ENV=bench mix run bench/torque_bench.exs
```

## Limitations

- **Nesting depth**: JSON documents nested deeper than 128 levels return `{:error, :nesting_too_deep}` from `decode/1`, `get/2`, `get_many/2`, and `encode/1` rather than crashing the VM. Real-world documents are never this deep; the limit exists to prevent stack overflow in the NIF (the dirty CPU scheduler, used for inputs over 20 KB, has a small stack).

## License

MIT

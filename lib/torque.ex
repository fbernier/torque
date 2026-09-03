defmodule Torque do
  @moduledoc """
  High-performance JSON library powered by sonic-rs via Rustler NIFs.

  ## Decoding strategies

    * **Parse + Get** — `parse/2` returns an opaque document reference.
      `get/2`, `get/3`, `get_many/2`, `get_many_nil/2` and
      `get_many_defaults/2` extract fields by JSON Pointer (RFC 6901) paths
      without materializing the full Elixir term tree. Ideal when the *same*
      document is queried more than once, which is what the handle is for.

    * **Compiled pointers** — when the same fixed set of paths is extracted
      from every document, `compile_pointers/2` pre-parses the paths once and
      `parse_get_many_nil/2` reads the document in a single pass, building
      values only where a path ends and skipping everything else. For one-shot
      extraction — parse a payload, take a few fields, discard it — prefer this
      over `parse/2` + `get/2`: it never builds the document it is about to
      throw away, which measures ~2× on a 440 KB payload and more as the
      document grows. `validate: false` on the handle trades reporting faults
      in the parts no path selects for a cheaper skip.

    * **Full decode** — `decode/1` converts an entire JSON binary into
      Elixir terms in one pass.

  ## Encoding

  `encode/1` serializes Elixir terms to JSON. Supports maps (atom,
  binary, or integer keys), lists, binaries, numbers, booleans, `nil`,
  and jiffy-style `{proplist}` tuples.

  ## Scheduler awareness

  Decoding and parsing automatically dispatch inputs larger than 20 KB to a
  dirty CPU scheduler to avoid blocking normal BEAM schedulers. Lookups
  dispatch on what the *caller* brings instead, which is what the document's
  size cannot report:

    * **Paths**, because a batch builds one result per path — 100k pointers
      against a two-byte document is 100k terms of work. At 2048 paths or more,
      `get_many/2`, `get_many_nil/2`, `get_many_defaults/2` and
      `parse_get_many_nil/2` go dirty.

    * **Path bytes**, because a pointer is walked to split it and again to
      unescape any `~` segment. Past 20 KB of path — one long one, or several
      adding up — `get/2`, `get/3`, `length/2` and the batch calls go dirty
      too. The walk that decides this stops at the first answer, so it never
      reads more than 2048 paths or 20 KB of them.

  `compile_pointers/2` answers the same two questions about the list it is
  given: a three-path set compiles in about a microsecond, which a dirty hop
  would roughly double, and 2048 of them take 400 µs, which is not a normal
  scheduler's to spend.

  What neither size can predict is a single huge result: `get/2` on a path
  holding a megabyte of array converts the whole subtree on a normal
  scheduler. It reports that work to ERTS afterwards rather than moving,
  which adjusts the calling process's reduction budget without preempting a
  call already made.

  Encoding cannot cheaply predict its output size up front, so dirty dispatch
  is opt-in there: pass `dirty: true` to `encode/2`, `encode!/2`,
  `encode_to_iodata/2`, or `encode_to_iodata!/2` when terms are expected
  to produce large output.

  ## Type conversion

  | JSON | Elixir |
  |------|--------|
  | object | map with binary keys |
  | array | list |
  | string | binary |
  | integer | integer |
  | float | float |
  | `true` / `false` | `true` / `false` |
  | `null` | `nil` |

  For objects with duplicate keys, the last value wins (unless
  `unique_keys: true` is passed to `parse/2`).
  """

  @timeslice_bytes 20_480

  # Batch lookup work scales with the path set, not the document. Calibrate this
  # threshold on `get_many_defaults/2`, the most expensive batch operation.
  @dirty_path_count 2048

  # A compiled handle reports the same two quantities the raw-list walk
  # computes, so both forms of the same path set dispatch alike.
  defguardp compiled_dirty?(count, bytes)
            when count >= @dirty_path_count or bytes > @timeslice_bytes

  @typedoc """
  An opaque handle to a set of pre-compiled JSON Pointer paths, returned by
  `compile_pointers/2`. Pass it to `get_many/2`, `get_many_nil/2`, or
  `parse_get_many_nil/2` in place of a path list to skip per-call path parsing.

  It carries its own path count and total path bytes, so dispatching a call to
  a dirty scheduler costs two tuple elements rather than a walk of the paths it
  was built from — and answers that question exactly as the raw list would.
  """
  @opaque pointers :: {reference(), non_neg_integer(), non_neg_integer()}

  # --- Decoding ---

  @doc """
  Decodes a JSON binary into Elixir terms.

  JSON objects become maps with binary keys, arrays become lists, strings
  become binaries, numbers become integers or floats, booleans become
  `true`/`false`, and `null` becomes `nil`.

  Integers outside the signed/unsigned 64-bit range decode as exact
  arbitrary-precision integers (Erlang bignums) rather than lossy floats.

  Automatically uses a dirty CPU scheduler for inputs larger than 20 KB.

  ## Examples

      iex> Torque.decode(~s({"a":1,"b":"hello"}))
      {:ok, %{"a" => 1, "b" => "hello"}}

      iex> Torque.decode(~s([1,2,3]))
      {:ok, [1, 2, 3]}

      iex> match?({:error, _}, Torque.decode("invalid"))
      true
  """
  @doc group: :decode
  @spec decode(binary()) :: {:ok, term()} | {:error, binary() | :nesting_too_deep}
  def decode(json) when is_binary(json) and byte_size(json) > @timeslice_bytes do
    Torque.Native.decode_dirty(json)
  end

  def decode(json) when is_binary(json) do
    Torque.Native.decode(json)
  end

  @doc """
  Decodes a JSON binary into Elixir terms, raising on error.

  ## Examples

      iex> Torque.decode!(~s({"a":1}))
      %{"a" => 1}
  """
  @doc group: :decode
  @spec decode!(binary()) :: term()
  def decode!(json) when is_binary(json) do
    case decode(json) do
      {:ok, term} -> term
      {:error, reason} -> raise ArgumentError, "decode error: #{reason}"
    end
  end

  # --- Encoding ---

  @doc """
  Encodes an Elixir term into a JSON binary.

  ## Supported terms

    * Maps with atom, binary, or integer keys (integer keys are
      stringified — JSON object names must be strings)
    * Lists (JSON arrays)
    * Binaries (JSON strings)
    * Integers and floats
    * `true`, `false`, `nil` (JSON `null`)
    * Other atoms (encoded as JSON strings)
    * `{keyword_list}` tuples (jiffy-style proplist objects)

  ## Options

    * `:dirty` — when `true`, runs the encode on a dirty CPU scheduler.
      Unlike `decode/1`, which dispatches on input byte size, encoding
      cannot cheaply predict its output size up front, so large encodes
      are opt-in. Enable when terms are expected to produce large output
      (more than roughly 20 KB). Defaults to `false`.

  ## Examples

      iex> Torque.encode(%{id: "abc", price: 1.5})
      {:ok, ~s({"id":"abc","price":1.5})}

      iex> Torque.encode({[{:id, "abc"}]})
      {:ok, ~s({"id":"abc"})}

      iex> Torque.encode(%{id: "abc"}, dirty: true)
      {:ok, ~s({"id":"abc"})}
  """
  @doc group: :encode
  @spec encode(term(), keyword()) :: {:ok, binary()} | {:error, binary() | :nesting_too_deep}
  def encode(term, opts \\ [])

  def encode(term, []) do
    Torque.Native.encode(term)
  end

  def encode(term, opts) do
    if Keyword.get(opts, :dirty, false) do
      Torque.Native.encode_dirty(term)
    else
      Torque.Native.encode(term)
    end
  end

  @doc """
  Encodes an Elixir term into a JSON binary, raising on error.

  Accepts the same options as `encode/2`.

  ## Examples

      iex> Torque.encode!(%{ok: true})
      ~s({"ok":true})
  """
  @doc group: :encode
  @spec encode!(term(), keyword()) :: binary()
  def encode!(term, opts \\ []) do
    case encode(term, opts) do
      {:ok, json} -> json
      {:error, reason} -> raise ArgumentError, "encode error: #{reason}"
    end
  end

  @doc """
  Encodes an Elixir term into a JSON binary (iodata-compatible).

  Returns the binary directly without `{:ok, ...}` tuple wrapping.
  Raises on error. This is the fastest encoding path when the result
  is passed directly to I/O (e.g. as an HTTP response body).

  Accepts the same options as `encode/2`.

  ## Examples

      iex> Torque.encode_to_iodata(%{ok: true})
      ~s({"ok":true})
  """
  @doc group: :encode
  @spec encode_to_iodata(term(), keyword()) :: binary()
  def encode_to_iodata(term, opts \\ [])

  def encode_to_iodata(term, []) do
    Torque.Native.encode_iodata(term)
  catch
    :error, value -> raise ArgumentError, "encode error: #{inspect(value)}"
  end

  def encode_to_iodata(term, opts) do
    if Keyword.get(opts, :dirty, false) do
      Torque.Native.encode_iodata_dirty(term)
    else
      Torque.Native.encode_iodata(term)
    end
  catch
    :error, value -> raise ArgumentError, "encode error: #{inspect(value)}"
  end

  @doc """
  Alias for `encode_to_iodata/2`, which already raises on error.

  Exists to satisfy Phoenix's `:json_library` contract, which calls
  `encode_to_iodata!/1` from its socket serializers, controllers, and
  longpoll transport. Set `config :phoenix, :json_library, Torque` to use
  Torque there.

  ## Examples

      iex> Torque.encode_to_iodata!(%{ok: true})
      ~s({"ok":true})
  """
  @doc group: :encode
  @spec encode_to_iodata!(term(), keyword()) :: binary()
  def encode_to_iodata!(term, opts \\ []), do: encode_to_iodata(term, opts)

  # --- Parse + Get ---

  @doc """
  Parses a JSON binary into an opaque document reference.

  The returned reference can be passed to `get/2`, `get/3`, `get_many/2`,
  `get_many_nil/2`, or `length/2` for efficient repeated field extraction
  without re-parsing.

  ## Options

    * `:unique_keys` — when `true`, assumes object keys are unique and uses
      a faster lookup path. Defaults to `false` (last-value-wins for
      duplicate keys).

  Automatically uses a dirty CPU scheduler for inputs larger than 20 KB.

  ## Examples

      iex> {:ok, doc} = Torque.parse(~s({"a":1}))
      iex> is_reference(doc)
      true

      iex> {:ok, doc} = Torque.parse(~s({"a":1}), unique_keys: true)
      iex> Torque.get(doc, "/a")
      {:ok, 1}
  """
  @doc group: :parse_get
  @spec parse(binary(), keyword()) :: {:ok, reference()} | {:error, binary() | :nesting_too_deep}
  def parse(json, opts \\ [])

  def parse(json, []) when is_binary(json) and byte_size(json) > @timeslice_bytes do
    Torque.Native.parse_dirty(json)
  end

  def parse(json, []) when is_binary(json) do
    Torque.Native.parse(json)
  end

  def parse(json, opts) when is_binary(json) and byte_size(json) > @timeslice_bytes do
    Torque.Native.parse_opts_dirty(json, Keyword.get(opts, :unique_keys, false))
  end

  def parse(json, opts) when is_binary(json) do
    Torque.Native.parse_opts(json, Keyword.get(opts, :unique_keys, false))
  end

  @doc """
  Extracts a value from a parsed document using a JSON Pointer path (RFC 6901).

  Paths must start with `"/"`. Array elements are addressed by index
  (e.g. `"/imp/0/banner/w"`). An empty path `""` returns the root value.

  One deviation from RFC 6901: `"/"` also returns the root value, where the
  RFC reads it as the member whose key is the empty string. Every pointer
  entry point in Torque agrees on that, and changing it would silently move
  existing callers' lookups, so it stands until a breaking release.

  ## Examples

      iex> {:ok, doc} = Torque.parse(~s({"site":{"domain":"example.com"}}))
      iex> Torque.get(doc, "/site/domain")
      {:ok, "example.com"}

      iex> {:ok, doc} = Torque.parse(~s({"site":{"domain":"example.com"}}))
      iex> Torque.get(doc, "/missing")
      {:error, :no_such_field}
  """
  @doc group: :parse_get
  @spec get(reference(), binary()) ::
          {:ok, term()} | {:error, :no_such_field | :nesting_too_deep}
  def get(doc, path)
      when is_reference(doc) and is_binary(path) and byte_size(path) > @timeslice_bytes do
    Torque.Native.get_dirty(doc, path)
  end

  def get(doc, path) when is_reference(doc) and is_binary(path) do
    Torque.Native.get(doc, path)
  end

  @doc """
  Extracts a value from a parsed document, returning `default` when the path
  does not exist.

  Raises `ArgumentError` for errors other than `:no_such_field`
  (e.g. `:nesting_too_deep`).

  Automatically uses a dirty CPU scheduler for paths larger than 20 KB: a path
  is walked to split it and again to unescape any `~` segment, which is the
  caller's bytes rather than the document's.

  ## Examples

      iex> {:ok, doc} = Torque.parse(~s({"a":1}))
      iex> Torque.get(doc, "/a", nil)
      1

      iex> {:ok, doc} = Torque.parse(~s({"a":1}))
      iex> Torque.get(doc, "/b", :default)
      :default
  """
  @doc group: :parse_get
  @spec get(reference(), binary(), term()) :: term()
  def get(doc, path, default) when is_reference(doc) and is_binary(path) do
    # Route through `get/2` so both arities use the same scheduler dispatch.
    case get(doc, path) do
      {:ok, value} -> value
      {:error, :no_such_field} -> default
      {:error, reason} -> raise ArgumentError, "get error: #{reason}"
    end
  end

  @doc """
  Extracts multiple values from a parsed document in a single NIF call.

  Returns a list of results in the same order as `paths`, each being
  `{:ok, value}` or `{:error, :no_such_field}`.

  Accepts either a list of JSON Pointer path strings or a `t:pointers/0` handle
  built by `compile_pointers/2`. The compiled form skips all per-call path
  parsing when the same paths query multiple documents.

  More efficient than calling `get/2` in a loop because it crosses
  the NIF boundary only once. For a document you query once and then discard,
  `compile_pointers/2` + `parse_get_many_nil/2` is faster still — it never
  builds the document at all.

  Raises `ArgumentError` if any path in a path list is not a valid UTF-8 binary.

  Automatically uses a dirty CPU scheduler at 2048 paths or more, or past
  20 KB of path in total: the work is one result per path plus the walk of
  each, neither of which the document's size reports.

  ## Examples

      iex> {:ok, doc} = Torque.parse(~s({"a":1,"b":2}))
      iex> Torque.get_many(doc, ["/a", "/b", "/c"])
      [{:ok, 1}, {:ok, 2}, {:error, :no_such_field}]
  """
  @doc group: :parse_get
  @spec get_many(reference(), [binary()] | pointers()) ::
          [{:ok, term()} | {:error, :no_such_field | :nesting_too_deep}]
  def get_many(doc, paths) when is_reference(doc) and is_list(paths) do
    if many_paths?(paths) do
      Torque.Native.get_many_dirty(doc, paths)
    else
      Torque.Native.get_many(doc, paths)
    end
  end

  def get_many(doc, {pointers, count, bytes})
      when is_reference(doc) and is_reference(pointers) and compiled_dirty?(count, bytes) do
    Torque.Native.get_many_compiled_dirty(doc, pointers)
  end

  def get_many(doc, {pointers, count, bytes})
      when is_reference(doc) and is_reference(pointers) and is_integer(count) and
             is_integer(bytes) do
    Torque.Native.get_many_compiled(doc, pointers)
  end

  @doc """
  Extracts multiple values from a parsed document, returning `nil` for missing
  fields.

  Like `get_many/2` but returns bare values instead of `{:ok, value}` tuples.
  Missing fields return `nil` (indistinguishable from JSON `null`).

  Faster than `get_many/2` when you don't need to distinguish between
  missing fields and null values, as it avoids allocating wrapper tuples.

  Accepts either a list of JSON Pointer path strings or a `t:pointers/0` handle
  built by `compile_pointers/2`. The compiled form skips all per-call path
  parsing and is the recommended option for a fixed, repeatedly-queried path
  set.

  Raises `ArgumentError` if any path is not a valid UTF-8 binary.

  Automatically uses a dirty CPU scheduler at 2048 paths or more, or past 20 KB
  of path in total - both counted from the handle when given one.

  ## Examples

      iex> {:ok, doc} = Torque.parse(~s({"a":1,"b":null}))
      iex> Torque.get_many_nil(doc, ["/a", "/b", "/c"])
      [1, nil, nil]

      iex> {:ok, doc} = Torque.parse(~s({"a":1,"b":null}))
      iex> ptrs = Torque.compile_pointers(["/a", "/b", "/c"])
      iex> Torque.get_many_nil(doc, ptrs)
      [1, nil, nil]
  """
  @doc group: :parse_get
  @spec get_many_nil(reference(), [binary()] | pointers()) :: [term()]
  def get_many_nil(doc, paths) when is_reference(doc) and is_list(paths) do
    if many_paths?(paths) do
      Torque.Native.get_many_nil_dirty(doc, paths)
    else
      Torque.Native.get_many_nil(doc, paths)
    end
  end

  def get_many_nil(doc, {pointers, count, bytes})
      when is_reference(doc) and is_reference(pointers) and compiled_dirty?(count, bytes) do
    Torque.Native.get_many_nil_compiled_dirty(doc, pointers)
  end

  def get_many_nil(doc, {pointers, count, bytes})
      when is_reference(doc) and is_reference(pointers) and is_integer(count) and
             is_integer(bytes) do
    Torque.Native.get_many_nil_compiled(doc, pointers)
  end

  @doc false
  # Exposed for scheduler-dispatch tests; normal and dirty calls return the same values.
  def dirty_paths?(paths) when is_list(paths), do: many_paths?(paths)
  def dirty_paths?(defaults) when is_map(defaults), do: many_default_paths?(defaults)
  def dirty_paths?(path) when is_binary(path), do: byte_size(path) > @timeslice_bytes

  def dirty_paths?({_pointers, count, bytes}) when is_integer(count) and is_integer(bytes),
    do: compiled_dirty?(count, bytes)

  # Stop once either the path count or cumulative path bytes require dirty
  # dispatch. The bounded walk avoids traversing an already oversized list.
  defp many_paths?(paths) do
    many_paths?(paths, @dirty_path_count, 0)
  end

  defp many_paths?(_paths, 0, _bytes), do: true
  defp many_paths?([], _left, _bytes), do: false

  defp many_paths?([path | rest], left, bytes) when is_binary(path) do
    bytes = bytes + byte_size(path)
    bytes > @timeslice_bytes or many_paths?(rest, left - 1, bytes)
  end

  defp many_paths?([_ | rest], left, bytes), do: many_paths?(rest, left - 1, bytes)

  @doc """
  Pre-compiles a list of JSON Pointer paths into a reusable handle.

  Use a compiled handle when the same paths are applied to many documents. It
  avoids splitting and unescaping the paths on every lookup and is accepted by
  `get_many/2`, `get_many_nil/2`, and `parse_get_many_nil/2`.

  Compile once at startup and keep the handle in `:persistent_term`, application
  state, or the process using it. The handle contains a NIF resource and cannot
  be stored in a module attribute.

  Large path sets compile on a dirty CPU scheduler.

  Raises `ArgumentError` if any path is not a valid UTF-8 binary or JSON Pointer.
  A JSON Pointer is either empty or begins with `/`.

  ## Options

    * `:unique_keys` — when `true`, object key lookups use a forward scan that
      stops at the first match (faster). Defaults to `false` (reverse scan,
      last-value-wins for duplicate keys), matching `parse/2`. Safe to enable
      when keys are known to be unique.

    * `:validate` — controls validation of regions not selected by any path.
      The default, `true`, reports malformed input anywhere in the document.
      When `false`, unselected regions are skipped without syntax validation;
      selected values and all consumed UTF-8 are still validated. The check
      for content *after* the document is skipped too, so `~s({"a":1} junk)`
      succeeds where `decode/1` and the default reject it. Truncated input is
      still rejected, because skipping has to find the closing delimiter. Use
      it only with trusted input.

  Extraction results are returned in the same order as `paths`.

  ## Examples

      iex> ptrs = Torque.compile_pointers(["/a", "/b/0"], unique_keys: true)
      iex> {:ok, doc} = Torque.parse(~s({"a":1,"b":[2,3]}))
      iex> Torque.get_many_nil(doc, ptrs)
      [1, 2]
  """
  @doc group: :parse_get
  @spec compile_pointers([binary()], keyword()) :: pointers()
  def compile_pointers(paths, opts \\ []) when is_list(paths) do
    unique_keys = Keyword.get(opts, :unique_keys, false)
    validate = Keyword.get(opts, :validate, true)

    if many_paths?(paths) do
      Torque.Native.compile_paths_dirty(paths, unique_keys, validate)
    else
      Torque.Native.compile_paths(paths, unique_keys, validate)
    end
  end

  @doc """
  Parses a JSON binary and extracts pre-compiled pointers in one NIF call.

  This is the parse-once, extract-once counterpart to `parse/2` followed by
  `get_many_nil/2`. It does not build a reusable document. Missing fields and
  JSON `null` both become `nil`; malformed input returns `{:error, reason}` when
  validation is enabled on the handle.

  Unescaped strings may reference the input binary. Small retained inputs and
  inputs dominated by returned strings are borrowed; other strings are copied
  so a small result does not retain a large allocation. The decision uses
  `:binary.referenced_byte_size/1`, so a short slice of a large binary is treated
  as large. Use `:binary.copy/1` first when retaining a document-sized copy is
  preferable to copying each result.

  Large inputs or path sets run on a dirty CPU scheduler.

  ## Examples

      iex> ptrs = Torque.compile_pointers(["/id", "/site/domain", "/missing"])
      iex> Torque.parse_get_many_nil(~s({"id":"x","site":{"domain":"e.com"}}), ptrs)
      {:ok, ["x", "e.com", nil]}

      iex> ptrs = Torque.compile_pointers(["/a"])
      iex> match?({:error, _}, Torque.parse_get_many_nil("not json", ptrs))
      true
  """
  @doc group: :parse_get
  @spec parse_get_many_nil(binary(), pointers()) ::
          {:ok, [term()]} | {:error, binary() | :nesting_too_deep}
  def parse_get_many_nil(json, {pointers, count, bytes})
      when is_binary(json) and is_reference(pointers) and
             (byte_size(json) > @timeslice_bytes or compiled_dirty?(count, bytes)) do
    Torque.Native.parse_get_many_nil_dirty(json, pointers, :binary.referenced_byte_size(json))
  end

  def parse_get_many_nil(json, {pointers, count, bytes})
      when is_binary(json) and is_reference(pointers) and is_integer(count) and
             is_integer(bytes) do
    Torque.Native.parse_get_many_nil(json, pointers, :binary.referenced_byte_size(json))
  end

  @doc """
  Extracts multiple values from a parsed document with per-path defaults.

  Takes a map of `%{path => default}` and returns a map of the same shape
  where each value is either the parsed value or the supplied default (if
  the path is missing).

  More ergonomic than the two-call `get_many_nil/2` + `Enum.map` pattern
  when consumers need defaults at the call site.

  Equivalent to:

      get_many_nil(doc, Map.keys(defaults))
      |> then(&Enum.zip(Map.keys(defaults), &1))
      |> Map.new(fn {p, nil} -> {p, Map.get(defaults, p)}; pv -> pv end)

  Note: a parsed JSON `null` at the path is indistinguishable from a missing
  field (same as `get_many_nil/2`) — both substitute the default.

  ## Examples

      iex> {:ok, doc} = Torque.parse(~s({"a":1,"b":null}))
      iex> Torque.get_many_defaults(doc, %{"/a" => 0, "/b" => 0, "/c" => "missing"})
      %{"/a" => 1, "/b" => 0, "/c" => "missing"}
  """
  @doc group: :parse_get
  @spec get_many_defaults(reference(), %{binary() => term()}) ::
          %{binary() => term()}
  def get_many_defaults(doc, defaults)
      when is_reference(doc) and is_map(defaults) and map_size(defaults) >= @dirty_path_count do
    Torque.Native.get_many_defaults_dirty(doc, defaults)
  end

  def get_many_defaults(doc, defaults) when is_reference(doc) and is_map(defaults) do
    if many_default_paths?(defaults) do
      Torque.Native.get_many_defaults_dirty(doc, defaults)
    else
      Torque.Native.get_many_defaults(doc, defaults)
    end
  end

  # `map_size/1` answers the path-count question outright, so all that is left
  # is the byte sum. Walk the map forward and stop at the threshold rather than
  # allocating the whole key list to measure it.
  defp many_default_paths?(defaults) do
    map_size(defaults) >= @dirty_path_count or
      default_path_bytes?(:maps.next(:maps.iterator(defaults)), 0)
  end

  defp default_path_bytes?(:none, _bytes), do: false

  defp default_path_bytes?({path, _default, iter}, bytes) when is_binary(path) do
    bytes = bytes + byte_size(path)
    bytes > @timeslice_bytes or default_path_bytes?(:maps.next(iter), bytes)
  end

  defp default_path_bytes?({_path, _default, iter}, bytes),
    do: default_path_bytes?(:maps.next(iter), bytes)

  @doc """
  Returns the length of an array at the given JSON Pointer path, or `nil` if
  the path does not exist or does not point to an array.

  ## Examples

      iex> {:ok, doc} = Torque.parse(~s({"a":[1,2,3]}))
      iex> Torque.length(doc, "/a")
      3

      iex> {:ok, doc} = Torque.parse(~s({"a":[1,2,3]}))
      iex> Torque.length(doc, "/missing")
      nil
  """
  @doc group: :parse_get
  @spec length(reference(), binary()) :: non_neg_integer() | nil
  def length(doc, path)
      when is_reference(doc) and is_binary(path) and byte_size(path) > @timeslice_bytes do
    Torque.Native.array_length_dirty(doc, path)
  end

  def length(doc, path) when is_reference(doc) and is_binary(path) do
    Torque.Native.array_length(doc, path)
  end
end

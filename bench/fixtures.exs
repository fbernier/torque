defmodule Bench.Json do
  @moduledoc """
  Minimal JSON writer with **explicit** member order.

  Fixtures are not built with `Jason.encode!`. A map's iteration order is
  Erlang term order, which is the one order `enif_make_map_from_arrays` sorts
  for free — so a fixture round-tripped through an encoder measures the case
  `map_order.rs` cannot improve, and reports "no change" whatever happens to
  it. `ab.sh` found this the hard way: keys `id`/`name`/`score` happen to be
  alphabetical and reported decode at +0.86% while the same records in
  producer order moved.

  Objects are `{:obj, [{key, value}]}` — an ordered list, never a map. There is
  no way to write a fixture here whose key order is accidental.

  It also has no dependencies, which is what lets `bench/pgo_workload.exs`
  share these fixtures: it runs in a bare mix environment where the bench dep
  tree is not available.
  """

  def encode(term), do: IO.iodata_to_binary(write(term))

  defp write({:obj, []}), do: "{}"

  defp write({:obj, pairs}) do
    [?{, pairs |> Enum.map(fn {k, v} -> [string(k), ?:, write(v)] end) |> intersperse(), ?}]
  end

  defp write(list) when is_list(list),
    do: [?[, list |> Enum.map(&write/1) |> intersperse(), ?]]

  defp write(bin) when is_binary(bin), do: string(bin)
  defp write(int) when is_integer(int), do: Integer.to_string(int)
  defp write(true), do: "true"
  defp write(false), do: "false"
  defp write(nil), do: "null"

  defp write(float) when is_float(float) do
    # `Float.to_string/1` emits shortest round-trip, which is valid JSON for
    # every finite float.
    Float.to_string(float)
  end

  # A pre-rendered fragment: raw JSON bytes spliced in verbatim. Used for
  # number spellings Elixir cannot produce (`1e400`) and for deep nesting.
  defp write({:raw, bytes}) when is_binary(bytes), do: bytes

  defp intersperse([]), do: []
  defp intersperse([one]), do: [one]
  defp intersperse([h | t]), do: [h, Enum.map(t, &[?,, &1])]

  defp string(bin) when is_binary(bin), do: [?", escape(bin, bin, 0, 0, []), ?"]

  # Preserve non-ASCII bytes; fixtures must reach UTF-8 paths literally.
  defp escape(<<>>, original, start, len, acc),
    do: Enum.reverse([binary_part(original, start, len) | acc])

  defp escape(<<c, rest::binary>>, original, start, len, acc) when c < 0x20 or c in [?", ?\\] do
    esc =
      case c do
        ?" -> "\\\""
        ?\\ -> "\\\\"
        ?\n -> "\\n"
        ?\t -> "\\t"
        ?\r -> "\\r"
        ?\b -> "\\b"
        ?\f -> "\\f"
        _ -> "\\u" <> String.pad_leading(Integer.to_string(c, 16), 4, "0")
      end

    acc = [esc, binary_part(original, start, len) | acc]
    escape(rest, original, start + len + 1, 0, acc)
  end

  defp escape(<<_, rest::binary>>, original, start, len, acc),
    do: escape(rest, original, start, len + 1, acc)
end

defmodule Bench.Thresholds do
  @moduledoc """
  The tuning constants the fixtures are built around, read out of the source
  they are defined in rather than copied.

  A mirrored constant is a blind spot with a delay fuse: raise
  `WIDE_OBJECT_MEMBERS` to 1024 and a 512-member "wide object" fixture goes on
  measuring the narrow path under its old name, reporting no regression
  forever. Parsing them means a threshold change either moves the fixtures with
  it or fails the regime check by name.
  """

  import Bitwise

  @rust %{
    wide_object_members: {"native/torque_nif/src/decoder.rs", "WIDE_OBJECT_MEMBERS"},
    long_key_bytes: {"native/torque_nif/src/decoder.rs", "LONG_KEY_BYTES"},
    index_key_bytes: {"native/torque_nif/src/decoder.rs", "INDEX_KEY_BYTES"},
    scan_bytes_above: {"native/torque_nif/src/decoder.rs", "SCAN_BYTES_ABOVE"},
    borrow_any_input: {"native/torque_nif/src/decoder.rs", "BORROW_ANY_INPUT"},
    borrow_input_fraction: {"native/torque_nif/src/decoder.rs", "BORROW_INPUT_FRACTION"},
    searcher_split_bytes: {"native/torque_nif/src/decoder.rs", "SEARCHER_SPLIT_BYTES"},
    short_string: {"native/torque_nif/src/escape.rs", "SHORT_STRING"},
    flatmap_limit: {"native/torque_nif/src/map_order.rs", "FLATMAP_LIMIT"},
    min_ordered_members: {"native/torque_nif/src/map_order.rs", "MIN_ORDERED_MEMBERS"},
    shape_slots: {"native/torque_nif/src/map_order.rs", "SHAPE_SLOTS"},
    key_cache_max_len: {"native/torque_nif/src/native_decode.rs", "KEY_CACHE_MAX_LEN"},
    extract_index_keys_above: {"native/sonic-rs/src/extract.rs", "INDEX_KEYS_ABOVE"},
    encode_budget: {"native/torque_nif/src/encoder.rs", "ENCODE_BUDGET"},
    encode_hard_limit: {"native/torque_nif/src/encoder.rs", "ENCODE_HARD_LIMIT"},
    max_depth: {"native/sonic-rs/src/parser.rs", "MAX_PARSE_DEPTH"}
  }

  @elixir %{
    timeslice_bytes: {"lib/torque.ex", "timeslice_bytes"},
    dirty_path_count: {"lib/torque.ex", "dirty_path_count"},
    dirty_result_count: {"lib/torque.ex", "dirty_result_count"},
    encode_inspect_work: {"lib/torque/native.ex", "inspect_work"},
    encode_inspect_node_cost: {"lib/torque/native.ex", "inspect_node_cost"}
  }

  @doc "Value of a named threshold. Raises if the definition moved."
  def get(name) do
    case :persistent_term.get({__MODULE__, :all}, nil) do
      nil ->
        all = load()
        :persistent_term.put({__MODULE__, :all}, all)
        fetch(all, name)

      all ->
        fetch(all, name)
    end
  end

  def all do
    get(:short_string)
    :persistent_term.get({__MODULE__, :all})
  end

  defp fetch(all, name) do
    case Map.fetch(all, name) do
      {:ok, v} -> v
      :error -> raise ArgumentError, "no such threshold: #{inspect(name)}"
    end
  end

  defp load do
    rust = Map.new(@rust, fn {name, {file, const}} -> {name, rust_const(file, const)} end)
    elixir = Map.new(@elixir, fn {name, {file, attr}} -> {name, elixir_attr(file, attr)} end)
    rust |> Map.merge(elixir) |> Map.merge(shape_hash!())
  end

  defp rust_const(file, const) do
    src = read!(file)

    case Regex.run(~r/^\s*(?:pub(?:\(\w+\))?\s+)?const\s+#{const}\s*:\s*\w+\s*=\s*([^;]+);/m, src) do
      [_, expr] ->
        eval_int(expr, "#{const} in #{file}")

      nil ->
        raise "Bench.Thresholds: `const #{const}` no longer defined in #{file}. " <>
                "A fixture is built around it; find where it went before benchmarking."
    end
  end

  defp elixir_attr(file, attr) do
    src = read!(file)

    case Regex.run(~r/^\s*@#{attr}\s+([0-9_]+)/m, src) do
      [_, digits] ->
        String.to_integer(String.replace(digits, "_", ""))

      nil ->
        raise "Bench.Thresholds: `@#{attr}` no longer defined in #{file}."
    end
  end

  # The constants are literals or small products of literals. Anything else is
  # a signal that the definition changed shape and the fixture needs a human.
  defp eval_int(expr, where) do
    expr
    |> String.replace(~r/\s|_|usize|u64|as/, "")
    |> String.split("*")
    |> Enum.reduce(1, fn part, acc ->
      case Integer.parse(part) do
        {n, ""} -> acc * n
        _ -> raise "Bench.Thresholds: cannot evaluate #{where}: #{inspect(expr)}"
      end
    end)
  end

  # This is deliberately source-checked, not a second unguarded hash definition.
  # A changed prefix, slot selector or cache addressing rule must stop fixtures.
  defp shape_hash! do
    src = read!("native/torque_nif/src/map_order.rs") |> String.replace(~r/\s+/, "")

    prefix =
      "pubfnprefix_be(bytes:&[u8])->u64{" <>
        "letn=ifbytes.len()<8{bytes.len()}else{8};letmutbuf=[0u8;8];" <>
        "buf[..n].copy_from_slice(&bytes[..n]);u64::from_be_bytes(buf)}"

    selector =
      ~r/fnslot_of\(prefixes:&\[u64\]\)->usize\{letn=prefixes.len\(\);leth=\(prefixes\[0\]\^prefixes\[n-1\].rotate_left\((\d+)\)\^nasu64\).wrapping_mul\(0x([0-9A-Fa-f_]+)\);\(h>>SHAPE_SHIFT\)asusize\}/

    unless String.contains?(src, prefix) and
             String.contains?(src, "constSHAPE_SHIFT:u32=u64::BITS-SHAPE_SLOTS.trailing_zeros();") and
             String.contains?(src, "letslot=slot_of(prefixes);ifself.entries[slot].hits(") and
             String.contains?(src, "letentry=&mutself.entries[slot];") do
      raise "Bench.Thresholds: map_order prefix or direct-mapped slot contract changed"
    end

    case Regex.run(selector, src) do
      [_, rotation, multiplier] ->
        %{
          shape_rotation: String.to_integer(rotation),
          shape_multiplier: multiplier |> String.replace("_", "") |> String.to_integer(16)
        }

      nil ->
        raise "Bench.Thresholds: map_order slot_of hash changed; update fixture slot checks"
    end
  end

  @doc "The source-checked direct-mapped slot of an emitted object's keys."
  def shape_slot(keys) do
    prefix = fn key ->
      bytes = binary_part(key, 0, min(byte_size(key), 8))
      :binary.decode_unsigned(bytes <> :binary.copy(<<0>>, 8 - byte_size(bytes)))
    end

    slots = get(:shape_slots)

    unless slots > 1 and band(slots, slots - 1) == 0,
      do: raise("SHAPE_SLOTS is not a power of two")

    shift = 64 - length(Integer.digits(slots - 1, 2))
    rotation = get(:shape_rotation)
    last = prefix.(List.last(keys))
    rotated = band(bor(bsl(last, rotation), bsr(last, 64 - rotation)), 0xFFFFFFFFFFFFFFFF)
    hash = bxor(bxor(prefix.(hd(keys)), rotated), length(keys))
    bsr(band(hash * get(:shape_multiplier), 0xFFFFFFFFFFFFFFFF), shift)
  end

  # A/B worktrees read thresholds from the invoking checkout so both revisions
  # use identical fixtures.
  defp read!(path) do
    root = System.get_env("TORQUE_SOURCE_ROOT") || Path.expand("..", __DIR__)

    case File.read(Path.join(root, path)) do
      {:ok, src} ->
        src

      {:error, reason} ->
        raise "Bench.Thresholds: cannot read #{path} under #{root}: #{reason}"
    end
  end
end

defmodule Bench.Fixtures do
  @moduledoc """
  Every payload the benchmarks run on, in one place, each stating the code path
  it exists to move and the regime it has to stay in to move it.

  Three properties, each fixing a way the old fixtures went blind:

  * **Explicit order.** Built through `Bench.Json`, never an encoder, so no
    fixture's key order is an accident of BEAM map iteration.

  * **Checked regime.** A fixture declares the facts the code it targets
    branches on — "wider than `WIDE_OBJECT_MEMBERS`", "shorter than
    `SHORT_STRING`", "over the dirty-dispatch threshold" — against
    `Bench.Thresholds`, which reads them from the source. `check!/1` asserts
    them. A fixture that drifts out of its regime fails by name instead of
    quietly measuring the path next door.

  * **Lazy and shared.** `fetch/1` builds on demand and memoises, so a
    single-operation workload constructs only what that operation touches.
    `ab.sh` subtracts a baseline that ran the same construction, which is exact
    only when the construction is the same set.

  Deterministic by construction: no `:rand`, so both revisions of an A/B see
  byte-identical input.
  """

  alias Bench.{Json, Thresholds}

  defmodule Fixture do
    @moduledoc false
    defstruct [:id, :targets, :json, :term, :regime, :notes]
  end

  # Registry

  @ids ~w(
    record-term record-schema record-reversed record-hashmap
    record-key-tail record-key-control encode-wide-map
    shape-single shape-memo-fit shape-memo-thrash
    str-short-clean str-long-clean str-escape-early str-escape-late
    str-escape-long str-utf8 str-utf8-tail str-utf8-boundary
    keys-atom keys-atom-unicode keys-integer
    numbers bignum
    decode-bigint repeated-results copied-results completed-result
    wide-object wide-chain wide-siblings wide-long-keys narrow-object
    plan-wide plan-numeric numeric-out-of-range duplicate-numeric duplicate-numeric-deep
    req-small feed-large feed-huge deep proplist
  )a

  def ids, do: @ids

  @doc """
  Builds (or returns the memoised) fixture, asserting its regime.
  """
  def fetch(id) when is_atom(id) do
    key = {__MODULE__, id}

    case :persistent_term.get(key, nil) do
      nil ->
        fixture = id |> build() |> check!()
        :persistent_term.put(key, fixture)
        fixture

      fixture ->
        fixture
    end
  end

  def fetch(id) when is_binary(id) do
    atom =
      Enum.find(@ids, fn known -> Atom.to_string(known) == id end) ||
        raise ArgumentError, "no such fixture: #{id}\nknown: #{Enum.join(@ids, " ")}"

    fetch(atom)
  end

  def fetch_all(ids), do: Enum.map(ids, &fetch/1)

  @doc "Asserts every declared regime fact. Raises naming the fixture and fact."
  def check!(%Fixture{} = f) do
    Enum.each(f.regime, fn
      {label, value, op, bound} ->
        unless compare(op, value, bound) do
          raise """
          Fixture #{f.id} left the regime it measures.

            #{label}: #{value} #{op} #{bound} is false

          #{f.targets}

          This fixture no longer exercises the path it is named for, and would
          report "no change" whatever happens to that path. Fix the fixture, or
          delete it if the path is gone.
          """
        end
    end)

    f
  end

  defp compare(:>=, a, b), do: a >= b
  defp compare(:>, a, b), do: a > b
  defp compare(:<=, a, b), do: a <= b
  defp compare(:<, a, b), do: a < b
  defp compare(:==, a, b), do: a == b

  @doc """
  One line per fixture: size, target, and the regime facts as measured.

  Generated from the fixtures rather than written next to them, so it cannot
  describe a payload the suite no longer builds.
  """
  def report(ids \\ @ids) do
    rows =
      for id <- ids do
        f = fetch(id)

        facts =
          Enum.map_join(f.regime, ", ", fn {label, value, op, bound} ->
            "#{label} #{value}#{op}#{bound}"
          end)

        {Atom.to_string(f.id), format_bytes(byte_size(f.json)), f.targets, facts}
      end

    w1 = rows |> Enum.map(&byte_size(elem(&1, 0))) |> Enum.max()
    w2 = rows |> Enum.map(&byte_size(elem(&1, 1))) |> Enum.max()

    for {id, size, targets, facts} <- rows do
      IO.puts([String.pad_trailing(id, w1), "  ", String.pad_leading(size, w2), "  ", targets])
      IO.puts([String.duplicate(" ", w1 + w2 + 4), facts])
    end

    :ok
  end

  defp format_bytes(n) when n < 1024, do: "#{n} B"
  defp format_bytes(n) when n < 1024 * 1024, do: "#{Float.round(n / 1024, 1)} KB"
  defp format_bytes(n), do: "#{Float.round(n / 1024 / 1024, 1)} MB"

  # Records: identical members in term, schema, and reversed order.
  # Term order is the control that cannot benefit from pre-sorting.

  @record_count 400

  # Binary term order is lexicographic, including mixed-length keys.
  defp record_pairs(i) do
    [
      {"amount", i * 3 / 2},
      {"billing_country", "US"},
      {"created_at", "2024-01-#{rem(i, 28) + 1}T09:#{rem(i, 60)}:00Z"},
      {"currency", "USD"},
      {"status", if(rem(i, 3) == 0, do: "settled", else: "pending")},
      {"user_id", 100_000 + i},
      {"vendor_name", "vendor_#{rem(i, 40)}"}
    ]
  end

  # What a schema-driven producer emits: declaration order, stable per record,
  # unrelated to term order. This is the order almost all real JSON arrives in.
  @schema_order ~w(user_id created_at amount currency status vendor_name billing_country)

  defp records(order) do
    for i <- 1..@record_count do
      {:obj, order_pairs(record_pairs(i), order)}
    end
  end

  defp order_pairs(pairs, :term), do: Enum.sort_by(pairs, &elem(&1, 0))
  defp order_pairs(pairs, :reversed), do: Enum.sort_by(pairs, &elem(&1, 0), :desc)

  defp order_pairs(pairs, :schema) do
    by_key = Map.new(pairs)
    Enum.map(@schema_order, fn k -> {k, Map.fetch!(by_key, k)} end)
  end

  defp order_pairs(pairs, order) when is_list(order) do
    by_key = Map.new(pairs)
    Enum.map(order, fn k -> {k, Map.fetch!(by_key, k)} end)
  end

  # These record/shape builders emit unescaped ASCII keys. Read their actual
  # JSON order rather than checking the source ordering recipe against itself.
  defp emitted_keys(json),
    do: Regex.scan(~r/"([^"\\]*)":/, json, capture: :all_but_first) |> List.flatten()

  defp ordered_rows(json, order) do
    keys = emitted_keys(json)
    width = length(record_pairs(1))
    rows = Enum.chunk_every(keys, width)
    ordered = Enum.count(rows, &(&1 == Enum.sort(&1, order)))

    [
      {"emitted_keys", length(keys), :==, @record_count * width},
      {"ordered_rows", ordered, :==, @record_count}
    ]
  end

  # Shared payload constructors

  defp shape_keys(seed) do
    for i <- 12..1//-1, do: "f#{pad(i, 2)}s#{pad(seed, 4)}_name"
  end

  defp select_shapes(per_slot) do
    slots = Thresholds.get(:shape_slots)

    chosen =
      Enum.reduce_while(1..100_000, %{}, fn seed, acc ->
        keys = shape_keys(seed)
        slot = Thresholds.shape_slot(keys)
        bucket = Map.get(acc, slot, [])
        acc = if length(bucket) < per_slot, do: Map.put(acc, slot, bucket ++ [keys]), else: acc

        if map_size(acc) == slots and
             Enum.all?(acc, fn {_, shapes} -> length(shapes) == per_slot end),
           do: {:halt, acc},
           else: {:cont, acc}
      end)

    unless map_size(chosen) == slots and
             Enum.all?(chosen, fn {_, shapes} -> length(shapes) == per_slot end),
           do: raise("could not fill map_order slots with distinct shapes")

    for round <- 0..(per_slot - 1),
        slot <- 0..(slots - 1),
        do: chosen |> Map.fetch!(slot) |> Enum.at(round)
  end

  defp shape_doc(shapes) do
    rows_per_shape = max(div(1200, length(shapes)), 2)

    rows =
      for r <- 1..rows_per_shape, keys <- shapes do
        {:obj, Enum.map(keys, fn k -> {k, "v_#{k}_#{pad(r, 3)}"} end)}
      end

    Json.encode({:obj, [{"rows", rows}]})
  end

  defp shape_fixture(id, shapes, thrash?) do
    json = shape_doc(shapes)
    ["rows" | keys] = emitted_keys(json)
    emitted = Enum.chunk_every(keys, 12)
    live = Enum.uniq(emitted)
    slots = Enum.map(live, &Thresholds.shape_slot/1)

    {hits, _cache} =
      Enum.map_reduce(emitted, %{}, fn keys, cache ->
        slot = Thresholds.shape_slot(keys)
        {Map.get(cache, slot) == keys, Map.put(cache, slot, keys)}
      end)

    fixture(id, json,
      targets:
        if(thrash?,
          do:
            "map_order.rs — three distinct shapes per direct-mapped slot, cyclic all-miss workload",
          else: "map_order.rs — distinct occupied slots, every shape hits after its first row"
        ),
      regime: [
        {"members", 12, :>=, Thresholds.get(:min_ordered_members)},
        {"members", 12, :<=, Thresholds.get(:flatmap_limit)},
        {"live_shapes", length(live), :==, length(shapes)},
        {"unordered_rows", Enum.count(emitted, &(&1 != Enum.sort(&1))), :==, length(emitted)},
        {"occupied_slots", length(Enum.uniq(slots)), :==,
         if(thrash?, do: div(length(live), 3), else: length(live))},
        {"cache_hits", Enum.count(hits, & &1), :==,
         if(thrash?, do: 0, else: length(emitted) - length(live))}
      ]
    )
  end

  @string_rows 900

  defp string_doc(build_value) do
    rows = for i <- 1..@string_rows, do: {:obj, [{"k", build_value.(i)}]}
    Json.encode(rows)
  end

  defp non_ascii(bin), do: bin |> :binary.bin_to_list() |> Enum.count(&(&1 >= 0x80))

  defp wide_members(n, prefix \\ "f") do
    for i <- 1..n, do: {"#{prefix}#{pad(i, 6)}", i}
  end

  @request_selection [
    ["id"],
    ["site", "domain"],
    ["site", "publisher", "id"],
    ["device", "ip"],
    ["device", "geo", "country"],
    ["device", "ua"],
    ["user", "id"],
    ["imp", 0, "banner", "w"],
    ["imp", 1, "bidfloor"],
    ["regs", "coppa"]
  ]

  @doc "Canonical typed selection, shared by JSON Pointer and Glazer request paths."
  def request_selection, do: @request_selection

  defp request_paths, do: Enum.map(@request_selection, &("/" <> Enum.join(&1, "/")))

  defp request_object(i) do
    {:obj,
     [
       {"id", "req-#{pad(i, 8)}"},
       {"site",
        {:obj,
         [
           {"domain", "example.com"},
           {"page", "https://example.com/articles/some-article-title"},
           {"publisher", {:obj, [{"id", "pub-12345"}]}},
           {"cat", ["IAB1", "IAB2-3"]}
         ]}},
       {"device",
        {:obj,
         [
           {"devicetype", 2},
           {"ua", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/120.0.0.0"},
           {"ip", "203.0.113.42"},
           {"geo", {:obj, [{"country", "US"}, {"lat", 40.7128}, {"lon", -74.006}]}},
           {"connectiontype", 2}
         ]}},
       {"user", {:obj, [{"id", "u-abcdef"}, {"buyeruid", "buyer-#{i}"}]}},
       {"imp",
        [
          {:obj,
           [{"id", "imp-1"}, {"banner", {:obj, [{"w", 300}, {"h", 250}]}}, {"bidfloor", 0.5}]},
          {:obj,
           [{"id", "imp-2"}, {"video", {:obj, [{"mimes", ["video/mp4"]}]}}, {"bidfloor", 2.0}]}
        ]},
       {"regs", {:obj, [{"coppa", 0}]}},
       {"test", true},
       {"ext", nil}
     ]}
  end

  # ---------------------------------------------------------------------------
  # Builders
  # ---------------------------------------------------------------------------

  defp build(:"record-term") do
    json = Json.encode(records(:term))

    fixture(:"record-term", json,
      targets: "map_order.rs — CONTROL: keys already in Erlang term order, reordering cannot pay",
      regime:
        [
          {"members", 7, :>=, Thresholds.get(:min_ordered_members)},
          {"members", 7, :<=, Thresholds.get(:flatmap_limit)}
        ] ++ ordered_rows(json, :asc)
    )
  end

  defp build(:"record-schema") do
    json = Json.encode(records(:schema))

    fixture(:"record-schema", json,
      targets: "map_order.rs — producer declaration order, the order real JSON arrives in",
      regime: [
        {"members", 7, :>=, Thresholds.get(:min_ordered_members)},
        {"members", 7, :<=, Thresholds.get(:flatmap_limit)}
      ]
    )
  end

  defp build(:"record-reversed") do
    json = Json.encode(records(:reversed))

    fixture(:"record-reversed", json,
      targets: "map_order.rs — reverse term order, the ERTS insertion-sort worst case",
      regime:
        [
          {"members", 7, :>=, Thresholds.get(:min_ordered_members)},
          {"members", 7, :<=, Thresholds.get(:flatmap_limit)}
        ] ++ ordered_rows(json, :desc)
    )
  end

  defp build(:"record-hashmap") do
    n = Thresholds.get(:flatmap_limit) + 8

    rows =
      for i <- 1..150 do
        {:obj,
         for j <- n..1//-1 do
           {"field_#{pad(j, 2)}", "value_#{i}_#{j}"}
         end}
      end

    fixture(:"record-hashmap", Json.encode(rows),
      targets: "map_order.rs — CONTROL: past FLATMAP_LIMIT, ERTS hashes and ordering is skipped",
      regime: [{"members", n, :>, Thresholds.get(:flatmap_limit)}]
    )
  end

  # Shape variety: map_order memo occupancy

  defp build(:"shape-single"), do: shape_fixture(:"shape-single", [shape_keys(1)], false)

  defp build(:"shape-memo-fit"), do: shape_fixture(:"shape-memo-fit", select_shapes(1), false)

  defp build(:"shape-memo-thrash"),
    do: shape_fixture(:"shape-memo-thrash", select_shapes(3), true)

  # String shapes. Clean ASCII is the control for escape-path changes.

  defp build(:"str-short-clean") do
    short = Thresholds.get(:short_string)
    value = fn i -> "clean_ascii_#{pad(i, 4)}" end

    fixture(:"str-short-clean", string_doc(value),
      targets: "escape.rs — CONTROL: under SHORT_STRING, all-ASCII, SWAR prefix only",
      regime: [{"value_bytes", byte_size(value.(1)), :<, short}]
    )
  end

  defp build(:"str-long-clean") do
    short = Thresholds.get(:short_string)
    value = fn i -> String.duplicate("abcdefgh", 12) <> pad(i, 4) end

    fixture(:"str-long-clean", string_doc(value),
      targets: "escape.rs — CONTROL: past SHORT_STRING, all-ASCII, the full SIMD chain",
      regime: [{"value_bytes", byte_size(value.(1)), :>=, short * 2}]
    )
  end

  defp build(:"str-escape-early") do
    short = Thresholds.get(:short_string)
    value = fn i -> "\"" <> pad(i, 4) <> String.duplicate("a", short - 6) end

    fixture(:"str-escape-early", string_doc(value),
      targets: "escape.rs — short entry, escape at byte 0 with no clean prefix",
      regime: string_regime(value, short, :<, 0)
    )
  end

  defp build(:"str-escape-late") do
    short = Thresholds.get(:short_string)
    # At SHORT_STRING=32: 30 clean bytes and one quote, not a 40-byte SIMD input.
    value = fn i -> pad(i, 4) <> String.duplicate("a", short - 6) <> "\"" end

    fixture(:"str-escape-late", string_doc(value),
      targets: "escape.rs — short entry, late quote exercises prefix-handoff resume",
      regime: string_regime(value, short, :<, short - 2)
    )
  end

  defp build(:"str-escape-long") do
    short = Thresholds.get(:short_string)
    value = fn i -> String.duplicate("a", short * 2) <> "\"tail#{pad(i, 4)}\"" end

    fixture(:"str-escape-long", string_doc(value),
      targets: "escape.rs — long clean run then quotes, SIMD escape kernels",
      regime: string_regime(value, short, :>=, short * 2)
    )
  end

  defp build(:"str-utf8") do
    value = fn i -> "café résumé naïve 日本語 #{pad(i, 4)} ✨" end

    fixture(:"str-utf8", string_doc(value),
      targets: "escape.rs — non-ASCII throughout, the validate_escape_* kernels",
      regime:
        [{"non_ascii_bytes", non_ascii(value.(1)), :>, 8}] ++
          string_regime(value, Thresholds.get(:short_string), :>=, 3)
    )
  end

  defp build(:"str-utf8-tail") do
    short = Thresholds.get(:short_string)
    value = fn i -> pad(i, 4) <> String.duplicate("a", short - 7) <> "é" end

    fixture(:"str-utf8-tail", string_doc(value),
      targets: "escape.rs — short entry, ASCII prefix then trailing two-byte UTF-8",
      regime: string_regime(value, short, :<, short - 3)
    )
  end

  defp build(:"str-utf8-boundary") do
    short = Thresholds.get(:short_string)
    value = fn i -> pad(i, 4) <> String.duplicate("a", short - 6) <> "é" end

    fixture(:"str-utf8-boundary", string_doc(value),
      targets: "escape.rs — exactly SHORT_STRING: trailing UTF-8 enters SIMD, not short prefix",
      regime: string_regime(value, short, :==, short - 2)
    )
  end

  # ---------------------------------------------------------------------------
  # Key kinds: encoder.rs map-key dispatch
  # ---------------------------------------------------------------------------

  defp build(:"keys-atom") do
    # Latin-1 representable, so `enif_get_atom_length` answers directly.
    term =
      for i <- 1..300 do
        %{amount: i * 3 / 2, café: "accented", status: "pending", user_id: i, vendor: "v#{i}"}
      end

    fixture_term(:"keys-atom", term,
      targets: "encoder.rs — atom keys inside Latin-1, read via enif_get_atom_length",
      regime: [{"records", 300, :>, 0}]
    )
  end

  defp build(:"keys-atom-unicode") do
    # Above U+00FF: `enif_get_atom_length` *fails* on these, so they take the
    # `enif_term_to_binary` path. Nothing else in the suite reaches it.
    term =
      for i <- 1..300 do
        %{:日本語 => "value_#{i}", :"🚀" => i, :ключ => true, :plain => "ascii"}
      end

    fixture_term(:"keys-atom-unicode", term,
      targets: "encoder.rs — atom names outside Latin-1, the enif_term_to_binary path",
      regime: [{"records", 300, :>, 0}]
    )
  end

  defp build(:"keys-integer") do
    term = for i <- 1..300, do: Map.new(1..8, fn j -> {i * 100 + j, "value_#{j}"} end)

    fixture_term(:"keys-integer", term,
      targets: "encoder.rs — integer keys, stringified through encode_integer",
      regime: [{"records", 300, :>, 0}]
    )
  end

  # ---------------------------------------------------------------------------
  # Numbers
  # ---------------------------------------------------------------------------

  defp build(:numbers) do
    rows =
      for i <- 1..1200 do
        {:obj,
         [
           {"big", 505_874_924_095_815_681 + i},
           {"exp", {:raw, "#{i}.#{rem(i, 97)}e#{rem(i, 12) - 6}"}},
           {"neg", -i * 7},
           {"ratio", i / 7},
           {"small", rem(i, 10)},
           {"zero", {:raw, "-0.0"}}
         ]}
      end

    fixture(:numbers, Json.encode(rows),
      targets: "zmij float path, encode_integer, and the ~6% validated-skip cost on numbers",
      regime: [{"rows", 1200, :>, 0}]
    )
  end

  defp build(:bignum) do
    # Base-10 conversion is quadratic: this is the shape that took 129 ms on a
    # normal scheduler before `encode_integer` learned to refuse in advance.
    term = %{"n" => Bitwise.bsl(1, 300_001) - 1}

    fixture_term(:bignum, term,
      targets: "encoder.rs — ENCODE_HARD_LIMIT refusal before a quadratic base-10 conversion",
      regime: [{"digit_bytes", 90_310, :>, Thresholds.get(:encode_hard_limit)}]
    )
  end

  # Wide objects: ObjectMemo admission, indexing, and accounting

  defp build(:"wide-object") do
    n = Thresholds.get(:wide_object_members) * 4
    json = Json.encode({:obj, wide_members(n)})

    fixture(:"wide-object", json,
      targets: "decoder.rs ObjectMemo — one wide object, scan credit through to index build",
      regime: [{"members", n, :>=, Thresholds.get(:wide_object_members)}]
    )
    |> put_paths(for i <- 1..64, do: "/f#{pad(i * 7, 6)}")
  end

  defp build(:"wide-chain") do
    # A path walks a *chain*, so the object it ends at is not the one the next
    # path starts from — the case that cost 18.7x with a single remembered slot.
    n = Thresholds.get(:wide_object_members) + 32

    parents =
      for(g <- 1..4, do: {"g#{g}", {:obj, wide_members(n)}}) ++ wide_members(n - 4, "padding")

    json = Json.encode({:obj, parents})
    term = Torque.decode!(json)

    fixture(:"wide-chain", json,
      targets: "decoder.rs ObjectMemo — wide parent above four wide children, slot addressing",
      regime: [
        {"parent_members", map_size(term), :>=, Thresholds.get(:wide_object_members)},
        {"smallest_child_members", Enum.min(for g <- 1..4, do: map_size(term["g#{g}"])), :>=,
         Thresholds.get(:wide_object_members)}
      ]
    )
    |> put_paths(for i <- 1..64, do: "/g#{rem(i, 4) + 1}/f#{pad(i * 2, 6)}")
  end

  defp build(:"wide-siblings") do
    n = Thresholds.get(:wide_object_members) + 32

    json =
      Json.encode(
        {:obj,
         [
           {"a", {:obj, wide_members(n, "a")}},
           {"b", {:obj, wide_members(n, "b")}}
         ]}
      )

    fixture(:"wide-siblings", json,
      targets: "decoder.rs ObjectMemo — two sibling dictionaries alternating, the 17.6x case",
      regime: [{"members", n, :>=, Thresholds.get(:wide_object_members)}]
    )
    |> put_paths(
      for i <- 1..64 do
        side = if rem(i, 2) == 0, do: "a", else: "b"
        "/#{side}/#{side}#{pad(i * 2, 6)}"
      end
    )
  end

  defp build(:"wide-long-keys") do
    long = Thresholds.get(:long_key_bytes)
    n = Thresholds.get(:wide_object_members)
    # Keys past LONG_KEY_BYTES that share a prefix: `memcmp` runs to the end, so
    # this is where `eq_counting` and `scan_long` earn their place.
    shared = String.duplicate("p", long)
    members = for i <- 1..n, do: {shared <> pad(i, 6), i}

    fixture(:"wide-long-keys", Json.encode({:obj, members}),
      targets: "decoder.rs — scan_long / eq_counting, key bytes rather than member count",
      regime: [
        {"key_bytes", long + 6, :>, long},
        {"members", n, :>=, Thresholds.get(:wide_object_members)}
      ]
    )
    |> put_paths(for i <- 1..32, do: "/" <> shared <> pad(i * 4, 6))
  end

  defp build(:"narrow-object") do
    n = Thresholds.get(:wide_object_members) - 1

    fixture(:"narrow-object", Json.encode({:obj, wide_members(n)}),
      targets: "decoder.rs — CONTROL: one member under WIDE_OBJECT_MEMBERS, plain scan only",
      regime: [{"members", n, :<, Thresholds.get(:wide_object_members)}]
    )
    |> put_paths(for i <- 1..64, do: "/f#{pad(i, 6)}")
  end

  # ---------------------------------------------------------------------------
  # Documents: dirty dispatch and the borrow policy
  # ---------------------------------------------------------------------------

  defp build(:"req-small") do
    json = Json.encode(request_object(1))

    fixture(:"req-small", json,
      targets:
        "decoder.rs borrow_input — at or under BORROW_ANY_INPUT, extracted strings are sub-binaries",
      regime: [
        {"bytes", byte_size(json), :<=, Thresholds.get(:borrow_any_input)},
        {"bytes", byte_size(json), :<, Thresholds.get(:timeslice_bytes)}
      ]
    )
    |> put_paths(request_paths())
    |> normal_paths()
  end

  defp build(:"feed-large") do
    json = Json.encode({:obj, [{"requests", for(i <- 1..200, do: request_object(i))}]})

    fixture(:"feed-large", json,
      targets:
        "decoder.rs borrow_input — past BORROW_ANY_INPUT, strings are copied; over the dirty threshold",
      regime: [
        {"bytes", byte_size(json), :>, Thresholds.get(:borrow_any_input)},
        {"bytes", byte_size(json), :>, Thresholds.get(:timeslice_bytes)}
      ]
    )
    |> put_paths(["/requests/0/id", "/requests/100/device/ua", "/requests/199/site/domain"])
  end

  defp build(:"feed-huge") do
    status = fn i ->
      {:obj,
       [
         {"metadata", {:obj, [{"result_type", "recent"}, {"iso_language_code", "en"}]}},
         {"id", 505_874_924_000_000_000 + i},
         {"id_str", "#{505_874_924_000_000_000 + i}"},
         {"text", "Sample tweet #{i} lorem ipsum dolor sit amet consectetur adipiscing elit"},
         {"truncated", false},
         {"in_reply_to_status_id", nil},
         {"user",
          {:obj,
           [
             {"id", 1_000_000 + i},
             {"screen_name", "username_#{i}"},
             {"location", "San Francisco, CA"},
             {"url", nil},
             {"followers_count", rem(i * 1337, 100_000)},
             {"verified", false},
             {"lang", "en"},
             {"profile_image_url", "http://pbs.twimg.com/profile_images/#{i}/photo.jpeg"}
           ]}},
         {"geo", nil},
         {"retweet_count", rem(i * 3, 1000)},
         {"favorite_count", rem(i * 7, 2000)},
         {"entities",
          {:obj,
           [
             {"hashtags", [{:obj, [{"text", "elixir"}, {"indices", [15, 22]}]}]},
             {"urls", []},
             {"user_mentions", [{:obj, [{"screen_name", "user_#{i}"}, {"id", 2_000_000 + i}]}]}
           ]}},
         {"favorited", false},
         {"lang", "en"}
       ]}
    end

    json =
      Json.encode(
        {:obj,
         [
           {"statuses", for(i <- 1..1200, do: status.(i))},
           {"search_metadata",
            {:obj, [{"count", 1200}, {"completed_in", 0.035}, {"query", "%23elixir"}]}}
         ]}
      )

    fixture(:"feed-huge", json,
      targets: "headline decode/encode payload; producer key order throughout",
      regime: [{"bytes", byte_size(json), :>, 512 * 1024}]
    )
    |> put_paths(["/statuses", "/search_metadata/count", "/statuses/1199/user/screen_name"])
  end

  defp build(:deep) do
    depth = Thresholds.get(:max_depth) - 8
    inner = String.duplicate("{\"n\":", depth) <> "1" <> String.duplicate("}", depth)
    json = Json.encode({:obj, [{"root", {:raw, inner}}]})

    fixture(:deep, json,
      targets: "sonic-rs parser — nesting just under MAX_PARSE_DEPTH, the level budget",
      regime: [{"depth", depth + 1, :<, Thresholds.get(:max_depth)}]
    )
    |> put_paths(["/root" <> String.duplicate("/n", 32)])
  end

  defp build(:proplist) do
    term = fetch(:"req-small").term

    to_proplist = fn f, v ->
      cond do
        is_map(v) -> {Enum.map(v, fn {k, val} -> {k, f.(f, val)} end)}
        is_list(v) -> Enum.map(v, &f.(f, &1))
        true -> v
      end
    end

    fixture_term(:proplist, List.duplicate(to_proplist.(to_proplist, term), 200),
      targets: "encoder.rs — jiffy-style {proplist} tuples",
      regime: [{"entries", 200, :>, 0}]
    )
  end

  # Plan compilation is measured separately from extraction. Both large edge
  # sets stay below caller-side dirty dispatch thresholds.
  defp build(id) when id in [:"plan-wide", :"plan-numeric"] do
    n = 1024
    numeric? = id == :"plan-numeric"
    members = for i <- 0..(n - 1), do: {if(numeric?, do: "#{i}", else: "f#{pad(i, 6)}"), i}
    json = Json.encode({:obj, members})

    fixture(id, json,
      targets:
        "ExtractPlan construction — #{if numeric?, do: "numeric", else: "object"} edges, normal scheduler",
      regime: [{"plan_edges", length(members), :>, Thresholds.get(:extract_index_keys_above)}]
    )
    |> put_paths(Enum.map(members, fn {key, _} -> "/" <> key end))
    |> normal_paths()
  end

  defp build(:"numeric-out-of-range") do
    array = List.duplicate(0, 4000)
    indices = Enum.to_list(5000..6023)

    fixture(:"numeric-out-of-range", Json.encode(array),
      targets:
        "ExtractPlan array matching — 4000 elements, 1024 out-of-range indices, normal scheduler",
      regime: [
        {"array_elements", length(array), :==, 4000},
        {"first_selected_index", Enum.min(indices), :>=, length(array)}
      ]
    )
    |> put_paths(Enum.map(indices, &"/#{&1}"))
    |> normal_paths()
  end

  defp build(:"duplicate-numeric") do
    members = for i <- 0..1023, do: {"#{i}", i + 1}
    pairs = [{"a", {:obj, members}} | List.duplicate({"a", 0}, 1000)]

    fixture(:"duplicate-numeric", Json.encode({:obj, pairs}),
      targets:
        "ExtractPlan duplicate replacement — numeric subtree invalidation then repeated a:0, normal scheduler",
      regime: [
        {"numeric_children", length(members), :>, Thresholds.get(:extract_index_keys_above)},
        {"replacements", length(pairs) - 1, :==, 1000}
      ]
    )
    |> put_paths(Enum.map(members, fn {key, _} -> "/a/" <> key end))
    |> normal_paths()
  end

  defp build(:"duplicate-numeric-deep") do
    depth = 40
    nested = Enum.reduce(1..depth, 17, fn _, value -> [value] end)
    pairs = [{"a", nested} | List.duplicate({"a", 0}, 1000)]

    fixture(:"duplicate-numeric-deep", Json.encode({:obj, pairs}),
      targets: "ExtractPlan duplicate replacement — deep aliased numeric edges, normal scheduler",
      regime: [
        {"depth", depth + 1, :<, Thresholds.get(:max_depth)},
        {"replacements", length(pairs) - 1, :==, 1000}
      ]
    )
    |> put_paths(["/a" <> String.duplicate("/0", depth)])
    |> normal_paths()
  end

  defp build(id) when id in [:"record-key-tail", :"record-key-control"] do
    keys =
      for i <- 0..7 do
        if id == :"record-key-tail", do: "created_0#{i}", else: <<?a + i>> <> "000000000"
      end

    rows = for i <- 1..1200, do: {:obj, Enum.map(keys, &{&1, i})}
    prefixes = keys |> Enum.map(&binary_part(&1, 0, 8)) |> Enum.uniq()

    fixture(id, Json.encode(rows),
      targets:
        "native_decode.rs KeyCache — repeated equal-length keys differing after byte eight",
      regime: [
        {"records", length(rows), :==, 1200},
        {"key_bytes", byte_size(hd(keys)), :<=, Thresholds.get(:key_cache_max_len)},
        {"distinct_prefixes", length(prefixes), :==, if(id == :"record-key-tail", do: 1, else: 8)}
      ]
    )
  end

  defp build(:"encode-wide-map") do
    pairs = for i <- 1..4096, do: {"k#{pad(i, 4)}", i}

    fixture(:"encode-wide-map", Json.encode({:obj, pairs}),
      targets:
        "Torque.Native encoder preflight — cardinality proves a map exceeds discovery fuel",
      regime: [
        {"minimum_node_work", (1 + 2 * length(pairs)) * Thresholds.get(:encode_inspect_node_cost),
         :>, Thresholds.get(:encode_inspect_work)}
      ]
    )
  end

  defp build(:"repeated-results") do
    values = List.duplicate(0, 9000)
    # Keep old revisions runnable at a repetition count calibrated on shared
    # results. The scheduler regression separately exercises 2000 selections.

    fixture(:"repeated-results", Json.encode(values),
      targets:
        "compiled extraction — one selected container reused across repeated result positions",
      regime: [{"array_members", length(values), :==, 9000}]
    )
    |> put_paths(List.duplicate("", 64))
    |> normal_paths()
  end

  defp build(:"copied-results") do
    value = String.duplicate("x", 512 * 1024)

    fixture(:"copied-results", Json.encode({:obj, [{"s", value}]}),
      targets: "parsed lookup — returned string copy bytes require bounded materialization",
      regime: [{"string_bytes", byte_size(value), :>, Thresholds.get(:timeslice_bytes)}]
    )
    |> put_paths(List.duplicate("/s", 64))
  end

  defp build(:"completed-result") do
    values = List.duplicate(0, 10_000)

    fixture(:"completed-result", Json.encode(values),
      targets:
        "parsed lookup — one large result on a fresh document, including first-call dispatch",
      regime: [{"array_members", length(values), :==, 10_000}]
    )
    |> put_paths([""])
    |> normal_paths()
  end

  defp build(:"decode-bigint") do
    term = Integer.pow(10, 700)

    %Fixture{
      id: :"decode-bigint",
      json: Integer.to_string(term),
      term: term,
      targets: "full decode — exact integer beyond finite-f64 range and stack magnitude capacity",
      regime: [{"decimal_digits", byte_size(Integer.to_string(term)), :==, 701}]
    }
  end

  defp build(id), do: raise(ArgumentError, "no builder for fixture #{inspect(id)}")

  # Construction helpers

  defp fixture(id, json, opts) when is_binary(json) do
    %Fixture{
      id: id,
      json: json,
      term: Torque.decode!(json),
      targets: Keyword.fetch!(opts, :targets),
      regime: Keyword.fetch!(opts, :regime),
      notes: Keyword.get(opts, :notes)
    }
  end

  # Term-only fixtures measure encoding; their JSON is the encoded result.

  defp fixture_term(id, term, opts) do
    %Fixture{
      id: id,
      json: Torque.encode!(term),
      term: term,
      targets: Keyword.fetch!(opts, :targets),
      regime: Keyword.fetch!(opts, :regime),
      notes: Keyword.get(opts, :notes)
    }
  end

  defp put_paths(%Fixture{} = f, paths), do: %{f | notes: paths}

  @doc "The JSON Pointer paths a fixture is meant to be queried with."
  def paths(%Fixture{notes: paths}) when is_list(paths), do: paths

  def paths(%Fixture{id: id}),
    do: raise(ArgumentError, "fixture #{id} declares no lookup paths")

  defp pad(n, width), do: String.pad_leading(Integer.to_string(n), width, "0")

  defp string_regime(value, short, op, first) do
    values = for i <- 1..@string_rows, do: value.(i)
    lengths = Enum.map(values, &byte_size/1)

    special = fn bin ->
      bin
      |> :binary.bin_to_list()
      |> Enum.find_index(&(&1 < 0x20 or &1 in [?", ?\\] or &1 >= 0x80))
    end

    [
      {"min_value_bytes", Enum.min(lengths), op, short},
      {"max_value_bytes", Enum.max(lengths), op, short},
      {"first_special_matches", Enum.count(values, &(special.(&1) == first)), :==, @string_rows},
      {"first_special_at", special.(hd(values)), :==, first}
    ]
  end

  defp normal_paths(%Fixture{} = f) do
    paths = paths(f)

    %{
      f
      | regime:
          f.regime ++
            [
              {"input_bytes", byte_size(f.json), :<=, Thresholds.get(:timeslice_bytes)},
              {"path_count", length(paths), :<, Thresholds.get(:dirty_path_count)},
              {"path_bytes", Enum.sum(Enum.map(paths, &byte_size/1)), :<=,
               Thresholds.get(:timeslice_bytes)}
            ]
    }
  end
end

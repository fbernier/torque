# Named operations measured by `scripts/ab.sh`.
#
#   mix run bench/ops.exs list|describe|plan
#   mix run bench/ops.exs calibrate <op> <target-ms>
#   mix run bench/ops.exs run <op> <reps>
#
# `run <op> 0` builds identical fixtures and setup without entering the loop,
# providing the matched baseline. CONTROL operations should not move.

Code.require_file("fixtures.exs", __DIR__)

defmodule Bench.Ops do
  @moduledoc false

  alias Bench.Fixtures

  defmodule Op do
    @moduledoc false
    defstruct [:id, :fixtures, :targets, :setup, :verify, :run, :baseline]
  end

  # Registry

  defp defs do
    decode_ops() ++ encode_ops() ++ lookup_ops() ++ compile_ops() ++ extract_ops()
  end

  # --- decode: native_decode.rs, map_order.rs, the SIMD parser ---------------

  defp decode_ops do
    [
      op(
        "decode-key-tail",
        [:"record-key-tail"],
        "KeyCache — same-length names differing after byte eight must retain copied key reuse",
        fn [f], _ -> Torque.decode!(f.json) end,
        setup: fn [f] -> f end,
        verify: &verify_decoded/1
      ),
      op(
        "decode-key-control",
        [:"record-key-control"],
        "CONTROL — equal-size records with distinct key prefixes",
        fn [f], _ -> Torque.decode!(f.json) end,
        setup: fn [f] -> f end,
        verify: &verify_decoded/1
      ),
      op(
        "decode-bigint",
        [:"decode-bigint"],
        "exact arbitrary integer decode beyond finite-f64 range",
        fn [f], _ -> Torque.decode!(f.json) end,
        setup: fn [f] -> f end,
        verify: &verify_decoded/1
      ),
      op(
        "decode-term",
        [:"record-term"],
        "CONTROL — records already in Erlang term order; map ordering cannot pay here",
        fn [f], _ -> Torque.decode!(f.json) end
      ),
      op(
        "decode-schema",
        [:"record-schema"],
        "map_order.rs — producer declaration order, the order real JSON arrives in",
        fn [f], _ -> Torque.decode!(f.json) end
      ),
      op(
        "decode-reversed",
        [:"record-reversed"],
        "map_order.rs — reverse term order, the ERTS insertion-sort worst case",
        fn [f], _ -> Torque.decode!(f.json) end
      ),
      op(
        "decode-hashmap",
        [:"record-hashmap"],
        "CONTROL — past FLATMAP_LIMIT, ERTS hashes and the ordering pass is skipped",
        fn [f], _ -> Torque.decode!(f.json) end
      ),
      op(
        "decode-shapes-fit",
        [:"shape-memo-fit"],
        "map_order.rs — distinct live shapes occupy every direct-mapped slot without collisions",
        fn [f], _ -> Torque.decode!(f.json) end
      ),
      op(
        "decode-shapes-thrash",
        [:"shape-memo-thrash"],
        "map_order.rs — three shapes per direct-mapped slot; cyclic workload has no cache hits",
        fn [f], _ -> Torque.decode!(f.json) end
      ),
      op(
        "decode-utf8",
        [:"str-utf8"],
        "native_decode.rs — non-ASCII string values through the unescape path",
        fn [f], _ -> Torque.decode!(f.json) end
      ),
      op(
        "decode-escapes",
        [:"str-escape-late"],
        "native_decode.rs — escaped strings, which cannot become sub-binaries",
        fn [f], _ -> Torque.decode!(f.json) end
      ),
      op(
        "decode-numbers",
        [:numbers],
        "sonic-rs number parsing — integers, floats, exponents, negative zero",
        fn [f], _ -> Torque.decode!(f.json) end
      ),
      op(
        "decode-large",
        [:"feed-huge"],
        "headline decode; over the dirty-dispatch threshold",
        fn [f], _ -> Torque.decode!(f.json) end
      ),
      op(
        "decode-deep",
        [:deep],
        "sonic-rs parser — nesting just under MAX_PARSE_DEPTH, the level budget",
        fn [f], _ -> Torque.decode!(f.json) end
      )
    ]
  end

  # --- encode: encoder.rs, escape.rs ----------------------------------------

  defp encode_ops do
    [
      op(
        "encode-ascii-short",
        [:"str-short-clean"],
        "CONTROL — under SHORT_STRING, all-ASCII: never enters an escape branch",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-ascii-long",
        [:"str-long-clean"],
        "CONTROL — past SHORT_STRING, all-ASCII: the SIMD chain with no stoppers",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-escape-early",
        [:"str-escape-early"],
        "escape.rs — short entry, escape at offset 0, no clean prefix to hand off",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-escape-late",
        [:"str-escape-late"],
        "escape.rs — short entry, clean run then a late quote; prefix-handoff resume offset",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-escape-long",
        [:"str-escape-long"],
        "escape.rs — long clean run then quotes, SIMD escape kernels",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-utf8",
        [:"str-utf8"],
        "escape.rs — the validate_escape_* kernels and their bytewise tails",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-utf8-tail",
        [:"str-utf8-tail"],
        "escape.rs — short entry, clean ASCII prefix then trailing non-ASCII",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-utf8-boundary",
        [:"str-utf8-boundary"],
        "escape.rs — exactly SHORT_STRING, trailing UTF-8 enters SIMD rather than short prefix",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-atoms",
        [:"keys-atom"],
        "encoder.rs — atom keys inside Latin-1, behind the failed binary probe",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-atoms-unicode",
        [:"keys-atom-unicode"],
        "encoder.rs — atom names above U+00FF, the enif_term_to_binary path",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-integer-keys",
        [:"keys-integer"],
        "encoder.rs — integer map keys stringified through encode_integer",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-numbers",
        [:numbers],
        "encoder.rs — zmij float writing and integer conversion",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op("encode-proplist", [:proplist], "encoder.rs — jiffy-style {proplist} tuples", fn [f],
                                                                                          _ ->
        Torque.encode!(f.term)
      end),
      op(
        "encode-small",
        [:"req-small"],
        "encoder metadata — request-sized nested maps that remain on a normal scheduler",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-schema",
        [:"record-schema"],
        "encoder.rs — binary map keys, the common record shape",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-wide-map",
        [:"encode-wide-map"],
        "encoder metadata — reject a provably oversized map before iterating members",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-large",
        [:"feed-huge"],
        "encoder.rs — large term automatically dispatches to a dirty scheduler",
        fn [f], _ -> Torque.encode!(f.term) end
      ),
      op(
        "encode-iodata",
        [:"feed-huge"],
        "encoder.rs — the iodata result path, which does not flatten",
        fn [f], _ -> Torque.encode_to_iodata!(f.term) end
      ),
      op(
        "encode-bignum",
        [:bignum],
        "encoder.rs — ENCODE_HARD_LIMIT refusal ahead of a quadratic base-10 conversion",
        fn [f], _ -> Torque.encode!(f.term) end
      )
    ]
  end

  # --- parse + get: decoder.rs, ObjectMemo, types.rs ------------------------

  defp lookup_ops do
    [
      op(
        "parse-small",
        [:"req-small"],
        "parse/1 — document construction for a request-sized payload",
        fn [f], _ -> Torque.parse(f.json) end
      ),
      op(
        "parse-large",
        [:"feed-large"],
        "parse/1 — document construction past the dirty-dispatch threshold",
        fn [f], _ -> Torque.parse(f.json) end
      ),
      op(
        "get-narrow",
        [:"narrow-object"],
        "CONTROL — one member under WIDE_OBJECT_MEMBERS: the plain scan, no memo",
        &parsed_batch/2,
        setup: &parse_with_paths/1,
        verify: &verify_all_found/1
      ),
      op(
        "get-wide",
        [:"wide-object"],
        "ObjectMemo — one wide object: scan credit, admission, index build",
        &parsed_batch/2,
        setup: &parse_with_paths/1,
        verify: &verify_all_found/1
      ),
      op(
        "get-wide-chain",
        [:"wide-chain"],
        "ObjectMemo — wide parent above wide children; the object a path ends at is not the next one's start",
        &parsed_batch/2,
        setup: &parse_with_paths/1,
        verify: &verify_all_found/1
      ),
      op(
        "get-wide-siblings",
        [:"wide-siblings"],
        "ObjectMemo — two sibling dictionaries alternating across a set's ways",
        &parsed_batch/2,
        setup: &parse_with_paths/1,
        verify: &verify_all_found/1
      ),
      op(
        "get-long-keys",
        [:"wide-long-keys"],
        "decoder.rs — scan_long / eq_counting: cost in key bytes, not member count",
        &parsed_batch/2,
        setup: &parse_with_paths/1,
        verify: &verify_all_found/1
      ),
      op(
        "get-subtree",
        [:"feed-huge"],
        "types.rs value_to_term + reorder_object — the other caller of the ordering pass",
        fn _fixtures, doc -> Torque.get(doc, "/statuses") end,
        setup: fn [f] ->
          {:ok, doc} = Torque.parse(f.json)
          doc
        end,
        baseline: "feed-huge+parsed",
        verify: fn doc ->
          {:ok, list} = Torque.get(doc, "/statuses")
          length(list) == 1200 || raise("get-subtree returned #{length(list)} statuses")
        end
      ),
      op(
        "get-small-object",
        [:"req-small"],
        "CONTROL — one request object conversion; per-call key-cache setup must remain cheap",
        fn _fixtures, doc -> Torque.get(doc, "") end,
        setup: fn [f] ->
          {:ok, doc} = Torque.parse(f.json)
          doc
        end,
        baseline: "req-small+root-parsed"
      ),
      op(
        "get-copied-results",
        [:"copied-results"],
        "returned string bytes — bounded conversion and dirty dispatch, not only result count",
        &parsed_batch/2,
        setup: &parse_with_paths/1,
        verify: &verify_all_found/1
      ),
      op(
        "get-completed-result",
        [:"completed-result"],
        "one large result on a fresh document; parse stays inside the measured operation",
        fn [f], _ ->
          {:ok, doc} = Torque.parse(f.json)
          Torque.get_many_nil(doc, Fixtures.paths(f))
        end,
        setup: fn [f] -> f end,
        verify: fn f ->
          {:ok, doc} = Torque.parse(f.json)
          [value] = Torque.get_many_nil(doc, Fixtures.paths(f))
          value == f.term || raise("completed result changed")
        end
      ),
      op(
        "lookup-compiled",
        [:"req-small"],
        "get_many_nil/2 against a parsed document through a compiled handle",
        fn _fixtures, {doc, ptrs} -> Torque.get_many_nil(doc, ptrs) end,
        setup: fn [f] ->
          {:ok, doc} = Torque.parse(f.json)
          {doc, Torque.compile_pointers(Fixtures.paths(f))}
        end,
        baseline: "req-small+parsed+compiled",
        verify: fn {doc, ptrs} ->
          values = Torque.get_many_nil(doc, ptrs)

          Enum.all?(values, &(&1 != nil)) ||
            raise("lookup-compiled found nil: #{inspect(values)}")
        end
      )
    ]
  end

  # Compilation belongs in run, not just setup: baseline subtraction must not
  # remove the operation whose construction cost this row is meant to measure.
  defp compile_ops do
    for {id, fixture, kind} <- [
          {"compile-wide", :"plan-wide", "object"},
          {"compile-numeric", :"plan-numeric", "numeric"}
        ] do
      op(
        id,
        [fixture],
        "compile_pointers/2 — repeatedly construct 1024 #{kind} edges on a normal scheduler",
        &compile_paths/2,
        setup: fn [f] -> {f, Fixtures.paths(f)} end,
        baseline: "#{fixture}+compile-verification",
        verify: &verify_compilation/1
      )
    end
  end

  # --- one-pass extraction: sonic-rs extract.rs -----------------------------

  defp extract_ops do
    [
      op(
        "extract-root-small",
        [:"req-small"],
        "CONTROL — selected request-sized root; conversion fixed cost",
        fn _fixtures, {json, ptrs, _expected} -> Torque.parse_get_many_nil(json, ptrs) end,
        setup: &compile_roots(&1, 1),
        baseline: "req-small+root-selection",
        verify: &verify_roots/1
      ),
      op(
        "extract-root",
        [:"feed-huge"],
        "selected-container conversion — repeated record keys and complete root materialization",
        fn _fixtures, {json, ptrs, _expected} -> Torque.parse_get_many_nil(json, ptrs) end,
        setup: &compile_roots(&1, 1),
        baseline: "feed-huge+root-selection",
        verify: &verify_roots/1
      ),
      op(
        "extract-repeated",
        [:"repeated-results"],
        "compiled duplicate terminals — materialize one container for repeated output positions",
        fn _fixtures, {json, ptrs, _expected} -> Torque.parse_get_many_nil(json, ptrs) end,
        setup: fn [f] -> compile_roots([f], length(Fixtures.paths(f))) end,
        baseline: "repeated-results+compiled",
        verify: &verify_roots/1
      ),
      op(
        "extract-small",
        [:"req-small"],
        "parse_get_many_nil/2 validated — and borrow_input's borrowed side",
        &extract/2,
        setup: &compile_for(&1, validate: true),
        baseline: "req-small+validating",
        verify: &verify_extracted/1
      ),
      op(
        "extract-small-skip",
        [:"req-small"],
        "parse_get_many_nil/2 with validate: false — structural SIMD skipping",
        &extract/2,
        setup: &compile_for(&1, validate: false),
        baseline: "req-small+skipping",
        verify: &verify_extracted/1
      ),
      op(
        "extract-large",
        [:"feed-large"],
        "parse_get_many_nil/2 validated past BORROW_ANY_INPUT — strings are copied out",
        &extract/2,
        setup: &compile_for(&1, validate: true),
        baseline: "feed-large+validating",
        verify: &verify_extracted/1
      ),
      op(
        "extract-large-skip",
        [:"feed-large"],
        "parse_get_many_nil/2 with validate: false on a large document",
        &extract/2,
        setup: &compile_for(&1, validate: false),
        baseline: "feed-large+skipping",
        verify: &verify_extracted/1
      ),
      op(
        "extract-wide",
        [:"wide-object"],
        "ExtractPlan — extraction through a precompiled wide object node; compilation excluded",
        &extract/2,
        setup: &compile_for(&1, validate: true),
        baseline: "wide-object+compiled",
        verify: &verify_extracted/1
      ),
      op(
        "extract-numeric-miss",
        [:"numeric-out-of-range"],
        "ExtractPlan — large array with every numeric selection out of range, normal scheduler",
        &extract/2,
        setup: &compile_for(&1, validate: true),
        baseline: "numeric-out-of-range+compiled",
        verify: &verify_extraction_misses/1
      ),
      op(
        "extract-duplicate-numeric",
        [:"duplicate-numeric"],
        "ExtractPlan — repeated a:0 invalidates a wide numeric subtree, normal scheduler",
        &extract/2,
        setup: &compile_for(&1, validate: true),
        baseline: "duplicate-numeric+compiled",
        verify: &verify_extraction_misses/1
      ),
      op(
        "extract-duplicate-numeric-deep",
        [:"duplicate-numeric-deep"],
        "ExtractPlan — repeated a:0 below deep numeric edges, normal scheduler",
        &extract/2,
        setup: &compile_for(&1, validate: true),
        baseline: "duplicate-numeric-deep+compiled",
        verify: &verify_extraction_misses/1
      )
    ]
  end

  # Shared setup and verification

  defp verify_decoded(f) do
    Torque.decode!(f.json) === f.term || raise("decoded #{f.id} changed")
  end

  defp compile_roots([f], count) do
    {f.json, Torque.compile_pointers(List.duplicate("", count)), List.duplicate(f.term, count)}
  end

  defp verify_roots({json, ptrs, expected}) do
    Torque.parse_get_many_nil(json, ptrs) === {:ok, expected} ||
      raise("root selections changed values or multiplicity")
  end

  defp parse_with_paths([f]) do
    {:ok, doc} = Torque.parse(f.json)
    {doc, Fixtures.paths(f)}
  end

  defp parsed_batch(_fixtures, {doc, paths}), do: Torque.get_many_nil(doc, paths)

  defp verify_all_found({doc, paths}) do
    values = Torque.get_many_nil(doc, paths)

    case Enum.find_index(values, &is_nil/1) do
      nil -> :ok
      i -> raise "lookup path #{Enum.at(paths, i)} is missing; this op measures a failed scan"
    end
  end

  defp compile_for([f], opts) do
    {f.json, Torque.compile_pointers(Fixtures.paths(f), opts)}
  end

  defp extract(_fixtures, {json, ptrs}), do: Torque.parse_get_many_nil(json, ptrs)

  defp verify_extracted({json, ptrs}) do
    {:ok, values} = Torque.parse_get_many_nil(json, ptrs)

    case Enum.find_index(values, &is_nil/1) do
      nil -> :ok
      i -> raise "extraction path ##{i} is missing; this op measures a miss, not an extraction"
    end
  end

  defp compile_paths(_fixtures, {_f, paths}), do: Torque.compile_pointers(paths)

  defp verify_compilation({f, paths} = state) do
    compiled = compile_paths([f], state)
    expected = Enum.map(paths, fn "/" <> key -> Map.fetch!(f.term, key) end)
    {:ok, doc} = Torque.parse(f.json)

    unless Torque.get_many_nil(doc, compiled) === expected and
             Torque.parse_get_many_nil(f.json, compiled) === {:ok, expected},
           do: raise("compiled #{f.id} changed selected values or result order")

    if f.id == :"plan-numeric" do
      # Numeric segments must address both string object keys and array indices.
      array = Bench.Json.encode(expected)

      unless Torque.parse_get_many_nil(array, compiled) === {:ok, expected},
        do: raise("compiled numeric paths do not resolve array indices")
    end
  end

  defp verify_extraction_misses({json, ptrs}) do
    {:ok, doc} = Torque.parse(json)
    expected = Torque.get_many_nil(doc, ptrs)

    unless expected != [] and Enum.all?(expected, &is_nil/1),
      do: raise("miss fixture unexpectedly contains a selected value")

    unless Torque.parse_get_many_nil(json, ptrs) === {:ok, expected},
      do: raise("missing or replaced numeric paths retained stale values")
  end

  # Registry plumbing

  defp op(id, fixtures, targets, run, opts \\ []) do
    fixtures_key = Enum.map_join(fixtures, "+", &Atom.to_string/1)

    %Op{
      id: id,
      fixtures: fixtures,
      targets: targets,
      run: run,
      setup: Keyword.get(opts, :setup, fn _ -> nil end),
      verify: Keyword.get(opts, :verify, fn _ -> :ok end),
      baseline: Keyword.get(opts, :baseline, fixtures_key)
    }
  end

  def all, do: defs()

  def fetch(id) do
    Enum.find(defs(), &(&1.id == id)) ||
      raise ArgumentError, "no such op: #{id}\nknown: #{Enum.map_join(defs(), " ", & &1.id)}"
  end

  @doc "Returns the key for operations that share a baseline measurement."
  def baseline_key(%Op{baseline: key}), do: key

  # Execution

  @doc "Builds, verifies, and executes an operation; zero repetitions is its baseline."
  def run(%Op{} = o, reps) do
    fixtures = Fixtures.fetch_all(o.fixtures)
    state = o.setup.(fixtures)
    o.verify.(state)
    loop(o, fixtures, state, reps)
  end

  defp loop(_o, _fixtures, _state, 0), do: :ok

  defp loop(%Op{run: run} = o, fixtures, state, reps) do
    run.(fixtures, state)
    loop(o, fixtures, state, reps - 1)
  end

  @doc "Returns a repetition count targeting approximately `target_ms`."
  def calibrate(%Op{} = o, target_ms) do
    fixtures = Fixtures.fetch_all(o.fixtures)
    state = o.setup.(fixtures)
    o.verify.(state)

    # Grow until a batch is long enough to time above timer resolution.
    reps = grow(o, fixtures, state, 1)
    {us, :ok} = :timer.tc(fn -> loop(o, fixtures, state, reps) end)
    per_rep = us / reps

    max(round(target_ms * 1000 / per_rep), 1)
  end

  defp grow(o, fixtures, state, reps) do
    {us, :ok} = :timer.tc(fn -> loop(o, fixtures, state, reps) end)

    cond do
      us > 20_000 -> reps
      reps > 50_000_000 -> reps
      true -> grow(o, fixtures, state, reps * 8)
    end
  end
end

# CLI

alias Bench.Ops

case System.argv() do
  # No arguments when loaded by the PGO workload.

  [] ->
    :ok

  ["list"] ->
    Enum.each(Ops.all(), &IO.puts(&1.id))

  ["describe"] ->
    width = Ops.all() |> Enum.map(&byte_size(&1.id)) |> Enum.max()

    Enum.each(Ops.all(), fn o ->
      IO.puts([String.pad_trailing(o.id, width), "  ", o.targets])
    end)

  ["plan"] ->
    Enum.each(Ops.all(), fn o -> IO.puts([o.id, "\t", Ops.baseline_key(o)]) end)

  ["fixtures"] ->
    Bench.Fixtures.report()

  ["calibrate", id, target_ms] ->
    IO.puts(Ops.calibrate(Ops.fetch(id), String.to_integer(target_ms)))

  ["run", id, reps] ->
    Ops.run(Ops.fetch(id), String.to_integer(reps))

  ["baseline", id] ->
    Ops.run(Ops.fetch(id), 0)

  argv ->
    IO.puts(:stderr, """
    unrecognised arguments: #{inspect(argv)}

    usage:
      mix run bench/ops.exs list
      mix run bench/ops.exs describe
      mix run bench/ops.exs fixtures
      mix run bench/ops.exs plan
      mix run bench/ops.exs calibrate <op> <target-ms>
      mix run bench/ops.exs run <op> <reps>
      mix run bench/ops.exs baseline <op>
    """)

    System.halt(2)
end

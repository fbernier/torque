# Torque vs glazer, jiffy, Jason, and OTP JSON.
#
#   MIX_ENV=bench mix run bench/torque_bench.exs
#   BENCH_OUTPUT=json MIX_ENV=bench mix run bench/torque_bench.exs
#
# Use `make ab` to compare revisions. Fixtures preserve explicit member order,
# and glazer runs with UTF-8 validation enabled for equivalent guarantees.

Code.require_file("fixtures.exs", __DIR__)

alias Bench.Fixtures
alias Benchee.Formatters.Console

json? = System.get_env("BENCH_OUTPUT") == "json"

# CI formatter for github-action-benchmark

if json? do
  defmodule CIFormatter do
    @behaviour Benchee.Formatter

    @impl true
    def format(suite, _opts) do
      group = Agent.get(:bench_group, & &1)

      Enum.map(suite.scenarios, fn scenario ->
        %{
          "name" => scenario.name,
          "group" => group,
          "unit" => "iterations/s",
          "value" => scenario.run_time_data.statistics.ips
        }
      end)
    end

    @impl true
    def write(entries, _opts) do
      Agent.update(:bench_results, &(&1 ++ entries))
    end
  end

  Agent.start_link(fn -> [] end, name: :bench_results)
  Agent.start_link(fn -> "" end, name: :bench_group)
end

formatters =
  [{Console, percentiles: [50, 95, 99]}] ++ if json?, do: [{CIFormatter, []}], else: []

# Durations are overridable so the suite can be smoke-run without waiting out a
# full measurement: BENCH_TIME=1 BENCH_WARMUP=0 BENCH_MEMORY=0.
secs = fn var, default ->
  case System.get_env(var) do
    nil -> default
    value -> String.to_integer(value)
  end
end

# One runner instead of eleven copies of the same eighteen-line Benchee call.
group = fn name, note, scenarios ->
  if json?, do: Agent.update(:bench_group, fn _ -> name end)

  IO.puts("\n=== #{String.upcase(name)} ===")
  if note, do: IO.puts(note)
  IO.puts("")

  Benchee.run(scenarios,
    warmup: secs.("BENCH_WARMUP", 2),
    time: secs.("BENCH_TIME", 5),
    memory_time: secs.("BENCH_MEMORY", 2),
    percentiles: [50, 95, 99],
    formatters: formatters
  )
end

# jiffy's `{proplist}` shape, for the encoders that accept it.
to_proplist = fn f, v ->
  cond do
    is_map(v) -> {Enum.map(v, fn {k, val} -> {k, f.(f, val)} end)}
    is_list(v) -> Enum.map(v, &f.(f, &1))
    true -> v
  end
end

proplist = &to_proplist.(to_proplist, &1)

request = Fixtures.fetch(:"req-small")
feed = Fixtures.fetch(:"feed-huge")

IO.puts("request payload: #{byte_size(request.json)} bytes")
IO.puts("feed payload:    #{byte_size(feed.json)} bytes")

# ---------------------------------------------------------------------------
# Decode
# ---------------------------------------------------------------------------

decoders = fn %{json: json} ->
  %{
    "glazer decode" => fn -> :glazer_json.decode(json, [:validate_utf8]) end,
    "jason decode" => fn -> Jason.decode!(json) end,
    "jiffy decode" => fn -> :jiffy.decode(json, [:return_maps]) end,
    "otp json decode" => fn -> :json.decode(json) end,
    "torque decode" => fn -> Torque.decode!(json) end
  }
end

group.("Decode — request", nil, decoders.(request))
group.("Decode — feed", nil, decoders.(feed))

# ---------------------------------------------------------------------------
# Encode
# ---------------------------------------------------------------------------

encoders = fn %{term: term} ->
  pl = proplist.(term)

  %{
    "jason [map() :: binary()]" => fn -> Jason.encode!(term) end,
    "jason [map() :: iodata()]" => fn -> Jason.encode_to_iodata!(term) end,
    "jiffy [map() :: iodata()]" => fn -> :jiffy.encode(term, [:force_utf8]) end,
    "jiffy [proplist() :: iodata()]" => fn -> :jiffy.encode(pl, [:force_utf8]) end,
    "otp json [map() :: iodata()]" => fn -> :json.encode(term) end,
    "glazer [map() :: binary()]" => fn -> :glazer_json.encode(term, [:force_utf8]) end,
    "torque [map() :: binary()]" => fn -> Torque.encode!(term) end,
    "torque [map() :: iodata()]" => fn -> Torque.encode_to_iodata(term) end,
    "torque [proplist() :: binary()]" => fn -> Torque.encode!(pl) end,
    "torque [proplist() :: iodata()]" => fn -> Torque.encode_to_iodata(pl) end
  }
end

group.("Encode — request", nil, encoders.(request))
group.("Encode — feed", nil, encoders.(feed))

# Key order and object shape. Term order is the control that cannot benefit.

group.(
  "Decode — object key order",
  "Same members, three orders. [term] is the control: it cannot improve.",
  Map.new(
    [term: :"record-term", schema: :"record-schema", reversed: :"record-reversed"],
    fn {label, id} ->
      json = Fixtures.fetch(id).json
      {"torque decode [#{label} order]", fn -> Torque.decode!(json) end}
    end
  )
  |> Map.merge(%{
    "jason decode [schema order]" =>
      (
        json = Fixtures.fetch(:"record-schema").json
        fn -> Jason.decode!(json) end
      )
  })
)

group.(
  "Decode — object shape variety",
  "Cyclic record shapes: fit uses distinct actual hash slots; thrash replaces every slot before reuse.",
  Map.new(
    [
      {"1 shape", :"shape-single"},
      {"#{Bench.Thresholds.get(:shape_slots)} shapes — one per slot", :"shape-memo-fit"},
      {"#{Bench.Thresholds.get(:shape_slots) * 3} shapes — three per slot, no hits",
       :"shape-memo-thrash"}
    ],
    fn {label, id} ->
      json = Fixtures.fetch(id).json
      {"torque decode [#{label}]", fn -> Torque.decode!(json) end}
    end
  )
)

# String shapes. Clean ASCII is the control outside escape branches.

group.(
  "Encode — string shapes",
  "[clean] rows are the control: no escape branch, no bytewise tail.",
  Map.new(
    [
      {"short clean", :"str-short-clean"},
      {"long clean", :"str-long-clean"},
      {"short: escape at byte 0", :"str-escape-early"},
      {"short: late quote after clean prefix", :"str-escape-late"},
      {"long: SIMD clean run then quotes", :"str-escape-long"},
      {"long: non-ASCII throughout", :"str-utf8"},
      {"short: trailing UTF-8 after clean prefix", :"str-utf8-tail"},
      {"SHORT_STRING boundary: trailing UTF-8", :"str-utf8-boundary"}
    ],
    fn {label, id} ->
      term = Fixtures.fetch(id).term
      {"torque encode [#{label}]", fn -> Torque.encode!(term) end}
    end
  )
)

group.(
  "Encode — key kinds",
  "Binary keys are probed before the type is asked for; atom names above U+00FF take a different path.",
  Map.new(
    [
      {"binary keys", :"record-schema"},
      {"atom keys", :"keys-atom"},
      {"atom keys above U+00FF", :"keys-atom-unicode"},
      {"integer keys", :"keys-integer"}
    ],
    fn {label, id} ->
      term = Fixtures.fetch(id).term
      {"torque encode [#{label}]", fn -> Torque.encode!(term) end}
    end
  )
)

# ---------------------------------------------------------------------------
# Parse, lookup, extract
# ---------------------------------------------------------------------------

group.("Parse — request", nil, %{
  "torque parse" => fn -> Torque.parse(request.json) end,
  "torque parse(unique_keys)" => fn -> Torque.parse(request.json, unique_keys: true) end
})

request_paths = Fixtures.paths(request)
compiled = Torque.compile_pointers(request_paths)
compiled_fast = Torque.compile_pointers(request_paths, unique_keys: true, validate: false)

# Glazer uses jq-style [N] array indices, not dotted numeric field names.
# Both libraries derive all ten paths from one typed selection.
glazer_paths =
  Enum.map(Fixtures.request_selection(), fn segments ->
    segments
    |> Enum.map_join(fn
      index when is_integer(index) -> "[#{index}]"
      key when is_binary(key) -> ".#{key}"
    end)
    |> :glazer.compile_path()
  end)

extractions = %{
  "glazer decode + find [precompiled paths]" => fn ->
    d = :glazer_json.decode(request.json, [:validate_utf8])
    for p <- glazer_paths, do: :glazer.find(d, p)
  end,
  "torque parse + get [raw pointers]" => fn ->
    {:ok, doc} = Torque.parse(request.json)
    for p <- request_paths, do: Torque.get(doc, p)
  end,
  "torque parse + get_many [raw pointers]" => fn ->
    {:ok, doc} = Torque.parse(request.json)
    Torque.get_many(doc, request_paths)
  end,
  "torque one pass [precompiled pointers]" => fn ->
    Torque.parse_get_many_nil(request.json, compiled)
  end,
  "torque one pass [precompiled, validate: false]" => fn ->
    Torque.parse_get_many_nil(request.json, compiled_fast)
  end
}

expected =
  Enum.map(Fixtures.request_selection(), fn segments ->
    get_in(
      request.term,
      Enum.map(segments, fn
        index when is_integer(index) -> Access.at(index)
        key -> key
      end)
    )
  end)

normalize = fn
  {:ok, values} ->
    values

  values when is_list(values) ->
    Enum.map(values, fn
      {:ok, value} -> value
      [value] -> value
      value -> value
    end)
end

Enum.each(extractions, fn {label, run} ->
  values = normalize.(run.())

  unless values === expected,
    do:
      raise(
        "#{label} selected different request fields: #{inspect(values)} != #{inspect(expected)}"
      )
end)

group.(
  "Extract fields — request",
  "Raw JSON to the same #{length(request_paths)} fields, including arrays. Decode/parse is timed; " <>
    "Glazer paths and Torque compiled handles are prepared once outside timing. Raw-pointer rows " <>
    "parse their pointers per call. The validate: false row skips validation of unselected values.",
  extractions
)

# ObjectMemo behavior across object widths.
group.(
  "Lookup — object width",
  "Batch lookups against a parsed document. narrow is under WIDE_OBJECT_MEMBERS: the plain scan.",
  Map.new(
    [
      {"narrow", :"narrow-object"},
      {"wide", :"wide-object"},
      {"wide chain", :"wide-chain"},
      {"wide siblings", :"wide-siblings"},
      {"long keys", :"wide-long-keys"}
    ],
    fn {label, id} ->
      f = Fixtures.fetch(id)
      {:ok, doc} = Torque.parse(f.json)
      paths = Fixtures.paths(f)
      {"torque get_many [#{label}]", fn -> Torque.get_many_nil(doc, paths) end}
    end
  )
)

# Exercises value_to_term ordering; decode groups cover the fused path.
group.(
  "Extract subtree — object key order",
  "value_to_term, not the fused decoder. [term] is again the control.",
  Map.new(
    [term: :"record-term", schema: :"record-schema", reversed: :"record-reversed"],
    fn {label, id} ->
      json = "{\"rows\":" <> Fixtures.fetch(id).json <> "}"
      {:ok, doc} = Torque.parse(json)
      {"torque get subtree [#{label} order]", fn -> Torque.get(doc, "/rows") end}
    end
  )
)

# CI output

if json? do
  results = Agent.get(:bench_results, & &1)
  {sha, 0} = System.cmd("git", ["rev-parse", "--short", "HEAD"])

  File.write!(
    "bench_comparison.json",
    Jason.encode!(%{
      "commit" => String.trim(sha),
      "date" => DateTime.utc_now() |> DateTime.to_iso8601(),
      "results" => results
    })
  )

  torque_results =
    results
    |> Enum.filter(&String.starts_with?(&1["name"], "torque"))
    |> Enum.map(fn r ->
      [category, payload] = String.split(r["group"], " — ", parts: 2)

      variant =
        r["name"]
        |> String.replace(~r/^torque\s*/, "")
        |> String.replace(~r/[\[\]()]/, "")
        |> String.trim()

      name =
        case category do
          "Encode" -> "encode #{variant} (#{payload})"
          _ -> "#{variant} (#{payload})"
        end

      %{"name" => name, "unit" => r["unit"], "value" => r["value"]}
    end)

  File.write!("bench_torque.json", Jason.encode!(torque_results))

  IO.puts("\nWrote #{length(results)} comparison + #{length(torque_results)} trend results")
end

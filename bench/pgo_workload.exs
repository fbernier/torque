# Weighted PGO workload for Torque's NIF.
#
# Uses the benchmark operation registry so training and measurement exercise
# the same paths. Common traffic is `:hot`, secondary paths are `:warm`, and
# corners are `:cover`; every branch still runs at least once. Keep this file
# dependency-free for release CI.

Code.require_file("ops.exs", __DIR__)

alias Bench.Ops

# Roughly how much of the profile each op should account for. Anything not
# named here is `:cover`.
weights = %{
  # What request/response JSON handling actually does, all day.
  "decode-schema" => :hot,
  "decode-large" => :hot,
  "encode-schema" => :hot,
  "encode-small" => :hot,
  "encode-large" => :hot,
  "extract-small" => :hot,
  "parse-small" => :hot,
  "lookup-compiled" => :hot,
  "encode-ascii-short" => :hot,
  "encode-ascii-long" => :hot,

  # Common, but a minority of traffic.
  "decode-term" => :warm,
  "decode-key-tail" => :warm,
  "decode-utf8" => :warm,
  "decode-escapes" => :warm,
  "decode-numbers" => :warm,
  "encode-utf8" => :warm,
  "encode-escape-late" => :warm,
  "encode-numbers" => :warm,
  "encode-atoms" => :warm,
  "encode-iodata" => :warm,
  "extract-large" => :warm,
  "extract-root" => :warm,
  "extract-small-skip" => :warm,
  "parse-large" => :warm,
  "get-subtree" => :warm,
  "get-narrow" => :warm
}

# Milliseconds of instrumented execution per op. The instrumented build is
# ~20% slower than the final one, so these are budgets rather than promises.
budget = %{hot: 900, warm: 300, cover: 80}

ops = Ops.all()

IO.puts("PGO workload: #{length(ops)} operations from bench/ops.exs")

# Older revisions may not support every current operation; skip those entries.
{elapsed_us, counts} =
  :timer.tc(fn ->
    Enum.map(ops, fn o ->
      weight = Map.get(weights, o.id, :cover)
      target = Map.fetch!(budget, weight)

      try do
        # Powers of two keep counts stable across calibration jitter.
        reps =
          o
          |> Ops.calibrate(target)
          |> then(fn n -> Bitwise.bsl(1, max(round(:math.log2(n)), 0)) end)

        Ops.run(o, reps)
        {o.id, weight}
      rescue
        e ->
          IO.puts("  skipped #{o.id}: #{Exception.message(e) |> String.slice(0, 90)}")
          {o.id, :skipped}
      end
    end)
  end)

tally = Enum.frequencies_by(counts, &elem(&1, 1))

IO.puts(
  "PGO workload complete: " <>
    Enum.map_join([:hot, :warm, :cover, :skipped], ", ", fn w ->
      "#{Map.get(tally, w, 0)} #{w}"
    end) <> " in #{Float.round(elapsed_us / 1_000_000, 1)}s"
)

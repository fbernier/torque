# PGO training workload for the glazer NIF.
#
# The counterpart to bench/pgo_workload.exs, driving glazer instead of torque,
# so a glazer PGO build (deps/glazer c_src Makefile PGO=generate/use) collects
# branch and call-frequency data before the comparison runs. Without it the
# README numbers are PGO-torque against plain glazer.
#
# It trains on **the same bytes** the comparison measures — `bench/fixtures.exs`,
# shared with torque's workload and with bench/torque_bench.exs. It used to
# carry its own hand-written copies of those payloads, which is the same drift
# that made torque's workload untrustworthy: a profile collected on one document
# and a benchmark run on another.
#
# UTF-8 validation is on (`validate_utf8` decode, `force_utf8` encode, both
# default OFF in glazer) to match how bench/torque_bench.exs calls it, so the
# profile matches the benchmarked configuration.
#
# Run with an instrumented glazer.so loaded: MIX_ENV=bench mix run this file.

Code.require_file("fixtures.exs", __DIR__)

alias Bench.Fixtures

request = Fixtures.fetch(:"req-small")
feed = Fixtures.fetch(:"feed-huge")
records = Fixtures.fetch(:"record-schema")
strings = Fixtures.fetch(:"str-utf8")

# glazer decodes to its own term shape, so encode training has to start from
# what glazer itself produced rather than from the fixture's Elixir term.
request_term = :glazer_json.decode(request.json, [:validate_utf8])
feed_term = :glazer_json.decode(feed.json, [:validate_utf8])
records_term = :glazer_json.decode(records.json, [:validate_utf8])

# jq paths equivalent to the JSON Pointers bench/torque_bench.exs extracts.
paths =
  Enum.map(
    [".id", ".site.domain", ".device.ip", ".device.geo.country", ".user.id"],
    &:glazer.compile_path/1
  )

IO.puts("glazer PGO workload: request=#{byte_size(request.json)}B feed=#{byte_size(feed.json)}B")

decode = fn ->
  :glazer_json.decode(request.json, [:validate_utf8])
  :glazer_json.decode(records.json, [:validate_utf8])
  :glazer_json.decode(strings.json, [:validate_utf8])
end

encode = fn ->
  :glazer_json.encode(request_term, [:force_utf8])
  :glazer_json.encode(records_term, [:force_utf8])
end

find = fn ->
  d = :glazer_json.decode(request.json, [:validate_utf8])
  Enum.each(paths, &:glazer.find(d, &1))
end

# The large payload is an order of magnitude bigger, so it gets proportionally
# fewer iterations: this is a weighting, not a coverage checklist.
large = fn ->
  :glazer_json.decode(feed.json, [:validate_utf8])
  :glazer_json.encode(feed_term, [:force_utf8])
end

Enum.each(1..5_000, fn _ -> decode.() end)
Enum.each(1..5_000, fn _ -> encode.() end)
Enum.each(1..10_000, fn _ -> find.() end)
Enum.each(1..50, fn _ -> large.() end)

IO.puts("glazer PGO workload complete")

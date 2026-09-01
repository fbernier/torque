# Wall-clock key-order checks run in their isolated CI job.
# Use `mix test --only perf` locally.
ExUnit.start(exclude: [:perf])

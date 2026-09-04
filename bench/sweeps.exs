# Threshold crossover experiments.
#
#   MIX_ENV=bench mix run bench/sweeps.exs [name ...]
#
# These parameterized payloads set tuning cutoffs; they are not regression or
# library-comparison benchmarks.

alias Benchee.Formatters.Console

pad = fn n, w -> String.pad_leading(Integer.to_string(n), w, "0") end
num = fn n, w -> String.pad_leading(Integer.to_string(n), w) end

# Overridable so a sweep can be smoke-run: BENCH_TIME=1 BENCH_WARMUP=0.
secs = fn var, default ->
  case System.get_env(var) do
    nil -> default
    value -> String.to_integer(value)
  end
end

run = fn title, note, scenarios ->
  IO.puts("\n=== #{title} ===")
  IO.puts("#{note}\n")

  Benchee.run(scenarios,
    warmup: secs.("BENCH_WARMUP", 1),
    time: secs.("BENCH_TIME", 3),
    percentiles: [50, 95, 99],
    formatters: [{Console, percentiles: [50, 95, 99]}]
  )
end

sweeps = %{
  "members" => fn ->
    # MIN_ORDERED_MEMBERS: term order pays only setup; reversed order benefits.
    scenarios =
      for n <- [2, 3, 4, 5, 8, 16, 32, 33], order <- [:term, :reversed], into: %{} do
        pairs = for i <- 1..n, do: {"key_#{pad.(i, 2)}", i}
        sorted = if order == :term, do: Enum.sort(pairs), else: Enum.sort(pairs, :desc)
        json = "{" <> Enum.map_join(sorted, ",", fn {k, v} -> ~s("#{k}":#{v}) end) <> "}"

        {"decode #{num.(n, 2)} members [#{order}]", fn -> Torque.decode!(json) end}
      end

    run.(
      "MEMBER COUNT SWEEP",
      "Sets MIN_ORDERED_MEMBERS (map_order.rs). Where does reordering start to pay?",
      scenarios
    )
  end,
  "extract-members" => fn ->
    # The value_to_term crossover, also swept by key-prefix shape.
    scenarios =
      for n <- [4, 6, 8, 12, 32],
          style <- [:distinct, :prefix],
          order <- [:term, :shuffled, :reversed],
          into: %{} do
        keys =
          case style do
            :distinct -> for i <- 1..n, do: <<?a + rem(i, 26)>> <> pad.(i, 2) <> "x"
            :prefix -> for i <- 1..n, do: "field_#{pad.(i, 2)}"
          end

        keys =
          case order do
            :term -> Enum.sort(keys)
            :reversed -> Enum.sort(keys, :desc)
            :shuffled -> Enum.sort_by(keys, &:erlang.phash2/1)
          end

        row = "{" <> Enum.map_join(keys, ",", fn k -> ~s("#{k}":"#{k}") end) <> "}"
        json = "{\"rows\":[" <> Enum.map_join(1..200, ",", fn _ -> row end) <> "]}"
        {:ok, doc} = Torque.parse(json)

        {"get #{num.(n, 2)} members [#{style}, #{order}]", fn -> Torque.get(doc, "/rows") end}
      end

    run.(
      "EXTRACT MEMBER SWEEP",
      "The same crossover in value_to_term, whose per-object fixed cost is about double.",
      scenarios
    )
  end,
  "accounting" => fn ->
    # Small-input accounting for successful and early/late rejected parses.
    sized = fn n, kind ->
      case kind do
        :valid -> ~s({"a":1}) <> String.duplicate(" ", n - 7)
        :reject_first -> "!" <> String.duplicate(" ", n - 1)
        :reject_last -> String.duplicate(" ", n - 1) <> "!"
      end
    end

    scenarios =
      for n <- [8, 68, 511, 512, 1143, 1600],
          {kind, label} <- [
            valid: "valid",
            reject_first: "rejected at byte 0",
            reject_last: "rejected at the end"
          ],
          into: %{} do
        json = sized.(n, kind)
        {"decode #{num.(n, 4)} B [#{label}]", fn -> Torque.Native.decode(json) end}
      end

    run.(
      "SMALL INPUT ACCOUNTING",
      "Where charging for the work stops being worth the call that charges it.",
      scenarios
    )
  end,
  "wide-lookup" => fn ->
    # Wide-object scan/index crossover by member and path counts.
    scenarios =
      for width <- [64, 128, 512, 2000, 8000], batch <- [1, 4, 8, 32], into: %{} do
        members = for i <- 1..width, do: ~s("f#{pad.(i, 6)}":#{i})
        json = "{" <> Enum.join(members, ",") <> "}"
        {:ok, doc} = Torque.parse(json)
        paths = for i <- 1..batch, do: "/f#{pad.(rem(i * 7, width) + 1, 6)}"

        {"get_many #{num.(width, 4)} members x #{num.(batch, 2)} paths",
         fn -> Torque.get_many_nil(doc, paths) end}
      end

    run.(
      "WIDE OBJECT LOOKUP SWEEP",
      "Sets WIDE_OBJECT_MEMBERS and index_after_visits: scan vs index break-even by width.",
      scenarios
    )
  end,
  "key-bytes" => fn ->
    # Long-key accounting by key length and shared-prefix shape.
    scenarios =
      for key_len <- [8, 64, 255, 256, 1024, 8192], shared <- [true, false], into: %{} do
        members =
          for i <- 1..128 do
            key =
              if shared,
                do: String.duplicate("p", key_len - 6) <> pad.(i, 6),
                else: pad.(i, 6) <> String.duplicate("p", key_len - 6)

            ~s("#{key}":#{i})
          end

        json = "{" <> Enum.join(members, ",") <> "}"
        {:ok, doc} = Torque.parse(json)

        needle =
          if shared,
            do: "/" <> String.duplicate("p", key_len - 6) <> pad.(64, 6),
            else: "/" <> pad.(64, 6) <> String.duplicate("p", key_len - 6)

        {:ok, _} = Torque.get(doc, needle)

        {"get #{num.(key_len, 4)} B keys [#{if shared, do: "shared prefix", else: "distinct"}]",
         fn -> Torque.get(doc, needle) end}
      end

    run.(
      "KEY BYTE SWEEP",
      "Sets LONG_KEY_BYTES and INDEX_KEY_BYTES: cost in bytes compared, not members scanned.",
      scenarios
    )
  end,
  "string-length" => fn ->
    # SHORT_STRING crossover for clean and trailing-escape strings.

    scenarios =
      for len <- [7, 8, 15, 16, 23, 24, 31, 32, 33, 63, 64, 65, 128],
          shape <- [:clean, :escape_at_end],
          into: %{} do
        body = String.duplicate("a", len)
        value = if shape == :clean, do: body, else: binary_part(body, 0, len - 1) <> "\""
        term = for _ <- 1..500, do: %{"k" => value}

        {"encode #{num.(len, 3)} B [#{shape}]", fn -> Torque.encode!(term) end}
      end

    run.(
      "STRING LENGTH SWEEP",
      "Sets SHORT_STRING (escape.rs): where the SIMD chain starts repaying its prologues.",
      scenarios
    )
  end
}

names =
  case System.argv() do
    [] ->
      Map.keys(sweeps)

    given ->
      Enum.each(given, fn n ->
        Map.has_key?(sweeps, n) ||
          raise ArgumentError, "no such sweep: #{n}\nknown: #{Enum.join(Map.keys(sweeps), " ")}"
      end)

      given
  end

Enum.each(names, fn name -> Map.fetch!(sweeps, name).() end)

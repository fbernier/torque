defmodule Torque.DecodeOrderPerfTest do
  # Timing assertions run alone to avoid scheduler contention.
  use ExUnit.Case, async: false

  @moduletag :perf

  @control_ceiling 2.0
  defp obj(pairs) do
    "{" <> Enum.map_join(pairs, ",", fn {k, v} -> ~s("#{k}":#{v}) end) <> "}"
  end

  # Alternate sample order so CPU frequency changes do not favor one side.
  defp order_ratio(term_json, other_json) do
    run = fn json ->
      {us, _} = :timer.tc(fn -> Enum.each(1..20_000, fn _ -> Torque.decode!(json) end) end)
      us
    end

    for json <- [term_json, other_json], _ <- 1..500, do: Torque.decode!(json)
    paired(term_json, other_json, run)
  end

  defp paired(term_side, other_side, run) do
    ratios =
      for round <- 1..5 do
        if rem(round, 2) == 0 do
          other = run.(other_side)
          other / run.(term_side)
        else
          term = run.(term_side)
          run.(other_side) / term
        end
      end

    {Enum.at(Enum.sort(ratios), 2), ratios}
  end

  test "common-prefix record keys share copied terms across hash boundaries" do
    # Escaping the first byte bypasses the key cache but produces exactly the
    # same maps. Comparing retained term size therefore measures key sharing,
    # not a difference in key length, result shape, or timing.
    for len <- [8, 9, 15, 16, 17, 63, 64] do
      keys = for suffix <- ?a..?x, do: String.duplicate("p", len - 1) <> <<suffix>>
      rows = for row <- 1..80, do: Enum.with_index(keys, row * 100)
      json = "[" <> Enum.map_join(rows, ",", &obj/1) <> "]"

      escaped_json =
        "[" <>
          Enum.map_join(rows, ",", fn pairs ->
            obj(
              Enum.map(pairs, fn {key, value} ->
                {"\\u0070" <> binary_part(key, 1, len - 1), value}
              end)
            )
          end) <> "]"

      cached = Torque.decode!(json)
      uncached = Torque.decode!(escaped_json)
      assert cached == Enum.map(rows, &Map.new/1)
      assert uncached == cached

      cached_words = :erts_debug.size(cached)
      uncached_words = :erts_debug.size(uncached)

      assert cached_words < uncached_words * 0.75,
             "#{len}-byte common-prefix keys lost sharing: " <>
               "#{cached_words} vs #{uncached_words} retained words"

      for row <- cached, key <- Map.keys(row) do
        assert :binary.referenced_byte_size(key) == byte_size(key)
      end
    end
  end

  test "equal key endpoints still compare the middle and preserve last duplicates" do
    for len <- [17, 64, 65] do
      middle = String.duplicate("m", len - 17)

      [first, second, third] = [
        "common__a" <> middle <> "tail____",
        "common__b" <> middle <> "tail____",
        "common__" <> middle <> "c" <> "tail____"
      ]

      rows =
        for value <- 1..8 do
          [{first, value}, {second, value + 10}, {third, value + 20}, {first, value + 30}]
        end

      json = "[" <> Enum.map_join(rows, ",", &obj/1) <> "]"
      assert Torque.decode!(json) == Enum.map(rows, &Map.new/1)
    end
  end

  test "unsafe key shapes do not disable ancestor ordering" do
    keys = for i <- 1..32, do: "k#{String.pad_leading(Integer.to_string(i), 2, "0")}"
    short = for k <- keys, do: {k, "1"}
    long = for k <- keys, do: {k, "1234567890"}

    build_nested = fn nested_key, order ->
      pairs = for i <- 1..31, do: {"k#{String.pad_leading(Integer.to_string(i), 2, "0")}", "1"}
      obj(order.([{"k99", ~s({"#{nested_key}":1})} | pairs])) <> "        "
    end

    asc = &Enum.sort_by(&1, fn {k, _} -> k end)
    desc = &Enum.sort_by(&1, fn {k, _} -> k end, :desc)

    cases = [
      {"key in the document's final bytes", obj(Enum.sort(short)), obj(Enum.sort(short, :desc)),
       obj(Enum.sort(long)), obj(Enum.sort(long, :desc))},
      {"escaped key in a child object", build_nested.("a\\u0062c", asc),
       build_nested.("a\\u0062c", desc), build_nested.("abc", asc), build_nested.("abc", desc)}
    ]

    for {label, subject_term, subject_other, control_term, control_other} <- cases do
      {subject_ratio, subject_samples} = order_ratio(subject_term, subject_other)
      {control_ratio, control_samples} = order_ratio(control_term, control_other)

      assert control_ratio < @control_ceiling,
             "reordering disabled for the #{label} control: #{Float.round(control_ratio, 2)}x\n" <>
               "  control: #{inspect(Enum.map(control_samples, &Float.round(&1, 2)))}"

      assert subject_ratio < control_ratio * 1.6,
             "#{label} disabled reordering: #{Float.round(subject_ratio, 2)}x " <>
               "vs control #{Float.round(control_ratio, 2)}x\n" <>
               "  subject: #{inspect(Enum.map(subject_samples, &Float.round(&1, 2)))}\n" <>
               "  control: #{inspect(Enum.map(control_samples, &Float.round(&1, 2)))}"
    end
  end

  test "extracting a subtree keeps its ordering advantage" do
    keys = for i <- 1..24, do: "field_#{String.pad_leading(Integer.to_string(i), 2, "0")}"
    rows = fn order -> for _ <- 1..400, do: order.(for k <- keys, do: {k, "\"#{k}\""}) end

    doc = fn order ->
      json = "{\"rows\":[" <> Enum.map_join(rows.(order), ",", &obj/1) <> "]}"
      {:ok, parsed} = Torque.parse(json)
      parsed
    end

    term_doc = doc.(&Enum.sort/1)
    reversed_doc = doc.(&Enum.sort(&1, :desc))

    run = fn parsed ->
      {us, _} = :timer.tc(fn -> Enum.each(1..200, fn _ -> Torque.get(parsed, "/rows") end) end)
      us
    end

    for parsed <- [term_doc, reversed_doc], _ <- 1..20, do: Torque.get(parsed, "/rows")
    {ratio, samples} = paired(term_doc, reversed_doc, run)

    assert ratio < 1.4,
           "reordering looks disabled in value_to_term: #{Float.round(ratio, 2)}x\n" <>
             "  samples: #{inspect(Enum.map(samples, &Float.round(&1, 2)))}"
  end
end

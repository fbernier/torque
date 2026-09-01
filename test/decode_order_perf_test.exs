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

  test "a key in the document's final bytes still allows reordering" do
    keys = for i <- 1..32, do: "k#{String.pad_leading(Integer.to_string(i), 2, "0")}"

    # The trailing member's value is one byte, so the last key sits inside the
    # document's final eight bytes — where a wide prefix load would read past
    # the end. The long-value document is the control.
    short = for k <- keys, do: {k, "1"}
    long = for k <- keys, do: {k, "1234567890"}

    {short_ratio, short_samples} =
      order_ratio(obj(Enum.sort(short)), obj(Enum.sort(short, :desc)))

    {long_ratio, long_samples} = order_ratio(obj(Enum.sort(long)), obj(Enum.sort(long, :desc)))

    assert long_ratio < @control_ceiling,
           "reordering looks disabled for both documents, not just the short one: " <>
             "control #{Float.round(long_ratio, 2)}x\n" <>
             "  long: #{inspect(Enum.map(long_samples, &Float.round(&1, 2)))}"

    assert short_ratio < long_ratio * 1.6,
           "short trailing value disabled reordering: #{Float.round(short_ratio, 2)}x " <>
             "vs control #{Float.round(long_ratio, 2)}x\n" <>
             "  short: #{inspect(Enum.map(short_samples, &Float.round(&1, 2)))}\n" <>
             "  long:  #{inspect(Enum.map(long_samples, &Float.round(&1, 2)))}"
  end

  test "an escaped key in a child object does not disable its ancestors" do
    build = fn nested_key, order ->
      pairs = for i <- 1..31, do: {"k#{String.pad_leading(Integer.to_string(i), 2, "0")}", "1"}
      pairs = [{"k99", ~s({"#{nested_key}":1})} | pairs]
      obj(order.(pairs)) <> "        "
    end

    asc = &Enum.sort_by(&1, fn {k, _} -> k end)
    desc = &Enum.sort_by(&1, fn {k, _} -> k end, :desc)

    {escaped_ratio, escaped_samples} =
      order_ratio(build.("a\\u0062c", asc), build.("a\\u0062c", desc))

    {plain_ratio, plain_samples} = order_ratio(build.("abc", asc), build.("abc", desc))

    assert plain_ratio < @control_ceiling,
           "reordering looks disabled for both documents, not just the escaped one: " <>
             "control #{Float.round(plain_ratio, 2)}x\n" <>
             "  plain: #{inspect(Enum.map(plain_samples, &Float.round(&1, 2)))}"

    assert escaped_ratio < plain_ratio * 1.6,
           "nested escaped key disabled parent reordering: #{Float.round(escaped_ratio, 2)}x " <>
             "vs control #{Float.round(plain_ratio, 2)}x\n" <>
             "  escaped: #{inspect(Enum.map(escaped_samples, &Float.round(&1, 2)))}\n" <>
             "  plain:   #{inspect(Enum.map(plain_samples, &Float.round(&1, 2)))}"
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

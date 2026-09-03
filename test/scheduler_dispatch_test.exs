defmodule Torque.SchedulerDispatchTest do
  # Scheduler counters are VM-wide, so this module must run synchronously.
  use ExUnit.Case, async: false

  @moduletag :perf

  test "every entry point with a path dispatches on its size" do
    {:ok, doc} = Torque.parse(~s({"a":[1,2,3]}))
    long = "/" <> String.duplicate("~2", 2 * 1024 * 1024)
    normal = :erlang.system_info(:schedulers)
    dirty_cpu = :erlang.system_info(:dirty_cpu_schedulers)
    previous = :erlang.system_flag(:scheduler_wall_time, true)

    sample = fn ->
      # Wall-time samples are unsorted; identify dirty CPU schedulers by ID.
      :erlang.statistics(:scheduler_wall_time_all)
      |> Enum.reduce({0, 0}, fn {id, active, total}, {sum_active, sum_total} = sums ->
        if id > normal and id <= normal + dirty_cpu do
          {sum_active + active, sum_total + total}
        else
          sums
        end
      end)
    end

    # Convert OTP's unspecified wall-time unit using the current sample window.
    ran_dirty? = fn fun ->
      # Reclaim resources from the previous probe before sampling.
      :erlang.garbage_collect()
      Process.sleep(5)
      {before_active, before_total} = sample.()
      {elapsed_us, _} = :timer.tc(fun)
      {active, total} = sample.()

      time_units_per_us = (total - before_total) / (dirty_cpu * max(elapsed_us, 1))

      assert time_units_per_us > 0, "dirty scheduler wall time did not advance"

      # Require at least 0.5 ms of dirty CPU work.
      active - before_active > time_units_per_us * 500
    end

    many = for i <- 1..20_000, do: "/field_#{i}"
    # A compiled long path still incurs key-comparison work.
    long_handle = Torque.compile_pointers([long])
    short_handle = Torque.compile_pointers(["/a"])

    try do
      refute ran_dirty?.(fn -> Torque.get(doc, "/a") end), "a short path stayed put"

      # Repeat cheap work so an accidental dirty dispatch is measurable.
      refute ran_dirty?.(fn ->
               Enum.each(1..10_000, fn _ -> Torque.compile_pointers(["/a", "/b"]) end)
             end),
             "a two-path compile stayed put"

      assert ran_dirty?.(fn -> Torque.get(doc, long) end), "get/2"
      assert ran_dirty?.(fn -> Torque.get(doc, long, :default) end), "get/3"
      assert ran_dirty?.(fn -> Torque.length(doc, long) end), "length/2"
      assert ran_dirty?.(fn -> Torque.get_many(doc, [long]) end), "get_many/2"
      assert ran_dirty?.(fn -> Torque.get_many_nil(doc, [long]) end), "get_many_nil/2"
      assert ran_dirty?.(fn -> Torque.get_many_defaults(doc, %{long => 1}) end), "defaults"
      assert ran_dirty?.(fn -> Torque.compile_pointers(many) end), "compile_pointers/2"

      # Repeat cheap compiled lookups so scheduler time is measurable.
      repeat = fn fun -> fn -> Enum.each(1..2000, fn _ -> fun.() end) end end

      refute ran_dirty?.(repeat.(fn -> Torque.get_many_nil(doc, short_handle) end)),
             "a short compiled handle stayed put"

      assert ran_dirty?.(repeat.(fn -> Torque.get_many(doc, long_handle) end)),
             "get_many/2 compiled"

      assert ran_dirty?.(repeat.(fn -> Torque.get_many_nil(doc, long_handle) end)),
             "get_many_nil/2 compiled"

      assert ran_dirty?.(repeat.(fn -> Torque.parse_get_many_nil("{}", long_handle) end)),
             "parse_get_many_nil/2 compiled"
    after
      :erlang.system_flag(:scheduler_wall_time, previous)
    end
  end
end

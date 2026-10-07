defmodule Torque.CPUTest do
  use ExUnit.Case, async: true

  alias Torque.CPU

  # A Haswell host as Linux reports it, trimmed to the flags that matter.
  @haswell "flags\t\t: fpu sse sse2 pni pclmulqdq ssse3 fma cx16 sse4_1 sse4_2 movbe popcnt " <>
             "avx f16c lahf_lm abm bmi1 avx2 bmi2"

  defp cpuinfo(flags), do: "processor\t: 0\nmodel name\t: test\n#{flags}\npower management:\n"

  test "a host with every v3 flag qualifies for both variants" do
    assert CPU.cpuinfo_has_flags?(cpuinfo(@haswell), CPU.v3_flags())
    assert CPU.cpuinfo_has_flags?(cpuinfo(@haswell), CPU.v2_flags())
  end

  test "AVX2 without the rest of v3 falls back to v2" do
    masked = String.replace(@haswell, " bmi2", "")
    refute CPU.cpuinfo_has_flags?(cpuinfo(masked), CPU.v3_flags())
    assert CPU.cpuinfo_has_flags?(cpuinfo(masked), CPU.v2_flags())
  end

  test "SSE4.2 without the rest of v2 falls back to baseline" do
    masked = String.replace(@haswell, " cx16", "")
    refute CPU.cpuinfo_has_flags?(cpuinfo(masked), CPU.v2_flags())
  end

  test "flags are matched as whole words" do
    refute CPU.cpuinfo_has_flags?(cpuinfo("flags\t\t: avx2x sse4_2x"), ["avx2"])
  end
end

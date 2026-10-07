defmodule Torque.CPU do
  @moduledoc false

  # The /proc/cpuinfo flags behind each x86-64 level the release builds
  # target (`abm` is how Linux reports LZCNT). Checking AVX2 alone is not
  # enough: some hypervisors expose AVX2 while masking BMI2 or FMA, and a v3
  # binary then dies with SIGILL.
  @v2_flags ~w(pni ssse3 sse4_1 sse4_2 popcnt cx16 lahf_lm)
  @v3_flags @v2_flags ++ ~w(avx avx2 bmi1 bmi2 f16c fma abm movbe pclmulqdq)

  def avx2? do
    case System.get_env("TORQUE_CPU_VARIANT") do
      "v3" -> true
      nil -> detect(@v3_flags, "hw.optional.avx2_0")
      _ -> false
    end
  end

  def sse42? do
    case System.get_env("TORQUE_CPU_VARIANT") do
      "v2" -> true
      nil -> detect(@v2_flags, "hw.optional.sse4_2")
      _ -> false
    end
  end

  # Every Mac with AVX2 (or SSE4.2) has the rest of that level too, including
  # PCLMULQDQ, which has no hw.optional sysctl.
  defp detect(linux_flags, darwin_sysctl) do
    case :os.type() do
      {:unix, :linux} -> linux_has_flags?(linux_flags)
      {:unix, :darwin} -> darwin_has_feature?(darwin_sysctl)
      _ -> false
    end
  end

  defp linux_has_flags?(wanted) do
    case File.read("/proc/cpuinfo") do
      {:ok, content} -> cpuinfo_has_flags?(content, wanted)
      _ -> false
    end
  end

  @doc false
  def cpuinfo_has_flags?(content, wanted) do
    content
    |> String.split("\n")
    |> Enum.any?(fn line ->
      String.starts_with?(line, "flags") and wanted -- String.split(line) == []
    end)
  end

  @doc false
  def v2_flags, do: @v2_flags

  @doc false
  def v3_flags, do: @v3_flags

  defp darwin_has_feature?(name) do
    case System.cmd("sysctl", ["-n", name], stderr_to_stdout: true) do
      {"1\n", 0} -> true
      _ -> false
    end
  end
end

defmodule Torque.CPU do
  @moduledoc false

  def avx2? do
    case System.get_env("TORQUE_CPU_VARIANT") do
      "v3" -> true
      nil -> detect(~w(avx2 pclmulqdq), "hw.optional.avx2_0")
      _ -> false
    end
  end

  def sse42? do
    case System.get_env("TORQUE_CPU_VARIANT") do
      "v2" -> true
      nil -> detect(~w(sse4_2), "hw.optional.sse4_2")
      _ -> false
    end
  end

  # Every AVX2 Mac also has PCLMULQDQ, which has no hw.optional sysctl.
  defp detect(linux_flags, darwin_sysctl) do
    case :os.type() do
      {:unix, :linux} -> linux_has_flags?(linux_flags)
      {:unix, :darwin} -> darwin_has_feature?(darwin_sysctl)
      _ -> false
    end
  end

  defp linux_has_flags?(wanted) do
    case File.read("/proc/cpuinfo") do
      {:ok, content} ->
        content
        |> String.split("\n")
        |> Enum.any?(fn line ->
          String.starts_with?(line, "flags") and wanted -- String.split(line) == []
        end)

      _ ->
        false
    end
  end

  defp darwin_has_feature?(name) do
    case System.cmd("sysctl", ["-n", name], stderr_to_stdout: true) do
      {"1\n", 0} -> true
      _ -> false
    end
  end
end

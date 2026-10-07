defmodule Torque.FetchNifTest do
  use ExUnit.Case, async: true

  # rebar/fetch_nif.escript picks the release asset for a target and CPU.
  # TORQUE_NIF_TARGET and TORQUE_CPUINFO stand in for the host, so every
  # branch of the selection runs here whatever the test machine is.

  @moduletag :tmp_dir

  # A Haswell host as Linux reports it, trimmed to the flags that matter.
  @haswell "flags\t\t: fpu sse sse2 pni pclmulqdq ssse3 fma cx16 sse4_1 sse4_2 movbe popcnt " <>
             "avx f16c lahf_lm abm bmi1 avx2 bmi2"

  defp asset(env) do
    env = [{"TORQUE_CPU_VARIANT", nil}, {"TORQUE_NIF_VSN", "1.2.3"} | env]

    {out, 0} =
      System.cmd("escript", ["rebar/fetch_nif.escript", "--print-asset"], env: env)

    String.trim(out)
  end

  defp x86(tmp_dir, flags, env \\ []) do
    cpuinfo = Path.join(tmp_dir, "cpuinfo")
    File.write!(cpuinfo, "processor\t: 0\n#{flags}\npower management:\n")

    asset([
      {"TORQUE_NIF_TARGET", "x86_64-unknown-linux-gnu"},
      {"TORQUE_CPUINFO", cpuinfo} | env
    ])
  end

  test "x86_64 picks the highest level whose every flag is present", %{tmp_dir: dir} do
    assert x86(dir, @haswell) ==
             "libtorque_nif-v1.2.3-nif-2.15-x86_64-unknown-linux-gnu--v3.so.tar.gz"

    assert x86(dir, String.replace(@haswell, " bmi2", "")) =~ ~r/linux-gnu--v2\.so\.tar\.gz$/
    assert x86(dir, String.replace(@haswell, " cx16", "")) =~ ~r/linux-gnu\.so\.tar\.gz$/
  end

  test "flags match as whole words", %{tmp_dir: dir} do
    assert x86(dir, "flags\t\t: avx2x sse4_2x") =~ ~r/linux-gnu\.so\.tar\.gz$/
  end

  test "TORQUE_CPU_VARIANT overrides detection", %{tmp_dir: dir} do
    assert x86(dir, @haswell, [{"TORQUE_CPU_VARIANT", "v2"}]) =~ ~r/--v2\.so\.tar\.gz$/
    assert x86(dir, "flags\t\t: fpu", [{"TORQUE_CPU_VARIANT", "v3"}]) =~ ~r/--v3\.so\.tar\.gz$/
    assert x86(dir, @haswell, [{"TORQUE_CPU_VARIANT", "base"}]) =~ ~r/linux-gnu\.so\.tar\.gz$/
  end

  test "variants apply to x86_64 only", %{tmp_dir: dir} do
    cpuinfo = Path.join(dir, "cpuinfo")
    File.write!(cpuinfo, @haswell)

    for target <- ["aarch64-unknown-linux-gnu", "aarch64-apple-darwin"] do
      assert asset([{"TORQUE_NIF_TARGET", target}, {"TORQUE_CPUINFO", cpuinfo}]) ==
               "libtorque_nif-v1.2.3-nif-2.15-#{target}.so.tar.gz"
    end
  end

  test "an unsupported target has no asset" do
    assert asset([{"TORQUE_NIF_TARGET", "riscv64-unknown-linux-gnu"}]) ==
             "none: unsupported platform riscv64-unknown-linux-gnu"
  end

  test "the version defaults to the app file's" do
    {out, 0} =
      System.cmd("escript", ["rebar/fetch_nif.escript", "--print-asset"],
        env: [{"TORQUE_NIF_VSN", nil}, {"TORQUE_NIF_TARGET", "aarch64-apple-darwin"}]
      )

    assert out =~ "libtorque_nif-v#{Mix.Project.config()[:version]}-nif-2.15-"
  end
end

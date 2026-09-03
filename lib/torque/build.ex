defmodule Torque.Build do
  @moduledoc false

  # Whether the NIF is compiled from Rust source instead of downloaded as a
  # published precompiled binary.
  #
  # The flag is captured when `Torque.Native` compiles, so a `_build` tree made
  # without it keeps loading a downloaded NIF however later commands are
  # invoked - silently running a released binary against local Rust changes.
  # `__mix_recompile__?/0` makes the variable part of this module's staleness,
  # and `Torque.Native` calls `force_build?/0` in its body, so recompiling this
  # module recompiles that one.
  #
  # This cannot live in `Torque.Native`: when its NIF fails to load the module
  # is not loadable at all, and Mix cannot ask an unloadable module anything.
  @captured System.get_env("TORQUE_BUILD")
  @force_build @captured in ["1", "true"]

  def force_build?, do: @force_build

  @doc false
  def __mix_recompile__?, do: System.get_env("TORQUE_BUILD") != @captured
end

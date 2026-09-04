defmodule Torque.Native do
  @moduledoc false

  version = Mix.Project.config()[:version]

  # `Torque.Build` owns this flag so that switching TORQUE_BUILD recompiles
  # this module; see the note there.
  use RustlerPrecompiled,
    otp_app: :torque,
    crate: "torque_nif",
    base_url: "https://github.com/lpgauth/torque/releases/download/v#{version}",
    force_build: Torque.Build.force_build?(),
    targets: ~w(
      aarch64-apple-darwin
      aarch64-unknown-linux-gnu
      x86_64-apple-darwin
      x86_64-unknown-linux-gnu
    ),
    nif_versions: ["2.15"],
    version: version,
    variants: %{
      "x86_64-unknown-linux-gnu" => [
        v3: &Torque.CPU.avx2?/0,
        v2: &Torque.CPU.sse42?/0
      ],
      "x86_64-apple-darwin" => [
        v3: &Torque.CPU.avx2?/0,
        v2: &Torque.CPU.sse42?/0
      ]
    }

  def parse(_json), do: :erlang.nif_error(:nif_not_loaded)
  def parse_dirty(_json), do: :erlang.nif_error(:nif_not_loaded)
  def parse_opts(_json, _unique_keys), do: :erlang.nif_error(:nif_not_loaded)
  def parse_opts_dirty(_json, _unique_keys), do: :erlang.nif_error(:nif_not_loaded)
  def get(_doc, _path), do: :erlang.nif_error(:nif_not_loaded)
  def get_dirty(_doc, _path), do: :erlang.nif_error(:nif_not_loaded)
  def get_many(_doc, _paths), do: :erlang.nif_error(:nif_not_loaded)
  def get_many_dirty(_doc, _paths), do: :erlang.nif_error(:nif_not_loaded)
  def decode(_json), do: :erlang.nif_error(:nif_not_loaded)
  def decode_dirty(_json), do: :erlang.nif_error(:nif_not_loaded)
  # The NIF API cannot query binary size without possibly copying an unaligned
  # sub-binary. Inspect metadata on the BEAM first, where traversal is preemptible
  # and byte_size/1 and integer comparisons do not materialize their operands.
  # Bound the walk too: external_size/1 does not yield on supported older OTPs.
  # Wide inputs go dirty after one discovery budget, not a hard-limit traversal.
  # One integer of fuel avoids allocating a counter tuple for every term.
  # Charging 16 per node also limits the walk to at most 1280 nodes.
  @inspect_work 20_480
  @inspect_node_cost 16
  @inspect_integer_limit Integer.pow(2, 512)

  def encode(term) do
    if inspectable?(term), do: encode_checked(term), else: :dirty_required
  end

  defp encode_checked(_term), do: :erlang.nif_error(:nif_not_loaded)
  def encode_dirty(_term), do: :erlang.nif_error(:nif_not_loaded)
  def encode_finish_dirty(_term, _partial, _next), do: :erlang.nif_error(:nif_not_loaded)
  def encode_iodata_finish_dirty(_term, _partial, _next), do: :erlang.nif_error(:nif_not_loaded)

  def encode_iodata(term) do
    if inspectable?(term), do: encode_iodata_checked(term), else: :dirty_required
  end

  defp encode_iodata_checked(_term), do: :erlang.nif_error(:nif_not_loaded)
  def encode_iodata_dirty(_term), do: :erlang.nif_error(:nif_not_loaded)
  def get_many_nil(_doc, _paths), do: :erlang.nif_error(:nif_not_loaded)
  def get_many_nil_dirty(_doc, _paths), do: :erlang.nif_error(:nif_not_loaded)
  def get_many_defaults(_doc, _defaults), do: :erlang.nif_error(:nif_not_loaded)
  def get_many_defaults_dirty(_doc, _defaults), do: :erlang.nif_error(:nif_not_loaded)
  def compile_paths(_paths, _unique_keys, _validate), do: :erlang.nif_error(:nif_not_loaded)

  def compile_paths_dirty(_paths, _unique_keys, _validate),
    do: :erlang.nif_error(:nif_not_loaded)

  def get_many_compiled(_doc, _compiled), do: :erlang.nif_error(:nif_not_loaded)
  def get_many_compiled_dirty(_doc, _compiled), do: :erlang.nif_error(:nif_not_loaded)

  def get_many_nil_compiled(_doc, _compiled), do: :erlang.nif_error(:nif_not_loaded)
  def get_many_nil_compiled_dirty(_doc, _compiled), do: :erlang.nif_error(:nif_not_loaded)
  def parse_get_many_nil(_json, _compiled, _alloc_len), do: :erlang.nif_error(:nif_not_loaded)

  def parse_get_many_nil_dirty(_json, _compiled, _alloc_len),
    do: :erlang.nif_error(:nif_not_loaded)

  def array_length(_doc, _path), do: :erlang.nif_error(:nif_not_loaded)
  def array_length_dirty(_doc, _path), do: :erlang.nif_error(:nif_not_loaded)

  defp inspectable?(term), do: inspect_term(term, @inspect_work) >= 0

  defp inspect_term(_, fuel) when fuel < @inspect_node_cost, do: -1

  defp inspect_term(term, fuel) when is_binary(term),
    do: fuel - @inspect_node_cost - byte_size(term)

  defp inspect_term(term, fuel) when is_integer(term) do
    if term >= -@inspect_integer_limit and term <= @inspect_integer_limit,
      do: fuel - @inspect_node_cost,
      else: -1
  end

  defp inspect_term([head | tail], fuel),
    do: inspect_term(tail, inspect_term(head, fuel - @inspect_node_cost))

  defp inspect_term(term, fuel) when is_map(term) do
    fuel = fuel - @inspect_node_cost

    # Even empty keys and scalar values cost two nodes per entry. Reject maps
    # that cannot fit before allocating their iterator or inspecting any member.
    if map_size(term) * (2 * @inspect_node_cost) > fuel,
      do: -1,
      else: inspect_map(:maps.iterator(term), fuel)
  end

  # Only {proplist} tuples are traversed by the encoder. Unsupported tuples
  # must not turn this guard into a walk over otherwise irrelevant terms.
  defp inspect_term({pairs}, fuel), do: inspect_pairs(pairs, fuel - @inspect_node_cost)
  defp inspect_term(_, fuel), do: fuel - @inspect_node_cost

  defp inspect_map(_, fuel) when fuel < @inspect_node_cost, do: -1

  defp inspect_map(iter, fuel) do
    case :maps.next(iter) do
      {key, value, next} -> inspect_map(next, inspect_term(value, inspect_term(key, fuel)))
      :none -> fuel
    end
  end

  defp inspect_pairs(_, fuel) when fuel < @inspect_node_cost, do: -1

  defp inspect_pairs([{key, value} | tail], fuel),
    do: inspect_pairs(tail, inspect_term(value, inspect_term(key, fuel - @inspect_node_cost)))

  defp inspect_pairs(_, fuel), do: fuel
end

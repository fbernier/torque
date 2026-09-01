defmodule Torque.DecodeTest do
  use ExUnit.Case, async: true

  describe "decode/1" do
    test "string" do
      assert {:ok, "hello"} = Torque.decode(~s("hello"))
    end

    test "integer" do
      assert {:ok, 42} = Torque.decode("42")
    end

    test "negative integer" do
      assert {:ok, -1} = Torque.decode("-1")
    end

    test "float" do
      assert {:ok, 3.14} = Torque.decode("3.14")
    end

    test "true" do
      assert {:ok, true} = Torque.decode("true")
    end

    test "false" do
      assert {:ok, false} = Torque.decode("false")
    end

    test "null" do
      assert {:ok, nil} = Torque.decode("null")
    end

    test "empty object" do
      assert {:ok, %{}} = Torque.decode("{}")
    end

    test "empty array" do
      assert {:ok, []} = Torque.decode("[]")
    end

    test "object with string values" do
      assert {:ok, %{"a" => "b", "c" => "d"}} = Torque.decode(~s({"a":"b","c":"d"}))
    end

    test "nested object" do
      json = ~s({"site":{"domain":"example.com"}})
      assert {:ok, %{"site" => %{"domain" => "example.com"}}} = Torque.decode(json)
    end

    test "array of integers" do
      assert {:ok, [1, 2, 3]} = Torque.decode("[1,2,3]")
    end

    test "array of objects" do
      json = ~s([{"id":1},{"id":2}])
      assert {:ok, [%{"id" => 1}, %{"id" => 2}]} = Torque.decode(json)
    end

    test "unicode string" do
      assert {:ok, "\u00e9\u00e8\u00ea"} = Torque.decode(~s("\u00e9\u00e8\u00ea"))
    end

    test "escaped characters" do
      assert {:ok, "line1\nline2"} = Torque.decode(~s("line1\\nline2"))
    end

    test "large integer (i64 max)" do
      assert {:ok, 9_223_372_036_854_775_807} = Torque.decode("9223372036854775807")
    end

    test "large integer (u64)" do
      assert {:ok, 9_223_372_036_854_775_808} = Torque.decode("9223372036854775808")
    end

    test "duplicate keys - last value wins" do
      assert {:ok, %{"a" => 2}} = Torque.decode(~s({"a":1,"a":2}))
    end

    test "duplicate keys in nested object - last value wins" do
      assert {:ok, %{"x" => %{"a" => 2}}} = Torque.decode(~s({"x":{"a":1,"a":2}}))
    end

    test "duplicate keys with different value types" do
      assert {:ok, %{"k" => "str"}} = Torque.decode(~s({"k":1,"k":true,"k":"str"}))
    end

    test "invalid json returns error" do
      assert {:error, _reason} = Torque.decode("{invalid}")
    end

    test "large payload uses dirty scheduler" do
      # Generate a payload > 10KB to exercise the dirty scheduler path
      large_map = Map.new(1..500, fn i -> {"key_#{i}", String.duplicate("v", 20)} end)
      json = Jason.encode!(large_map)
      assert byte_size(json) > 10_240
      assert {:ok, decoded} = Torque.decode(json)
      assert decoded == large_map
    end
  end

  describe "decode!/1" do
    test "valid json" do
      assert %{"a" => 1} = Torque.decode!(~s({"a":1}))
    end

    test "invalid json raises" do
      assert_raise ArgumentError, fn ->
        Torque.decode!("{invalid}")
      end
    end
  end

  # Objects are handed to ERTS pre-sorted into Erlang term order (see
  # native/torque_nif/src/map_order.rs). These pin the cases where the
  # reordering must still produce exactly what an unordered build would.
  describe "object key ordering" do
    test "member order does not affect the decoded map" do
      # Keys covering every case the eight-byte prefix cannot settle alone:
      # a shared prefix, one key extending another, an embedded NUL, an
      # escaped key (which disables reordering for its object), and the
      # empty key.
      pairs = [
        {"zone", "z"},
        {"id", 1},
        {"created_at", "t"},
        {"created_by", "u"},
        {"created_byte", "v"},
        {"a\\u0000b", true},
        {"esc\\u0062", 3},
        {"", "empty"},
        {"a_very_long_key_that_exceeds_eight_bytes_and_then_some", 2}
      ]

      expected = %{
        "zone" => "z",
        "id" => 1,
        "created_at" => "t",
        "created_by" => "u",
        "created_byte" => "v",
        <<?a, 0, ?b>> => true,
        "escb" => 3,
        "" => "empty",
        "a_very_long_key_that_exceeds_eight_bytes_and_then_some" => 2
      }

      # A fixed rotation rather than Enum.shuffle/1: a failure has to be
      # reproducible from the file, not from the run's seed.
      rotated = Enum.drop(pairs, 3) ++ Enum.take(pairs, 3)

      for order <- [pairs, Enum.reverse(pairs), Enum.sort(pairs), rotated] do
        json =
          "{" <> Enum.map_join(order, ",", fn {k, v} -> ~s("#{k}":#{Jason.encode!(v)}) end) <> "}"

        assert Torque.decode!(json) == expected,
               "failed for order #{inspect(Enum.map(order, &elem(&1, 0)))}"
      end
    end

    test "duplicate keys keep last-value-wins across any order" do
      assert Torque.decode!(~s({"z":1,"a":2,"z":3})) == %{"a" => 2, "z" => 3}
      assert Torque.decode!(~s({"a":1,"z":2,"a":3})) == %{"a" => 3, "z" => 2}
    end

    test "objects too large for a flatmap decode correctly unordered" do
      big = Map.new(1..40, fn i -> {"k#{100 - i}", i} end)

      assert Torque.decode!(desc_obj(big)) == big
    end

    # Above 32 members ERTS builds a hash map, and its duplicate-key rejection
    # was not reliable there (erlang/otp#10975): duplicates were collapsed
    # silently into a map still shaped like a hash map, which then compares
    # unequal to the same pairs built in Erlang. Both conversion paths check
    # what came back rather than trusting the return value, so both are
    # compared against a map the VM built itself — equality alone would miss a
    # representation that only matching can tell apart.
    test "duplicate keys in a hashmap-sized object still give the Erlang map" do
      pairs = for i <- 1..40, do: {"k#{String.pad_leading(Integer.to_string(i), 2, "0")}", i}
      dup = pairs ++ [{"k01", 999}, {"k40", 998}]
      json = obj(dup)
      expected = Enum.reduce(dup, %{}, fn {k, v}, acc -> Map.put(acc, k, v) end)

      decoded = Torque.decode!(json)
      assert decoded == expected
      assert map_size(decoded) == 40
      assert ^expected = decoded

      {:ok, doc} = Torque.parse(json)
      assert {:ok, got} = Torque.get(doc, "")
      assert got == expected
      assert ^expected = got
    end

    test "get/2 returns an object subtree regardless of member order" do
      json = ~s({"o":{"zone":1,"id":2,"created_at":3,"a":4}})
      {:ok, doc} = Torque.parse(json)

      assert Torque.get(doc, "/o") ==
               {:ok, %{"zone" => 1, "id" => 2, "created_at" => 3, "a" => 4}}
    end

    # A repeated shape is reordered from a memo of the last permutation for its
    # key prefixes, so shapes have to survive arriving interleaved and repeated.
    # The order chosen is only observable through duplicate keys, which
    # `make_map` resolves by rebuilding in the order it was handed.
    test "interleaved shapes keep their own order, with duplicates unaffected" do
      shapes = [
        %{"zone" => 1, "id" => 2, "created_at" => 3},
        %{"url" => 4, "text" => 5, "indices" => 6},
        %{"screen_name" => 7, "name" => 8, "id_str" => 9, "id" => 10}
      ]

      rows = for _ <- 1..8, shape <- shapes, do: shape
      json = "[" <> Enum.map_join(rows, ",", &desc_obj/1) <> "]"
      assert Torque.decode!(json) == rows

      body = ~s({"zone":1,"dup":2,"a":3,"dup":4})
      repeated = "[" <> Enum.map_join(1..64, ",", fn _ -> body end) <> "]"

      assert Torque.decode!(repeated) ==
               List.duplicate(%{"zone" => 1, "dup" => 4, "a" => 3}, 64)
    end

    # Two shapes whose keys agree on their first eight bytes share a memo entry,
    # so the second object can be handed the first one's permutation. That is
    # allowed to cost ERTS a re-sort, but it must not decide which duplicate
    # wins: `make_map` resolves duplicates from document order.
    test "a permutation borrowed from a colliding shape does not reorder duplicates" do
      first = ~s({"zz987654d":0,"zz987654c":0,"zz987654b":0,"zz987654a":0})
      dup = ~s({"zz987654d":1,"zz987654c":0,"zz987654b":0,"zz987654d":2})

      assert Torque.decode!("[" <> first <> "," <> dup <> "]") == [
               %{"zz987654a" => 0, "zz987654b" => 0, "zz987654c" => 0, "zz987654d" => 0},
               %{"zz987654b" => 0, "zz987654c" => 0, "zz987654d" => 2}
             ]
    end

    defp obj(pairs) do
      "{" <> Enum.map_join(pairs, ",", fn {k, v} -> ~s("#{k}":#{v}) end) <> "}"
    end

    defp desc_obj(map), do: obj(Enum.sort(map, :desc))
  end
end

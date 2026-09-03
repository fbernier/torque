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

  # Key ordering is observable only when duplicate handling falls back to
  # source-order insertion. Cover both ERTS representations and cache collision.
  describe "object key ordering" do
    # Large maps exercise the OTP duplicate-collapse workaround in both decoders.
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

    # Equal prefixes share a cache shape; a reused permutation must not change
    # which duplicate wins.
    test "a colliding shape's permutation does not reorder duplicates" do
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
  end

  describe "numbers" do
    test "root integers stay exact across the finite-float and stack-bignum limits" do
      # Finite fallback, overflow recovery, and the heap-backed integer converter.
      for value <- [
            Integer.pow(10, 308) + 7,
            -Integer.pow(10, 309) - 7,
            Integer.pow(10, 700) + 7
          ] do
        assert {:ok, ^value} = Torque.decode(Integer.to_string(value))
      end
    end

    test "overflowed integers remain exact inside arrays and objects" do
      value = Integer.pow(10, 400) + 7
      negative = -value
      json = ~s({"values":[#{value},{"negative":#{negative}}],"negative":#{negative}})

      assert {:ok, %{"values" => [^value, %{"negative" => ^negative}], "negative" => ^negative}} =
               Torque.decode(json)
    end

    test "huge malformed numbers and trailing junk are not recovered as integers" do
      huge = String.duplicate("9", 700)

      for json <- [
            "0" <> huge,
            "-" <> huge <> ".",
            huge <> "e+",
            huge <> "x",
            "[" <> huge <> " false]",
            ~s({"value":#{huge}x})
          ] do
        assert {:error, _} = Torque.decode(json)
      end
    end

    test "overflow recovery does not accept nonfinite decimal or exponent floats" do
      huge = String.duplicate("9", 400)

      for json <- ["1e309", "[-" <> huge <> ".0]", ~s({"value":#{huge}e0})] do
        assert {:error, _} = Torque.decode(json)
      end

      assert {:ok, 1.0e308} = Torque.decode("1e308")
      assert {:ok, 1.0} = Torque.decode("1" <> String.duplicate("0", 400) <> "e-400")
    end

    test "DOM parsing still rejects integers beyond its finite-float representation" do
      huge = "1" <> String.duplicate("0", 400)

      assert {:error, _} = Torque.parse(huge)
      assert {:error, _} = Torque.parse(~s({"value":#{huge}}))
    end

    test "every decoding strategy keeps the sign of a negative zero" do
      # Sonic-number loses the sign; every conversion path must restore it.
      json = ~s({"z":-0.0})
      neg = <<-0.0::float-64>>

      assert <<Torque.decode!(json)["z"]::float-64>> == neg

      {:ok, doc} = Torque.parse(json)
      assert {:ok, got} = Torque.get(doc, "/z")
      assert <<got::float-64>> == neg

      ptrs = Torque.compile_pointers(["/z"])
      assert [from_doc] = Torque.get_many_nil(doc, ptrs)
      assert <<from_doc::float-64>> == neg
      assert {:ok, [fused]} = Torque.parse_get_many_nil(json, ptrs)
      assert <<fused::float-64>> == neg
    end
  end
end

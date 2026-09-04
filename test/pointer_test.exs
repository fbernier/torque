defmodule Torque.PointerTest do
  use ExUnit.Case, async: true

  @sample_json ~s({
    "id": "req-123",
    "site": {
      "domain": "example.com",
      "page": "https://example.com/page",
      "publisher": {"id": "pub-456"}
    },
    "device": {
      "devicetype": 2,
      "ua": "Mozilla/5.0",
      "ip": "1.2.3.4",
      "geo": {
        "country": "US",
        "lat": 40.7128,
        "lon": -74.006,
        "region": "NY",
        "type": 2,
        "zip": "10001"
      }
    },
    "user": {
      "id": "user-789",
      "buyeruid": "buyer-abc",
      "ext": {
        "eids": [
          {"source": "adserver.org", "uids": [{"id": "uid-1"}]},
          {"source": "criteo.com", "uids": [{"id": "uid-2"}]}
        ]
      }
    },
    "imp": [
      {
        "id": "imp-1",
        "banner": {"w": 300, "h": 250, "pos": 1},
        "bidfloor": 0.5,
        "pmp": {
          "private_auction": 1,
          "deals": [{"id": "deal-1"}, {"id": "deal-2"}]
        }
      }
    ],
    "regs": {"coppa": 0}
  })

  setup do
    {:ok, doc} = Torque.parse(@sample_json)
    %{doc: doc}
  end

  describe "parse/1 + get/2" do
    test "string field", %{doc: doc} do
      assert {:ok, "req-123"} = Torque.get(doc, "/id")
    end

    test "nested string field", %{doc: doc} do
      assert {:ok, "example.com"} = Torque.get(doc, "/site/domain")
    end

    test "deeply nested string", %{doc: doc} do
      assert {:ok, "pub-456"} = Torque.get(doc, "/site/publisher/id")
    end

    test "integer field", %{doc: doc} do
      assert {:ok, 2} = Torque.get(doc, "/device/devicetype")
    end

    test "float field", %{doc: doc} do
      assert {:ok, lat} = Torque.get(doc, "/device/geo/lat")
      assert_in_delta 40.7128, lat, 0.0001
    end

    test "negative float", %{doc: doc} do
      assert {:ok, lon} = Torque.get(doc, "/device/geo/lon")
      assert_in_delta -74.006, lon, 0.001
    end

    test "integer zero", %{doc: doc} do
      assert {:ok, 0} = Torque.get(doc, "/regs/coppa")
    end

    test "array field returns full list", %{doc: doc} do
      assert {:ok, imps} = Torque.get(doc, "/imp")
      assert is_list(imps)
      assert length(imps) == 1
      [imp] = imps
      assert imp["id"] == "imp-1"
    end

    test "array index", %{doc: doc} do
      assert {:ok, imp} = Torque.get(doc, "/imp/0")
      assert imp["id"] == "imp-1"
    end

    test "nested array access", %{doc: doc} do
      assert {:ok, 300} = Torque.get(doc, "/imp/0/banner/w")
      assert {:ok, 250} = Torque.get(doc, "/imp/0/banner/h")
    end

    test "deep nested array", %{doc: doc} do
      assert {:ok, eids} = Torque.get(doc, "/user/ext/eids")
      assert is_list(eids)
      assert length(eids) == 2
    end

    test "array element nested field", %{doc: doc} do
      assert {:ok, "adserver.org"} = Torque.get(doc, "/user/ext/eids/0/source")
    end

    test "missing field returns error", %{doc: doc} do
      assert {:error, :no_such_field} = Torque.get(doc, "/nonexistent")
    end

    test "missing nested field returns error", %{doc: doc} do
      assert {:error, :no_such_field} = Torque.get(doc, "/site/nonexistent/deep")
    end

    test "get/3 returns default for missing field", %{doc: doc} do
      assert nil == Torque.get(doc, "/nonexistent", nil)
      assert "default" == Torque.get(doc, "/missing", "default")
    end

    test "get/3 returns value for existing field", %{doc: doc} do
      assert "example.com" == Torque.get(doc, "/site/domain", nil)
    end

    test "object field returns map", %{doc: doc} do
      assert {:ok, geo} = Torque.get(doc, "/device/geo")
      assert is_map(geo)
      assert geo["country"] == "US"
      assert geo["zip"] == "10001"
    end

    test "numeric string object key is reachable via JSON Pointer" do
      {:ok, doc} = Torque.parse(~s({"2":"two","10":"ten"}))
      assert {:ok, "two"} = Torque.get(doc, "/2")
      assert {:ok, "ten"} = Torque.get(doc, "/10")
    end

    test "nested numeric string object key is reachable via JSON Pointer" do
      {:ok, doc} = Torque.parse(~s({"k1":"v1","k2":{"10":"ten","n1":"nv1"}}))
      assert {:ok, "v1"} = Torque.get(doc, "/k1")
      assert {:ok, %{"10" => "ten", "n1" => "nv1"}} = Torque.get(doc, "/k2")
      assert {:ok, "ten"} = Torque.get(doc, "/k2/10")
      assert {:ok, "nv1"} = Torque.get(doc, "/k2/n1")
    end

    test "numeric segment dispatches on node type" do
      {:ok, obj_doc} = Torque.parse(~s({"0":"from-object"}))
      {:ok, arr_doc} = Torque.parse(~s(["from-array"]))

      assert {:ok, "from-object"} = Torque.get(obj_doc, "/0")
      assert {:ok, "from-array"} = Torque.get(arr_doc, "/0")
    end
  end

  describe "get_many/2" do
    test "returns all values", %{doc: doc} do
      paths = ["/id", "/site/domain", "/device/devicetype", "/nonexistent"]

      assert [
               {:ok, "req-123"},
               {:ok, "example.com"},
               {:ok, 2},
               {:error, :no_such_field}
             ] = Torque.get_many(doc, paths)
    end

    test "empty paths list", %{doc: doc} do
      assert [] = Torque.get_many(doc, [])
    end

    test "numeric string object keys" do
      {:ok, doc} = Torque.parse(~s({"1":"one","2":"two"}))
      assert [{:ok, "one"}, {:ok, "two"}] = Torque.get_many(doc, ["/1", "/2"])
    end

    test "all fields", %{doc: doc} do
      paths = [
        "/id",
        "/site/domain",
        "/site/page",
        "/site/publisher/id",
        "/device/devicetype",
        "/device/ua",
        "/device/ip",
        "/device/geo/country",
        "/device/geo/lat",
        "/device/geo/lon",
        "/device/geo/region",
        "/device/geo/zip",
        "/user/id",
        "/user/buyeruid",
        "/user/ext/eids",
        "/imp",
        "/regs/coppa"
      ]

      results = Torque.get_many(doc, paths)
      assert length(results) == 17
      assert Enum.all?(results, &match?({:ok, _}, &1))
    end
  end

  describe "get_many_nil/2" do
    test "returns values directly with nil for missing", %{doc: doc} do
      paths = ["/id", "/site/domain", "/device/devicetype", "/nonexistent"]
      assert ["req-123", "example.com", 2, nil] = Torque.get_many_nil(doc, paths)
    end

    test "empty paths list", %{doc: doc} do
      assert [] = Torque.get_many_nil(doc, [])
    end

    test "matches get_many unwrapped", %{doc: doc} do
      paths = [
        "/id",
        "/site/domain",
        "/site/page",
        "/site/publisher/id",
        "/device/devicetype",
        "/device/geo/lat",
        "/device/geo/lon",
        "/user/ext/eids",
        "/imp",
        "/nonexistent",
        "/regs/coppa"
      ]

      wrapped = Torque.get_many(doc, paths)
      unwrapped = Torque.get_many_nil(doc, paths)

      expected =
        Enum.map(wrapped, fn
          {:ok, v} -> v
          {:error, :no_such_field} -> nil
        end)

      assert unwrapped == expected
    end
  end

  describe "length/2" do
    test "returns array length", %{doc: doc} do
      assert 1 = Torque.length(doc, "/imp")
      assert 2 = Torque.length(doc, "/user/ext/eids")
    end

    test "returns nil for non-array", %{doc: doc} do
      assert nil == Torque.length(doc, "/id")
      assert nil == Torque.length(doc, "/site")
    end

    test "returns nil for missing path", %{doc: doc} do
      assert nil == Torque.length(doc, "/nonexistent")
    end
  end

  describe "parse/2 unique_keys" do
    test "uses fast lookup path" do
      {:ok, doc} = Torque.parse(~s({"a":1,"b":2}), unique_keys: true)
      assert {:ok, 1} = Torque.get(doc, "/a")
      assert {:ok, 2} = Torque.get(doc, "/b")
      assert {:error, :no_such_field} = Torque.get(doc, "/c")
    end

    test "nested objects" do
      {:ok, doc} = Torque.parse(~s({"x":{"y":"z"}}), unique_keys: true)
      assert {:ok, "z"} = Torque.get(doc, "/x/y")
    end

    test "get_many" do
      {:ok, doc} = Torque.parse(~s({"a":1,"b":2}), unique_keys: true)
      assert [{:ok, 1}, {:ok, 2}] = Torque.get_many(doc, ["/a", "/b"])
    end

    test "get_many_nil" do
      {:ok, doc} = Torque.parse(~s({"a":1,"b":2}), unique_keys: true)
      assert [1, 2, nil] = Torque.get_many_nil(doc, ["/a", "/b", "/c"])
    end

    test "dirty scheduler for large payload" do
      large_map = Map.new(1..500, fn i -> {"key_#{i}", String.duplicate("v", 20)} end)
      json = Jason.encode!(large_map)
      {:ok, doc} = Torque.parse(json, unique_keys: true)
      assert {:ok, _} = Torque.get(doc, "/key_1")
    end
  end

  describe "duplicate keys" do
    test "parse + get on object with duplicate keys - last value wins" do
      {:ok, doc} = Torque.parse(~s({"a":1,"b":2,"a":3}))
      assert {:ok, %{"a" => 3, "b" => 2}} = Torque.get(doc, "")
    end

    test "parse + get nested object with duplicate keys" do
      {:ok, doc} = Torque.parse(~s({"x":{"k":"first","k":"last"}}))
      assert {:ok, %{"k" => "last"}} = Torque.get(doc, "/x")
    end

    test "parse + get_many with duplicate key object" do
      {:ok, doc} = Torque.parse(~s({"a":1,"a":2}))
      assert [{:ok, %{"a" => 2}}] = Torque.get_many(doc, [""])
    end
  end

  describe "get/3 error propagation" do
    test "returns default for missing field" do
      {:ok, doc} = Torque.parse(~s({"a":1}))
      assert "default" == Torque.get(doc, "/b", "default")
    end
  end

  describe "parse/1 errors" do
    test "invalid json" do
      assert {:error, _} = Torque.parse("{invalid}")
    end

    test "empty string" do
      assert {:error, _} = Torque.parse("")
    end
  end

  describe "parse/1 dirty scheduler" do
    test "large payload uses dirty scheduler" do
      large_map = Map.new(1..500, fn i -> {"key_#{i}", String.duplicate("v", 20)} end)
      json = Jason.encode!(large_map)
      assert byte_size(json) > 10_240
      {:ok, doc} = Torque.parse(json)
      assert {:ok, _} = Torque.get(doc, "/key_1")
    end
  end

  describe "non-binary path entries raise" do
    test "get_many raises ArgumentError", %{doc: doc} do
      assert_raise ArgumentError, fn -> Torque.get_many(doc, ["/id", :bad]) end
      assert_raise ArgumentError, fn -> Torque.get_many(doc, [42]) end
    end

    test "get_many_nil raises ArgumentError", %{doc: doc} do
      assert_raise ArgumentError, fn -> Torque.get_many_nil(doc, ["/id", :bad]) end
    end

    test "compile_pointers raises ArgumentError" do
      assert_raise ArgumentError, fn -> Torque.compile_pointers(["/a", 42]) end
      assert_raise ArgumentError, fn -> Torque.compile_pointers([nil]) end
    end

    test "non-UTF-8 binary paths raise ArgumentError", %{doc: doc} do
      assert_raise ArgumentError, fn -> Torque.get_many(doc, [<<0xFF>>]) end
      assert_raise ArgumentError, fn -> Torque.compile_pointers([<<0xFF>>]) end
    end
  end

  describe "unknown options raise" do
    test "a misspelled option is not silently defaulted" do
      # `validate` and `unique_keys` change what the caller gets back, so a
      # typo that silently kept the default was a correctness trap.
      assert_raise ArgumentError, fn -> Torque.compile_pointers(["/a"], unique_key: true) end
      assert_raise ArgumentError, fn -> Torque.compile_pointers(["/a"], validation: false) end
      assert_raise ArgumentError, fn -> Torque.parse(~s({"a":1}), unique_key: true) end
      assert_raise ArgumentError, fn -> Torque.encode(%{a: 1}, dirtyy: true) end
      assert_raise ArgumentError, fn -> Torque.encode!(%{a: 1}, dirtyy: true) end
      assert_raise ArgumentError, fn -> Torque.encode_to_iodata(%{a: 1}, dirtyy: true) end
    end

    test "the documented options still work" do
      assert Torque.compile_pointers(["/a"], unique_keys: true, validate: false) |> is_tuple()
      assert {:ok, _} = Torque.parse(~s({"a":1}), unique_keys: true)
      assert {:ok, ~s({"a":1})} = Torque.encode(%{a: 1}, dirty: true)
      assert ~s({"a":1}) == Torque.encode_to_iodata(%{a: 1}, dirty: true)
    end
  end

  describe "large subtree extraction" do
    # Exercises the node-count timeslice accounting paths (>512 terms built).
    test "get of a large root returns the full document" do
      large = Map.new(1..2000, fn i -> {"k#{i}", i} end)
      {:ok, doc} = Torque.parse(Jason.encode!(large))
      assert {:ok, ^large} = Torque.get(doc, "")
    end

    test "get_many_nil of large arrays returns full lists" do
      items = Enum.to_list(1..5000)
      {:ok, doc} = Torque.parse(Jason.encode!(%{"arr" => items}))
      assert [^items] = Torque.get_many_nil(doc, ["/arr"])
    end

    test "compiled pointer extraction of a large subtree" do
      items = Enum.to_list(1..5000)
      json = Jason.encode!(%{"arr" => items})
      ptrs = Torque.compile_pointers(["/arr"])
      {:ok, doc} = Torque.parse(json)
      assert [^items] = Torque.get_many_nil(doc, ptrs)
      assert {:ok, [^items]} = Torque.parse_get_many_nil(json, ptrs)
    end
  end

  describe "roundtrip" do
    test "decode then encode preserves data" do
      json = ~s({"a":1,"b":"hello","c":[1,2,3],"d":true,"e":null})
      {:ok, decoded} = Torque.decode(json)
      {:ok, encoded} = Torque.encode(decoded)
      {:ok, decoded2} = Torque.decode(encoded)
      assert decoded == decoded2
    end
  end

  describe "get_many_defaults/2" do
    setup do
      {:ok, doc} = Torque.parse(~s({"a":1,"b":null,"deep":{"x":[1,2]},"s":"str"}))
      %{d: doc}
    end

    test "keeps the default for a missing path, a null, and nothing else", %{d: doc} do
      assert Torque.get_many_defaults(doc, %{"/a" => 0, "/b" => 0, "/missing" => :gone}) ==
               %{"/a" => 1, "/b" => 0, "/missing" => :gone}
    end

    test "extracts subtrees and the whole document", %{d: doc} do
      assert %{"/deep" => %{"x" => [1, 2]}, "" => whole} =
               Torque.get_many_defaults(doc, %{"/deep" => :d, "" => :d})

      assert whole == Torque.decode!(~s({"a":1,"b":null,"deep":{"x":[1,2]},"s":"str"}))
    end

    test "empty map, and more keys than a flatmap holds", %{d: doc} do
      assert Torque.get_many_defaults(doc, %{}) == %{}

      many = Map.new(1..100, fn i -> {"/k#{i}", i} end)
      result = Torque.get_many_defaults(doc, many)
      assert map_size(result) == 100
      assert result == many
    end

    test "a non-binary key is a caller bug", %{d: doc} do
      assert_raise ArgumentError, fn -> Torque.get_many_defaults(doc, %{:a => 1}) end
      assert_raise ArgumentError, fn -> Torque.get_many_defaults(doc, %{<<0xFF>> => 1}) end
    end
  end

  describe "compile_pointers/2 + parsed lookups" do
    @ptr_paths [
      "/id",
      "/site/domain",
      "/device/geo/lat",
      "/imp/0/banner/w",
      "/nonexistent"
    ]

    test "matches raw paths", %{doc: doc} do
      ptrs = Torque.compile_pointers(@ptr_paths)
      assert Torque.get_many_nil(doc, ptrs) == Torque.get_many_nil(doc, @ptr_paths)
      assert Torque.get_many(doc, ptrs) == Torque.get_many(doc, @ptr_paths)
    end

    test "extracts scalars, arrays, and nil for missing", %{doc: doc} do
      ptrs = Torque.compile_pointers(@ptr_paths)
      assert ["req-123", "example.com", 40.7128, 300, nil] = Torque.get_many_nil(doc, ptrs)
    end

    test "unique_keys handle matches default for unique-keyed input", %{doc: doc} do
      uniq = Torque.compile_pointers(@ptr_paths, unique_keys: true)
      default = Torque.compile_pointers(@ptr_paths)
      assert Torque.get_many_nil(doc, uniq) == Torque.get_many_nil(doc, default)
      assert Torque.get_many(doc, uniq) == Torque.get_many(doc, default)
    end

    test "empty pointer list" do
      {:ok, doc} = Torque.parse(~s({"a":1}))
      assert [] = Torque.get_many_nil(doc, Torque.compile_pointers([]))
    end

    test "root path returns whole document" do
      {:ok, doc} = Torque.parse(~s({"a":1}))
      ptrs = Torque.compile_pointers([""])
      assert [%{"a" => 1}] = Torque.get_many_nil(doc, ptrs)
    end

    test "numeric segment dispatches on node type via compiled handle" do
      {:ok, obj_doc} = Torque.parse(~s({"0":"from-object"}))
      {:ok, arr_doc} = Torque.parse(~s(["from-array"]))
      ptrs = Torque.compile_pointers(["/0"])
      assert ["from-object"] = Torque.get_many_nil(obj_doc, ptrs)
      assert ["from-array"] = Torque.get_many_nil(arr_doc, ptrs)
    end

    test "tilde escapes are unescaped at compile time" do
      {:ok, doc} = Torque.parse(~s({"a/b":1,"c~d":2}))
      ptrs = Torque.compile_pointers(["/a~1b", "/c~0d"])
      assert [1, 2] = Torque.get_many_nil(doc, ptrs)
    end
  end

  describe "parse_get_many_nil/2" do
    @fused_paths [
      "/id",
      "/site/domain",
      "/device/geo/lat",
      "/imp/0/banner/w",
      "/nonexistent"
    ]

    test "fused parse + extract matches parse + get_many_nil" do
      ptrs = Torque.compile_pointers(@fused_paths)
      {:ok, fused} = Torque.parse_get_many_nil(@sample_json, ptrs)
      {:ok, doc} = Torque.parse(@sample_json)
      assert fused == Torque.get_many_nil(doc, @fused_paths)
    end

    test "returns {:ok, values} with nil for missing and null" do
      ptrs = Torque.compile_pointers(["/id", "/site/domain", "/missing", "/null"])
      json = ~s({"id":"x","site":{"domain":"e.com"},"null":null})
      assert {:ok, ["x", "e.com", nil, nil]} = Torque.parse_get_many_nil(json, ptrs)
    end

    test "honors unique_keys from the handle" do
      ptrs = Torque.compile_pointers(["/a", "/b"], unique_keys: true)
      assert {:ok, [1, 2]} = Torque.parse_get_many_nil(~s({"a":1,"b":2}), ptrs)
    end

    test "returns error tuple for malformed json" do
      ptrs = Torque.compile_pointers(["/a"])
      assert {:error, _} = Torque.parse_get_many_nil("not json", ptrs)
      assert {:error, _} = Torque.parse_get_many_nil("", ptrs)
    end

    test "dirty scheduler for large payload" do
      large_map = Map.new(1..600, fn i -> {"key_#{i}", String.duplicate("v", 40)} end)
      json = Jason.encode!(large_map)
      assert byte_size(json) > 20_480
      ptrs = Torque.compile_pointers(["/key_1", "/key_600", "/missing"])
      assert {:ok, [v1, v600, nil]} = Torque.parse_get_many_nil(json, ptrs)
      assert is_binary(v1) and is_binary(v600)
    end
  end

  describe "parse_get_many_nil/2 path shapes" do
    @doc_json ~s({"user":{"id":"u-7","tags":["a","b"]},"imp":[{"w":300},{"w":250}],"n":{"0":"as-key"}})

    test "a path that is a prefix of another fills both" do
      ptrs = Torque.compile_pointers(["/user", "/user/id", "/user/tags/1", "/user/missing"])

      assert {:ok, [%{"id" => "u-7"}, "u-7", "b", nil]} =
               Torque.parse_get_many_nil(@doc_json, ptrs)
    end

    test "a numeric segment picks array index or object key per node" do
      ptrs = Torque.compile_pointers(["/imp/1/w", "/n/0", "/imp/9/w"])
      assert {:ok, [250, "as-key", nil]} = Torque.parse_get_many_nil(@doc_json, ptrs)
    end

    test "a path deeper than the nesting limit is absent, not an error" do
      deep = "/" <> Enum.map_join(1..200, "/", &"s#{&1}")
      ptrs = Torque.compile_pointers([deep, "/user/id"])
      assert {:ok, [nil, "u-7"]} = Torque.parse_get_many_nil(@doc_json, ptrs)
    end

    test "unique_keys with validate: false stops at the first match" do
      json = ~s({"a":1,"z":"skipped","a":2})
      ptrs = Torque.compile_pointers(["/a"], unique_keys: true, validate: false)
      assert {:ok, [1]} = Torque.parse_get_many_nil(json, ptrs)
    end
  end

  describe "parse_get_many_nil/2 skipped regions" do
    test "a fault outside every selected path is still reported" do
      ptrs = Torque.compile_pointers(["/keep"])
      assert {:error, _} = Torque.parse_get_many_nil(~s({"keep":1,"other":tru}), ptrs)
      assert {:error, _} = Torque.parse_get_many_nil(~s({"keep":1,"other":[1,,2]}), ptrs)
      assert {:error, _} = Torque.parse_get_many_nil(~s({"keep":1,"other":01}), ptrs)
      assert {:error, _} = Torque.parse_get_many_nil(~s({"keep":1,"other":{"x" 1}}), ptrs)
      assert {:error, _} = Torque.parse_get_many_nil(~s({"keep":1} trailing), ptrs)

      assert {:error, _} =
               Torque.parse_get_many_nil(<<"{\"keep\":1,\"o\":\"", 0xFF, "\"}">>, ptrs)
    end

    test "validated skipped values match full parsing" do
      # Checked extraction must reject malformed values outside selected paths.
      faults = [
        ~s({"keep":1,"drop":"\\uZZZZ"}),
        ~s({"keep":1,"drop":"\\u00zz"}),
        ~s({"keep":1,"drop":"\\ud800"}),
        ~s({"keep":1,"drop":"\\ud800x"}),
        ~s({"keep":1,"drop":1e400}),
        ~s({"keep":1,"drop":[-1e400]}),
        ~s({"keep":1,"drop":{"n":1e400}})
      ]

      clean = [
        ~s({"keep":1,"drop":"\\ud83d\\ude04"}),
        ~s({"keep":1,"drop":"\\u00e9\\n\\t"}),
        ~s({"keep":1,"drop":[1.5e-3,-0.0,18446744073709551616]})
      ]

      strict = Torque.compile_pointers(["/keep"])
      loose = Torque.compile_pointers(["/keep"], validate: false)

      for json <- faults do
        assert {:error, _} = Torque.parse(json), json
        assert {:error, _} = Torque.parse_get_many_nil(json, strict), json
        assert {:ok, [1]} = Torque.parse_get_many_nil(json, loose), json
      end

      for json <- clean do
        assert {:ok, _} = Torque.parse(json), json
        assert {:ok, [1]} = Torque.parse_get_many_nil(json, strict), json
        assert {:ok, [1]} = Torque.parse_get_many_nil(json, loose), json
      end
    end

    test "validate: false answers from the selected paths alone" do
      ptrs = Torque.compile_pointers(["/keep"], validate: false)
      assert {:ok, [1]} = Torque.parse_get_many_nil(~s({"keep":1,"other":[1,,2]}), ptrs)
      assert {:ok, [1]} = Torque.parse_get_many_nil(~s({"keep":1} trailing), ptrs)
      # Selected values are still parsed under the unchecked policy.
      assert {:error, _} = Torque.parse_get_many_nil(~s({"keep":}), ptrs)
    end

    test "validate: false accepts trailing content but not truncation" do
      # Skipping relaxes the trailing-content check, which is the one way an
      # unchecked extraction accepts what `decode/1` refuses. It does not
      # relax truncation: a skip still has to find its closing delimiter, and
      # the early exit under `unique_keys` ends in the same scan.
      for unique <- [false, true] do
        ptrs = Torque.compile_pointers(["/keep"], validate: false, unique_keys: unique)

        assert {:ok, [1]} = Torque.parse_get_many_nil(~s({"keep":1} junk), ptrs)

        for truncated <- [
              ~s({"keep":1,"other":[1,2,),
              ~s({"keep":1,"other":2),
              ~s({"keep":1,"s":"unterminated)
            ] do
          assert {:error, _} = Torque.parse_get_many_nil(truncated, ptrs), truncated
        end
      end
    end

    test "validate: false and unique_keys do not change any answer" do
      # Skipping may relax error reporting, but it must not change selected values.
      json = ~s({"a":{"b":[10,20]},"b":["x"],"0":1,"s":"v"})
      paths = ["/a/b/1", "/b/0", "/0", "/s", "/missing", ""]
      {:ok, doc} = Torque.parse(json)
      expected = Torque.get_many_nil(doc, paths)

      fast = Torque.compile_pointers(paths, unique_keys: true, validate: false)
      strict = Torque.compile_pointers(paths)

      assert {:ok, expected} == Torque.parse_get_many_nil(json, strict)
      assert {:ok, expected} == Torque.parse_get_many_nil(json, fast)
    end

    test "validate: false still reports invalid UTF-8 it walked over" do
      # Structural skipping relaxes syntax checks, not UTF-8 validation.
      ptrs = Torque.compile_pointers(["/keep"], validate: false)
      bad = <<"{\"keep\":1,\"drop\":\"", 0xFF, "\"}">>
      assert {:error, _} = Torque.parse_get_many_nil(bad, ptrs)
    end

    test "a repeated key does not leave the previous value's fields behind" do
      # A later duplicate must clear descendants supplied only by the old value.
      cases = [
        {~s({"a":{"x":1},"a":{}}), ["/a/x", "/a"], [nil, %{}]},
        {~s({"a":{"x":1},"a":{"y":2}}), ["/a/x", "/a/y"], [nil, 2]},
        {~s({"a":{"x":1},"a":[9]}), ["/a/x", "/a/0", "/a"], [nil, 9, [9]]},
        {~s({"a":{"x":1},"a":5}), ["/a/x", "/a"], [nil, 5]},
        {~s({"a":{"b":{"c":1}},"a":{"b":{}}}), ["/a/b/c"], [nil]},
        {~s({"a":[{"k":1}],"a":[{}]}), ["/a/0/k"], [nil]}
      ]

      for {json, paths, expected} <- cases do
        {:ok, doc} = Torque.parse(json)
        assert Torque.get_many_nil(doc, paths) == expected

        for ptrs <- [
              Torque.compile_pointers(paths),
              Torque.compile_pointers(paths, validate: false)
            ] do
          assert Torque.get_many_nil(doc, ptrs) == expected
          assert {:ok, expected} == Torque.parse_get_many_nil(json, ptrs)
        end
      end
    end

    test "unique_keys keeps the first value's fields under a repeated key" do
      json = ~s({"a":{"x":1},"a":{}})
      paths = ["/a/x", "/a"]
      expected = [1, %{"x" => 1}]

      {:ok, doc} = Torque.parse(json, unique_keys: true)
      ptrs = Torque.compile_pointers(paths, unique_keys: true)

      assert Torque.get_many_nil(doc, ptrs) == expected
      assert {:ok, expected} == Torque.parse_get_many_nil(json, ptrs)
    end

    # One object's tracking of which planned keys it has already supplied used
    # to be a single word, so the 65th key at a node had no bit: `unique_keys`
    # silently became last-wins there, and the unchecked early exit could never
    # fire because its found-count could not reach the node's width.
    test "duplicate keys resolve the same way at every plan width" do
      for width <- [1, 63, 64, 65, 200] do
        keys = for i <- 1..width, do: "k#{i}"
        paths = Enum.map(keys, &"/#{&1}")
        last = "k#{width}"

        # Every key once, then the widest one repeated with a different value.
        json =
          "{" <>
            Enum.map_join(keys, ",", fn k -> ~s("#{k}":1) end) <>
            ~s(,"#{last}":2) <> "}"

        for unique <- [false, true] do
          {:ok, doc} = Torque.parse(json, unique_keys: unique)
          expected = List.duplicate(1, width - 1) ++ [if(unique, do: 1, else: 2)]
          assert Torque.get_many_nil(doc, paths) == expected

          for validate <- [true, false] do
            ptrs = Torque.compile_pointers(paths, unique_keys: unique, validate: validate)

            assert Torque.get_many_nil(doc, ptrs) == expected,
                   "width #{width}, unique_keys: #{unique}, compiled lookup"

            assert {:ok, expected} == Torque.parse_get_many_nil(json, ptrs),
                   "width #{width}, unique_keys: #{unique}, validate: #{validate}"
          end
        end
      end
    end

    test "a repeated key clears descendants at every plan width" do
      for width <- [1, 64, 65, 200] do
        # Pad to `width` planned keys at the root so the repeated key sits past
        # the inline word, then take a field only the dead first value supplies.
        pad = for i <- 1..(width - 1)//1, do: ~s("p#{i}":0)
        json = "{" <> Enum.join(pad ++ [~s("a":{"x":1}), ~s("a":{"y":2})], ",") <> "}"
        paths = Enum.map(1..(width - 1)//1, &"/p#{&1}") ++ ["/a/x"]
        expected = List.duplicate(0, width - 1) ++ [nil]

        {:ok, doc} = Torque.parse(json)
        assert Torque.get_many_nil(doc, paths) == expected

        for validate <- [true, false] do
          ptrs = Torque.compile_pointers(paths, validate: validate)

          assert {:ok, expected} == Torque.parse_get_many_nil(json, ptrs),
                 "width #{width}, validate: #{validate}"
        end
      end
    end

    test "unique_keys skips the object remainder at every plan width" do
      for width <- [1, 64, 65, 200] do
        keys = for i <- 1..width, do: "k#{i}"
        paths = Enum.map(keys, &"/#{&1}")

        # A member the plan walk cannot parse. Reaching the closing brace means
        # the whole remainder was skipped once every requested key was found.
        json =
          "{" <> Enum.map_join(keys, ",", fn k -> ~s("#{k}":1) end) <> ~s(,"z" 5) <> "}"

        ptrs = Torque.compile_pointers(paths, unique_keys: true, validate: false)

        assert {:ok, List.duplicate(1, width)} == Torque.parse_get_many_nil(json, ptrs),
               "width #{width} did not stop after the last requested key"
      end
    end

    test "nesting deeper than the limit is refused, not crashed" do
      deep = String.duplicate("[", 200) <> String.duplicate("]", 200)
      json = ~s({"keep":1,"other":) <> deep <> "}"
      ptrs = Torque.compile_pointers(["/keep"])
      assert {:error, :nesting_too_deep} = Torque.parse_get_many_nil(json, ptrs)

      # Structural skipping is iterative and does not consume the nesting budget.
      loose = Torque.compile_pointers(["/keep"], validate: false)
      assert {:ok, [1]} = Torque.parse_get_many_nil(json, loose)
    end

    test "the nesting limit spans the plan and the value it selects" do
      # Plan descent and selected-value parsing share one nesting budget.
      path = "/" <> Enum.map_join(1..100, "/", fn _ -> "a" end)
      strict = Torque.compile_pointers([path])
      loose = Torque.compile_pointers([path], validate: false)

      nest = fn arrays ->
        String.duplicate(~s({"a":), 100) <>
          String.duplicate("[", arrays) <>
          "1" <>
          String.duplicate("]", arrays) <> String.duplicate("}", 100)
      end

      # Pin the boundary with nesting split across the plan and selected value.
      assert {:ok, _} = Torque.parse(nest.(28))
      assert {:error, :nesting_too_deep} = Torque.parse(nest.(29))

      for ptrs <- [strict, loose] do
        assert {:ok, [selected]} = Torque.parse_get_many_nil(nest.(28), ptrs)
        assert is_list(selected)
        assert {:error, :nesting_too_deep} = Torque.parse_get_many_nil(nest.(29), ptrs)
      end
    end
  end

  # Unescaped results may borrow the input; escaped results copy. These tests
  # also pin the policy that avoids retaining large backing allocations.
  describe "extracted strings" do
    test "plain and escaped strings match parsed-document extraction" do
      json =
        ~s({"plain":"hello","esc":"a\\nb\\"c\\\\d","uni":"caf\\u00e9",) <>
          ~s("empty":"","tail":"at the very end"})

      paths = ["/plain", "/esc", "/uni", "/empty", "/tail"]
      {:ok, doc} = Torque.parse(json)
      expected = ["hello", "a\nb\"c\\d", "café", "", "at the very end"]
      assert Torque.get_many_nil(doc, paths) == expected

      for ptrs <- [
            Torque.compile_pointers(paths),
            Torque.compile_pointers(paths, validate: false),
            Torque.compile_pointers(paths, unique_keys: true)
          ] do
        assert Torque.parse_get_many_nil(json, ptrs) == {:ok, expected}
      end
    end

    test "borrowing follows the backing allocation size" do
      ptrs = Torque.compile_pointers(["/ua"])
      ua = String.duplicate("u", 100)

      extract = fn json ->
        assert {:ok, [value]} = Torque.parse_get_many_nil(json, ptrs)
        assert value == ua
        value
      end

      small = ~s({"pad":"#{String.duplicate("x", 800)}","ua":"#{ua}"})
      borrowed = extract.(small)
      assert :binary.referenced_byte_size(borrowed) == :binary.referenced_byte_size(small)

      large = ~s({"pad":"#{String.duplicate("x", 400_000)}","ua":"#{ua}"})
      assert :binary.referenced_byte_size(extract.(large)) == byte_size(ua)

      doc = ~s({"pad":"#{String.duplicate("x", 400)}","ua":"#{ua}"})
      parent = :binary.copy(String.duplicate("z", 400_000) <> doc)
      slice = :binary.part(parent, 400_000, byte_size(doc))
      assert :binary.referenced_byte_size(slice) > 400_000
      assert :binary.referenced_byte_size(extract.(slice)) == byte_size(ua)
    end
  end

  describe "compiled pointer plans" do
    # Plan construction must index wide fan-out instead of scanning every prior
    # child. The ratio detects quadratic growth; the ceiling catches both sides
    # becoming slow together.
    @tag :perf
    test "compiling a wide path set scales with its size, not its square" do
      compile = fn n ->
        paths = for i <- 1..n, do: "/k#{i}"
        {us, _} = :timer.tc(fn -> Torque.compile_pointers(paths) end)
        us
      end

      compile.(2048)
      small = Enum.min(for _ <- 1..5, do: compile.(2048))
      large = Enum.min(for _ <- 1..5, do: compile.(8192))

      # The larger fixture distinguishes linear growth from the old quadratic path.
      assert large / small < 8.0, "compile scaled #{Float.round(large / small, 1)}x over 4x paths"
      assert large < 100_000, "8192 paths took #{Float.round(large / 1000, 1)} ms to compile"
    end
  end

  # Batch lookups index recurring wide objects while preserving scan semantics.
  # The cache must retain multiple objects because one path set can alternate
  # between ancestors or siblings.
  describe "wide object lookups" do
    defp wide_members(n, prefix \\ "k") do
      Enum.map_join(1..n, ",", fn i -> ~s("#{prefix}#{i}":#{i}) end)
    end

    test "an indexed object preserves missing and duplicate-key semantics" do
      json = ~s({"dup":1,#{wide_members(200)},"dup":2})
      paths = ["/dup"] ++ for(i <- 1..64, do: "/k#{i}") ++ ["/nope", "/dup"]
      wins_last = [2] ++ Enum.to_list(1..64) ++ [nil, 2]
      wins_first = [1] ++ Enum.to_list(1..64) ++ [nil, 1]

      {:ok, doc} = Torque.parse(json)
      assert Torque.get_many_nil(doc, paths) == wins_last

      {:ok, uniq} = Torque.parse(json, unique_keys: true)
      assert Torque.get_many_nil(uniq, paths) == wins_first
    end

    # Relative cost distinguishes index reuse from a scan repeated per path.
    @tag :perf
    test "a batch's cost stops tracking its path count once the object is indexed" do
      {:ok, doc} = Torque.parse(~s({#{wide_members(2000)}}))

      run = fn n ->
        paths = for i <- 1..n, do: "/k#{rem(i * 7, 2000) + 1}"
        Torque.get_many_nil(doc, paths)
        Enum.min(for _ <- 1..7, do: elem(:timer.tc(fn -> Torque.get_many_nil(doc, paths) end), 0))
      end

      few = run.(16)
      many = run.(512)

      assert many / few < 6.0,
             "32x the paths cost #{Float.round(many / few, 1)}x, so the scan is still per path"

      assert many < 300, "512 lookups into a 2000-member object took #{many} us"
    end

    # Parent and child lookups alternate, so both indexes must remain resident.
    @tag :perf
    test "an object is indexed even when the path set walks another wide one" do
      lookups = fn json, paths ->
        {:ok, doc} = Torque.parse(json)
        Torque.get_many_nil(doc, paths)
        Enum.min(for _ <- 1..5, do: elem(:timer.tc(fn -> Torque.get_many_nil(doc, paths) end), 0))
      end

      child = wide_members(2000, "f")
      narrow_parent = lookups.(~s({"a":{#{child}}}), for(i <- 1..512, do: "/a/f#{i}"))

      wide_parent =
        lookups.(
          ~s({#{Enum.map_join(1..200, ",", fn i -> ~s("g#{i}":{#{child}}) end)}}),
          for(i <- 1..512, do: "/g1/f#{i}")
        )

      assert wide_parent / narrow_parent < 4.0,
             "a wide ancestor cost #{Float.round(wide_parent / narrow_parent, 1)}x"
    end

    # Interleaved sibling paths expose cache thrashing between objects: the
    # memo has to hold both indexes at once, or every alternation evicts the
    # one the next path needs.
    @tag :perf
    test "two wide objects in one batch are both indexed" do
      json = ~s({"a":{#{wide_members(2000)}},"b":{#{wide_members(2000)}}})
      {:ok, doc} = Torque.parse(json)

      # The control holds everything but the order constant: the same 512
      # paths, the same two objects, the same two index builds. Measuring
      # against a single-object batch instead compared one index build against
      # two *plus* the alternation, so a healthy run sat at 2.8-4.3x under a
      # 4.0 limit — a boundary, not a margin, and about one suite in six failed
      # on it.
      blocked = for s <- ["a", "b"], i <- 1..256, do: "/#{s}/k#{i}"
      interleaved = for i <- 1..256, s <- ["a", "b"], do: "/#{s}/k#{i}"

      time = fn paths -> max(elem(:timer.tc(fn -> Torque.get_many_nil(doc, paths) end), 0), 1) end
      time.(blocked)
      time.(interleaved)

      # Median of back-to-back pairs, not a ratio of separately taken minima.
      # This suite runs 64 cases at a time, so a scheduling spike lands in
      # whichever phase is running and moves the ratio one way; paired samples
      # take it on both sides and the median discards the pairs it lands on.
      # Against 16 busy cores individual pairs ranged 0.14x to 3.5x while the
      # median stayed within 0.84-1.02.
      ratios =
        Enum.sort(
          for _ <- 1..15 do
            time.(interleaved) / time.(blocked)
          end
        )

      median = Enum.at(ratios, 7)

      # Both indexes resident makes this 1.0; losing one to eviction made two
      # sibling dictionaries 17.6x.
      assert median < 3.0, "interleaving two wide objects cost #{Float.round(median, 1)}x"
    end
  end

  # Dirty dispatch depends on path count and bytes, not document size.
  describe "path-count dispatch" do
    @doc_json ~s({"a":1,"b":{"c":"x"},"arr":[10,20]})

    test "a batch is dispatched on its path count and path bytes" do
      assert {ref, 3, 8} = Torque.compile_pointers(["/a", "/b/c", "/a"])
      assert is_reference(ref)
      assert {_, 0, 0} = Torque.compile_pointers([])
      assert Torque.dirty_paths?(for(i <- 1..2048, do: "/k#{i}"))
      refute Torque.dirty_paths?(for(i <- 1..2047, do: "/k#{i}"))

      # Pin the strict greater-than byte boundary.
      big = "/" <> String.duplicate("k", 20_480)
      small = "/" <> String.duplicate("k", 20_479)
      assert byte_size(big) == 20_481
      assert byte_size(small) == 20_480

      assert Torque.dirty_paths?(big)
      refute Torque.dirty_paths?(small)
      assert Torque.dirty_paths?([big])
      refute Torque.dirty_paths?([small])
      # A map only ever reaches a lookup, so its byte rule is `dirty_lookup?/1`.
      assert Torque.dirty_lookup?(%{big => 1})
      refute Torque.dirty_lookup?(%{small => 1})

      # Spread across paths, and past the count while still short.
      assert Torque.dirty_paths?(for(_ <- 1..8, do: "/" <> String.duplicate("k", 4096)))
      assert Torque.dirty_paths?(Torque.compile_pointers(for(i <- 1..2048, do: "/k#{i}")))
      refute Torque.dirty_paths?(Torque.compile_pointers(["/a"]))
    end

    # A compiled handle must answer the dispatch question exactly as the list
    # it was built from. Compiling removes the per-call split and unescape, not
    # the key bytes every lookup still compares, so the handle carries both
    # quantities the raw walk computes.
    test "a compiled handle dispatches like the paths it was built from" do
      big = "/" <> String.duplicate("k", 20_480)
      small = "/" <> String.duplicate("k", 20_479)

      sets = [
        [],
        ["/a"],
        [small],
        [big],
        [small, small],
        for(_ <- 1..8, do: "/" <> String.duplicate("k", 4096)),
        for(i <- 1..2047, do: "/k#{i}"),
        for(i <- 1..2048, do: "/k#{i}")
      ]

      for paths <- sets do
        bytes = Enum.sum(Enum.map(paths, &byte_size/1))

        assert Torque.dirty_paths?(Torque.compile_pointers(paths)) ==
                 Torque.dirty_paths?(paths),
               "#{length(paths)} paths, #{bytes} bytes"
      end
    end

    # Document-dependent overruns request a dirty retry.
    test "a batch that overruns its budget is retried dirty" do
      wide = Map.new(1..100_000, fn i -> {"k#{i}", i} end) |> Jason.encode!()
      {:ok, doc} = Torque.parse(wide)
      paths = Enum.map(1..8, &"/k#{&1 * 12_000}")
      {ref, _, _} = handle = Torque.compile_pointers(paths)
      expected = Enum.map(1..8, &(&1 * 12_000))

      # The path set stays below caller-side dispatch thresholds.
      assert Torque.Native.get_many_nil_compiled(doc, ref) == :dirty_required
      assert Torque.Native.get_many_nil_compiled_dirty(doc, ref) == expected
      assert Torque.get_many_nil(doc, handle) == expected
      assert Torque.get_many_nil(doc, paths) == expected
      assert Torque.get_many(doc, paths) == Enum.map(expected, &{:ok, &1})

      assert Torque.get_many_defaults(doc, Map.new(paths, &{&1, :missing})) ==
               Map.new(Enum.zip(paths, expected))
    end

    # A large but cheap path set remains on a normal scheduler.
    test "a long but cheap batch stays on a normal scheduler" do
      {:ok, doc} = Torque.parse(@doc_json)
      paths = Enum.map(1..2048, fn i -> "/k#{i}" end)
      {ref, 2048, _} = handle = Torque.compile_pointers(paths)

      assert is_list(Torque.Native.get_many_nil_compiled(doc, ref))
      assert Torque.get_many_nil(doc, handle) == List.duplicate(nil, 2048)
      assert Torque.get_many_nil(doc, paths) == List.duplicate(nil, 2048)
    end

    # Heavy state is scoped to the parsed document. Front keys force full
    # last-wins scans in the wide document.
    test "one wide document does not send every later batch dirty" do
      wide = Map.new(1..100_000, fn i -> {"k#{i}", i} end) |> Jason.encode!()
      {:ok, heavy} = Torque.parse(wide)
      {wide_ref, _, _} = Torque.compile_pointers(Enum.map(1..4, &"/k#{&1}"))
      assert Torque.Native.get_many_nil_compiled(heavy, wide_ref) == :dirty_required

      {:ok, small} = Torque.parse(@doc_json)
      {ref, _, _} = Torque.compile_pointers(["/a", "/b/c"])
      assert Torque.Native.get_many_nil_compiled(small, ref) == [1, "x"]
    end

    # Caller-driven overruns must not mark the document as heavy.
    test "a path-set overrun does not mark the document heavy" do
      {:ok, doc} = Torque.parse(@doc_json)
      {big_ref, _, _} = Torque.compile_pointers(List.duplicate("/a", 40_000))
      {small_ref, _, _} = Torque.compile_pointers(["/a"])

      assert Torque.Native.get_many_nil_compiled(doc, big_ref) == :dirty_required
      assert Torque.Native.get_many_nil_compiled(doc, small_ref) == [1]

      # The public API retries the oversized batch without poisoning the
      # document.
      assert Torque.get_many_nil(doc, Torque.compile_pointers(List.duplicate("/a", 40_000))) ==
               List.duplicate(1, 40_000)

      assert Torque.get_many_nil(doc, ["/a", "/b/c"]) == [1, "x"]
      assert Torque.Native.get_many_nil_compiled(doc, small_ref) == [1]
    end

    # Result count alone can require dirty dispatch.
    test "a path set long enough to spend the budget on results starts dirty" do
      refute Torque.dirty_lookup?(Torque.compile_pointers(List.duplicate("/a", 7999)))
      assert Torque.dirty_lookup?(Torque.compile_pointers(List.duplicate("/a", 8000)))
      refute Torque.dirty_lookup?(List.duplicate("/a", 7999))
      assert Torque.dirty_lookup?(List.duplicate("/a", 8000))
      # Distinct map keys cross the byte threshold before the count threshold.
      refute Torque.dirty_lookup?(Map.new(1..64, &{"/k#{&1}", 0}))
      assert Torque.dirty_lookup?(Map.new(1..8000, &{"/k#{&1}", 0}))

      assert Torque.dirty_lookup?([String.duplicate("/k", 12_000)])
    end

    # A batch that ends in `badarg` has still done the lookups before it, and
    # what it reports to the scheduler is what keeps a process from running
    # them for free. Reductions are the observable end of that.
    #
    # All three batch APIs. A map has no order the caller picks, but it does
    # have one the NIF walks, and `Map.keys/1` reports it: two bad keys, one
    # the iterator reaches early and one late, do for the map what putting the
    # bad path at either end does for a list.
    test "a batch that fails late still reports the work it did" do
      members = Enum.map_join(1..64, ",", fn i -> ~s("k#{i}":#{i}) end)
      needle = String.duplicate("a", 16)
      {:ok, doc} = Torque.parse(~s({"#{needle}":1,#{members}}))
      good = List.duplicate("/#{needle}", 1000)
      late = good ++ [:not_a_path]
      early = [:not_a_path | good]

      charged = fn call, paths ->
        {:reductions, before} = Process.info(self(), :reductions)

        try do
          call.(paths)
        rescue
          ArgumentError -> :error
        end

        {:reductions, now} = Process.info(self(), :reductions)
        now - before
      end

      # Two invalid keys, chosen for where the map puts them: the NIF walks a
      # map in the order `Map.keys/1` reports, so this is the same experiment.
      keys = for i <- 1..1000, do: "/" <> String.pad_leading(Integer.to_string(i), 15, "0")

      placed = fn bad ->
        Enum.find_index(Map.keys(Map.new([{bad, 0} | Enum.map(keys, &{&1, 0})])), &(&1 == bad))
      end

      candidates = for i <- 0..63, do: <<0xFF, i>>
      first = Enum.min_by(candidates, placed)
      last = Enum.max_by(candidates, placed)

      assert placed.(first) < 100 and placed.(last) > 900,
             "no pair of keys lands at opposite ends of this map's order"

      defaults = fn bad -> Map.new([{bad, 0} | Enum.map(keys, &{&1, 0})]) end

      for {name, call, late, early} <- [
            {"get_many/2", &Torque.get_many(doc, &1), late, early},
            {"get_many_nil/2", &Torque.get_many_nil(doc, &1), late, early},
            {"get_many_defaults/2", &Torque.get_many_defaults(doc, &1), defaults.(last),
             defaults.(first)}
          ] do
        # Same length either way, so the walk that picks a scheduler costs the
        # same and what is left is the lookups the NIF made before the bad one.
        # Taken as a minimum over repeats: the first raise of a run costs more
        # than the ones after it, and that noise is larger than the signal.
        least = fn paths -> Enum.min(for(_ <- 1..9, do: charged.(call, paths))) end
        least.(late)
        least.(early)
        late_charge = least.(late)
        early_charge = least.(early)

        assert late_charge > early_charge + 100,
               "#{name}: late batch charged #{late_charge}, early charged #{early_charge}"
      end
    end

    # A path that is not a binary is the NIF's to reject, and it does so on
    # either scheduler: the walk must not raise while deciding.
    test "deciding a scheduler does not validate the paths" do
      {:ok, doc} = Torque.parse(@doc_json)
      long = "/" <> String.duplicate("k", 20_480)

      refute Torque.dirty_paths?([:not_a_path])
      assert Torque.dirty_paths?([long, :not_a_path])
      assert Torque.dirty_paths?([:not_a_path, long])

      for paths <- [[:not_a_path], [long, :not_a_path], [:not_a_path, long]] do
        assert_raise ArgumentError, fn -> Torque.get_many_nil(doc, paths) end
      end

      assert_raise ArgumentError, fn -> Torque.get_many_defaults(doc, %{long => 1, 7 => 2}) end
      assert_raise ArgumentError, fn -> Torque.get_many_defaults(doc, %{7 => 2, long => 1}) end
    end
  end
end

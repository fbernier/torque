# PGO training workload for the glazer NIF.
#
# Mirror of bench/pgo_workload.exs but driving glazer instead of torque, so a
# glazer PGO build (deps/glazer c_src Makefile PGO=generate/use) collects
# branch/call-frequency data over the same representative payloads the README
# benchmark uses — keeping the glazer-vs-torque comparison apples-to-apples
# (both libraries profiled against equivalent decode/encode/find workloads).
#
# Run with an instrumented glazer.so loaded: MIX_ENV=bench mix run this file.

small_json =
  ~s({"id":"req-0001","site":{"domain":"example.com","page":"https://example.com/articles/x","cat":["IAB1","IAB2-3"],"publisher":{"id":"pub-12345"}},"device":{"devicetype":2,"ua":"Mozilla/5.0 Macintosh; Intel Mac OS X 10_15_7 Chrome/120.0.0.0","ip":"203.0.113.42","geo":{"country":"US","lat":40.7128,"lon":-74.006,"zip":"10001"},"connectiontype":2},"user":{"id":"u-abcdef","name":"cafe resume"},"imp":[{"id":"imp-1","banner":{"w":300,"h":250},"bidfloor":0.5},{"id":"imp-2","video":{"mimes":["video/mp4"],"maxduration":30},"bidfloor":2.0}],"regs":{"coppa":0},"ext":null,"test":true})

record = fn i ->
  ~s({"metadata":{"result_type":"recent","iso_language_code":"en"},"id":#{505_874_924_000_000_000 + i},"id_str":"#{505_874_924_000_000_000 + i}","text":"Sample tweet #{i} lorem ipsum dolor sit amet consectetur adipiscing elit","truncated":false,"in_reply_to_status_id":null,"user":{"id":#{1_000_000 + i},"screen_name":"username_#{i}","location":"San Francisco, CA","url":null,"followers_count":#{rem(i * 1337, 100_000)},"verified":false,"lang":"en","profile_image_url":"http://pbs.twimg.com/profile_images/#{i}/photo.jpeg"},"geo":null,"retweet_count":#{rem(i * 3, 1000)},"favorite_count":#{rem(i * 7, 2000)},"entities":{"hashtags":[{"text":"elixir","indices":[15,22]}],"urls":[],"user_mentions":[{"screen_name":"user_#{i}","id":#{2_000_000 + i}}]},"favorited":false,"lang":"en"})
end

large_json =
  ~s({"statuses":[) <>
    Enum.map_join(1..200, ",", record) <>
    ~s(],"search_metadata":{"count":200,"completed_in":0.035,"max_id":505874924095815681,"query":"%23elixir"}})

small_term = :glazer_json.decode(small_json)
large_term = :glazer_json.decode(large_json)

paths =
  Enum.map(
    [
      ".id",
      ".site.domain",
      ".site.page",
      ".device.ip",
      ".device.geo.country",
      ".user.id",
      ".regs.coppa"
    ],
    &:glazer.compile_path/1
  )

IO.puts("glazer PGO workload: small=#{byte_size(small_json)}B large=#{byte_size(large_json)}B")

decode = fn ->
  :glazer_json.decode(small_json)
  :glazer_json.decode(large_json)
end

encode = fn ->
  :glazer_json.encode(small_term)
  :glazer_json.encode(large_term)
end

find = fn ->
  d = :glazer_json.decode(small_json)
  Enum.each(paths, &:glazer.find(d, &1))
end

Enum.each(1..5_000, fn _ -> decode.() end)
Enum.each(1..5_000, fn _ -> encode.() end)
Enum.each(1..10_000, fn _ -> find.() end)

IO.puts("glazer PGO workload complete")

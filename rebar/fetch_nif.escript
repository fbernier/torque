#!/usr/bin/env escript
%% -*- erlang -*-
%%
%% Puts the Torque NIF at priv/native/torque_nif.so for a rebar3 build: the
%% precompiled release asset for this platform, checked against the checksums
%% shipped in the package, or a cargo build when TORQUE_BUILD=true or no asset
%% fits this platform. Mirrors what RustlerPrecompiled does for Mix builds.
%%
%%   TORQUE_BUILD=true       build from source with cargo
%%   TORQUE_CPU_VARIANT=v3   force an x86_64 variant (v3, v2, or anything else for baseline)
%%   TORQUE_NIF_VSN=0.4.7    fetch another release's asset (it must be in the checksum file)
%%
%% `escript rebar/fetch_nif.escript --print-asset' prints the asset this host would use.
-mode(compile).

-define(REPO, "https://github.com/lpgauth/torque").
-define(NIF_VERSION, "2.15").
-define(DEST, "priv/native/torque_nif.so").
-define(V2_FLAGS, ["pni", "ssse3", "sse4_1", "sse4_2", "popcnt", "cx16", "lahf_lm"]).
-define(V3_FLAGS,
    ?V2_FLAGS ++ ["avx", "avx2", "bmi1", "bmi2", "f16c", "fma", "abm", "movbe", "pclmulqdq"]
).

main(["--print-asset"]) ->
    case asset() of
        {ok, Asset} -> io:format("~s~n", [Asset]);
        {error, Why} -> io:format("none: ~s~n", [Why])
    end;
main([]) ->
    try
        run()
    catch
        throw:{fail, Fmt, Args} ->
            io:format(standard_error, "torque: " ++ Fmt ++ "~n", Args),
            halt(1)
    end.

run() ->
    case {force_build(), filelib:is_regular(?DEST)} of
        {true, _} ->
            build();
        {false, true} ->
            ok;
        {false, false} ->
            case asset() of
                {ok, Asset} ->
                    fetch(Asset);
                {error, Why} ->
                    io:format("torque: no precompiled NIF (~s), building from source~n", [Why]),
                    build()
            end
    end.

force_build() ->
    lists:member(os:getenv("TORQUE_BUILD"), ["1", "true"]).

%% --- Choosing the asset ---

asset() ->
    case target() of
        {ok, Target} ->
            {ok,
                lists:flatten([
                    "libtorque_nif-v", vsn(), "-nif-", ?NIF_VERSION, "-", Target,
                    variant_suffix(Target), ".so.tar.gz"
                ])};
        Error ->
            Error
    end.

vsn() ->
    case os:getenv("TORQUE_NIF_VSN") of
        Vsn when is_list(Vsn), Vsn =/= "" ->
            Vsn;
        _ ->
            {ok, [{application, torque, Props}]} = file:consult("src/torque.app.src"),
            proplists:get_value(vsn, Props)
    end.

target() ->
    SystemArch = erlang:system_info(system_architecture),
    Arch =
        case hd(string:split(SystemArch, "-")) of
            "amd64" -> "x86_64";
            "arm64" -> "aarch64";
            Other -> Other
        end,
    Os =
        case {string:find(SystemArch, "darwin"), string:find(SystemArch, "linux-gnu")} of
            {nomatch, nomatch} -> unsupported;
            {nomatch, _} -> "unknown-linux-gnu";
            {_, _} -> "apple-darwin"
        end,
    case lists:member(Arch, ["aarch64", "x86_64"]) andalso Os =/= unsupported of
        true -> {ok, Arch ++ "-" ++ Os};
        false -> {error, "unsupported platform " ++ SystemArch}
    end.

variant_suffix("x86_64-" ++ _) ->
    case os:getenv("TORQUE_CPU_VARIANT") of
        "v3" -> "--v3";
        "v2" -> "--v2";
        false ->
            case {has_level(?V3_FLAGS, "hw.optional.avx2_0"), has_level(?V2_FLAGS, "hw.optional.sse4_2")} of
                {true, _} -> "--v3";
                {false, true} -> "--v2";
                {false, false} -> ""
            end;
        _ -> ""
    end;
variant_suffix(_) ->
    "".

%% Every Mac with AVX2 (or SSE4.2) has the rest of that level, so one sysctl
%% stands for it, as in lib/torque/cpu.ex.
has_level(LinuxFlags, DarwinSysctl) ->
    case os:type() of
        {unix, linux} ->
            case file:read_file("/proc/cpuinfo") of
                {ok, Info} ->
                    lists:any(
                        fun(Line) ->
                            case string:prefix(Line, "flags") of
                                nomatch -> false;
                                _ -> LinuxFlags -- string:lexemes(Line, " \t:") =:= []
                            end
                        end,
                        string:split(binary_to_list(Info), "\n", all)
                    );
                _ ->
                    false
            end;
        {unix, darwin} ->
            os:cmd("sysctl -n " ++ DarwinSysctl ++ " 2>/dev/null") =:= "1\n";
        _ ->
            false
    end.

%% --- Fetching and verifying ---

fetch(Asset) ->
    Expected = expected_sha256(Asset),
    Cached = filename:join(filename:basedir(user_cache, "torque"), Asset),
    Tarball =
        case file:read_file(Cached) of
            {ok, Bin} ->
                case sha256(Bin) of
                    Expected -> Bin;
                    _ -> download(Asset, Cached)
                end;
            _ ->
                download(Asset, Cached)
        end,
    case sha256(Tarball) of
        Expected -> ok;
        Got -> throw({fail, "checksum mismatch for ~s: expected ~s, got ~s", [Asset, Expected, Got]})
    end,
    install(Asset, Tarball),
    io:format("torque: installed ~s~n", [Asset]).

expected_sha256(Asset) ->
    {ok, Checksums} = file:read_file("checksum-Elixir.Torque.Native.exs"),
    Pattern = "\"" ++ re_escape(Asset) ++ "\" => \"sha256:([0-9a-f]{64})\"",
    case re:run(Checksums, Pattern, [{capture, all_but_first, list}]) of
        {match, [Sha]} -> Sha;
        nomatch -> throw({fail, "~s is not in the shipped checksum file", [Asset]})
    end.

re_escape(String) ->
    lists:flatmap(
        fun(C) ->
            case lists:member(C, ".-+[](){}^$|?*\\") of
                true -> [$\\, C];
                false -> [C]
            end
        end,
        String
    ).

download(Asset, Cached) ->
    Url = ?REPO ++ "/releases/download/v" ++ vsn() ++ "/" ++ Asset,
    io:format("torque: downloading ~s~n", [Url]),
    {ok, _} = application:ensure_all_started(ssl),
    {ok, _} = application:ensure_all_started(inets),
    Request = {Url, [{"user-agent", "torque-rebar3"}]},
    HttpOpts = [{ssl, tls_opts()}, {autoredirect, true}, {timeout, 120000}],
    case httpc:request(get, Request, HttpOpts, [{body_format, binary}]) of
        {ok, {{_, 200, _}, _, Body}} ->
            ok = filelib:ensure_dir(Cached),
            _ = file:write_file(Cached, Body),
            Body;
        {ok, {{_, Status, _}, _, _}} ->
            throw({fail, "download of ~s failed with HTTP ~p", [Url, Status]});
        {error, Reason} ->
            throw({fail, "download of ~s failed: ~p", [Url, Reason]})
    end.

%% Verified TLS needs the OS trust store, which public_key reads from OTP 25.
tls_opts() ->
    _ = code:ensure_loaded(public_key),
    case erlang:function_exported(public_key, cacerts_get, 0) of
        true ->
            [
                {verify, verify_peer},
                {cacerts, public_key:cacerts_get()},
                {depth, 4},
                {customize_hostname_check, [
                    {match_fun, public_key:pkix_verify_hostname_match_fun(https)}
                ]}
            ];
        false ->
            throw({fail, "downloading the NIF needs OTP 25 or later; set TORQUE_BUILD=true to build it", []})
    end.

sha256(Bin) ->
    string:lowercase(binary_to_list(binary:encode_hex(crypto:hash(sha256, Bin)))).

%% The archive holds the library under the asset's name without `.tar.gz'.
install(Asset, Tarball) ->
    Name = string:replace(Asset, ".tar.gz", "", trailing),
    {ok, Files} = erl_tar:extract({binary, Tarball}, [compressed, memory]),
    case lists:keyfind(lists:flatten(Name), 1, Files) of
        {_, Lib} ->
            ok = filelib:ensure_dir(?DEST),
            ok = file:write_file(?DEST, Lib);
        false ->
            throw({fail, "~s holds no ~s", [Asset, Name]})
    end.

%% --- Building from source ---

build() ->
    Cargo =
        case os:find_executable("cargo") of
            false -> filename:join([os:getenv("HOME", ""), ".cargo", "bin", "cargo"]);
            Found -> Found
        end,
    filelib:is_regular(Cargo) orelse
        throw({fail, "cargo not found; install a Rust toolchain to build the NIF", []}),
    io:format("torque: building the NIF with cargo~n", []),
    Port = open_port({spawn_executable, Cargo}, [
        {args, ["build", "--release", "--package", "torque_nif"]},
        exit_status,
        stderr_to_stdout,
        binary
    ]),
    case cargo_output(Port) of
        0 -> ok;
        Status -> throw({fail, "cargo build exited with ~p", [Status]})
    end,
    Ext =
        case os:type() of
            {unix, darwin} -> ".dylib";
            _ -> ".so"
        end,
    ok = filelib:ensure_dir(?DEST),
    {ok, _} = file:copy("target/release/libtorque_nif" ++ Ext, ?DEST),
    ok.

cargo_output(Port) ->
    receive
        {Port, {data, Data}} ->
            io:put_chars(Data),
            cargo_output(Port);
        {Port, {exit_status, Status}} ->
            Status
    end.

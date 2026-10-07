%% @doc Erlang API for Torque, a JSON library backed by sonic-rs NIFs.
%%
%% JSON null is the atom `null', objects decode to maps with binary keys,
%% arrays to lists and strings to binaries. In the bulk lookups a path the
%% document does not contain returns `undefined', so it stays distinct from a
%% JSON null. Inputs larger than 20 KB run on a dirty CPU scheduler.
%%
%% Documents and pointer handles answer with the atoms of the API that
%% created them: use handles from this module with this module.
-module(torque).

-export([
    decode/1, decode/2,
    encode/1, encode/2,
    parse/1, parse/2,
    get/2, get/3,
    get_many/2,
    get_many_values/2,
    length/2,
    compile_pointers/1, compile_pointers/2,
    parse_get_many_values/2
]).

-export_type([json/0, document/0, pointers/0]).

-define(NIF, 'Elixir.Torque.Native').
-define(TIMESLICE_BYTES, 20480).

-type json() ::
    null
    | boolean()
    | number()
    | binary()
    | [json()]
    | #{binary() => json()}.
-opaque document() :: reference().
-opaque pointers() :: reference().
-type decode_error() :: binary() | nesting_too_deep.
-type encode_error() ::
    nesting_too_deep
    | unsupported_type
    | non_finite_float
    | invalid_key
    | malformed_proplist
    | invalid_utf8
    | unhandled_struct.

%% @equiv decode(Json, [])
-spec decode(binary()) -> {ok, json()} | {error, decode_error()}.
decode(Json) ->
    decode(Json, []).

%% @doc Decodes a JSON binary.
%%
%% Options: `{strings, reference | copy}'. With `reference' (the default),
%% strings longer than 64 bytes that needed no unescaping are sub-binaries of
%% `Json', which keeps all of it alive while any of them is; `copy' gives every
%% string its own binary.
-spec decode(binary(), [{strings, reference | copy}]) ->
    {ok, json()} | {error, decode_error()}.
decode(Json, Opts) when is_binary(Json), is_list(Opts) ->
    check_opts(Opts, [strings]),
    Copy =
        case proplists:get_value(strings, Opts, reference) of
            reference -> false;
            copy -> true;
            Other -> erlang:error({invalid_option, {strings, Other}})
        end,
    case byte_size(Json) > ?TIMESLICE_BYTES of
        true -> ?NIF:decode_opts_dirty(Json, Copy, null);
        false -> ?NIF:decode_opts(Json, Copy, null)
    end.

%% @equiv encode(Term, [])
-spec encode(term()) -> {ok, binary()} | {error, encode_error()}.
encode(Term) ->
    encode(Term, []).

%% @doc Encodes a term as JSON.
%%
%% `null' encodes as JSON null; `true' and `false' as booleans; other atoms as
%% strings. Maps take atom, binary or integer keys, and `{Proplist}' tuples
%% encode as objects. Options: `dirty' runs the encode on a dirty CPU
%% scheduler, for terms expected to produce large output.
-spec encode(term(), [dirty | {dirty, boolean()}]) ->
    {ok, binary()} | {error, encode_error()}.
encode(Term, Opts) when is_list(Opts) ->
    check_opts(Opts, [dirty]),
    case proplists:get_bool(dirty, Opts) of
        true -> ?NIF:encode_opts_dirty(Term, null);
        false -> ?NIF:encode_opts(Term, null)
    end.

%% @equiv parse(Json, [])
-spec parse(binary()) -> {ok, document()} | {error, decode_error()}.
parse(Json) ->
    parse(Json, []).

%% @doc Parses a JSON binary into a document to query with JSON Pointers
%% (RFC 6901). Options: `{unique_keys, true}' speeds up key lookups when the
%% document has no duplicate keys (the last value wins by default).
-spec parse(binary(), [{unique_keys, boolean()}]) ->
    {ok, document()} | {error, decode_error()}.
parse(Json, Opts) when is_binary(Json), is_list(Opts) ->
    check_opts(Opts, [unique_keys]),
    UniqueKeys = proplists:get_value(unique_keys, Opts, false),
    case byte_size(Json) > ?TIMESLICE_BYTES of
        true -> ?NIF:parse_opts_dirty(Json, UniqueKeys, null, undefined);
        false -> ?NIF:parse_opts(Json, UniqueKeys, null, undefined)
    end.

%% @doc Returns the value at `Path', a JSON Pointer such as `<<"/a/0/b">>'.
%% A malformed pointer matches nothing.
-spec get(document(), binary()) ->
    {ok, json()} | {error, no_such_field | nesting_too_deep}.
get(Doc, Path) when is_reference(Doc), is_binary(Path) ->
    ?NIF:get(Doc, Path).

%% @doc Returns the value at `Path', or `Default' when there is none.
-spec get(document(), binary(), Default) -> json() | Default.
get(Doc, Path, Default) when is_reference(Doc), is_binary(Path) ->
    case ?NIF:get(Doc, Path) of
        {ok, Value} -> Value;
        {error, no_such_field} -> Default;
        {error, Reason} -> erlang:error(Reason)
    end.

%% @doc Looks up several paths in one call, each as `get/2' would.
-spec get_many(document(), [binary()]) ->
    [{ok, json()} | {error, no_such_field | nesting_too_deep}].
get_many(Doc, Paths) when is_reference(Doc), is_list(Paths) ->
    ?NIF:get_many(Doc, Paths).

%% @doc Looks up several paths in one call, returning bare values:
%% `undefined' for a path the document does not contain. Takes a list of
%% pointers or a handle from `compile_pointers/1,2'.
-spec get_many_values(document(), [binary()] | pointers()) -> [json() | undefined].
get_many_values(Doc, Paths) when is_reference(Doc), is_list(Paths) ->
    ?NIF:get_many_nil(Doc, Paths);
get_many_values(Doc, Pointers) when is_reference(Doc), is_reference(Pointers) ->
    ?NIF:get_many_nil_compiled(Doc, Pointers).

%% @doc Returns the length of the array at `Path', or `undefined' when the
%% path does not exist or is not an array.
-spec length(document(), binary()) -> non_neg_integer() | undefined.
length(Doc, Path) when is_reference(Doc), is_binary(Path) ->
    ?NIF:array_length(Doc, Path).

%% @equiv compile_pointers(Paths, [])
-spec compile_pointers([binary()]) -> pointers().
compile_pointers(Paths) ->
    compile_pointers(Paths, []).

%% @doc Compiles a fixed set of JSON Pointers once, for `get_many_values/2'
%% and `parse_get_many_values/2'. Raises `badarg' for a malformed pointer.
%%
%% Options: `{unique_keys, boolean()}' as in `parse/2', and
%% `{validate, false}' to skip regions no path selects without validating
%% their syntax: faster when the paths select a small part of the document,
%% and only for trusted input.
-spec compile_pointers([binary()], [{unique_keys, boolean()} | {validate, boolean()}]) ->
    pointers().
compile_pointers(Paths, Opts) when is_list(Paths), is_list(Opts) ->
    check_opts(Opts, [unique_keys, validate]),
    UniqueKeys = proplists:get_value(unique_keys, Opts, false),
    Validate = proplists:get_value(validate, Opts, true),
    ?NIF:compile_paths(Paths, UniqueKeys, Validate, null, undefined).

%% @doc Parses `Json' and returns the values at the compiled pointers in one
%% pass, without building the document: `undefined' for a path the document
%% does not contain.
-spec parse_get_many_values(binary(), pointers()) ->
    {ok, [json() | undefined]} | {error, decode_error()}.
parse_get_many_values(Json, Pointers) when is_binary(Json), is_reference(Pointers) ->
    case byte_size(Json) > ?TIMESLICE_BYTES of
        true -> ?NIF:parse_get_many_nil_dirty(Json, Pointers);
        false -> ?NIF:parse_get_many_nil(Json, Pointers)
    end.

check_opts(Opts, Allowed) ->
    lists:foreach(
        fun
            ({Key, _} = Opt) ->
                lists:member(Key, Allowed) orelse erlang:error({invalid_option, Opt});
            (Key) when is_atom(Key) ->
                lists:member(Key, Allowed) orelse erlang:error({invalid_option, Key});
            (Opt) ->
                erlang:error({invalid_option, Opt})
        end,
        Opts
    ).

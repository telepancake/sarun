:- module(grammar_codec, [codec_value/3]).

/** <module> Grammar-neutral relational terminal codecs

Terminal behavior is immutable grammar data interpreted here.  A grammar may
compose finite enumerations, text and integer wrappers, and closed JSON shapes
without supplying parser or printer predicates.

JSON shape values are:

  * `string`
  * `object(Functor, Fields)` where fields are `field(Name, Shape)`
  * `array(Shape)`
  * `tuple(Functor, Shapes)`
  * `nullable(Shape, EmptyValue, PresentFunctor)`
*/

codec_value(enumeration(Surfaces), Value, Surface) :-
    surface_data(Value, Surface, Surfaces).
codec_value(integer(Functor), Value, Surface) :-
    atom(Functor),
    Value =.. [Functor, Integer],
    integer_surface(Integer, Surface).
codec_value(text(Functor), Value, Surface) :-
    atom(Functor),
    Value =.. [Functor, Text],
    text_surface(Text, Surface).
codec_value(choice(Codecs), Value, Surface) :-
    codec_member(Codec, Codecs),
    codec_value(Codec, Value, Surface).
codec_value(json(Shape), Value, Surface) :-
    json_surface(Shape, Value, Surface).

surface_data(Value, Surface, [surface(Value, Surface)|_]).
surface_data(Value, Surface, [_|Surfaces]) :-
    surface_data(Value, Surface, Surfaces).

codec_member(Codec, [Codec|_]).
codec_member(Codec, [_|Codecs]) :- codec_member(Codec, Codecs).

integer_surface(Integer, Surface) :-
    ( integer(Integer)
    -> number_string(Integer, Surface)
    ;  text_string(Surface, String),
       number_string(Integer, String),
       integer(Integer)
    ).

text_surface(Text, Surface) :-
    ( text(Text)
    -> text_string(Text, Surface)
    ;  text_string(Surface, Text)
    ).

json_surface(Shape, Value, Surface) :-
    ground(Surface),
    !,
    text_string(Surface, SurfaceString),
    string_codes(SurfaceString, Codes),
    phrase(json_document(Json), Codes),
    shape_json(Shape, Value, Json).
json_surface(Shape, Value, Surface) :-
    ground(Value),
    shape_json(Shape, Value, Json),
    json_render_value(Json, Codes),
    string_codes(Surface, Codes).

shape_json(string, Value, json_string(Value)) :- string(Value).
shape_json(object(Functor, Fields), Value, json_object(Pairs)) :-
    atom(Functor),
    ( ground(Pairs)
    -> object_fields_from_json(Fields, Pairs, Values, Rest),
       Rest = [],
       Value =.. [Functor|Values]
    ;  Value =.. [Functor|Values],
       object_fields_to_json(Fields, Values, Pairs)
    ).
shape_json(array(Shape), Values, json_array(JsonValues)) :-
    shape_json_values(Shape, Values, JsonValues).
shape_json(tuple(Functor, Shapes), Value, json_array(JsonValues)) :-
    atom(Functor),
    ( ground(JsonValues)
    -> shapes_json(Shapes, Values, JsonValues), Value =.. [Functor|Values]
    ;  Value =.. [Functor|Values], shapes_json(Shapes, Values, JsonValues)
    ).
shape_json(nullable(Shape, Empty, PresentFunctor), Value, Json) :-
    atom(PresentFunctor),
    ( ground(Json)
    -> ( Json = json_null
       -> Value = Empty
       ;  shape_json(Shape, Present, Json),
          Value =.. [PresentFunctor, Present]
       )
    ;  ( Value = Empty
       -> Json = json_null
       ;  Value =.. [PresentFunctor, Present],
          shape_json(Shape, Present, Json)
       )
    ).

object_fields_from_json([], Pairs, [], Pairs).
object_fields_from_json([field(Name, Shape)|Fields], Pairs0,
                        [Value|Values], Rest) :-
    json_take(Name, Pairs0, Json, Pairs1),
    shape_json(Shape, Value, Json),
    object_fields_from_json(Fields, Pairs1, Values, Rest).

object_fields_to_json([], [], []).
object_fields_to_json([field(Name, Shape)|Fields], [Value|Values],
                      [Name-Json|Pairs]) :-
    shape_json(Shape, Value, Json),
    object_fields_to_json(Fields, Values, Pairs).

shape_json_values(_, [], []).
shape_json_values(Shape, [Value|Values], [Json|JsonValues]) :-
    shape_json(Shape, Value, Json),
    shape_json_values(Shape, Values, JsonValues).

shapes_json([], [], []).
shapes_json([Shape|Shapes], [Value|Values], [Json|JsonValues]) :-
    shape_json(Shape, Value, Json),
    shapes_json(Shapes, Values, JsonValues).

json_take(Name, [Name-Value|Pairs], Value, Pairs) :- !.
json_take(Name, [Pair|Pairs], Value, [Pair|Rest]) :-
    json_take(Name, Pairs, Value, Rest).

json_document(Value) --> json_space, json_value(Value), json_space.

json_value(json_string(Value)) --> json_string(Value).
json_value(json_object(Pairs)) --> [123], json_space,
                                  json_object_members(Pairs), json_space, [125].
json_value(json_array(Values)) --> [91], json_space,
                                  json_array_members(Values), json_space, [93].
json_value(json_null) --> [110, 117, 108, 108].
json_value(json_true) --> [116, 114, 117, 101].
json_value(json_false) --> [102, 97, 108, 115, 101].

json_object_members([]) --> [].
json_object_members([Key-Value|Pairs]) -->
    json_string(Key), json_space, [58], json_space, json_value(Value),
    json_object_members_tail(Pairs).

json_object_members_tail([]) --> [].
json_object_members_tail([Key-Value|Pairs]) -->
    json_space, [44], json_space, json_string(Key), json_space, [58],
    json_space, json_value(Value), json_object_members_tail(Pairs).

json_array_members([]) --> [].
json_array_members([Value|Values]) -->
    json_value(Value), json_array_members_tail(Values).

json_array_members_tail([]) --> [].
json_array_members_tail([Value|Values]) -->
    json_space, [44], json_space, json_value(Value),
    json_array_members_tail(Values).

json_string(Value) --> [34], json_string_codes(Codes), [34],
                       { string_codes(Value, Codes) }.

json_string_codes([]) --> [].
json_string_codes([Code|Codes]) -->
    json_string_code(Code),
    json_string_codes(Codes).

json_string_code(Code) --> [92], json_escape(Code), !.
json_string_code(Code) --> [Code],
    { Code >= 32, Code =\= 34, Code =\= 92 }.

json_escape(34) --> [34].
json_escape(92) --> [92].
json_escape(47) --> [47].
json_escape(8) --> [98].
json_escape(12) --> [102].
json_escape(10) --> [110].
json_escape(13) --> [114].
json_escape(9) --> [116].
json_escape(Code) --> [117], json_hex_quad(High),
                      json_unicode_tail(High, Code).

json_unicode_tail(High, Code) -->
    { High >= 55296, High =< 56319 },
    !,
    [92, 117], json_hex_quad(Low),
    { Low >= 56320, Low =< 57343,
      Code is 65536 + ((High - 55296) * 1024) + Low - 56320
    }.
json_unicode_tail(Code, Code) -->
    { ( Code < 55296 ; Code > 57343 ) }.

json_hex_quad(Value) -->
    json_hex_digit(A), json_hex_digit(B),
    json_hex_digit(C), json_hex_digit(D),
    { Value is A * 4096 + B * 256 + C * 16 + D }.

json_hex_digit(Value) --> [Code], { json_hex_code(Code, Value) }.

json_hex_code(Code, Value) :-
    Code >= 48, Code =< 57, !, Value is Code - 48.
json_hex_code(Code, Value) :-
    Code >= 65, Code =< 70, !, Value is Code - 55.
json_hex_code(Code, Value) :-
    Code >= 97, Code =< 102, Value is Code - 87.

json_space --> [Code], { json_space_code(Code) }, !, json_space.
json_space --> [].

json_space_code(32).
json_space_code(9).
json_space_code(10).
json_space_code(13).

json_render_value(json_string(Value), [34|Codes]) :-
    string_codes(Value, StringCodes),
    json_render_string_codes(StringCodes, Escaped),
    append(Escaped, [34], Codes).
json_render_value(json_object(Pairs), Codes) :-
    json_render_object_members(Pairs, Members),
    append([123|Members], [125], Codes).
json_render_value(json_array(Values), Codes) :-
    json_render_array_members(Values, Members),
    append([91|Members], [93], Codes).
json_render_value(json_null, [110, 117, 108, 108]).
json_render_value(json_true, [116, 114, 117, 101]).
json_render_value(json_false, [102, 97, 108, 115, 101]).

json_render_object_members([], []).
json_render_object_members([Key-Value|Pairs], Codes) :-
    json_render_value(json_string(Key), KeyCodes),
    json_render_value(Value, ValueCodes),
    append(KeyCodes, [58|ValueCodes], Field),
    json_render_object_tail(Pairs, Tail),
    append(Field, Tail, Codes).

json_render_object_tail([], []).
json_render_object_tail(Pairs, [44|Codes]) :-
    json_render_object_members(Pairs, Codes).

json_render_array_members([], []).
json_render_array_members([Value|Values], Codes) :-
    json_render_value(Value, ValueCodes),
    json_render_array_tail(Values, Tail),
    append(ValueCodes, Tail, Codes).

json_render_array_tail([], []).
json_render_array_tail(Values, [44|Codes]) :-
    json_render_array_members(Values, Codes).

json_render_string_codes([], []).
json_render_string_codes([Code|Codes], Escaped) :-
    json_render_string_code(Code, Head),
    json_render_string_codes(Codes, Tail),
    append(Head, Tail, Escaped).

json_render_string_code(34, [92, 34]) :- !.
json_render_string_code(92, [92, 92]) :- !.
json_render_string_code(8, [92, 98]) :- !.
json_render_string_code(9, [92, 116]) :- !.
json_render_string_code(10, [92, 110]) :- !.
json_render_string_code(12, [92, 102]) :- !.
json_render_string_code(13, [92, 114]) :- !.
json_render_string_code(Code, [Code]) :-
    Code >= 33, Code =< 126, !.
json_render_string_code(Code, Escaped) :-
    Code >= 0, Code =< 65535, !,
    json_render_unicode_escape(Code, Escaped).
json_render_string_code(Code, Escaped) :-
    Code =< 1114111,
    Plane is Code - 65536,
    High is 55296 + Plane // 1024,
    Low is 56320 + Plane mod 1024,
    json_render_unicode_escape(High, HighCodes),
    json_render_unicode_escape(Low, LowCodes),
    append(HighCodes, LowCodes, Escaped).

json_render_unicode_escape(Code, [92, 117, A, B, C, D]) :-
    AValue is (Code // 4096) mod 16,
    BValue is (Code // 256) mod 16,
    CValue is (Code // 16) mod 16,
    DValue is Code mod 16,
    json_render_hex_code(AValue, A),
    json_render_hex_code(BValue, B),
    json_render_hex_code(CValue, C),
    json_render_hex_code(DValue, D).

json_render_hex_code(Value, Code) :-
    Value < 10, !, Code is Value + 48.
json_render_hex_code(Value, Code) :- Code is Value + 87.

text(Text) :- string(Text), !.
text(Text) :- atom(Text).

text_string(Text, Text) :- string(Text), !.
text_string(Text, String) :- atom_string(Text, String).

% Core-only embedded SWI does not load library(lists).
append([], Tail, Tail).
append([Head|Items], Tail, [Head|Result]) :- append(Items, Tail, Result).

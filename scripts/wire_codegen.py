#!/usr/bin/env python3
"""Generate concrete Rust transport types/codecs from the Prolog relation."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import glob
import hashlib
from pathlib import Path
import platform
import re
import subprocess
import tempfile


@dataclass(frozen=True)
class Compound:
    name: str
    args: tuple["Term", ...]


Term = str | int | list["Term"] | Compound


SCHEMA_SOURCES = (
    "engine/pl/action_catalog.pl",
    "engine/pl/action_grammar.pl",
    "engine/pl/grammar_engine.pl",
    "engine/pl/text_grammar_engine.pl",
    "engine/pl/brush_grammar.pl",
    "engine/pl/evidence_projection.pl",
    "engine/pl/grammar_codec.pl",
    "engine/pl/grammar_store.pl",
    "engine/pl/grammar_ir.pl",
    "engine/pl/relation_api.pl",
    "engine/pl/context_relation.pl",
    "engine/pl/transport_catalog.pl",
    "engine/pl/wire_codegen.pl",
    "scripts/wire_codegen.py",
)


class TermParser:
    def __init__(self, source: str):
        self.source = source
        self.at = 0

    def parse(self) -> Term:
        value = self.term()
        if self.at != len(self.source):
            raise ValueError(f"trailing Prolog term text: {self.source[self.at:]!r}")
        return value

    def term(self) -> Term:
        if self.peek() == "[":
            return self.list()
        if self.peek() == "'":
            atom = self.quoted_atom()
        elif self.peek() == "-" or self.peek().isdigit():
            match = re.match(r"-?[0-9]+", self.source[self.at :])
            if match is not None:
                self.at += len(match.group(0))
                return int(match.group(0))
            atom = self.atom()
        else:
            atom = self.atom()
        if self.peek() != "(":
            return atom
        self.at += 1
        args: list[Term] = []
        if self.peek() != ")":
            while True:
                args.append(self.term())
                if self.peek() != ",":
                    break
                self.at += 1
        self.expect(")")
        return Compound(atom, tuple(args))

    def list(self) -> list[Term]:
        self.expect("[")
        values: list[Term] = []
        if self.peek() != "]":
            while True:
                values.append(self.term())
                if self.peek() != ",":
                    break
                self.at += 1
        self.expect("]")
        return values

    def atom(self) -> str:
        match = re.match(r"[A-Za-z_$][A-Za-z0-9_$]*", self.source[self.at :])
        if match is None:
            raise ValueError(f"expected atom at {self.source[self.at:]!r}")
        self.at += len(match.group(0))
        return match.group(0)

    def quoted_atom(self) -> str:
        self.expect("'")
        out: list[str] = []
        while self.at < len(self.source):
            char = self.source[self.at]
            self.at += 1
            if char == "'":
                if self.peek() == "'":
                    self.at += 1
                    out.append("'")
                    continue
                return "".join(out)
            if char == "\\" and self.at < len(self.source):
                out.append(self.source[self.at])
                self.at += 1
            else:
                out.append(char)
        raise ValueError("unterminated quoted atom")

    def peek(self) -> str:
        return self.source[self.at : self.at + 1]

    def expect(self, expected: str) -> None:
        if self.peek() != expected:
            raise ValueError(f"expected {expected!r} at {self.source[self.at:]!r}")
        self.at += 1


def parse_term(source: str) -> Term:
    return TermParser(source).parse()


def find_swipl() -> str:
    machine = platform.machine()
    candidates = glob.glob(
        str(Path.home() / ".cache/sarun/swipl/9.2.9/pipeline-*" / machine
            / "native-swipl-build/src/swipl")
    )
    if not candidates:
        raise RuntimeError("pinned host SWI-Prolog is missing; run `make swipl`")
    return sorted(candidates)[-1]


def relation_rows(repo: Path, swipl: str) -> list[tuple[str, list[Term]]]:
    command = [
        swipl,
        "-q", "-f", "none",
        "-s", str(repo / "engine/pl/wire_codegen.pl"),
        "-g", "wire_codegen:emit_manifest",
        "-t", "halt",
    ]
    output = subprocess.run(command, check=True, text=True,
                            stdout=subprocess.PIPE).stdout
    rows = []
    for line in output.splitlines():
        cells = line.split("\t")
        rows.append((cells[0], [parse_term(cell) for cell in cells[1:]]))
    return rows


def words(name: str) -> list[str]:
    return [word for word in re.split(r"[^A-Za-z0-9]+", name) if word]


def pascal(name: str) -> str:
    return "".join(word[:1].upper() + word[1:] for word in words(name))


RUST_KEYWORDS = {
    "as", "async", "await", "become", "box", "break", "const", "continue",
    "crate", "do", "dyn", "else", "enum", "extern", "false", "final", "fn",
    "for", "gen", "if", "impl", "in", "let", "loop", "macro", "match", "mod",
    "move", "mut", "override", "priv", "pub", "ref", "return", "self", "Self",
    "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "union", "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
}


def field_name(name: str) -> str:
    normalized = re.sub(r"[^A-Za-z0-9_]", "_", name)
    return f"r#{normalized}" if normalized in RUST_KEYWORDS else normalized


def const_name(name: str) -> str:
    return "LIMIT_" + re.sub(r"[^A-Za-z0-9]", "_", name).upper()


def compound(term: Term, name: str, arity: int | None = None) -> tuple[Term, ...]:
    if not isinstance(term, Compound) or term.name != name:
        raise ValueError(f"expected {name}, got {term!r}")
    if arity is not None and len(term.args) != arity:
        raise ValueError(f"expected {name}/{arity}, got {term!r}")
    return term.args


def atom(term: Term) -> str:
    if not isinstance(term, str):
        raise ValueError(f"expected atom, got {term!r}")
    return term


class Generator:
    def __init__(self, rows: list[tuple[str, list[Term]]],
                 source_hashes: dict[str, str]):
        self.rows = rows
        self.source_hashes = source_hashes
        self.limits = {
            atom(values[0]): int(values[1])
            for category, values in rows if category == "limit"
        }
        self.enums: dict[str, list[tuple[str, int]]] = {}
        self.variants: dict[str, list[tuple[str, int, list[Term]]]] = {}
        self.types: dict[str, Term] = {}
        for category, values in rows:
            if category == "type":
                name = atom(values[0])
                if name in self.types:
                    raise ValueError(f"duplicate wire type {name}")
                self.types[name] = values[1]
            elif category == "enum":
                self.enums.setdefault(atom(values[0]), []).append(
                    (atom(values[1]), int(values[2])))
            elif category == "variant":
                self.variants.setdefault(atom(values[0]), []).append(
                    (atom(values[1]), int(values[2]), self.fields(values[3])))

    def fields(self, term: Term) -> list[Term]:
        if not isinstance(term, list):
            raise ValueError(f"expected field list, got {term!r}")
        for value in term:
            compound(value, "field", 2)
        return term

    def rust_type(self, term: Term) -> str:
        if isinstance(term, str):
            primitive = {
                "bool": "bool", "u16": "u16", "u32": "u32", "u64": "u64",
                "s32": "i32", "s64": "i64", "f64": "f64",
                "response": "TransportResponse",
                "action_success": "ActionSuccess",
            }
            return primitive.get(term, pascal(term))
        if isinstance(term, Compound):
            if term.name in {"text", "bytes"}:
                limit = const_name(atom(term.args[0]))
                wrapper = "BoundedText" if term.name == "text" else "BoundedBytes"
                return f"{wrapper}<{limit}>"
            if term.name == "fixed_bytes":
                return f"FixedBytes<{int(term.args[0])}>"
            if term.name == "option":
                return f"Option<{self.rust_type(term.args[0])}>"
            if term.name == "list":
                if len(term.args) == 2:
                    item, limit = term.args
                    minimum = 0
                else:
                    item, minimum, limit = term.args
                return (f"BoundedVec<{self.rust_type(item)}, {int(minimum)}, "
                        f"{const_name(atom(limit))}>")
            if term.name == "map":
                key, value, limit = term.args
                return (f"BoundedMap<{self.rust_type(key)}, {self.rust_type(value)}, "
                        f"{const_name(atom(limit))}>")
        raise ValueError(f"unsupported wire type {term!r}")

    def field_parts(self, field: Term) -> tuple[str, Term]:
        name, kind = compound(field, "field", 2)
        return atom(name), kind

    def header(self, manifest_hash: str) -> str:
        version_rows = [values for category, values in self.rows if category == "version"]
        if version_rows != [[1]]:
            raise ValueError(f"unexpected protocol versions: {version_rows!r}")
        lines = [
            "// @generated by scripts/wire_codegen.py from the validated Prolog relation.",
            "// Do not edit this file by hand.",
        ]
        for path, digest in self.source_hashes.items():
            lines.append(f"// source-sha256 {path} {digest}")
        lines += [
            "",
            "use crate::wire::{",
            "    BoundedBytes, BoundedMap, BoundedText, BoundedVec, DecodeError,",
            "    FixedBytes, WireValue, get_atom, get_u64, put_compound_payload,",
            "    put_u64, require_empty,",
            "};",
            "use crate::prolog::RelationValue;",
            "use base64::Engine as _;",
            "use std::collections::BTreeMap;",
            "",
            "pub trait RelationWireValue: Sized {",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String>;",
            "}",
            "",
            "fn relation_atom(value: &RelationValue) -> Result<&str, String> {",
            "    match value {",
            "        RelationValue::Atom(value) => Ok(value),",
            "        _ => Err(\"expected relation atom\".into()),",
            "    }",
            "}",
            "",
            "fn relation_integer(value: &RelationValue) -> Result<i64, String> {",
            "    match value {",
            "        RelationValue::Integer(value) => Ok(*value),",
            "        _ => Err(\"expected relation integer\".into()),",
            "    }",
            "}",
            "",
            "fn relation_compound<'a>(value: &'a RelationValue, name: &str) -> Result<&'a [RelationValue], String> {",
            "    match value {",
            "        RelationValue::Compound(actual, fields) if actual == name => Ok(fields),",
            "        _ => Err(format!(\"expected relation {name} compound\")),",
            "    }",
            "}",
            "",
            "fn relation_list(value: &RelationValue) -> Result<&[RelationValue], String> {",
            "    match value {",
            "        RelationValue::List(values) => Ok(values),",
            "        _ => Err(\"expected relation list\".into()),",
            "    }",
            "}",
            "",
            "fn require_relation_arity(values: &[RelationValue], expected: usize) -> Result<(), String> {",
            "    if values.len() == expected { Ok(()) } else {",
            "        Err(format!(\"expected {expected} relation fields, got {}\", values.len()))",
            "    }",
            "}",
            "",
            "macro_rules! relation_integer_value {",
            "    ($type:ty) => {",
            "        impl RelationWireValue for $type {",
            "            fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "                relation_integer(value)?.try_into().map_err(|_| format!(\"relation integer is out of range for {}\", stringify!($type)))",
            "            }",
            "        }",
            "    };",
            "}",
            "relation_integer_value!(u16);",
            "relation_integer_value!(u32);",
            "relation_integer_value!(u64);",
            "relation_integer_value!(i32);",
            "relation_integer_value!(i64);",
            "",
            "impl RelationWireValue for f64 {",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        Ok(relation_integer(value)? as f64)",
            "    }",
            "}",
            "",
            "impl RelationWireValue for bool {",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        match relation_atom(value)? {",
            "            \"true\" => Ok(true),",
            "            \"false\" => Ok(false),",
            "            _ => Err(\"expected true or false relation atom\".into()),",
            "        }",
            "    }",
            "}",
            "",
            "impl RelationWireValue for () {",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        let fields = relation_compound(value, \"record\")?;",
            "        require_relation_arity(fields, 0)",
            "    }",
            "}",
            "",
            "impl<const MAXIMUM: usize> RelationWireValue for BoundedText<MAXIMUM> {",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        let RelationValue::String(value) = value else { return Err(\"expected relation string\".into()) };",
            "        Self::new(value.clone()).map_err(|error| format!(\"bounded relation text: {error:?}\"))",
            "    }",
            "}",
            "",
            "impl<const MAXIMUM: usize> RelationWireValue for BoundedBytes<MAXIMUM> {",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        let bytes = match value {",
            "            RelationValue::String(value) => value.as_bytes().to_vec(),",
            "            RelationValue::Compound(kind, fields) if kind == \"base64\" => {",
            "                require_relation_arity(fields, 1)?;",
            "                let RelationValue::String(value) = &fields[0] else { return Err(\"expected base64 relation string\".into()) };",
            "                base64::engine::general_purpose::STANDARD.decode(value).map_err(|error| format!(\"invalid relation base64: {error}\"))?",
            "            }",
            "            _ => return Err(\"expected relation byte value\".into()),",
            "        };",
            "        Self::new(bytes).map_err(|error| format!(\"bounded relation bytes: {error:?}\"))",
            "    }",
            "}",
            "",
            "impl<const LENGTH: usize> RelationWireValue for FixedBytes<LENGTH> {",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        let values = relation_list(value)?;",
            "        require_relation_arity(values, LENGTH)?;",
            "        let mut bytes = [0u8; LENGTH];",
            "        for (output, value) in bytes.iter_mut().zip(values) {",
            "            *output = relation_integer(value)?.try_into().map_err(|_| \"relation byte is out of range\")?;",
            "        }",
            "        Ok(Self(bytes))",
            "    }",
            "}",
            "",
            "impl<T: RelationWireValue, const MINIMUM: usize, const MAXIMUM: usize> RelationWireValue for BoundedVec<T, MINIMUM, MAXIMUM> {",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        let values = relation_list(value)?.iter().map(T::from_relation).collect::<Result<Vec<_>, _>>()?;",
            "        Self::new(values).map_err(|error| format!(\"bounded relation list: {error:?}\"))",
            "    }",
            "}",
            "",
            "impl<K: Ord + RelationWireValue, V: RelationWireValue, const MAXIMUM: usize> RelationWireValue for BoundedMap<K, V, MAXIMUM> {",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        let mut output = BTreeMap::new();",
            "        for pair in relation_list(value)? {",
            "            let fields = relation_compound(pair, \"pair\")?;",
            "            require_relation_arity(fields, 2)?;",
            "            let key = K::from_relation(&fields[0])?;",
            "            let value = V::from_relation(&fields[1])?;",
            "            if output.insert(key, value).is_some() { return Err(\"duplicate relation map key\".into()); }",
            "        }",
            "        Self::new(output).map_err(|error| format!(\"bounded relation map: {error:?}\"))",
            "    }",
            "}",
            "",
            "impl<T: RelationWireValue> RelationWireValue for Option<T> {",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        if relation_atom(value).ok() == Some(\"none\") { return Ok(None); }",
            "        let fields = relation_compound(value, \"some\")?;",
            "        require_relation_arity(fields, 1)?;",
            "        Ok(Some(T::from_relation(&fields[0])?))",
            "    }",
            "}",
            "",
            "pub const WIRE_PROTOCOL_VERSION: u64 = 1;",
            f'pub const WIRE_SCHEMA_SHA256: &str = "{manifest_hash}";',
        ]
        for name, value in self.limits.items():
            lines.append(f"pub const {const_name(name)}: usize = {value};")
        lines.append("")
        return "\n".join(lines)

    def named_type(self, name: str, definition: Term) -> str:
        rust = pascal(name)
        if isinstance(definition, Compound) and definition.name == "alias":
            return f"pub type {rust} = {self.rust_type(definition.args[0])};\n"
        if isinstance(definition, Compound) and definition.name == "record":
            fields = self.fields(definition.args[0])
            if not fields:
                return f"pub type {rust} = ();\n"
            return self.record(rust, fields)
        if definition == "enum":
            return self.enum(rust, self.enums[name])
        if definition == "choice":
            return self.choice(rust, self.variants[name])
        raise ValueError(f"unsupported definition for {name}: {definition!r}")

    def record(self, name: str, fields: list[Term]) -> str:
        lines = ["#[derive(Clone, Debug, PartialEq)]", f"pub struct {name} {{"]
        for field in fields:
            fname, kind = self.field_parts(field)
            lines.append(f"    pub {field_name(fname)}: {self.rust_type(kind)},")
        lines += ["}", "", f"impl WireValue for {name} {{",
                  "    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {",
                  "        let mut fields = Vec::new();"]
        for field in fields:
            fname, _ = self.field_parts(field)
            lines.append(f"        self.{field_name(fname)}.encode_atom(&mut fields)?;")
        lines += [
            "        put_compound_payload(output, &fields)",
            "    }",
            "",
            "    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {",
            "        let mut fields = get_atom(input, LIMIT_FRAME_BYTES)?;",
            "        let value = Self {",
        ]
        for field in fields:
            fname, kind = self.field_parts(field)
            lines.append(
                f"            {field_name(fname)}: <{self.rust_type(kind)} as WireValue>::decode_atom(&mut fields)?,")
        lines += [
            "        };",
            "        require_empty(fields)?;",
            "        Ok(value)",
            "    }",
            "}",
            "",
            f"impl RelationWireValue for {name} {{",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        let fields = relation_compound(value, \"record\")?;",
            f"        require_relation_arity(fields, {len(fields)})?;",
            "        let mut fields = fields.iter();",
            "        Ok(Self {",
        ]
        for field in fields:
            fname, kind = self.field_parts(field)
            lines.append(
                f"            {field_name(fname)}: <{self.rust_type(kind)} as RelationWireValue>::from_relation(fields.next().unwrap())?,")
        lines += ["        })", "    }", "}", ""]
        return "\n".join(lines)

    def enum(self, name: str, cases: list[tuple[str, int]]) -> str:
        lines = ["#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]",
                 f"pub enum {name} {{"]
        for case, _ in cases:
            lines.append(f"    {pascal(case)},")
        lines += ["}", "", f"impl WireValue for {name} {{",
                  "    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {",
                  "        let code = match self {"]
        for case, code in cases:
            lines.append(f"            Self::{pascal(case)} => {code},")
        lines += [
            "        };",
            "        put_u64(output, code);",
            "        Ok(())",
            "    }",
            "",
            "    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {",
            "        match get_u64(input)? {",
        ]
        for case, code in cases:
            lines.append(f"            {code} => Ok(Self::{pascal(case)}),")
        lines += [
            "            _ => Err(DecodeError::InvalidValue),",
            "        }",
            "    }",
            "}",
            "",
            f"impl RelationWireValue for {name} {{",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        match relation_atom(value)? {",
        ]
        for case, _ in cases:
            lines.append(f'            "{case}" => Ok(Self::{pascal(case)}),')
        lines += [
            f"            case => Err(format!(\"unknown {name} relation case {{case}}\")),",
            "        }",
            "    }",
            "}",
            "",
        ]
        return "\n".join(lines)

    def validate_sum(self, name: str,
                     variants: list[tuple[str, int, list[Term]]]) -> None:
        names = [case for case, _, _ in variants]
        rust_names = [pascal(case) for case in names]
        codes = [code for _, code, _ in variants]
        if len(names) != len(set(names)):
            raise ValueError(f"duplicate {name} variant name")
        if len(rust_names) != len(set(rust_names)):
            raise ValueError(f"colliding Rust {name} variant names")
        if len(codes) != len(set(codes)):
            raise ValueError(f"duplicate {name} variant code")

    def choice(self, name: str, variants: list[tuple[str, int, list[Term]]],
               identity_name: str | None = None) -> str:
        self.validate_sum(name, variants)
        lines = ["#[derive(Clone, Debug, PartialEq)]", f"pub enum {name} {{"]
        for case, _, fields in variants:
            variant = pascal(case)
            if fields:
                lines.append(f"    {variant} {{")
                for field in fields:
                    fname, kind = self.field_parts(field)
                    lines.append(f"        {field_name(fname)}: {self.rust_type(kind)},")
                lines.append("    },")
            else:
                lines.append(f"    {variant},")
        lines += ["}", "", f"impl {name} {{",
                  "    pub const fn code(&self) -> u64 {",
                  "        match self {"]
        for case, code, fields in variants:
            suffix = " { .. }" if fields else ""
            lines.append(f"            Self::{pascal(case)}{suffix} => {code},")
        lines += ["        }", "    }"]
        if identity_name is not None:
            lines += ["", f"    pub const fn {identity_name}(&self) -> &'static str {{",
                      "        match self {"]
            for case, _, fields in variants:
                suffix = " { .. }" if fields else ""
                lines.append(
                    f'            Self::{pascal(case)}{suffix} => "{case}",')
            lines += ["        }", "    }"]
        lines += ["}", "", f"impl WireValue for {name} {{",
                  "    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {",
                  "        let mut fields = Vec::new();",
                  "        put_u64(&mut fields, self.code());",
                  "        match self {"]
        for case, code, fields in variants:
            variant = pascal(case)
            if fields:
                bindings = ", ".join(field_name(self.field_parts(f)[0]) for f in fields)
                lines.append(f"            Self::{variant} {{ {bindings} }} => {{")
            else:
                lines.append(f"            Self::{variant} => {{")
            for field in fields:
                fname, _ = self.field_parts(field)
                lines.append(f"                {field_name(fname)}.encode_atom(&mut fields)?;")
            lines.append("            }")
        lines += [
            "        }",
            "        put_compound_payload(output, &fields)",
            "    }",
            "",
            "    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {",
            "        let mut fields = get_atom(input, LIMIT_FRAME_BYTES)?;",
            "        let value = match get_u64(&mut fields)? {",
        ]
        for case, code, fields in variants:
            variant = pascal(case)
            if fields:
                lines.append(f"            {code} => Self::{variant} {{")
                for field in fields:
                    fname, kind = self.field_parts(field)
                    lines.append(
                        f"                {field_name(fname)}: <{self.rust_type(kind)} as WireValue>::decode_atom(&mut fields)?,")
                lines.append("            },")
            else:
                lines.append(f"            {code} => Self::{variant},")
        lines += [
            "            _ => return Err(DecodeError::InvalidValue),",
            "        };",
            "        require_empty(fields)?;",
            "        Ok(value)",
            "    }",
            "}",
            "",
            f"impl RelationWireValue for {name} {{",
            "    fn from_relation(value: &RelationValue) -> Result<Self, String> {",
            "        let (case, fields): (&str, &[RelationValue]) = match value {",
            "            RelationValue::Atom(case) => (case, &[]),",
            "            RelationValue::Compound(case, fields) => (case, fields),",
            f"            _ => return Err(\"expected {name} relation choice\".into()),",
            "        };",
            "        match case {",
        ]
        for case, _, fields in variants:
            variant = pascal(case)
            lines.append(f'            "{case}" => {{')
            lines.append(f"                require_relation_arity(fields, {len(fields)})?;")
            if fields:
                lines.append("                let mut fields = fields.iter();")
                lines.append(f"                Ok(Self::{variant} {{")
                for field in fields:
                    fname, kind = self.field_parts(field)
                    lines.append(
                        f"                    {field_name(fname)}: <{self.rust_type(kind)} as RelationWireValue>::from_relation(fields.next().unwrap())?,")
                lines.append("                })")
            else:
                lines.append(f"                Ok(Self::{variant})")
            lines.append("            }")
        lines += [
            f"            _ => Err(format!(\"unknown {name} relation choice {{case}}\")),",
            "        }",
            "    }",
            "}",
            "",
        ]
        return "\n".join(lines)

    def relation_sum(self, name: str, rows: list[tuple[str, int, list[Term]]],
                     identity_name: str | None = None) -> str:
        return self.choice(name, rows, identity_name)

    def sample_named(self, name: str, stack: tuple[str, ...] = ()) -> str:
        if name in stack:
            raise ValueError(f"recursive sample type graph: {' -> '.join(stack + (name,))}")
        definition = self.types.get(name)
        if definition is None:
            raise ValueError(f"sample requested for unknown type {name}")
        rust = pascal(name)
        nested = stack + (name,)
        if isinstance(definition, Compound) and definition.name == "alias":
            return self.sample_expr(definition.args[0], nested)
        if isinstance(definition, Compound) and definition.name == "record":
            fields = self.fields(definition.args[0])
            if not fields:
                return "()"
            values = ", ".join(
                f"{field_name(self.field_parts(field)[0])}: "
                f"{self.sample_expr(self.field_parts(field)[1], nested)}"
                for field in fields
            )
            return f"{rust} {{ {values} }}"
        if definition == "enum":
            return f"{rust}::{pascal(self.enums[name][0][0])}"
        if definition == "choice":
            case, _, fields = self.variants[name][0]
            return self.variant_expr(rust, case, fields, nested)
        raise ValueError(f"unsupported sample type {name}: {definition!r}")

    def sample_expr(self, term: Term, stack: tuple[str, ...] = ()) -> str:
        if isinstance(term, str):
            primitive = {
                "bool": "false", "u16": "0u16", "u32": "0u32",
                "u64": "0u64", "s32": "0i32", "s64": "0i64",
                "f64": "0.0f64", "response": "TransportResponse::Empty",
                "action_success": "ActionSuccess::Ping { value: () }",
            }
            if term in primitive:
                return primitive[term]
            return self.sample_named(term, stack)
        if isinstance(term, Compound):
            if term.name == "text":
                return "BoundedText::new(String::new()).unwrap()"
            if term.name == "bytes":
                return "BoundedBytes::new(Vec::new()).unwrap()"
            if term.name == "fixed_bytes":
                return f"FixedBytes([0u8; {int(term.args[0])}])"
            if term.name == "option":
                return "None"
            if term.name == "list":
                item = term.args[0]
                minimum = 0 if len(term.args) == 2 else int(term.args[1])
                values = ", ".join(self.sample_expr(item, stack)
                                   for _ in range(minimum))
                return f"BoundedVec::new(vec![{values}]).unwrap()"
            if term.name == "map":
                return "BoundedMap::new(BTreeMap::new()).unwrap()"
        raise ValueError(f"unsupported sample wire type {term!r}")

    def variant_expr(self, rust_name: str, case: str, fields: list[Term],
                     stack: tuple[str, ...] = ()) -> str:
        variant = f"{rust_name}::{pascal(case)}"
        if not fields:
            return variant
        values = ", ".join(
            f"{field_name(self.field_parts(field)[0])}: "
            f"{self.sample_expr(self.field_parts(field)[1], stack)}"
            for field in fields
        )
        return f"{variant} {{ {values} }}"

    def identity_constant(self, name: str,
                          rows: list[tuple[str, int, list[Term]]]) -> str:
        lines = [f"pub const {name}: &[(&str, u64)] = &["]
        for case, code, _ in rows:
            lines.append(f'    ("{case}", {code}),')
        lines += ["];", ""]
        return "\n".join(lines)

    def action_relation_decoder(
            self, rows: list[tuple[str, int, list[Term]]]) -> str:
        lines = [
            "impl ActionRequest {",
            "    pub fn from_relation(handler: &str, code: u64, values: &[RelationValue]) -> Result<Self, String> {",
            "        match handler {",
        ]
        for case, code, fields in rows:
            variant = pascal(case)
            lines += [
                f'            "{case}" => {{',
                f"                if code != {code} {{ return Err(format!(\"relation opcode {{code}} does not match {case}\")); }}",
                f"                require_relation_arity(values, {len(fields)})?;",
            ]
            if fields:
                lines += ["                let mut values = values.iter();",
                          f"                Ok(Self::{variant} {{"]
                for field in fields:
                    fname, kind = self.field_parts(field)
                    lines.append(
                        f"                    {field_name(fname)}: <{self.rust_type(kind)} as RelationWireValue>::from_relation(values.next().unwrap())?,")
                lines.append("                })")
            else:
                lines.append(f"                Ok(Self::{variant})")
            lines.append("            }")
        lines += [
            "            _ => Err(format!(\"unknown relation action handler {handler}\")),",
            "        }",
            "    }",
            "}",
            "",
        ]
        return "\n".join(lines)

    def request_envelope(self) -> str:
        return "\n".join([
            "#[derive(Clone, Debug, PartialEq)]",
            "pub enum RequestEnvelope {",
            "    Action(ActionRequest),",
            "    Transport(TransportRequest),",
            "}",
            "",
            "impl RequestEnvelope {",
            "    pub const fn code(&self) -> u64 {",
            "        match self {",
            "            Self::Action(value) => value.code(),",
            "            Self::Transport(value) => value.code(),",
            "        }",
            "    }",
            "}",
            "",
            "impl WireValue for RequestEnvelope {",
            "    fn encode_atom(&self, output: &mut Vec<u8>) -> Result<(), DecodeError> {",
            "        match self {",
            "            Self::Action(value) => value.encode_atom(output),",
            "            Self::Transport(value) => value.encode_atom(output),",
            "        }",
            "    }",
            "",
            "    fn decode_atom(input: &mut &[u8]) -> Result<Self, DecodeError> {",
            "        let mut probe = *input;",
            "        let mut fields = get_atom(&mut probe, LIMIT_FRAME_BYTES)?;",
            "        let code = get_u64(&mut fields)?;",
            "        if ACTION_REQUEST_IDENTITIES.iter().any(|(_, known)| *known == code) {",
            "            return ActionRequest::decode_atom(input).map(Self::Action);",
            "        }",
            "        if TRANSPORT_REQUEST_IDENTITIES.iter().any(|(_, known)| *known == code) {",
            "            return TransportRequest::decode_atom(input).map(Self::Transport);",
            "        }",
            "        Err(DecodeError::InvalidValue)",
            "    }",
            "}",
            "",
        ])

    def tests(self, sums: list[tuple[str, str,
                                     list[tuple[str, int, list[Term]]]]]) -> str:
        action_rows = next(rows for name, _, rows in sums
                           if name == "ActionRequest")
        empty_action_code = next(code for _, code, fields in action_rows
                                 if not fields)
        lines = [
            "#[cfg(test)]",
            "mod generated_tests {",
            "    use super::*;",
            "    use std::collections::{BTreeMap, BTreeSet};",
            "",
            "    fn roundtrip<T: WireValue + std::fmt::Debug + PartialEq>(value: T) {",
            "        let mut encoded = Vec::new();",
            "        value.encode_atom(&mut encoded).unwrap();",
            "        let mut input = encoded.as_slice();",
            "        assert_eq!(T::decode_atom(&mut input).unwrap(), value);",
            "        assert!(input.is_empty());",
            "    }",
            "",
            "    #[test]",
            "    fn all_named_wire_types_roundtrip() {",
        ]
        for name, definition in self.types.items():
            rust = pascal(name)
            if definition == "enum":
                for case, _ in self.enums[name]:
                    lines.append(f"        roundtrip({rust}::{pascal(case)});")
            elif definition == "choice":
                for case, _, fields in self.variants[name]:
                    lines.append(
                        f"        roundtrip({self.variant_expr(rust, case, fields, (name,))});")
            else:
                lines.append(f"        roundtrip::<{rust}>({self.sample_named(name)});")
        lines += ["    }", ""]
        for rust_name, constant, rows in sums:
            function = re.sub(r"(?<!^)(?=[A-Z])", "_", rust_name).lower()
            lines += [
                "    #[test]",
                f"    fn every_{function}_variant_roundtrips_with_its_relational_identity() {{",
                f"        let values = vec![",
            ]
            for case, _, fields in rows:
                lines.append(
                    f"            {self.variant_expr(rust_name, case, fields)},")
            lines += [
                "        ];",
                f"        assert_eq!(values.len(), {constant}.len());",
                f"        for (value, (name, code)) in values.into_iter().zip({constant}) {{",
                "            assert_eq!(value.code(), *code, \"{name}\");",
            ]
            if rust_name == "ActionRequest":
                lines.append("            assert_eq!(value.handler(), *name);")
            lines += ["            roundtrip(value);", "        }", "    }", ""]
        lines += [
            "    #[test]",
            "    fn combined_request_envelope_uses_the_relation_opcode_namespaces() {",
            "        roundtrip(RequestEnvelope::Action(ActionRequest::Ping));",
            "        roundtrip(RequestEnvelope::Transport(TransportRequest::Subscribe));",
            "        assert_eq!(RequestEnvelope::Action(ActionRequest::Ping).code(), 70);",
            "        assert_eq!(RequestEnvelope::Transport(TransportRequest::Subscribe).code(), 256);",
            "    }",
            "",
            "    #[test]",
            "    fn relational_identity_tables_are_unique() {",
        ]
        for _, constant, _ in sums:
            lines += [
                f"        assert_eq!({constant}.iter().map(|(name, _)| *name).collect::<BTreeSet<_>>().len(), {constant}.len());",
                f"        assert_eq!({constant}.iter().map(|(_, code)| *code).collect::<BTreeSet<_>>().len(), {constant}.len());",
            ]
        lines += [
            "    }",
            "",
            "    #[test]",
            "    fn action_request_rejects_unknown_code_and_trailing_fields() {",
            "        let mut unknown_fields = Vec::new();",
            "        put_u64(&mut unknown_fields, u64::MAX);",
            "        let mut unknown = Vec::new();",
            "        put_compound_payload(&mut unknown, &unknown_fields).unwrap();",
            "        assert_eq!(ActionRequest::decode_atom(&mut unknown.as_slice()), Err(DecodeError::InvalidValue));",
            "",
            "        let mut trailing_fields = Vec::new();",
            f"        put_u64(&mut trailing_fields, {empty_action_code});",
            "        put_u64(&mut trailing_fields, 1);",
            "        let mut trailing = Vec::new();",
            "        put_compound_payload(&mut trailing, &trailing_fields).unwrap();",
            "        assert_eq!(ActionRequest::decode_atom(&mut trailing.as_slice()), Err(DecodeError::TrailingFields));",
            "    }",
            "",
            "    #[test]",
            "    fn typed_action_requests_materialize_from_relational_values() {",
            "        let request = ActionRequest::from_relation(",
            "            \"mirror_pause\", 62,",
            "            &[RelationValue::Integer(5), RelationValue::Atom(\"false\".into())],",
            "        ).unwrap();",
            "        assert_eq!(request, ActionRequest::MirrorPause { id: 5, paused: false });",
            "",
            "        let request = ActionRequest::from_relation(",
            "            \"mirror_add\", 60,",
            "            &[",
            "                RelationValue::String(\"git\".into()),",
            "                RelationValue::String(\"source\".into()),",
            "                RelationValue::String(\"destination\".into()),",
            "                RelationValue::Compound(\"some\".into(), vec![RelationValue::Integer(30)]),",
            "            ],",
            "        ).unwrap();",
            "        assert_eq!(request.code(), 60);",
            "        assert_eq!(request.handler(), \"mirror_add\");",
            "",
            "        let clause = RelationValue::Compound(",
            "            \"record\".into(),",
            "            vec![",
            "                RelationValue::Atom(\"path\".into()),",
            "                RelationValue::String(\"src/main.rs\".into()),",
            "                RelationValue::Atom(\"and\".into()),",
            "                RelationValue::Atom(\"false\".into()),",
            "                RelationValue::Atom(\"true\".into()),",
            "            ],",
            "        );",
            "        let request = ActionRequest::from_relation(",
            "            \"view.open\", 38,",
            "            &[",
            "                RelationValue::Atom(\"changes\".into()),",
            "                RelationValue::Integer(7),",
            "                RelationValue::Compound(\"some\".into(), vec![RelationValue::List(vec![clause])]),",
            "                RelationValue::Atom(\"false\".into()),",
            "            ],",
            "        ).unwrap();",
            "        assert_eq!(request.code(), 38);",
            "",
            "        let request = ActionRequest::from_relation(",
            "            \"review.write_file\", 33,",
            "            &[",
            "                RelationValue::Integer(7),",
            "                RelationValue::String(\"file\".into()),",
            "                RelationValue::Compound(\"base64\".into(), vec![RelationValue::String(\"eA==\".into())]),",
            "            ],",
            "        ).unwrap();",
            "        let ActionRequest::ReviewWriteFile { b64, .. } = request else { panic!(\"wrong generated request\") };",
            "        assert_eq!(b64.as_slice(), b\"x\");",
            "",
            "        assert!(ActionRequest::from_relation(\"mirror_add\", 999, &[]).is_err());",
            "        assert!(ActionRequest::from_relation(\"missing\", 1, &[]).is_err());",
            "        assert!(ActionRequest::from_relation(\"mirror_pause\", 62, &[]).is_err());",
            "    }",
            "}",
            "",
        ]
        return "\n".join(lines)

    def generate(self) -> str:
        canonical = "\n".join(
            category + "\t" + "\t".join(repr(value) for value in values)
            for category, values in self.rows
        )
        digest = hashlib.sha256(canonical.encode()).hexdigest()
        chunks = [self.header(digest)]
        for category, values in self.rows:
            if category == "type":
                chunks.append(self.named_type(atom(values[0]), values[1]))

        actions = []
        for category, values in self.rows:
            if category == "action":
                actions.append((atom(values[0]), int(values[1]), self.fields(values[2])))
        chunks.append(self.identity_constant("ACTION_REQUEST_IDENTITIES", actions))
        chunks.append(self.relation_sum("ActionRequest", actions, "handler"))
        chunks.append(self.action_relation_decoder(actions))

        successes = []
        for category, values in self.rows:
            if category == "action":
                successes.append((atom(values[0]), int(values[1]),
                                  [Compound("field", ("value", values[3]))]))
        chunks.append(self.identity_constant("ACTION_SUCCESS_IDENTITIES", successes))
        chunks.append(self.relation_sum("ActionSuccess", successes))

        categories = {
            "request": "TransportRequest",
            "response": "TransportResponse",
            "mode": "ConnectionMode",
            "event": "SubscriptionEvent",
        }
        sums = [
            ("ActionRequest", "ACTION_REQUEST_IDENTITIES", actions),
            ("ActionSuccess", "ACTION_SUCCESS_IDENTITIES", successes),
        ]
        for category, rust_name in categories.items():
            rows = []
            for seen, values in self.rows:
                if seen == category:
                    rows.append((atom(values[0]), int(values[1]), self.fields(values[3] if category == "request" else values[2])))
            constant = re.sub(r"(?<!^)(?=[A-Z])", "_", rust_name).upper() + "_IDENTITIES"
            chunks.append(self.identity_constant(constant, rows))
            chunks.append(self.relation_sum(rust_name, rows))
            sums.append((rust_name, constant, rows))

        chunks.append(self.request_envelope())

        frames: dict[str, list[tuple[str, int, list[Term]]]] = {}
        for category, values in self.rows:
            if category == "frame":
                frames.setdefault(atom(values[0]), []).append(
                    (atom(values[1]), int(values[2]), self.fields(values[4])))
        for stream, rows in frames.items():
            rust_name = pascal(stream) + "Frame"
            constant = re.sub(r"(?<!^)(?=[A-Z])", "_", rust_name).upper() + "_IDENTITIES"
            chunks.append(self.identity_constant(constant, rows))
            chunks.append(self.relation_sum(rust_name, rows))
            sums.append((rust_name, constant, rows))
        chunks.append(self.tests(sums))
        return "\n".join(chunks)


def write_if_changed(path: Path, content: str) -> None:
    content = content.rstrip() + "\n"
    if path.exists() and path.read_text() == content:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=path.parent, delete=False) as stream:
        stream.write(content)
        temporary = Path(stream.name)
    temporary.replace(path)


def format_rust(content: str) -> str:
    return subprocess.run(
        ["rustfmt", "--edition", "2024", "--emit", "stdout"],
        check=True,
        text=True,
        input=content,
        stdout=subprocess.PIPE,
    ).stdout


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--swipl")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    output = args.output or repo / "engine/src/generated_wire.rs"
    rows = relation_rows(repo, args.swipl or find_swipl())
    source_hashes = {
        path: hashlib.sha256((repo / path).read_bytes()).hexdigest()
        for path in SCHEMA_SOURCES
    }
    generated = format_rust(Generator(rows, source_hashes).generate()).rstrip() + "\n"
    if args.check:
        if not output.is_file() or output.read_text() != generated:
            raise SystemExit(
                f"stale generated transport projection: run {Path(__file__).name}")
        return
    write_if_changed(output, generated)


if __name__ == "__main__":
    main()

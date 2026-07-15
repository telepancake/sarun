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
        ]
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

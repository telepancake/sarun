# Direct binary `ui.sock` protocol

This is the implementation inventory and cutover contract for replacing the
newline-JSON control protocol. It refines `PROLOG-HUB-ROADMAP.md`; it does not
define an optional mode or compatibility protocol.

## Boundaries

- `ui.sock` delivery is direct Rust-to-Rust I/O. Prolog is never in the
  send/receive/dispatch/recording hot path.
- The Prolog hub defines the stable message identity, typed field schema, and
  relationship between wire messages and command line, help, logging, and
  display representations. Rust opcode/codec code is projected from it.
- Binary wire frames and original packet bytes are recorded without eager
  human rendering. A view invokes Prolog only for the selected display window.
- One Rust atom implementation, `src/wire.rs`, implements the abstraction in
  `tv/wire/wire.h`. TRACE, box-channel, PTY, echo, and control frames share it.

## Atom and connection shape

An atom is exactly tv's self-byte / inline / long byte string. A compound is an
outer atom whose payload is a sequence of inner atoms. Decoding returns borrowed
payload slices and accepts arbitrary stream fragmentation.

Every new connection begins with one version atom, followed by one compound
request atom. A request begins with its stable numeric identity; its remaining
positional fields follow the relation-defined schema. The first reply is one
compound atom selecting the connection mode:

- `reply`: one request/reply exchange, then EOF;
- `subscribe`: a stream of typed event atoms;
- `box`: the persistent runner/engine mux, with SCM_RIGHTS where specified;
- `pty`: bidirectional PTY data/resize/EOF atoms;
- `raw_http`: the remaining bytes are HTTP for the API proxy;
- `raw_service`: the remaining bytes are spliced byte-for-byte.

Mode selection is a typed handshake result, not protocol sniffing. Bytes after
a raw handoff are deliberately not atom-decoded.

## Current ingress inventory

The newline-JSON handler currently multiplexes these families:

| Family | Current messages | Resulting mode |
| --- | --- | --- |
| UI actions | `{"type":"ui","verb", "args"}` covering the control verb catalog | one reply |
| Top-level actions | `apply`, `discard`, `rename`, `select`, `attach`, `checkout`, `patch`, `stuck`, `shutdown`, `sudtrace`, `sud_ingest` | one reply |
| Runner lifecycle | `register` plus pidfd/TAP/trace SCM_RIGHTS | persistent box mux |
| Build/provenance | `brush_prov_nested`, `brush_prov_done`, `recipe_fixup`, `build_edges`, `make_vars`, `box_activity`, `build_edge_state` plus pidfd | one reply |
| Events | `subscribe`; server events include session new/removed/renamed, changes, process/build/provenance activity, and pong | subscribe stream |
| PTY | `pty_spawn` with argv, size, cwd, environment | persistent PTY mux |
| API handoff | `api.proxy`, authenticated by broker-provided box identity | raw HTTP |
| Services | `svc.declare`, `svc.serve`, `svc.dial` | reply, parked raw service, or raw splice |
| Agent budget | `budget.grant` with explicit or broker-implied box | one reply |

After `register`, the existing mux carries echo, echo-done, mute/unmute,
provenance, open-connection/SCM_RIGHTS, and connection handoff frames. After
`pty_spawn`, it carries PTY data, resize, and EOF. These mux frames have already
been cut over from the separate four-byte big-endian framing to compound tv
atoms.

## Encoding constraints

- No field-name strings or JSON type tags in known request/event records.
- Stable opcodes are explicit relation facts and never array/order-derived.
- Integers use minimal little-endian atoms; signed integers use zigzag.
- Text is UTF-8 only where its schema says text. Paths, packet bodies, PTY data,
  trace data, and other blobs remain arbitrary bytes.
- Lists and recursive typed values are count/compound framed and bounded by
  their schema. Known records are positional; genuinely open maps use an
  explicit typed map representation rather than JSON objects.
- The connection frame cap is enforced from the atom prefix before allocation.
  Decode state is committed only after the complete compound validates.
- SCM_RIGHTS count and role are part of the request/frame schema. Unexpected
  descriptors are closed.
- Unknown versions/opcodes, extra fields, missing fields, invalid UTF-8 text,
  and trailing compound bytes fail closed and terminate the connection.

## Cutover sequence

1. Keep the tv-compatible atom primitive and converted box/PTY mux as the only
   Rust atom/framing implementation; retain cross-format golden tests.
2. Add explicit wire identities and request/reply/event schemas to the Prolog
   relation, including non-action negotiation messages.
3. Project typed Rust message/opcode definitions and prove complete handler
   coverage. Generated code contains no descriptions, CLI aliases, or other
   semantic registry data.
4. Replace the server's `read_line`/`serde_json::from_str` loop and every Rust
   client writer/reader in one cutover. Delete JSON helpers and test servers;
   do not add a dual decoder or negotiation fallback.
5. Store typed binary event/log frames and convert only at display/export
   boundaries. Remove eager JSON/text recording from the affected paths.
6. Verify byte fragmentation, caps, malformed compounds, SCM_RIGHTS roles,
   every connection mode, runner registration, request/reply and event streams
   on aarch64 and x86_64 static builds.

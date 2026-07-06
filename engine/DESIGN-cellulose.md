# cellulose — the in-engine textmode browser (carbonyl's replacement)

Decisions are numbered `C*`. This is the plan for **stage 2** of replacing
the vendored carbonyl fork (frozen Chromium ~M110, an OCI image launched as a
PTY child) with an engine-native renderer driving a *stock, current* Chromium
over the DevTools Protocol. Stage 1 (a drop-in swap of the `LaunchTarget::
Browser` argv, keeping the PTY-child shape) is skipped in favour of going
straight to the native pane, because the native pane is what makes the browser
a *client of the web subsystem* the way `DESIGN-web.md` W0 wants, rather than
an opaque TUI in a box.

The prototype this is ported from lives at repo-root `cellulose/` (Python).
It is the executable spec: font scheme, DOMSnapshot text extraction,
half/quarter-block pixel compositing, input mapping. This document is only the
*sarun-shaped* concerns the prototype doesn't have: where Chromium runs, how
the engine reaches its CDP endpoint, and how the output becomes a pane.

## C0 · The shape — driver in the engine, Chromium in a box

carbonyl is a *self-rendering* browser: Chromium draws itself to the terminal,
so sarun runs it as a PTY child (`ui.rs` `open_pty`) and never understands
what it shows. cellulose inverts this: Chromium runs **headless** (renders
nothing to a terminal) inside the same kind of tap box carbonyl uses, and the
**engine** is the renderer — it drives CDP, pulls a DOM snapshot + a
screenshot, and composes a `(char, fg, bg)` cell grid the UI draws directly.

Two consequences, both wins:

- **The browser is finally just a box that browses.** Its HTTP(S) still flows
  through the per-box MITM (`net/mitm.rs`), so webcap capture (W1/W2) and
  replay (W4.2) work unchanged — they never knew or cared that carbonyl was on
  the other end, and they won't care that headless Chromium is.
- **The CDP client the crawler wants (W-archival) already exists.** The same
  driver that renders a page can scroll it, follow links, and wait for network
  idle — the browsertrix-style crawl driver DESIGN-web sketches is this module
  with a different front end.

Chromium stays sandboxed exactly as carbonyl was: an OCI image, `oci_run_argv`,
tap netns, `--ignore-certificate-errors-spki-list=<root_spki_sha256_b64()>`
so it trusts the MITM leaf (`net/ca.rs:43`). Nothing about the box, the CA, or
the overlay changes. Only the argv (headless + a debugging endpoint instead of
carbonyl's TUI) and the *consumer* of the box (an engine renderer, not a PTY
pane) change.

## C1 · Transport — how the engine reaches Chromium's CDP

The engine must speak CDP to a Chromium living in a bwrap netns. Three
candidates, in order of how clean they'd be:

1. **`--remote-debugging-pipe` over inherited fds** (cleanest: no port, no
   networking). Chromium reads CDP on fd 3, writes on fd 4, framed as
   NUL-delimited JSON — *except* current Chromium (verified against M141 in
   this container) appears to negotiate CBOR on the pipe and did not answer a
   NUL-JSON probe. Parking this until the framing is pinned down; it remains
   the target end-state because it needs no netns dial.
2. **`--remote-debugging-port=N` bound in the box, engine dials in via the tap
   stack.** Requires the engine to *originate* a connection toward the box
   through `net/bridge.rs`'s smoltcp stack, which today is built for
   box-originated flows. Viable but needs new inbound-dial plumbing.
3. **`--remote-debugging-port=0` on loopback + a bwrap fd/UDS bridge** — the
   pragmatic first cut. Chromium binds `127.0.0.1:0` *inside the box netns*;
   the engine can't reach box-loopback directly, so we bridge one UDS/pipe
   across the boundary (the same trick the oaita `--api` box uses to expose a
   host service to the box, run in reverse).

**Decision (C1):** ship on a `CdpTransport` trait (`Read + Write` of CDP
messages) with a **WebSocket-over-stream** implementation first, because it is
the exact path the Python prototype proved against M141 and a minimal RFC 6455
client is ~120 lines of pure Rust (no new crate — keeps the static-musl single
binary constraint). The stream under the websocket starts as a host-side TCP
dial for the unit-test/host path, and becomes the box bridge (option 3) for
the real launch. Swapping in the pipe (option 1) later is a new `impl
CdpTransport`, no client changes.

## C2 · Module layout

New module `engine/src/browser/` (add `mod browser;` to `main.rs`):

- `cdp.rs` — the transport trait, the minimal websocket framing, and the sync
  `Cdp` client: monotonic id → `send`, a reader thread that correlates
  responses by id and queues session events, `call(method, params, session)`
  blocking with a timeout. Direct port of the Python `CDP` class; same
  fail-fast-on-closed and race-free sweep the prototype hardened.
- `render.rs` — DOMSnapshot text extraction (UTF-16-code-unit slicing, control/
  format/surrogate/combining sanitization, wide-char handling — all the bugs
  the prototype's review already found and fixed) plus screenshot compositing
  into quadrant blocks. Produces `Vec<Vec<Cell>>` where `Cell = (String, Rgb,
  Rgb)`, drawn by the UI with `buffer[(x,y)].set_symbol().set_style()` and
  `Color::Rgb` (both already how `render_pty_into` paints, `ui.rs:11816`).
- `font.rs` — the synthetic cell font. The 9.8 KB TTF is generated by the
  prototype's `cellfont.py` and **checked in** as `cellfont.ttf`, pulled with
  `include_bytes!` and base64'd into the forced `@font-face` stylesheet at
  startup. (Regenerating in Rust buys nothing; the asset is deterministic.)
- `session.rs` — a `BrowserSession`: owns the boxed Chromium handle + a `Cdp`,
  exposes `navigate`, `snapshot`, `screenshot`, `scroll`, `click`, `key`,
  `history_go`, and `frame() -> Grid`. This is the object the UI pane and the
  (future) crawl driver both hold.

Dependencies are already in `Cargo.toml`: `serde_json`, `base64`, `image`
(png), `tokio`/`std` threads; the sixel encoder for a pixel-peek is
`src/sixel.rs`. No new crate.

## C3 · The pane — `Screen::Browser`, a cell grid not a PTY

`ui.rs` already has `Screen::Browser` (`ui.rs:355`) and cycles browser windows
with F4/F5. Today those windows are `PtyPane`s running carbonyl. cellulose adds
a sibling pane type that owns a `BrowserSession` and a `Grid`:

- **Draw**: identical mechanism to `render_pty_into` — walk the grid, write
  each cell with `set_symbol`/`set_style(Color::Rgb…)`. No PTY, no VT emulator.
- **Input**: the same focused-child routing (`ui.rs:13090+`) but instead of
  encoding keys to PTY bytes, map them to `session.key()/click()/scroll()`
  CDP calls — the prototype's `interactive()` loop, minus the terminal
  escape decoding (ratatui/crossterm already give us `KeyEvent`/`MouseEvent`).
- **Refresh**: `Page.startScreencast` pushes a frame only when the compositor
  changes; the reader thread parks the newest frame and marks the pane dirty,
  so an idle page costs nothing (the prototype's screencast model).
- **Pixel peek**: on a sixel terminal, blit the real viewport with
  `sixel::encode_rgb` over the pane rect (`ui.rs:12888` blit path), reusing the
  existing image-viewer machinery.

## C4 · MITM compatibility — verified, no change needed

The risk that killed the prototype behind *this container's* proxy (an
ECH-intolerant relay resetting Chromium's GREASE-ECH ClientHello) does **not**
apply to sarun's own MITM: `net/mitm.rs:357` terminates the box side with
**rustls 0.23**, which ignores ClientHello extensions it doesn't implement
(ECH-GREASE `0xfe0d`, ALPS `0x44cd`, compressed certs, the large
X25519MLKEM768 key share). ALPN is pinned to `http/1.1` (`mitm.rs:361`), so a
Chromium offering h2 cleanly negotiates down to 1.1 — which the upstream path
also speaks (`http1::handshake`, `mitm.rs:227/240`). So modern Chromium's TLS
is compatible as-is; the only thing to keep is the `EncryptedClientHello`
enterprise-policy off switch is *not* needed here (that was the relay, not
rustls).

## C5 · Increments (each one commits + builds green)

- **A — DONE** `browser/cdp.rs`: the sync CDP client behind a transport trait.
  Bootstrapped on a hand-rolled websocket client; once the pipe framing was
  pinned down (D) the websocket was deleted. Unit-tested for framing and
  id/event correlation.
- **B — DONE** `browser/font.rs` + `render.rs` + `cellfont.ttf`: checked-in
  font, DOMSnapshot text extraction (UTF-16 slicing, sanitization, wide
  cells), quadrant compositing. Fully unit-tested (image-crate-free `compose`).
- **C — DONE** `browser/session.rs`: the page→grid pipeline (attach, inject
  font, navigate, snapshot+screenshot, compose). An e2e test renders
  positioned text and asserts exact-cell parity with the Python prototype.
- **D — DONE** `browser/launch.rs`: the transport question settled on
  `--remote-debugging-pipe` (NUL-JSON over fds 3/4 — sarun's native
  fd-passing idiom, no port, no netns dial). `spawn_host_chromium()` +
  `grid_to_ansi()` + the **`sarun browser [--dump|--dump-text] [--size WxH]
  URL`** CLI verb. Verified against live and offline pages in a real
  bwrap/FUSE environment. This is the complete engine-native renderer.

Remaining (each independently landable):

- **E1 — box transport.** A new `inner_browser` mode (sibling to `inner_pty`
  / `inner_capture` in `runner.rs`) that dup2's a ferried CDP fd to 3/4 and
  execs headless Chromium with `--remote-debugging-pipe` inside a **tap** box,
  so its HTTP(S) flows through the MITM and webcap capture/replay (W1/W2/W4.2)
  work. The engine holds the pipe's other end and builds a `BrowserSession`
  over it. Threads a socketpair through the register/spawn path the same way
  `conn_fd` and the ferried engine binary already cross the bwrap boundary.
  Verifiable headlessly (via webcap rows).
- **E2 — the `Screen::Browser` pane.** Draw the `Grid` via
  `buffer[(x,y)].set_symbol().set_style(Color::Rgb…)` (as `render_pty_into`
  does), route key/mouse to `session.key()/click()/scroll()`,
  `Page.startScreencast`-driven refresh, sixel pixel-peek via `sixel.rs`.
  Needs an attached interactive terminal to verify — not doable in a headless
  harness.
- **E3 — retire carbonyl.** Delete the `CARBONYL_IMAGE` `build_launch` arm and
  the vendored image; point the launcher at the pane. The crawl driver
  (W-archival) reuses `BrowserSession`.

Stage-1 (drop-in carbonyl→cellulose PTY swap) is intentionally skipped: it
would build a throwaway argv path E2 deletes.

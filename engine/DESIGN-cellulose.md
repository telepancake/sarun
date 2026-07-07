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

## C0 · The shape — renderer in the box, over the box's PTY

carbonyl is a *self-rendering* browser: Chromium draws itself to the terminal,
so sarun runs it as a PTY child (`ui.rs` `open_pty`) and never understands
what it shows. cellulose keeps that exact integration but splits the browser
in two, **both inside the box**: headless Chromium plus a small renderer
(`sarun browser`) that drives it over CDP and paints the cell grid. The
renderer emits its frames to stdout, so the box's existing PTY channel carries
them to the UI — the UI embeds `sarun browser` precisely as it embeds carbonyl.

> **The engine does NOT drive CDP.** An earlier draft had headless Chromium in
> the box and the *engine* driving CDP across the box boundary (threading the
> pipe out via a new `inner_browser` fd-passing mode). That coupling is
> unnecessary: CDP is a local pipe between two processes that already live in
> the same box, so it never needs to cross the sandbox. The renderer runs
> in-box next to Chromium and speaks to it over `--remote-debugging-pipe`
> locally; only finished cell frames leave the box, over the PTY. This deletes
> the fd-threading, the engine-side CDP client, and any new UI pane type.

Two consequences, both wins:

- **The browser is still just a box that browses.** Its HTTP(S) flows through
  the per-box MITM (`net/mitm.rs`), so webcap capture (W1/W2) and replay
  (W4.2) work unchanged — Chromium is headless in a tap box, exactly where
  carbonyl was.
- **The CDP client the crawler wants (W-archival) already exists** — it's the
  in-box `browser` module; a crawl driver is the same code with a scripted
  front end instead of the interactive loop.

Chromium stays sandboxed exactly as carbonyl was: a box on tap netns,
`--ignore-certificate-errors-spki-list=<root_spki_sha256_b64()>` so it trusts
the MITM leaf (`net/ca.rs:43`). The only change is *what the box runs*:
`sarun browser URL` (the ferried engine binary) instead of
`/carbonyl/carbonyl URL`, against a box image that carries a stock Chromium.

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

- **E2 — DONE** the interactive TUI. `session.rs` gained the input +
  screencast methods; `launch.rs` has the full interactive loop (raw mode,
  `Page.startScreencast` refresh, per-row diff redraw, input→CDP, URL bar).
  `sarun browser URL` runs interactive on a tty. **Because the renderer runs
  in-box and emits over the PTY, no new UI pane type is needed** — the UI's
  existing PTY-pane machinery embeds it exactly like carbonyl. Verified by
  driving the real binary through a PTY (allocate pty, send keystrokes, read
  frames): a page renders, scroll/reload respond, `^Q` exits.

Remaining:

- **E3 — the launcher swap.** Point `build_launch`'s `Browser` arm
  (`ui.rs:11397`) at `sarun browser URL` (the ferried engine binary, via the
  `/proc/self/exe` idiom `inner` already resolves) instead of
  `/carbonyl/carbonyl`, on a **box image that carries a stock Chromium** —
  passing `--ignore-certificate-errors-spki-list` through to Chromium so it
  trusts the MITM leaf, and pointing `$CELLULOSE_BROWSER` at the in-image
  Chromium. The code change is ~10 lines; the gating dependency is producing
  that Chromium box image (build a Dockerfile via `sarun oci build`, or pin a
  public one) to replace `CARBONYL_IMAGE`. Then delete the carbonyl image
  reference. Left undone deliberately: pointing the launcher at a
  not-yet-built image would break the browser launcher, and the image
  choice/packaging is a call to make explicitly, not fake.

Stage-1 (drop-in carbonyl→cellulose swap over carbonyl's own frozen Chromium)
is intentionally skipped: it would wire the launcher to a patched M110 fork,
undermining the whole "stock current Chromium" point.

# sarun-engine — the web subsystem (carbonyl · capture · replay · archival)

Decisions are numbered `W*` for reference, in the style of `DESIGN.md`. This
document is the plan for turning the bolted-on carbonyl launcher into a
coherent web subsystem that serves three roles the scattered code never
joined up:

1. an **interactive browser** (carbonyl — Chromium in the terminal pane),
2. a **local snapshot viewer** (replay captured pages offline), and
3. an **archival engine** for browsertrix-style crawls (the WACZ web-archive
   work the gimir notes planned — `gimir/SCOPING.md:104,119`,
   `gimir-intent-history.md:93` — but never wired into sarun).

## W0 · Premise — the MITM proxy is the archival engine

Every `--net tap` box already funnels 100% of its HTTP(S) through one
function: `net/mitm.rs::proxy_request`. The engine terminates the box-side
TLS with a leaf minted from its own CA, dials the real upstream from the host
namespace, and streams the response back. It sees **every request line, every
header, every response body, decrypted, in the clear** — and today throws all
of it away, keeping only encrypted pcapng frames (`net/flows.rs`) plus a TLS
keylog sidecar. That is a packet record, not a content record: to reconstruct
a single HTTP response you must decrypt the pcap through tshark.

The entire web story falls out of one move: **tee the decrypted
request/response pair into a per-box content store, addressed by URL.** Once
that store exists:

- the **browser** is just a box that browses; its captures are a side effect,
  exactly like its filesystem overlay is a side effect of running a command.
- the **snapshot viewer** replays that store — serve the captured response for
  a URL back to a browser (or render it), no network.
- the **oaita tools** read and drive that store — fetch a page (through the
  capturing proxy) and inspect captured content, all engine-side.
- **browsertrix-style archival** is a crawl driver (drive the page via CDP,
  scroll, follow links) plus a WACZ exporter that walks the store.

Carbonyl stops being a bespoke argv template special-cased in `build_launch`
and becomes one client of a subsystem every box shares. **That is the
de-kludge**: the browser is no longer the feature; web *capture* is the
feature, and the browser is one way to drive it.

This mirrors sarun's own founding shape (`DESIGN.md` D3/D4): a box run
produces a reviewable, applicable filesystem overlay as a side effect of
executing a command. Here a box run produces a reviewable, exportable **web
capture** as a side effect of making HTTP requests. Same escrow-and-review
philosophy, second data plane.

## W1 · The capture store — a per-box `webcap` table (parallel to `api_log`)

`api_log` (`capture.rs:122`) is the exact precedent: one sqlar table on the
box's `BoxState`, one row per request the engine forwarded on the box's
behalf, full request/response bytes, read back over control verbs and shown in
a UI pane. `webcap` is the same shape for tap-proxy traffic instead of oaita
traffic:

```sql
CREATE TABLE IF NOT EXISTS webcap(
  id     INTEGER PRIMARY KEY AUTOINCREMENT,
  ts     REAL,      -- capture time (wall clock, secs)
  method TEXT,      -- GET / POST / …
  url    TEXT,      -- full absolute URL (scheme://host[:port]/path?query)
  host   TEXT,      -- authority, indexed — "all captures for this site"
  status INT,       -- upstream HTTP status
  mime   TEXT,      -- response Content-Type (sans params), for viewer routing
  req_headers  TEXT,  -- canonical "K: V\n" block
  resp_headers TEXT,
  req_body   BLOB,  -- request body bytes (may be empty)
  resp_body  BLOB   -- response body bytes, decompressed to identity (W2)
);
CREATE INDEX IF NOT EXISTS idx_webcap_host ON webcap(host);
CREATE INDEX IF NOT EXISTS idx_webcap_url  ON webcap(url);
```

Why a table and not loose files or the depot blob pool:

- **Consistency with the box's rest form.** A box is "index file + blob dir"
  (`DESIGN.md` D6). `webcap` rides the same sqlar the overlay, provenance, and
  api_log already ride — one artifact, inspectable with stock sqlite tools,
  applied/discarded/dissolved with the box. No new lifecycle.
- **Addressed by URL, newest-first per URL.** This is exactly gimir's
  `PageView` model (`gimir-design.md:99-103`: "per-URL sequence of captures
  with body bytes and asset references; revision identity is timestamp +
  body-hash") and the tiered-VBF depot variant the gimir notes assign to web
  archives (`DEPOT-DESIGN.md:231-236`). We store rows now; the depot's
  delta-chained cold tier is the at-rest optimization for *kept* archives
  (W6), not the capture-time format.
- **Bookkeeping, never behavior** (`depot.rs:9-13`): `webcap` is a sibling
  bookkeeping table beside the depot, like `api_log`/`brushprov`. It does not
  touch the `BoxDepot` layer surface.

`BoxState::add_web_capture(...)` (capture.rs) inserts one row, mirroring
`add_api_log` (`capture.rs:884`). Capture is **opt-in per box** (W3): a flag
on the box's registration, so an ordinary build box that happens to `curl`
something isn't silently accumulating a web archive. Default off; the browser
and the crawler turn it on.

## W2 · Where the tee lives — `proxy_request`

`proxy_request` (`mitm.rs:61`) already owns the request and the upstream
response. The tee is: buffer the request body, buffer the response body,
insert a `webcap` row with the **raw** upstream bytes and verbatim headers,
then hand the (re-materialized) response onward to the box unchanged.

Store raw, decode on read. Storing the byte-identical upstream body with its
`Content-Encoding` intact makes replay (W4) trivially correct — the stored
response *is* a valid response — and pays no gzip/br/zstd decode at capture
time. Readers that need the identity payload (inspection, WACZ export with
`WARC-Payload-Digest`) decode on demand from the recorded encoding
(`webcap::decode_body`: identity/gzip/deflate/zstd via the crates already in
the tree; brotli is stored-and-noted since no pure-Rust decoder is vendored).
This is the opposite of the WARC convention of storing identity payloads, and
deliberately so: sarun's first job is faithful replay, and decode is a
read-time concern.

Design constraints that keep this honest:

- **Streaming is preserved for the box.** The box must not stall waiting for
  the engine to finish buffering a 200 MB download. Capture applies a size cap
  (`WEBCAP_BODY_MAX`, a few MB): bodies over the cap are passed through
  un-teed and the row records `status`/headers with a truncated/omitted body
  marker. Interactive browsing and typical crawl payloads (HTML, CSS, JS,
  JSON, images) fit; large media is noted but not stored inline. This is the
  same frugality the result-budget code applies to oaita output.
- **The capture sink is optional.** `proxy_request` gains an
  `Option<Arc<WebCapSink>>`. `None` → today's pure pass-through, zero added
  cost. The sink is threaded from `control.rs::register_net` (which knows the
  box id and holds the overlay to reach `live_box(box_id)`), through the
  `Dispatcher` (`net/dispatch.rs`), into `serve_http`/`serve_https`. This is
  the same overlay→`live_box`→`add_*` path `oaita/proxy.rs::log_call`
  (`proxy.rs:323`) already uses — no new attribution machinery.
- **Only successful, in-policy flows are teed.** The policy gate
  (`dispatch.rs:80`) runs first, as today; a denied flow never reaches
  `proxy_request`. HTTP and HTTPS share the tee because they share
  `proxy_request` — plain-HTTP captures for free.

The pcapng/keylog path (`flows.rs`) stays exactly as is: it's the packet-level
ground truth (non-HTTP TCP, TLS metadata, timing) and is orthogonal. `webcap`
is the content-level view. Two records, two questions ("what bytes crossed the
wire" vs "what pages did this box see").

## W3 · The browser as a first-class launch target

The current `Action::BrowserCarbonyl` (`ui.rs:9006`) is three kludges:

1. its doc comment (`ui.rs:498`) still advertises a persistent named `BROWSER`
   box with profile reuse and live-refusal — **none of which the handler
   does** (it uses `Placement::New`, adds no `--name`, has no lifecycle);
2. the URL is a bare `https://` spliced into a raw shell command line the user
   hand-edits — no URL field, no history;
3. the menu label promises "flows captured" but nothing browser-aware records
   anything; it's the generic per-box pcap.

The fix makes the browser a real target, not an argv template:

- **Persistent by default, correctly.** The browser launches as
  `Placement::Reuse("BROWSER")` (a named oci box) so `--user-data-dir=/carbonyl/data`
  — the Chromium profile — persists across launches via `load_mirror`
  (`control.rs:1047`), which is what the stale doc always meant. Reruns reuse
  the box; the profile (cookies, history, logins) survives.
- **Capture on by default.** The browser box registers with web capture
  enabled (W1/W2), so browsing *is* archiving. The menu label's "flows
  captured" finally means content, not just packets.
- **A real URL prompt.** A dedicated `Modal::BrowserUrl` field (validated,
  history-backed) replaces the dangling-`https://`-in-a-shell-line splice. The
  argv is assembled by the launch model from a validated URL, not typed into a
  command string.
- **SPKI is not silently optional.** If the CA can't be loaded the launch is a
  visible error, not a browser that will fail TLS interception with no signal
  (`ui.rs:9011` `.ok()` today swallows it).

Carbonyl remains a `LaunchTarget` (it stays in the universal launch model —
that part was right), but its knobs (persistence, capture, URL) are first-class
inputs, not hardcoded inline constants. A **headless** variant of the same
target (`--headless=new` instead of the TUI) is what the crawler drives (W6):
same image, same capture, no pane.

## W4 · Local snapshot viewer — replay

A snapshot viewer answers "show me what this box saw at `url`, offline."
Two rungs, cheapest first:

1. **Inspect rung (ships with W1).** Control verbs `webcap.list {box}` and
   `webcap.get {box, id}` (mirroring `flows.list`/`flows.detail`,
   `control.rs:1903`) plus a UI **Captures pane** (mirroring the api_log pane,
   `ui.rs:3497`): a list of `ts · method · status · mime · url`, drill into one
   to see headers + body (text rendered, binary through the existing untrusted
   viewer). This alone makes captures reviewable and is the minimum "viewer."
2. **Replay rung (browser-grade).** An engine-side replay HTTP server that,
   for a given box's `webcap`, answers a request for `url` with the stored
   response (newest-first; `?asof=<ts>` selects an older capture — the
   wayback-style date selector from `wikipedia-browsing-plan.md:192`). Point
   carbonyl at it (a box whose tap policy routes everything to the replay
   server, or a direct `--proxy-server`) and you re-browse the captured site
   with no live network — the "local snapshot viewer" role. Same store, same
   proxy seam, reversed direction: W2 writes the store from upstream, replay
   reads the store instead of dialing upstream.

Replay is deliberately a thin dispatch on the same `proxy_request` shape:
`decide_upstream(url) -> Live | Replay(box)`. Live is today's behavior; Replay
short-circuits to a `webcap` lookup. One code path, a mode switch — no second
HTTP stack.

## W5 · oaita web tools — the agent reads and drives the store

oaita tools are three edits each (registry `tools.rs:430`, dispatch
`driver.rs:851`, a handler) per the tool architecture. The web tools all sit
on the W1 store so the agent, the browser, and the crawler share one substrate:

- **`web_fetch {url}`** — fetch a URL through the capturing proxy (engine-side
  egress, credentialed and budgeted exactly like the LLM proxy) and return the
  response text, trimmed to the result budget. Side effect: a `webcap` row, so
  a fetch is also an archive. This is the tool the agent reaches for instead of
  `shell` + `curl` (which needs box network + the binary and isn't captured).
- **`web_snapshot_list {host?}`** / **`web_snapshot_read {url|id}`** — read the
  session box's `webcap`: what has this agent (or its browser) seen, and the
  content of one capture. Lets the agent inspect archived pages — the "tools
  for oaita to access and inspect content" role — without re-fetching.
- **`web_archive {url, depth?}`** (later, on W6) — kick a bounded crawl and
  return a summary + the WACZ path.

Engine-side egress modeled on `oaita/proxy.rs` keeps credentials and network
off the box boundary and gives the same auditable, budgeted, logged path the
LLM calls already have — the store *is* the audit log.

## W6 · Archival — crawl driver + WACZ export (the browsertrix reimplementation)

browsertrix = drive a real browser over a seed list, run page behaviors
(autoscroll, expand, click-throughs) so lazy content loads, capture every
network response, and bundle it as WACZ. sarun already has the browser
(carbonyl/Chromium, which speaks CDP) and now the capture (W1/W2). What's
missing is the driver and the exporter:

- **Crawl driver.** A headless Chromium box driven over CDP: navigate to each
  seed, run a scroll/settle behavior, harvest in-page links, enqueue same-scope
  links to a depth/page cap. Every response the page triggers flows through the
  tap MITM → `webcap` (no separate capture path — the crawler drives, the proxy
  records). This is a reimplementation of browsertrix's *orchestration*, not
  its capture stack, because our capture stack is the MITM proxy we already own.
  NB: the CDP target is a **headless `chrome-headless-shell` sidecar**, not
  carbonyl — carbonyl (the pinned v0.0.3, an old-mode headless Chromium fork)
  does not reliably expose a remote-debugging endpoint; see W8.
- **WACZ export.** `webcap` → WACZ 1.1.1: a WARC of the request/response
  records (identity payloads, `WARC-Payload-Digest`), a CDXJ index, `pages.jsonl`
  for the seeds, and `datapackage.json`, zipped. This is the on-demand,
  user-initiated export the gimir notes specify (`gimir-design.md:110`,
  "`gimir export-wacz`; user-initiated") — here `sarun web export-wacz <box>`.
  WACZ is the interchange boundary; it opens in ReplayWeb.page / pywb / the
  gimir viewer, so archives leave sarun in a standard format.
- **Depot cold tier for kept archives.** A long-kept crawl of a site with many
  near-identical pages is exactly the "deep near-identical revision chains"
  case CAS handles badly and the tiered-VBF depot handles well
  (`SCOPING.md:75-85`, `DEPOT-DESIGN.md:231`). At-rest, a kept `webcap` can be
  re-encoded into the depot's newest-first delta chain per URL. This is an
  apply-time/GC concern (like `DESIGN.md` D4's "long-KEPT boxes are
  uncompressed at rest"), not a capture-time one — capture stays simple rows.

## W7 · Proxy-side filtering — adblock + rewriting, entirely outside the browser

Filtering belongs on the same seam as capture, for the same reason: the engine
MITM proxy sees every request and response in the clear, so ad/tracker
blocking and content rewriting can happen there — in the engine, outside the
browser — and apply to **any** box, not just carbonyl. A `curl` in a build
box, a headless crawl, the interactive browser: all get the same filtering,
because it lives at `proxy_request`, not in a browser extension.

This is strictly better than in-browser adblock: it needs no extension (which
carbonyl can't easily run anyway), it can't be defeated by the page, it works
for non-browser HTTP clients, and — because it runs before the capture tee —
the web archive records what was *actually served to the box* (blocked
requests noted, injected content present), so replay reproduces the filtered
view.

Two filter kinds, both on the `proxy_request` path:

1. **Request block (adblock).** Before dialing upstream, match the request URL
   and host against the filter ruleset. On a block match, short-circuit with a
   synthetic response (204 No Content, or an empty 200) — upstream is never
   contacted. This is ad/tracker/malware-domain blocking. A blocked request is
   still recorded in `webcap` (status 204, a `blocked` marker) so the archive
   shows what was filtered.
2. **Response rewriting.** After fetching, before handing the response to the
   box: rewrite headers (strip `Content-Security-Policy` / `X-Frame-Options` so
   archived pages render in the viewer, drop tracking headers) and — for
   `text/html` — inject cosmetic CSS (`##selector`-style element hiding) into
   the body. Header rewriting ships in the first cut; body/cosmetic rewriting
   is the follow-on (it re-encodes through the same streaming tee).

The ruleset is a plain file, `{config_home}/webfilter`, one rule per line:

```
block  <glob>          block requests whose URL matches <glob> (synthetic 204)
block-host <glob>      block by host authority (tracker/ad domains)
strip-header <name>    remove <name> from every response (e.g. CSP)
```

`<glob>` is a case-insensitive match with `*` wildcards. This native format is
deliberately tiny; an EasyList importer (`||domain^`, `##selector`) that
compiles the standard lists into these rules is a follow-on — the engine
mechanism is list-format-agnostic.

Filtering is opt-in per box via `--webfilter` (SARUN_WEBFILTER env, mirroring
`--webcap`); the browser turns it on alongside capture. It composes with the
existing per-flow `policy::decide` (`net/dispatch.rs`) — policy is coarse
allow/deny by host/port/scheme; the filter is fine URL/header rewriting. They
stack: policy gates the connection, the filter shapes the request/response.

## Ordering & what lands when

- **W1 + W2** (the store + the tee) is the spine — everything else needs it.
  It is self-contained: a new table, a threaded optional sink, a body tee. No
  UI, no new commands. Lands first, with capture defaulting off.
- **W4 rung 1** (inspect verbs + Captures pane) makes the spine visible and is
  the cheapest proof it works.
- **W3** (browser de-kludge) turns capture on for the browser and fixes the
  three kludges; depends only on W1/W2.
- **W5** (oaita tools) and **W4 rung 2** (replay) are parallel consumers.
- **W7** (filtering) rides the same `proxy_request` seam as W2; request-block
  and header-strip land with the browser (which turns filtering on), body
  rewriting follows.
- **W6** (crawl + WACZ) is the capstone; depends on W3 (headless browser) and
  W1 (the store to export).

Each rung is a real, compiling, testable increment on `main`, not a big-bang
branch — matching the repo's "one clean commit per logical change, push
immediately" workflow (`CLAUDE.md`).

## Naming alignment with the gimir depot

To keep this consistent with the storage work happening in parallel
(`gimir/DEPOT-DESIGN.md`), the vocabulary tracks gimir's:

- a single capture is a **PageView** revision (URL + ts + body-hash identity),
  gimir's term (`gimir-design.md:99`);
- a URL's capture history is a **newest-first chain** — front inserts are
  `Prepend`, gimir's enforced naming (`gimir-intent-history.md:164`);
- kept archives land in the **VBF/cold** depot variant
  (`DEPOT-DESIGN.md:231`); the capture-time `webcap` table is the **hot**
  tier feeding it.

So a web archive is not a new storage concept — it is the depot's web-shaped
data kind (`DEPOT-BRIEF.md:52`), captured hot as sqlar rows and (when kept)
sealed cold as delta chains, viewed through `serve`/replay, exported as WACZ.

## W8 · DevTools as terminal panes

Because carbonyl is Chromium and the proxy sees all traffic, a large slice of
browser DevTools maps onto the TUI. The panes split by data source, and the
split is what makes them tractable:

**Out-of-band panes (the proxy — no CDP, no browser cooperation, any box,
replayable).** These render `webcap` / the pcapng+keylog and need nothing from
the browser:

- **Network** — SHIPPED (`Pane::Network`, key `w`): the request list + a
  drill-in to headers and body, straight off `webcap`. This is the Network
  panel, and because it's proxy-sourced it works for a headless crawl or a bare
  `curl`, and archivally — you can open the Network panel of a page captured
  last week.
- **Security** (cert chain / TLS version / mixed content) and **Cookies**
  (`Set-Cookie` seen on the wire) are the same freebie — the engine terminates
  the TLS and sees every header, so it already holds this data. Not yet built.

**In-band panes (CDP — the live browser's internal state).** DOM, Console, the
debugger, live storage internals, and Performance are about state the proxy
can't see; they need CDP. The data is overwhelmingly trees/tables/text and
renders in the existing pane substrate (the DOM reuses the process-forest tree
renderer; the Console is a text stream + a `Runtime.evaluate` input line; the
debugger is a source pane + call-stack + scope tree, i.e. what gdb-TUI/delve
already are). What does NOT translate is the *visual* layer — the Elements
hover overlay, the box-model diagram, screenshots, and the Performance flame
chart's shape; the underlying data survives as tables/icicles but the gestalt
is lost.

**CDP transport decision (probed, decisive):** the CDP panes drive a dedicated
headless **`chrome-headless-shell` sidecar**, NOT carbonyl. The pinned carbonyl
(v0.0.3, an unmaintained old-mode headless Chromium fork) exposes no documented
flag surface and no reliable `--remote-debugging-port`; the one serious
carbonyl-automation project drives it by *screen-scraping the terminal via a
PTY*, which is the tell that nobody gets CDP out of it. So the sidecar model:
per box, launch `chrome-headless-shell --remote-debugging-port=0` (loopback
only; parse the `ws://…` URL off stderr — never hardcode 9222), dial it from
the engine over one WebSocket, `Target.setAutoAttach{flatten:true}`, enable
domains up front (events buffer nothing pre-`enable`), and fan events into
panes. Domains: Runtime+Log (Console), DOM+CSS+Overlay (Elements), Debugger,
Network (redundant with the proxy but adds initiator/timing), Page. Response
bodies are ephemeral — grab them on `loadingFinished`. On the old M111-era
protocol, codegen bindings from the sidecar's own `/json/protocol`, not the
current published spec.

The higher-leverage form of the in-band panes is **as oaita tools**, not just a
human UI: "get the DOM of this captured page", "run this JS and return the
result", "list the console errors" — DevTools-as-inspection-API, which fits the
archival/agent goals (W5) better than a human-only pane. Same CDP driver
backs both.

Build order by value-per-effort: Network (shipped) → Cookies/Security (proxy
freebies, cheap) → the CDP sidecar + Console REPL (cheapest CDP win) → DOM tree
→ Debugger (expensive, high fidelity) → Performance tables (skip the flame
chart). The visual overlays are the only part a terminal genuinely can't
follow.

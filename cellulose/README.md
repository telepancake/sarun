# cellulose

A textmode web browser: carbonyl's design rebuilt on **public API only** —
stock headless Chromium driven over the DevTools Protocol. No Chromium fork,
no patches, nothing to rebase when Chrome ships its next major.

```
./cellulose.py https://en.wikipedia.org/wiki/Terminal_emulator   # interactive
./cellulose.py --dump URL        # one ANSI frame to stdout
./cellulose.py --dump-text URL   # one plain-text frame (used by the tests)
```

Interactive keys: `Ctrl-Q` quit, `Ctrl-L` URL, `Ctrl-R` reload, arrows/PgUp/
PgDn scroll, mouse clicks click, everything else (typing, Tab, Enter) is
forwarded to the page.

## How it works

carbonyl patched Blink/Skia/viz internals to intercept glyphs and the
framebuffer. cellulose gets the same three primitives through CDP:

1. **The cell font** (`cellfont.py`). A synthetic TTF forced on every
   document (`@font-face` + `* { !important }` injected via
   `Page.addScriptToEvaluateOnNewDocument`). Every Unicode codepoint maps to
   an *empty* glyph — advance exactly ½em (1 terminal cell) or 1em for
   East-Asian wide (2 cells) — via a cmap format-13 table (constant glyph
   per range; the whole font is <10KB). With `font-size: 16px !important;
   line-height: 16px !important` and justification/kerning/ligatures/spacing
   flattened by the same stylesheet, text layout lands on an exact
   8x16 CSS-px grid, and text paints **no pixels**.
2. **Text** comes from `DOMSnapshot.captureSnapshot`: every inline text box
   with document coordinates (exact cell multiples, thanks to the font) and
   computed color/visibility.
3. **Pixels** (images, canvas, backgrounds) come from
   `Page.captureScreenshot`, downscaled to 2 vertical samples per cell and
   drawn as U+2584 half-blocks. Because glyphs are empty, frames contain
   only non-text content — no text-hiding double-capture like Browsh.

Input maps straight onto `Input.dispatchKeyEvent` / `dispatchMouseEvent` /
`insertText`; terminal mouse cells convert to CSS px by the cell size.

## Browser binary

Any Chromium-family binary works (set `$CELLULOSE_BROWSER`). Defaults prefer
the Playwright full-Chromium build, then `headless_shell`, then system
chromium/chrome.

**Behind a TLS-intercepting proxy** (like this container): two pieces of
one-time setup, both done the supported way — no verification is disabled:

- Import the proxy CA into the NSS store Chromium reads:
  `certutil -d sql:$HOME/.pki/nssdb -A -t "C,," -n proxy -i <ca.crt>`
  (create the db first with `certutil -d sql:$HOME/.pki/nssdb -N
  --empty-password`; needs `libnss3-tools`).
- Disable ECH via enterprise policy — this proxy's relay resets any
  ClientHello carrying the ECH extension, including Chromium's default
  GREASE-ECH (bisected by replaying the hello bytes; dropping ext 0xfe0d
  alone fixed it): `echo '{"EncryptedClientHelloEnabled": false}' >
  /etc/chromium/policies/managed/cellulose.json` (also mirror to
  `/etc/opt/chrome/policies/managed/`). Note `headless_shell` has **no
  policy machinery and no ECH kill-switch at all**, which is why the full
  Chromium binary is preferred.

`$HTTPS_PROXY` is passed through as `--proxy-server` automatically.

## Known v1 limits

- **Text-run collisions on dense pages**: forcing every font to 16px makes
  text wider than containers laid out for smaller sizes (e.g. HN's 10px
  metadata rows), so overflowing inline runs can overlap and overwrite each
  other's cells, clipping word tails. This is the main quality gap vs
  carbonyl (which scales the whole layout instead of just fonts); the fix
  direction is quantizing author font sizes to cell multiples rather than
  a single global size.

- Main frame only: text inside cross-origin iframes shows as pixels, not
  crisp text (the snapshot walk takes `documents[0]`).
- Box geometry (margins, images, flex gaps) is continuous px snapped to
  cells at render time — same bounded rounding carbonyl accepts.
- Hostile pages can fight the `!important` stylesheet (author-origin
  injection, not engine-enforced); closed shadow roots and constructed
  stylesheets are not yet intercepted.
- Full redraws, no damage tracking; fine for reading, not for video.

## Tests

```
uv run --with websocket-client,fonttools,pillow python3 test_cellulose.py
```

Renders `fixture.html` through the real browser and asserts exact cell
placement, justification flattening, and CJK double-width.

#!/usr/bin/env python3
"""Capture reader-output straightedge fixtures for the wikitext renderer.

For each (lang, title) seed this fetches a SELF-CONSISTENT snapshot at capture
time — the page wikitext, its FULL template+module transclusion closure (their
wikitext too), the reader HTML, and siteinfo — and writes one gzip'd JSON
bundle per page under this directory.

Deliberate choices (see the task these fixtures serve):
  * READER output, not Parsoid: HTML comes from the legacy `action=parse`
    read view, never the Parsoid `/page/html` REST endpoint. The straightedge
    grades user-visible output, not the data-mw/data-parsoid/about/section-edit
    scaffolding Parsoid emits for the in-page editor.
  * Consistent snapshot: page + closure + render captured together, so the
    raw→closure→rendered triple is locally reproducible (MediaWiki does not
    store historical renders, so "at that point in time" = one capture instant).
  * Polite: descriptive UA, one connection, a pause between requests, closure
    content fetched in <=40-title batches; closures are bounded and any
    truncation is recorded in the bundle (never silent).

Run:  python3 capture.py            # full curated seed list
      python3 capture.py ja:招き猫  # ad-hoc single page (lang:title)
"""

import gzip
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

UA = "wikimak-corpus/0 (github.com/telepancake/sarun mirror test fixtures; contact: local)"
CLOSURE_CAP = 500          # bound stored transclusions per page (logged if hit)
PAUSE = 1.5                # seconds between API requests (etiquette)
HERE = __file__.rsplit("/", 1)[0]

# Curated for SCRIPT diversity (CJK, RTL, Cyrillic, Greek, Devanagari, Latin)
# and STRUCTURE diversity (infobox, refs, tables, lists, math). Moderate-size
# topics preferred so closures stay fetchable/storable; a couple are heavier
# on purpose.
SEEDS = [
    ("ja", "招き猫"),        # CJK (Japanese) — cultural, moderate infobox
    ("zh", "茶"),            # CJK (Chinese) — tea, tables + refs
    ("ko", "김치"),          # CJK (Korean/Hangul) — infobox + refs
    ("ar", "قهوة"),          # RTL (Arabic) — coffee, bidi + infobox
    ("he", "קפה"),           # RTL (Hebrew) — coffee
    ("fa", "چای"),           # RTL (Persian) — tea, Arabic-script + Eastern digits
    ("ru", "Чай"),           # Cyrillic (Russian) — tea, refs + tables
    ("uk", "Борщ"),          # Cyrillic (Ukrainian) — borscht
    ("el", "Καφές"),         # Greek script — coffee
    ("hi", "चाय"),           # Devanagari — tea
    ("en", "Espresso"),      # Latin — structured article, infobox/refs/tables
    ("de", "Schach"),        # Latin (German) — chess, tables/lists
]


def api(lang, params):
    params = {**params, "format": "json", "formatversion": "2"}
    url = f"https://{lang}.wikipedia.org/w/api.php?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept-Encoding": "gzip"})
    # Honor 429/503 with Retry-After and exponential backoff (wikitech Robot
    # policy). Never hammer through a throttle.
    delay = 4.0
    for attempt in range(6):
        try:
            with urllib.request.urlopen(req, timeout=40) as r:
                raw = r.read()
                if r.headers.get("Content-Encoding") == "gzip":
                    raw = gzip.decompress(raw)
            time.sleep(PAUSE)
            return json.loads(raw)
        except urllib.error.HTTPError as e:
            if e.code not in (429, 503) or attempt == 5:
                raise
            wait = float(e.headers.get("Retry-After") or delay)
            print(f"  {e.code}; backing off {wait:.0f}s", file=sys.stderr)
            time.sleep(wait)
            delay *= 2
    raise RuntimeError("unreachable")


def page_wikitext(lang, title):
    d = api(lang, {"action": "query", "prop": "revisions", "titles": title,
                   "rvprop": "content|ids|timestamp", "rvslots": "main"})
    pg = d["query"]["pages"][0]
    rev = pg["revisions"][0]
    return {
        "pageid": pg["pageid"], "title": pg["title"], "revid": rev["revid"],
        "timestamp": rev["timestamp"], "wikitext": rev["slots"]["main"]["content"],
    }


def closure_content(lang, titles):
    """Batch-fetch current wikitext for every title (Template:/Module:/...)."""
    out = {}
    for i in range(0, len(titles), 40):
        batch = titles[i:i + 40]
        d = api(lang, {"action": "query", "prop": "revisions",
                       "titles": "|".join(batch), "rvprop": "content",
                       "rvslots": "main"})
        for pg in d["query"].get("pages", []):
            revs = pg.get("revisions")
            if not revs:
                continue  # missing page (a red transclusion) — skip
            content = revs[0]["slots"]["main"].get("content")
            if content is not None:
                out[pg["title"]] = content
    return out


def siteinfo(lang):
    d = api(lang, {"action": "query", "meta": "siteinfo",
                   "siprop": "general|namespaces|namespacealiases|magicwords"})
    return d["query"]


def capture(lang, title):
    parse = api(lang, {"action": "parse", "page": title,
                       "prop": "text|templates|displaytitle|revid"})["parse"]
    reader_html = parse["text"]
    trans = [t["title"] for t in parse.get("templates", [])]
    truncated = len(trans) > CLOSURE_CAP
    trans = trans[:CLOSURE_CAP]
    pg = page_wikitext(lang, title)
    content = closure_content(lang, trans) if trans else {}
    si = siteinfo(lang)
    gen = si["general"]
    bundle = {
        "meta": {"lang": lang, "seed_title": title, "resolved_title": pg["title"],
                 "pageid": pg["pageid"], "revid": pg["revid"],
                 "timestamp": pg["timestamp"], "rtl": bool(gen.get("rtl")),
                 "sitename": gen.get("sitename"), "content_lang": gen.get("lang"),
                 "closure_total": len(parse.get("templates", [])),
                 "closure_stored": len(content), "closure_truncated": truncated},
        "page_wikitext": pg["wikitext"],
        "reader_html": reader_html,
        "closure": content,      # {title: wikitext} — Templates AND Modules
        "siteinfo": {"general": gen, "namespaces": si["namespaces"],
                     "namespacealiases": si.get("namespacealiases", []),
                     "magicwords": si.get("magicwords", [])},
    }
    slug = f"{lang}-{pg['pageid']}"
    path = f"{HERE}/{slug}.json.gz"
    with gzip.open(path, "wt", encoding="utf-8") as f:
        json.dump(bundle, f, ensure_ascii=False, separators=(",", ":"))
    n_mod = sum(1 for t in content if t.startswith(("Module:", "モジュール:", "وحدة:",
                "মডিউল:", "Модуль:", "類別:")) or ":" in t and "Module" in t)
    import os
    kb = os.path.getsize(path) / 1024
    print(f"{slug:16} {pg['title'][:24]:24} closure {len(content)}/{parse and len(parse.get('templates', []))}"
          f"{' (TRUNC)' if truncated else '':8} rtl={bundle['meta']['rtl']!s:5} {kb:6.0f} KiB")
    return path


def main(argv):
    seeds = [tuple(a.split(":", 1)) for a in argv] if argv else SEEDS
    for lang, title in seeds:
        try:
            capture(lang, title)
        except Exception as e:  # noqa: BLE001 — one bad page must not kill the run
            print(f"{lang}:{title}  FAILED: {e}", file=sys.stderr)


if __name__ == "__main__":
    main(sys.argv[1:])

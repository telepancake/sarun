"""Synthetic grid font for cellulose.

Builds a TTF where every Unicode codepoint maps to an *empty* glyph with a
fixed advance: half an em for narrow characters, a full em for East-Asian
wide characters. Forced on a page at font-size = CELL_H with line-height =
CELL_H, every character occupies exactly one (or two) terminal cells and
paints no pixels — so screenshots contain only non-text content, while text
positions come from DOMSnapshot at exact cell multiples.

No GPOS/GSUB/kern tables exist, so the shaper cannot introduce fractional
adjustments. Coverage is the full Unicode range via a cmap format-12 table
(compiled as consecutive-range groups, so the file stays small).

Deps: fonttools
"""

import unicodedata
from functools import lru_cache

UPEM = 1000

# East-Asian wide/fullwidth ranges get a 1em advance (two terminal cells).
# Everything else gets 0.5em (one cell). Derived once from unicodedata for
# the BMP + the explicit supplementary ideographic/emoji planes.
_WIDE_SUPPLEMENTARY = [
    (0x16FE0, 0x1B2FF),  # Tangut, Kana supplement...
    (0x1F300, 0x1FAFF),  # emoji blocks
    (0x20000, 0x3FFFD),  # CJK extension planes
]


def _is_wide_bmp(cp):
    return unicodedata.east_asian_width(chr(cp)) in ("W", "F")


def _setup_cmap13(fb, cmap):
    """Full-Unicode coverage with a constant glyph per range needs format 13
    (format 12 requires glyph IDs to increment within a group, so it
    degenerates to one group per codepoint; a full-BMP format 4 overflows
    outright). Ship format 13 plus a small format 4 for Latin-1 only."""
    from fontTools.ttLib import newTable
    from fontTools.ttLib.tables._c_m_a_p import CmapSubtable

    sub13 = CmapSubtable.getSubtableClass(13)(13)
    sub13.platformID, sub13.platEncID, sub13.language = 3, 10, 0
    sub13.cmap = cmap

    sub4 = CmapSubtable.getSubtableClass(4)(4)
    sub4.platformID, sub4.platEncID, sub4.language = 3, 1, 0
    sub4.cmap = {cp: g for cp, g in cmap.items() if cp <= 0xFF}

    table = newTable("cmap")
    table.tableVersion = 0
    table.tables = [sub4, sub13]
    fb.font["cmap"] = table


def build_font_bytes():
    from fontTools.fontBuilder import FontBuilder
    from fontTools.pens.ttGlyphPen import TTGlyphPen

    fb = FontBuilder(UPEM, isTTF=True)
    glyphs = [".notdef", "half", "wide"]
    fb.setupGlyphOrder(glyphs)

    pen = TTGlyphPen(None)
    empty = pen.glyph()  # no outline
    fb.setupGlyf({g: empty for g in glyphs})
    fb.setupHorizontalMetrics(
        {".notdef": (UPEM // 2, 0), "half": (UPEM // 2, 0), "wide": (UPEM, 0)}
    )
    fb.setupHorizontalHeader(ascent=UPEM * 4 // 5, descent=-(UPEM // 5))

    cmap = {}
    for cp in range(0x20, 0x10000):
        if 0xD800 <= cp <= 0xDFFF:
            continue
        cmap[cp] = "wide" if _is_wide_bmp(cp) else "half"
    for cp in range(0x10000, 0x110000):
        cmap[cp] = "half"
    for lo, hi in _WIDE_SUPPLEMENTARY:
        for cp in range(lo, hi + 1):
            cmap[cp] = "wide"

    _setup_cmap13(fb, cmap)
    fb.setupOS2(
        sTypoAscender=UPEM * 4 // 5,
        sTypoDescender=-(UPEM // 5),
        usWinAscent=UPEM * 4 // 5,
        usWinDescent=UPEM // 5,
    )
    fb.setupNameTable({"familyName": "CelluloseCell", "styleName": "Regular"})
    fb.setupPost()

    import io

    buf = io.BytesIO()
    fb.save(buf)
    return buf.getvalue()


@lru_cache(maxsize=1)
def font_data_url():
    import base64

    return "data:font/ttf;base64," + base64.b64encode(build_font_bytes()).decode()


def char_cells(ch):
    """How many terminal cells a character occupies under this font."""
    cp = ord(ch)
    if cp < 0x20:
        return 1
    if cp < 0x10000:
        return 2 if _is_wide_bmp(cp) else 1
    for lo, hi in _WIDE_SUPPLEMENTARY:
        if lo <= cp <= hi:
            return 2
    return 1


if __name__ == "__main__":
    data = build_font_bytes()
    print(f"font: {len(data)} bytes")

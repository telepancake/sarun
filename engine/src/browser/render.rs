// DOMSnapshot text extraction + quadrant-block compositing
// (DESIGN-cellulose.md C2). Ported from the prototype's `snapshot_text` and
// `compose_frame`, keeping the review-hardened details: UTF-16 code-unit
// slicing of text boxes, control/format/combining sanitization, wide-char
// continuation cells, and no double-width overflow past the last column.
//
// This module is image-crate-free: `compose` takes a pixel sampler, so the
// whole extraction+composition path is unit-testable without a browser.

use serde_json::Value;

use super::font::{char_cells, CELL_H, CELL_W};

pub type Rgb = (u8, u8, u8);

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub ch: String,
    pub fg: Rgb,
    pub bg: Rgb,
}

pub type Grid = Vec<Vec<Cell>>;

/// One placed character from the DOM snapshot: grid position, glyph, color.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub row: usize,
    pub col: usize,
    pub ch: char,
    pub fg: Rgb,
}

/// Parse `rgb(r,g,b)` / `rgba(r,g,b,a)`; returns None for fully/near
/// transparent colors (so invisible text isn't drawn).
pub fn parse_css_color(s: &str) -> Option<Rgb> {
    let s = s.trim();
    let inner = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))?
        .strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() < 3 {
        return None;
    }
    let comp = |t: &str| t.parse::<f32>().ok().map(|v| v.clamp(0.0, 255.0) as u8);
    let (r, g, b) = (comp(parts[0])?, comp(parts[1])?, comp(parts[2])?);
    if parts.len() >= 4 {
        if let Ok(a) = parts[3].parse::<f32>() {
            if a < 0.05 {
                return None;
            }
        }
    }
    Some((r, g, b))
}

/// Controls, format chars, and combining/zero-width marks would occupy a
/// layout cell (the cell font gives everything an advance) but print at the
/// wrong width and shift the row — replace them with a space.
fn sanitize(ch: char) -> char {
    use unicode_width::UnicodeWidthChar;
    if ch.is_control() || ch.width().unwrap_or(0) == 0 {
        ' '
    } else {
        ch
    }
}

fn as_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// Extract placed characters from a `DOMSnapshot.captureSnapshot` result. The
/// computed styles are requested in the order `[color, visibility, opacity]`.
pub fn snapshot_text(snapshot: &Value, cols: usize, rows: usize) -> Vec<Placement> {
    let mut out = Vec::new();
    let strings = match snapshot.get("strings").and_then(Value::as_array) {
        Some(s) => s,
        None => return out,
    };
    let get_str = |i: i64| -> &str {
        if i < 0 {
            ""
        } else {
            strings.get(i as usize).and_then(Value::as_str).unwrap_or("")
        }
    };
    let Some(doc) = snapshot
        .get("documents")
        .and_then(Value::as_array)
        .and_then(|d| d.first())
    else {
        return out;
    };
    let layout = doc.get("layout");
    let text_idx = layout
        .and_then(|l| l.get("text"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let styles = layout
        .and_then(|l| l.get("styles"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sx = doc.get("scrollOffsetX").and_then(Value::as_f64).unwrap_or(0.0);
    let sy = doc.get("scrollOffsetY").and_then(Value::as_f64).unwrap_or(0.0);

    let tb = doc.get("textBoxes");
    let arr = |k: &str| -> Vec<Value> {
        tb.and_then(|t| t.get(k))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let (layout_index, bounds, start, length) =
        (arr("layoutIndex"), arr("bounds"), arr("start"), arr("length"));
    let n = layout_index
        .len()
        .min(bounds.len())
        .min(start.len())
        .min(length.len());

    for k in 0..n {
        let li = match as_i64(&layout_index[k]) {
            Some(v) if v >= 0 => v as usize,
            _ => continue,
        };
        let ti = text_idx.get(li).and_then(as_i64).unwrap_or(-1);
        if ti < 0 {
            continue;
        }

        // color / visibility / opacity from this layout node's styles
        let mut fg: Rgb = (0, 0, 0);
        if let Some(st) = styles.get(li).and_then(Value::as_array) {
            if let Some(c) = st.first().and_then(as_i64).map(get_str) {
                if let Some(rgb) = parse_css_color(c) {
                    fg = rgb;
                }
            }
            if st.get(1).and_then(as_i64).map(get_str) == Some("hidden") {
                continue;
            }
            if let Some(op) = st.get(2).and_then(as_i64).map(get_str) {
                if let Ok(v) = op.parse::<f32>() {
                    if v < 0.05 {
                        continue;
                    }
                }
            }
        }

        // bounds is [x, y, w, h]
        let b = match bounds[k].as_array() {
            Some(b) if b.len() >= 4 => b,
            _ => continue,
        };
        let (x, y, _w, h) = (
            b[0].as_f64().unwrap_or(0.0),
            b[1].as_f64().unwrap_or(0.0),
            b[2].as_f64().unwrap_or(0.0),
            b[3].as_f64().unwrap_or(0.0),
        );
        let row = ((y - sy + h / 2.0) / CELL_H as f64).floor();
        if row < 0.0 || row as usize >= rows {
            continue;
        }
        let row = row as usize;
        let mut col = ((x - sx) / CELL_W as f64).round() as i64;

        // UTF-16 code-unit slice of the text node (CDP TextBox semantics)
        let (s, l) = (
            as_i64(&start[k]).unwrap_or(0).max(0) as usize,
            as_i64(&length[k]).unwrap_or(0).max(0) as usize,
        );
        let units: Vec<u16> = get_str(ti).encode_utf16().collect();
        let seg_units = &units[s.min(units.len())..(s + l).min(units.len())];
        let seg = String::from_utf16_lossy(seg_units);

        for ch in seg.chars() {
            let cells = char_cells(ch);
            if col < 0 {
                col += cells as i64;
                continue;
            }
            if col as usize + cells > cols {
                break;
            }
            out.push(Placement {
                row,
                col: col as usize,
                ch: sanitize(ch),
                fg,
            });
            col += cells as i64;
        }
    }
    out
}

// quadrant blocks indexed by lit-pixel bitmask: bit 1 = upper-left,
// 2 = upper-right, 4 = lower-left, 8 = lower-right
const QUADS: [char; 16] = [
    ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█',
];

fn dist2(a: Rgb, b: Rgb) -> i64 {
    let d = |x: u8, y: u8| (x as i64 - y as i64).pow(2);
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

fn avg(ps: &[Rgb]) -> Rgb {
    let n = ps.len().max(1) as u32;
    let s = ps.iter().fold((0u32, 0u32, 0u32), |acc, p| {
        (acc.0 + p.0 as u32, acc.1 + p.1 as u32, acc.2 + p.2 as u32)
    });
    ((s.0 / n) as u8, (s.1 / n) as u8, (s.2 / n) as u8)
}

/// Reduce four RGB samples (UL, UR, LL, LR) to a quadrant glyph plus a
/// two-color partition seeded by the most distant pair.
pub fn quad_cell(pix: [Rgb; 4]) -> Cell {
    let (mut seed_a, mut seed_b, mut worst) = (pix[0], pix[0], -1i64);
    for i in 0..4 {
        for j in (i + 1)..4 {
            let d = dist2(pix[i], pix[j]);
            if d > worst {
                worst = d;
                seed_a = pix[i];
                seed_b = pix[j];
            }
        }
    }
    if worst == 0 {
        return Cell {
            ch: " ".into(),
            fg: pix[0],
            bg: pix[0],
        };
    }
    let mut bits = 0usize;
    let (mut on, mut off): (Vec<Rgb>, Vec<Rgb>) = (Vec::new(), Vec::new());
    for (k, &p) in pix.iter().enumerate() {
        if dist2(p, seed_a) <= dist2(p, seed_b) {
            bits |= 1 << k;
            on.push(p);
        } else {
            off.push(p);
        }
    }
    Cell {
        ch: QUADS[bits].to_string(),
        fg: avg(&on),
        bg: avg(&off),
    }
}

/// Compose a full grid: quadrant blocks for pixels everywhere, DOM text
/// overlaid on top. `sample(x, y)` reads the viewport resized to
/// `(cols*2, rows*2)` — two subpixels per cell in each axis.
pub fn compose(
    sample: impl Fn(usize, usize) -> Rgb,
    text: &[Placement],
    cols: usize,
    rows: usize,
) -> Grid {
    let quad_at = |c: usize, r: usize| {
        [
            sample(2 * c, 2 * r),
            sample(2 * c + 1, 2 * r),
            sample(2 * c, 2 * r + 1),
            sample(2 * c + 1, 2 * r + 1),
        ]
    };
    let mut grid: Grid = (0..rows)
        .map(|r| (0..cols).map(|c| quad_cell(quad_at(c, r))).collect())
        .collect();
    for p in text {
        if p.row < rows && p.col < cols {
            let bg = avg(&quad_at(p.col, p.row));
            grid[p.row][p.col] = Cell {
                ch: p.ch.to_string(),
                fg: p.fg,
                bg,
            };
            if char_cells(p.ch) == 2 && p.col + 1 < cols {
                grid[p.row][p.col + 1] = Cell {
                    ch: String::new(), // wide continuation cell
                    fg: p.fg,
                    bg,
                };
            }
        }
    }
    grid
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn color_parsing() {
        assert_eq!(parse_css_color("rgb(200, 0, 0)"), Some((200, 0, 0)));
        assert_eq!(parse_css_color("rgba(1,2,3,1)"), Some((1, 2, 3)));
        assert_eq!(parse_css_color("rgba(1,2,3,0)"), None); // transparent
        assert_eq!(parse_css_color("rgb(255.6, 0, 0)"), Some((255, 0, 0)));
        assert_eq!(parse_css_color("purple"), None);
    }

    #[test]
    fn quadrant_flat_and_split() {
        // uniform → space, fg==bg
        let flat = quad_cell([(10, 20, 30); 4]);
        assert_eq!(flat.ch, " ");
        assert_eq!(flat.fg, (10, 20, 30));
        // left column black, right column white → left-half block ▌
        let black = (0, 0, 0);
        let white = (255, 255, 255);
        let split = quad_cell([black, white, black, white]);
        // bits: UL(0)=black→seed_a set, UR(1)=white, LL(2)=black set, LR(3)=white
        // lit = UL|LL = bit0|bit2 = 0b0101 = 5 → '▌'
        assert_eq!(split.ch, "▌");
        assert_eq!(split.fg, black);
        assert_eq!(split.bg, white);
    }

    #[test]
    fn snapshot_places_positioned_text() {
        // Mirrors the prototype fixture: "REDTEXT" at x=160,y=128 (col 20,
        // row 8), color rgb(200,0,0).
        let snap = json!({
            "strings": ["REDTEXT", "rgb(200, 0, 0)", "visible", "1"],
            "documents": [{
                "scrollOffsetX": 0, "scrollOffsetY": 0,
                "layout": {
                    "text": [0],
                    "styles": [[1, 2, 3]]
                },
                "textBoxes": {
                    "layoutIndex": [0],
                    "bounds": [[160, 128, 56, 16]],
                    "start": [0],
                    "length": [7]
                }
            }]
        });
        let placed = snapshot_text(&snap, 80, 30);
        assert_eq!(placed.len(), 7);
        assert_eq!(placed[0].row, 8);
        assert_eq!(placed[0].col, 20);
        assert_eq!(placed[0].ch, 'R');
        assert_eq!(placed[0].fg, (200, 0, 0));
        assert_eq!(placed[6].col, 26);
        assert_eq!(placed[6].ch, 'T');
    }

    #[test]
    fn snapshot_utf16_offsets_and_hidden() {
        // A leading astral char (2 UTF-16 units) then "ok"; slice starting at
        // unit 2 must yield "ok" (byte/codepoint slicing would corrupt it).
        let snap = json!({
            "strings": ["\u{1F389}ok", "rgb(0,0,0)", "visible", "1"],
            "documents": [{
                "layout": { "text": [0], "styles": [[1, 2, 3]] },
                "textBoxes": {
                    "layoutIndex": [0], "bounds": [[0, 0, 16, 16]],
                    "start": [2], "length": [2]
                }
            }]
        });
        let placed = snapshot_text(&snap, 80, 30);
        let word: String = placed.iter().map(|p| p.ch).collect();
        assert_eq!(word, "ok");

        // visibility:hidden drops the box
        let hidden = json!({
            "strings": ["X", "rgb(0,0,0)", "hidden", "1"],
            "documents": [{
                "layout": { "text": [0], "styles": [[1, 2, 3]] },
                "textBoxes": {
                    "layoutIndex": [0], "bounds": [[0, 0, 16, 16]],
                    "start": [0], "length": [1]
                }
            }]
        });
        assert!(snapshot_text(&hidden, 80, 30).is_empty());
    }

    #[test]
    fn compose_overlays_text_on_pixels() {
        // all-white viewport, one red 'A' placed at (row 1, col 2)
        let text = vec![Placement { row: 1, col: 2, ch: 'A', fg: (255, 0, 0) }];
        let grid = compose(|_, _| (255, 255, 255), &text, 5, 3);
        assert_eq!(grid.len(), 3);
        assert_eq!(grid[1][2].ch, "A");
        assert_eq!(grid[1][2].fg, (255, 0, 0));
        assert_eq!(grid[1][2].bg, (255, 255, 255));
        // a flat white pixel cell elsewhere is a space
        assert_eq!(grid[0][0].ch, " ");
    }
}

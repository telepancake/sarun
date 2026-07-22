// Sixel image encoding + terminal-support detection, for the standalone image
// viewer (a popover, NOT inline character content — DESIGN-web.md W8 image
// rung). The viewer decodes an image (PNG/JPEG/GIF/WebP via the `image` crate
// already in the tree) to RGB and this module turns RGB into a DECSIXEL DCS
// sequence the host terminal draws as real pixels. Everything here is pure
// data-in/bytes-out and unit-tested; the actual blit-to-tty lives in ui.rs
// (it must bypass ratatui's cell buffer, so it can't be a widget).
//
// Sixel in one paragraph: a DCS string `\x1bP q … \x1b\\` carrying a palette
// (`#n;2;R;G;B`, R/G/B in 0..=100 PERCENT, not 0..255) and pixel data in bands
// of 6 rows. Each data byte encodes 6 vertical pixels as (bits)+0x3F; `#n`
// selects a color, `$` returns to the band start to overlay the next color,
// `-` advances to the next band, `!<n><ch>` run-length-repeats a byte.

/// DECSIXEL introducer (`\x1bPq`) and String Terminator (`\x1b\\`).
const DCS_INTRO: &[u8] = b"\x1bPq";
const ST: &[u8] = b"\x1b\\";

/// Does the terminal's primary Device Attributes (DA1) reply advertise sixel?
/// The reply is `CSI ? <p1> ; <p2> ; … c`; attribute `4` means "sixel
/// graphics". We match `4` as a whole semicolon-delimited field so `24` or
/// `40` never false-positive.
pub fn da1_reports_sixel(reply: &str) -> bool {
    // Trim to the `?…c` core if present; tolerate leading/trailing noise.
    let core = reply
        .split(['?'])
        .nth(1)
        .and_then(|s| s.split('c').next())
        .unwrap_or(reply);
    core.split(';').any(|f| f.trim() == "4")
}

/// Given a source image WxH and a target box of COLSxROWS terminal cells with
/// PXxPY pixels per cell, return the pixel dimensions to scale the image to so
/// it fits inside the box while preserving aspect ratio (never upscaling past
/// the source, so a tiny favicon stays crisp rather than blowing up blurry).
pub fn fit_pixels(img_w: u32, img_h: u32, cols: u16, rows: u16, px: u16, py: u16) -> (u32, u32) {
    let box_w = (cols as u32) * (px.max(1) as u32);
    let box_h = (rows as u32) * (py.max(1) as u32);
    if img_w == 0 || img_h == 0 || box_w == 0 || box_h == 0 {
        return (0, 0);
    }
    // Scale factor = min(box/img, 1.0) on each axis, take the smaller so both
    // fit. Work in rationals (u64) to avoid float rounding drift.
    let sw = (box_w as u64) * (img_h as u64);
    let sh = (box_h as u64) * (img_w as u64);
    let (mut w, mut h) = if sw <= sh {
        // width-bound
        (box_w, (box_w as u64 * img_h as u64 / img_w as u64) as u32)
    } else {
        ((box_h as u64 * img_w as u64 / img_h as u64) as u32, box_h)
    };
    // Never upscale beyond the source.
    if w > img_w || h > img_h {
        w = img_w;
        h = img_h;
    }
    (w.max(1), h.max(1))
}

/// Quantize an 8-bit channel to the 0..=5 rung of a 6×6×6 color cube.
#[inline]
fn rung(c: u8) -> u8 {
    ((c as u16 * 5 + 127) / 255) as u8
}

/// Palette index (0..216) for an RGB pixel in the fixed 6×6×6 cube.
#[inline]
fn pal_index(r: u8, g: u8, b: u8) -> u8 {
    36 * rung(r) + 6 * rung(g) + rung(b)
}

/// The 0..=100 percent value a cube rung maps to (rung 0→0, 5→100).
#[inline]
fn rung_pct(rung: u8) -> u8 {
    (rung as u16 * 100 / 5) as u8
}

/// Encode a tightly-packed RGB8 buffer (`w*h*3` bytes, row-major) as a
/// self-contained DECSIXEL sequence over a fixed 6×6×6 palette. The palette is
/// coarse (216 colors) but the point is a legible preview in a terminal, not a
/// print proof. Returns the full `\x1bPq…\x1b\\` byte string.
pub fn encode_rgb(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h / 2 + 1024);
    out.extend_from_slice(DCS_INTRO);
    // Raster attributes: 1:1 pixel aspect, explicit W×H so the terminal sizes
    // the graphic exactly.
    out.extend_from_slice(format!("\"1;1;{w};{h}").as_bytes());
    if w == 0 || h == 0 || rgb.len() < w * h * 3 {
        out.extend_from_slice(ST);
        return out;
    }

    // Map every pixel to a palette index once; track which indices appear so we
    // only emit palette entries and per-band passes for colors actually used.
    let mut idx = vec![0u8; w * h];
    let mut used = [false; 216];
    for (p, px) in idx.iter_mut().enumerate() {
        let o = p * 3;
        let i = pal_index(rgb[o], rgb[o + 1], rgb[o + 2]);
        *px = i;
        used[i as usize] = true;
    }
    // Palette definitions (percent RGB).
    for (i, u) in used.iter().enumerate() {
        if !u {
            continue;
        }
        let ri = (i / 36) as u8;
        let gi = ((i / 6) % 6) as u8;
        let bi = (i % 6) as u8;
        out.extend_from_slice(
            format!("#{i};2;{};{};{}", rung_pct(ri), rung_pct(gi), rung_pct(bi)).as_bytes(),
        );
    }

    // Bands of 6 rows.
    let mut band_top = 0usize;
    while band_top < h {
        let band_h = 6.min(h - band_top);
        // Which colors appear anywhere in this band?
        let mut band_used = [false; 216];
        for y in band_top..band_top + band_h {
            let row = &idx[y * w..y * w + w];
            for &c in row {
                band_used[c as usize] = true;
            }
        }
        let mut first = true;
        for c in 0..216u16 {
            if !band_used[c as usize] {
                continue;
            }
            if !first {
                out.push(b'$');
            } // CR: overlay this color on the band
            first = false;
            out.extend_from_slice(format!("#{c}").as_bytes());
            // Build this color's sixel bytes across the row, then RLE.
            let mut prev: u8 = 0;
            let mut run: u32 = 0;
            for x in 0..w {
                let mut bits: u8 = 0;
                for (i, y) in (band_top..band_top + band_h).enumerate() {
                    if idx[y * w + x] == c as u8 {
                        bits |= 1 << i;
                    }
                }
                let ch = 0x3F + bits;
                if x == 0 {
                    prev = ch;
                    run = 1;
                } else if ch == prev {
                    run += 1;
                } else {
                    emit_run(&mut out, prev, run);
                    prev = ch;
                    run = 1;
                }
            }
            emit_run(&mut out, prev, run);
        }
        out.push(b'-'); // graphics newline → next band
        band_top += band_h;
    }
    out.extend_from_slice(ST);
    out
}

/// Emit one sixel byte `ch` repeated `run` times, run-length-compressed
/// (`!<n><ch>`) when that's shorter than the literal repetition.
fn emit_run(out: &mut Vec<u8>, ch: u8, run: u32) {
    if run == 0 {
        return;
    }
    if run >= 4 {
        out.extend_from_slice(format!("!{run}").as_bytes());
        out.push(ch);
    } else {
        for _ in 0..run {
            out.push(ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn da1_sixel_flag() {
        assert!(da1_reports_sixel("\x1b[?62;4;22c"));
        assert!(da1_reports_sixel("\x1b[?4c"));
        assert!(!da1_reports_sixel("\x1b[?62;22c"));
        // Whole-field match: 24 / 40 must NOT count as sixel.
        assert!(!da1_reports_sixel("\x1b[?24;40c"));
    }

    #[test]
    fn fit_preserves_aspect_and_never_upscales() {
        // 100×50 into a 10×10 cell box at 10×20 px/cell = 100×200 px box.
        // width-bound: 100px wide → 50px tall.
        assert_eq!(fit_pixels(100, 50, 10, 10, 10, 20), (100, 50));
        // A tiny 8×8 icon into a big box stays 8×8 (no upscale).
        assert_eq!(fit_pixels(8, 8, 40, 40, 10, 20), (8, 8));
        // Tall image is height-bound.
        let (w, h) = fit_pixels(50, 100, 10, 5, 10, 20);
        assert!(
            h <= 100 && w <= 100 && h >= w,
            "tall → height-bound: {w}x{h}"
        );
    }

    #[test]
    fn encode_is_well_formed_dcs() {
        // 2×6 solid red image.
        let rgb: Vec<u8> = std::iter::repeat([255u8, 0, 0])
            .take(12)
            .flatten()
            .collect();
        let s = encode_rgb(&rgb, 2, 6);
        assert!(s.starts_with(b"\x1bPq"), "DCS introducer");
        assert!(s.ends_with(b"\x1b\\"), "String Terminator");
        let text = String::from_utf8_lossy(&s);
        assert!(text.contains("\"1;1;2;6"), "raster attrs W×H present");
        // Red = rung(255)=5,0,0 → index 180; palette def with 100;0;0.
        assert!(text.contains("#180;2;100;0;0"), "red palette entry: {text}");
        // A full band of 6 set rows for one color is 0x3F+0b111111 = '~'.
        assert!(text.contains('~'), "a fully-filled sixel column ('~')");
    }

    #[test]
    fn encode_runs_are_length_compressed() {
        // 10×6 solid → a run of 10 identical '~' columns → RLE "!10~".
        let rgb: Vec<u8> = std::iter::repeat([0u8, 255, 0])
            .take(60)
            .flatten()
            .collect();
        let s = String::from_utf8(encode_rgb(&rgb, 10, 6)).unwrap();
        assert!(s.contains("!10~"), "run-length compressed: {s}");
    }

    #[test]
    fn degenerate_sizes_are_safe() {
        assert!(encode_rgb(&[], 0, 0).starts_with(b"\x1bPq"));
        // Short buffer → introducer + terminator, no panic.
        let s = encode_rgb(&[1, 2, 3], 4, 4);
        assert!(s.ends_with(b"\x1b\\"));
    }
}

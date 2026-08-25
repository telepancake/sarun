//! Upload-URL math (plan §4, VERIFIED scheme).
//!
//! MediaWiki lays files out under an md5 shard of the *normalized*
//! filename:
//!   original = <upload_base>/<x>/<xy>/<F>
//!   thumb    = <upload_base>/thumb/<x>/<xy>/<F>/<NNN>px-<F>[suffix]
//! where F is the filename with spaces→underscores and the first letter
//! upper-cased (the ns-6 first-letter case rule), and `md5(F)` gives the
//! shard: x = first hex char, xy = first two hex chars.
//!
//! Thumb suffix rules (the ones MediaWiki actually applies because the
//! source format has no browser-native raster form — documented and
//! implemented here):
//!   - `.svg`            → thumb name gains a trailing `.png`
//!   - `.tif` / `.tiff`  → thumb name gains a trailing `.jpg`
//! Everything else keeps its extension (`120px-Foo.jpg` etc.). Other
//! rasterized-source rules exist upstream (`.pdf`, `.djvu`, `.stl`,
//! multi-page `pageN-`) and are intentionally NOT handled — see crate
//! `gaps`.

use md5::{Digest, Md5};

/// Normalize a File: name to its on-disk/on-CDN form: spaces →
/// underscores, first letter upper-cased. First-letter casing uses the
/// title's own char boundaries (handles multibyte leading letters).
pub fn normalize_filename(name: &str) -> String {
    let underscored = name.trim().replace(' ', "_");
    let mut chars = underscored.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            let mut out: String = first.to_uppercase().collect();
            out.push_str(chars.as_str());
            out
        }
    }
}

/// Lowercase hex md5 of the normalized filename.
pub fn md5_hex(normalized: &str) -> String {
    let digest = Md5::digest(normalized.as_bytes());
    let mut out = String::with_capacity(32);
    for byte in digest {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((byte & 0xf) as u32, 16).unwrap());
    }
    out
}

/// The (x, xy) shard for a normalized filename: first hex char and first
/// two hex chars of its md5.
pub fn shard(normalized: &str) -> (String, String) {
    let hex = md5_hex(normalized);
    (hex[..1].to_string(), hex[..2].to_string())
}

/// The thumb suffix appended after `<NNN>px-<F>` for source formats with
/// no browser-native raster form. Empty for the common case.
fn thumb_suffix(normalized: &str) -> &'static str {
    let lower = normalized.to_ascii_lowercase();
    if lower.ends_with(".svg") {
        ".png"
    } else if lower.ends_with(".tif") || lower.ends_with(".tiff") {
        ".jpg"
    } else {
        ""
    }
}

/// Original (unscaled) URL under `upload_base`
/// (e.g. `https://upload.wikimedia.org/wikipedia/commons`).
pub fn original_url(upload_base: &str, filename: &str) -> String {
    let f = normalize_filename(filename);
    let (x, xy) = shard(&f);
    format!("{}/{}/{}/{}", upload_base.trim_end_matches('/'), x, xy, f)
}

/// Thumb URL at `width` px under `upload_base`, with the format suffix
/// rule applied.
pub fn thumb_url(upload_base: &str, filename: &str, width: u32) -> String {
    let f = normalize_filename(filename);
    let (x, xy) = shard(&f);
    let suffix = thumb_suffix(&f);
    format!(
        "{}/thumb/{}/{}/{}/{}px-{}{}",
        upload_base.trim_end_matches('/'),
        x,
        xy,
        f,
        width,
        f,
        suffix
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMONS: &str = "https://upload.wikimedia.org/wikipedia/commons";

    #[test]
    fn normalization_underscores_and_case() {
        assert_eq!(normalize_filename("foo bar.png"), "Foo_bar.png");
        assert_eq!(normalize_filename("Example.jpg"), "Example.jpg");
        // First letter only; interior case untouched.
        assert_eq!(normalize_filename("iPhone X.png"), "IPhone_X.png");
    }

    #[test]
    fn md5_matches_known_values() {
        // Verified against `md5(normalize)` for the canonical MediaWiki
        // example and a spaced name.
        assert_eq!(md5_hex("Example.jpg"), "a91fe217e45a700fc2dab0cc476f01c7");
        assert_eq!(md5_hex("Foo_bar.png"), "08055548a9d9aa68eef75340b685dd1b");
    }

    #[test]
    fn example_jpg_canonical_shard() {
        // The textbook case: File:Example.jpg lives at commons /a/a9/.
        assert_eq!(shard("Example.jpg"), ("a".to_string(), "a9".to_string()));
        assert_eq!(
            original_url(COMMONS, "Example.jpg"),
            "https://upload.wikimedia.org/wikipedia/commons/a/a9/Example.jpg"
        );
    }

    #[test]
    fn spaced_name_shards_on_underscored_form() {
        // "Foo bar.png" → Foo_bar.png → md5 08055548… → 0/08.
        assert_eq!(
            original_url(COMMONS, "Foo bar.png"),
            "https://upload.wikimedia.org/wikipedia/commons/0/08/Foo_bar.png"
        );
    }

    #[test]
    fn thumb_url_basic() {
        assert_eq!(
            thumb_url(COMMONS, "Example.jpg", 120),
            "https://upload.wikimedia.org/wikipedia/commons/thumb/a/a9/Example.jpg/120px-Example.jpg"
        );
    }

    #[test]
    fn thumb_suffix_svg_gets_png() {
        // Tux.svg → md5 35eb3480… → 3/35, and the thumb gains `.png`.
        assert_eq!(
            thumb_url(COMMONS, "Tux.svg", 250),
            "https://upload.wikimedia.org/wikipedia/commons/thumb/3/35/Tux.svg/250px-Tux.svg.png"
        );
    }

    #[test]
    fn thumb_suffix_tiff_gets_jpg() {
        // Image.tiff → md5 e324757d… → e/e3, thumb gains `.jpg`.
        assert_eq!(
            thumb_url(COMMONS, "Image.tiff", 500),
            "https://upload.wikimedia.org/wikipedia/commons/thumb/e/e3/Image.tiff/500px-Image.tiff.jpg"
        );
    }

    #[test]
    fn upload_base_trailing_slash_trimmed() {
        assert_eq!(
            original_url("https://host/wikipedia/en/", "Example.jpg"),
            "https://host/wikipedia/en/a/a9/Example.jpg"
        );
    }
}

//! Render-width buckets (plan §4): MediaWiki serves pre-scaled thumbs
//! only at standard sizes; a non-standard request width is snapped
//! server-side anyway, so we snap client-side too and only ever ask for
//! (and cache) a bucket. `None` means the original, unscaled file.

/// The standard thumb widths, ascending. Kept in sync with the plan's
/// "20/40/60/120/250/330/500/960 px render buckets".
pub const BUCKETS: [u32; 8] = [20, 40, 60, 120, 250, 330, 500, 960];

/// A resolved width: either the original file or one standard bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// The unscaled original — no `/thumb/` path.
    Orig,
    /// A standard thumb width in px.
    Px(u32),
}

impl Bucket {
    /// Cache-key / route label: "orig" or the bare pixel count.
    pub fn label(self) -> String {
        match self {
            Bucket::Orig => "orig".to_string(),
            Bucket::Px(w) => w.to_string(),
        }
    }

    /// Pixel width for a thumb URL, or None for the original.
    pub fn width(self) -> Option<u32> {
        match self {
            Bucket::Orig => None,
            Bucket::Px(w) => Some(w),
        }
    }
}

/// Snap a requested width to a bucket. `None` → [`Bucket::Orig`].
/// A concrete width snaps UP to the smallest bucket that is ≥ it, so a
/// thumb never renders smaller than asked; anything wider than the top
/// bucket clamps to the top bucket (never fetch an original for a
/// merely-large thumb request — Robot policy prefers thumbs).
pub fn snap(width: Option<u32>) -> Bucket {
    match width {
        None => Bucket::Orig,
        Some(w) => {
            for &b in BUCKETS.iter() {
                if w <= b {
                    return Bucket::Px(b);
                }
            }
            Bucket::Px(BUCKETS[BUCKETS.len() - 1])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_orig() {
        assert_eq!(snap(None), Bucket::Orig);
        assert_eq!(Bucket::Orig.label(), "orig");
        assert_eq!(Bucket::Orig.width(), None);
    }

    #[test]
    fn snaps_up_to_nearest_bucket() {
        // Exact bucket values stay put.
        assert_eq!(snap(Some(120)), Bucket::Px(120));
        assert_eq!(snap(Some(20)), Bucket::Px(20));
        assert_eq!(snap(Some(960)), Bucket::Px(960));
        // Between buckets snaps UP.
        assert_eq!(snap(Some(1)), Bucket::Px(20));
        assert_eq!(snap(Some(21)), Bucket::Px(40));
        assert_eq!(snap(Some(121)), Bucket::Px(250));
        assert_eq!(snap(Some(300)), Bucket::Px(330));
        // Wider than the top bucket clamps to the top bucket.
        assert_eq!(snap(Some(961)), Bucket::Px(960));
        assert_eq!(snap(Some(100000)), Bucket::Px(960));
    }

    #[test]
    fn label_is_bare_pixels() {
        assert_eq!(Bucket::Px(250).label(), "250");
        assert_eq!(Bucket::Px(250).width(), Some(250));
    }
}

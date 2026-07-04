//! Footer layout.
//!
//! The footer is exactly the last 8 bytes of a shard file:
//!
//! ```text
//! [0..4)   u32 tail_len    (LE)
//! [4..8)   u32 entry_count (LE)
//! ```

pub const FOOTER_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    pub tail_len: u32,
    pub entry_count: u32,
}

/// Parse an 8-byte footer. Returns `None` only if the slice is the wrong size.
pub fn parse_footer(bytes: &[u8]) -> Option<Footer> {
    if bytes.len() != FOOTER_SIZE {
        return None;
    }
    let tail_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let entry_count = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    Some(Footer {
        tail_len,
        entry_count,
    })
}

/// Serialize a footer into an 8-byte buffer.
pub fn write_footer_bytes(footer: Footer) -> [u8; FOOTER_SIZE] {
    let mut out = [0u8; FOOTER_SIZE];
    out[0..4].copy_from_slice(&footer.tail_len.to_le_bytes());
    out[4..8].copy_from_slice(&footer.entry_count.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let f = Footer {
            tail_len: 17,
            entry_count: 5,
        };
        let bytes = write_footer_bytes(f);
        assert_eq!(parse_footer(&bytes), Some(f));
    }

    #[test]
    fn wrong_size_rejected() {
        assert_eq!(parse_footer(&[0u8; 4]), None);
        assert_eq!(parse_footer(&[0u8; 16]), None);
    }
}

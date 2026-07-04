//! # strpool
//!
//! A sharded, append-only byte-string pool. Each string gets a dense `u64` id
//! at insert. The pool lives in a directory of *shards*, where each shard is a
//! **single file** that contains zero or more zstd frames followed by a
//! plaintext tail and a fixed 8-byte footer.
//!
//! There is no lookup-by-id API; readers iterate or substring-scan.
//!
//! ## On-disk format (one shard = one file, no sidecars)
//!
//! From offset 0:
//!
//! * Zero or more zstd frames. Each frame's decompressed payload is a
//!   concatenation of null-terminated byte strings.
//! * Plaintext tail: flat null-terminated byte strings appended since the last
//!   seal.
//! * Footer (last 8 bytes):
//!   * `u32 tail_len` (little-endian)
//!   * `u32 entry_count` (little-endian)
//!
//! Derived: `tail_start = file_size - 8 - tail_len`, and the frame region is
//! exactly `[0 .. tail_start)`. Zstd is given exactly this byte range — never
//! one byte more, never one byte less.
//!
//! ## Crash-safety contract
//!
//! * The shard is ONE FILE plus, transiently, a `<shard>.tmp` during seal.
//! * [`Pool::flush`] returns when all appends issued before it are durable.
//! * If you crash without flushing, you may lose appends. The file's state on
//!   disk after a crash without flush is whatever the OS/filesystem left
//!   there; we don't promise anything more.
//! * A crash mid-seal leaves either the new sealed shard durable (rename
//!   completed) or the pre-seal shard durable (rename didn't). Either way, on
//!   next open, `<shard>.tmp` is deleted if present and the shard works.

mod error;
mod footer;
mod pool;
mod shard;

pub use error::StrpoolError;
pub use pool::Pool;

/// Result alias used by the public API.
pub type Result<T> = std::result::Result<T, StrpoolError>;

/// Configuration for a [`Pool`].
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Number of shard files in the pool. Must be at least 1.
    pub shard_count: u32,
    /// Plaintext-tail size at which [`Pool::maybe_seal`] performs a seal.
    pub seal_threshold_bytes: u64,
}

/// Resolves dictionary bytes by id. The pool calls this during decode (where
/// the id comes from the frame header) and during seal (where the id was set
/// by [`Pool::set_dict`]).
///
/// `Ok(None)` means "no such id". The pool surfaces this as
/// [`StrpoolError::MissingDict`] at the call site that requires the dict.
pub trait DictProvider: Send + Sync {
    fn dict(&self, id: u32) -> Result<Option<Vec<u8>>>;
}

/// Re-exported for tests that inspect the shard-file layout directly.
#[doc(hidden)]
pub mod _internal {
    pub use crate::footer::{parse_footer, write_footer_bytes, Footer, FOOTER_SIZE};
}

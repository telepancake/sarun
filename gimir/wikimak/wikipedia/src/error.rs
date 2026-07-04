//! Error type for the wikipedia crate.
//!
//! Per SPEC: errors via `thiserror` enum. No `anyhow`/`eyre`/`Box<dyn Error>`.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("depot: {0}")]
    Depot(#[from] wikimak_depot::Error),

    #[error("strpool: {0}")]
    Strpool(#[from] strpool::StrpoolError),

    #[error("mediawiki: {0}")]
    Mediawiki(#[from] wikimak_mediawiki::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// `page_id` exceeds the depot's `max_chain_id`. Reopen with a larger cap.
    #[error("page id {page_id} exceeds max_chain_id {max_chain_id}")]
    PageIdOverflow { page_id: u64, max_chain_id: u64 },

    /// Per-revision binary record was malformed on decode.
    #[error("revision codec: {0}")]
    Codec(&'static str),

    #[error("corrupt: {0}")]
    Corrupt(&'static str),
}

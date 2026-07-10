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

    /// `page_id` is at or above the depot's 2^40 chain-id sanity
    /// ceiling. Below the ceiling there is no bound to hit — the
    /// depot's index auto-grows — so this fires only on a corrupt id
    /// (or the wrong planet's wiki), and it fires BEFORE any write.
    #[error("page id {page_id} exceeds the chain-id sanity ceiling {ceiling}")]
    PageIdOverflow { page_id: u64, ceiling: u64 },

    /// Per-revision binary record was malformed on decode.
    #[error("revision codec: {0}")]
    Codec(&'static str),

    #[error("corrupt: {0}")]
    Corrupt(&'static str),

    /// Another process holds this instance root (meta.db exclusive
    /// lock). One process at a time per root — by lock, not convention.
    #[error("instance {0} is locked by another process")]
    InstanceLocked(std::path::PathBuf),
}

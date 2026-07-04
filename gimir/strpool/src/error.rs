use thiserror::Error;

/// Errors surfaced by the public API.
#[derive(Error, Debug)]
pub enum StrpoolError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("zstd: {0}")]
    Zstd(String),

    #[error("missing dict id {0}")]
    MissingDict(u32),
}

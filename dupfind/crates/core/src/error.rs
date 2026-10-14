//! The single error type used across `core`. `cli` may wrap it in `anyhow`.

/// Convenience alias for `core` fallible functions.
pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("schema version {found}, expected {expected}")]
    SchemaVersion { found: i64, expected: i64 },

    #[error("volume mismatch at {mount}: db {db_uuid}, found {found_uuid}")]
    VolumeMismatch {
        mount: String,
        db_uuid: String,
        found_uuid: String,
    },

    #[error("hash config mismatch: {0}")]
    HashConfigMismatch(String),

    #[error("db writer thread died")]
    WriterDied,

    #[error("WAL unavailable (journal_mode = {got}); put --db on a local disk")]
    WalUnavailable { got: String },

    #[error("cancelled")]
    Cancelled,

    #[error("nothing to resume")]
    NothingToResume,

    #[error("path too long: {0}")]
    PathTooLong(String),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid state {0}")]
    InvalidState(i64),

    #[error("invalid argument: {0}")]
    InvalidArg(String),

    #[error("encoding: {0}")]
    Encoding(String),

    #[error("{0}")]
    Other(String),
}

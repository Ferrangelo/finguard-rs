//! Crate-wide error type.
//!
//! Where the original Python code raises `ValueError` or `KeyError`, the Rust
//! port returns an [`Err`] carrying the appropriate [`Error`] variant instead
//! of panicking.

/// Convenience result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// All errors that can be produced by finguard_rs.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An underlying I/O error (file read/write, directory creation, ...).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON (de)serialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// An error originating from the polars dataframe engine.
    #[error("polars error: {0}")]
    Polars(#[from] polars::prelude::PolarsError),

    /// A value was outside its accepted range, or otherwise invalid.
    ///
    /// This is the analogue of Python's `ValueError` for range/parse failures,
    /// e.g. an out-of-range month or an unparseable filename.
    #[error("{0}")]
    InvalidArgument(String),

    /// A requested item was not found.
    ///
    /// This is the analogue of Python's `KeyError`.
    #[error("{0}")]
    NotFound(String),

    /// An item already exists where uniqueness was required.
    ///
    /// This is the analogue of the `ValueError` raised on duplicate entries
    /// (e.g. adding a mapping without `overwrite`, or a duplicate category).
    #[error("{0}")]
    AlreadyExists(String),

    /// The user's home directory could not be determined.
    #[error("could not determine home directory")]
    NoHomeDir,

    /// A network request to an external service failed, or its response was
    /// not in the shape the caller expected.
    #[error("{0}")]
    Network(String),

    /// A loaded table has no `row_id` column, or a row without a `row_id`.
    /// Only the startup migration in [`crate::row_id_migration`] assigns
    /// IDs to existing rows, because an ID assigned on read and never saved
    /// would change on every request and make a delete miss its row.
    #[error("{0}")]
    RowIdsMissing(String),

    /// Another handle, in this process or another one, already holds the sync
    /// change log at `path`. Only one writer at a time may append to it,
    /// because two would recover the same clock state and then issue the same
    /// stamps, and a stamp has to name one change forever.
    #[error(
        "the sync change log at {} is already open: another process, or another handle in this \
         one, holds it. Only one writer at a time may append, because two would issue the same \
         stamps.",
        path.display()
    )]
    SyncLogLocked {
        /// The change log the caller tried to open.
        path: std::path::PathBuf,
    },

    /// A batch of changes from another device does not fit this device's
    /// data: a column a file lacks, a value of the wrong type for its
    /// column, or a row it cannot address. Nothing was written. The message
    /// names the table, the column, and the expected type, never a value.
    #[error("{0}")]
    MergeRejected(String),

    /// The row ID migration failed on `path`. The backend must not serve
    /// requests after this error.
    #[error("row ID migration failed for {}: {source}", path.display())]
    RowIdMigration {
        /// The file or folder the migration was reading, backing up, or
        /// writing when it failed.
        path: std::path::PathBuf,
        /// The underlying failure.
        source: Box<Error>,
    },
}

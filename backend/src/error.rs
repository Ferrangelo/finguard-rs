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
}

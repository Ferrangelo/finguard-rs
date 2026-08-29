//! finguard_rs: a Rust rewrite of the finguard personal-finance app.
//!
//! This crate is the library backing the application; the `finguard_rs_backend`
//! binary built from `src/main.rs` is a thin Axum HTTP layer on top of it.
//! Module responsibilities:
//!
//! - [`paths`]: resolves on-disk Parquet file locations under
//!   `$XDG_DATA_HOME/finguard/dbs/`.
//! - [`config`]: reads and writes the JSON category-mapping and known-category
//!   files under `$XDG_CONFIG_HOME/finguard/`.
//! - [`df_operations`]: the domain layer. Loads, mutates, and persists the
//!   Polars dataframes backing every financial table (expenses, cashflow,
//!   investments, liquidity, credits/debts).
//! - [`plots`]: derives chart-ready series (pie/bar/line data, no styling)
//!   from the dataframes in [`df_operations`].
//! - [`expr`]: a small safe arithmetic evaluator for amount input fields.
//! - [`error`]: the crate-wide [`Error`]/[`Result`] pair used by every module.
//!
//! Several parts of this crate deliberately reproduce behavior from the
//! original Python application at `/home/anferrar/Projects/finguard` byte for
//! byte, most notably category-name normalization
//! ([`df_operations::normalize_category_value`]) and banker's rounding
//! ([`plots`]'s `round_half_even`/`round2_half_even`). The on-disk Parquet
//! schema is a compatibility boundary shared with that Python project, so
//! check it before changing a schema-affecting type in [`df_operations`].

pub mod config;
pub mod df_operations;
pub mod error;
pub mod expr;
pub mod paths;
pub mod plots;

pub use error::{Error, Result};

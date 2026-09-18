//! finguard_rs: a Rust rewrite of the finguard personal-finance app.
//!
//! This crate is the library backing the application. The `finguard_rs_backend`
//! binary built from `src/main.rs` is only startup: it binds the listener,
//! runs the row ID migration, records the change log baseline, and serves
//! [`api::router`]. Module responsibilities:
//!
//! - [`api`]: the HTTP surface, routes, request/response DTOs, and handlers,
//!   built as [`api::router`] so any embedder, such as a future Tauri app,
//!   can serve the same router the desktop binary does.
//! - [`paths`]: resolves on-disk Parquet file locations under
//!   `$XDG_DATA_HOME/finguard/dbs/`.
//! - [`config`]: reads and writes the JSON category-mapping and known-category
//!   files under `$XDG_CONFIG_HOME/finguard/`.
//! - [`df_operations`]: the domain layer. Loads, mutates, and persists the
//!   Polars dataframes backing every financial table (expenses, cashflow,
//!   investments, liquidity, credits/debts), and records each saved change in
//!   [`sync`].
//! - [`plots`]: derives chart-ready series (pie/bar/line data, no styling)
//!   from the dataframes in [`df_operations`].
//! - [`expr`]: a small safe arithmetic evaluator for amount input fields.
//! - [`fx`]: currency settings, the cached Frankfurter (ECB) exchange-rate
//!   table, and the rate resolvers used to convert amounts between currencies.
//! - [`row_id_migration`]: the startup migration that gives every row of the
//!   synced tables a stable row ID, after backing up the data folder. Every
//!   program that serves this data calls it before its first request.
//! - [`sync`]: the append-only change log under
//!   `$XDG_DATA_HOME/finguard/sync/`, its hybrid logical clock, and this
//!   device's id. Every method in [`df_operations`] that saves a change
//!   appends one entry here, right after the write that saved it.
//! - [`merge`]: the merge decision engine. Decides what a batch of changes
//!   from another device does to this device's data, as pure logic over two
//!   [`sync`] logs: it touches no file and writes nothing.
//! - [`merge_apply`]: carries a [`merge`] plan out against this device's
//!   Parquet files and change log, storing the other device's entries
//!   verbatim. The transport that will call it is later work.
//! - [`write_lock`]: the process-wide lock that every data-changing request
//!   and every merge holds, so the two never interleave.
//! - [`sync_baseline`]: the startup pass that records the rows a data folder
//!   already holds, so the log describes the whole current state rather than
//!   starting mid history. It runs after [`row_id_migration`], writes nothing
//!   when the log already holds a line, and takes the log's single writer
//!   lock for the life of the process. Every program that serves this data
//!   calls it before its first request.
//! - [`error`]: the crate-wide [`Error`]/[`Result`] pair used by every module.
//!
//! Several parts of this crate deliberately reproduce behavior from the
//! original Python application at `/home/anferrar/Projects/finguard` byte for
//! byte, most notably category-name normalization
//! ([`df_operations::normalize_category_value`]) and banker's rounding
//! ([`plots`] module). The on-disk Parquet
//! schema is a compatibility boundary shared with that Python project, so
//! check it before changing a schema-affecting type in [`df_operations`].

pub mod api;
pub mod config;
pub mod df_operations;
pub mod error;
pub mod expr;
pub mod fx;
mod http_error;
pub mod merge;
pub mod merge_apply;
pub mod paths;
pub mod plots;
pub mod row_id_migration;
pub mod sync;
pub mod sync_baseline;
pub mod write_lock;

pub use error::{Error, Result};

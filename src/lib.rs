//! finguard_rs — a Rust rewrite of the finguard personal-finance app.
//!
//! This crate is the library backing the application. Wave 1 provides the
//! foundation: filesystem path resolution ([`paths`]) and JSON configuration
//! management ([`config`]). Later waves add the dataframe operations (polars)
//! and UI layers.

pub mod config;
pub mod df_operations;
pub mod error;
pub mod expr;
pub mod paths;
pub mod plots;

pub use error::{Error, Result};

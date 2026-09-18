//! Finguard backend binary: process startup only.
//!
//! Binds the listener, runs the row ID migration, records the change log
//! baseline, then serves [`finguard_rs_backend::api::router`]. The HTTP route
//! table, request and response DTOs, and handlers live in
//! [`finguard_rs_backend::api`]; this binary depends on the library and adds
//! nothing but startup.

use std::net::SocketAddr;

use finguard_rs_backend::{row_id_migration, sync_baseline};

/// Bind the listener, run the row ID migration, record the change log
/// baseline, then serve [`finguard_rs_backend::api::router`].
///
/// Binding comes first so that a taken or invalid address stops the process
/// before any data file is touched. The migration
/// ([`row_id_migration::migrate_row_ids`]) runs before serving, because every
/// handler that loads a synced table rejects a file without row IDs. The
/// baseline ([`sync_baseline::baseline_change_log`]) runs after it, because a
/// row recorded before it has an ID could not be merged with the same row on
/// another device; it also takes the change log's single writer lock for the
/// life of the process. Requests that arrive during either step wait in the
/// listen queue. If binding, the migration, or the baseline fails, the
/// process exits with status 1 and never serves a request.
///
/// `FINGUARD_HOST`/`FINGUARD_PORT` override the default bind address
/// (`127.0.0.1:3111`); both are read once at startup, not per request.
#[tokio::main]
async fn main() {
    let host = std::env::var("FINGUARD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("FINGUARD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3111);

    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .expect("Invalid address");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("Finguard server not started: cannot listen on {addr}: {err}");
            std::process::exit(1);
        }
    };

    match row_id_migration::migrate_row_ids() {
        Ok(report) => println!("{report}"),
        Err(err) => {
            eprintln!("Finguard server not started: {err}");
            std::process::exit(1);
        }
    }

    match sync_baseline::baseline_change_log() {
        Ok(report) => println!("{report}"),
        Err(err) => {
            eprintln!("Finguard server not started: {err}");
            std::process::exit(1);
        }
    }

    println!("Finguard server running on http://{}", addr);
    axum::serve(listener, finguard_rs_backend::api::router())
        .await
        .unwrap()
}

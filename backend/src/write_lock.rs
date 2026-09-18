//! The process-wide data write lock.
//!
//! One lock serializes every change to this device's data: each request in
//! [`crate::api::router`] that can write takes it for the whole request, and
//! [`crate::merge_apply::apply_remote_batch`] takes it for its whole run.
//! Without it a local edit could land between a merge reading the change log
//! and storing the other device's entries, and the merge would then write
//! over a change the log says is newer. It also stops two concurrent saves of
//! one file from losing one of the two edits.
//!
//! The lock is a static rather than router state, because the Android app
//! may build the router more than once and every copy has to share one lock
//! with the merge. It is a [`tokio::sync::Mutex`] because it has two kinds of
//! caller: the async request layer awaits it, and the merge, which is
//! blocking code, takes it with [`tokio::sync::Mutex::blocking_lock`]. A
//! standard mutex guard cannot be held across an `.await` in a request
//! future, and an async lock needs no runtime to be taken from a plain
//! thread.
//!
//! Reads never take it. No `GET` route writes a data file: the only file a
//! read can write is the exchange rate cache, which has its own lock in
//! [`crate::fx`] and which no merge touches.

use axum::extract::Request;
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use tokio::sync::{Mutex, MutexGuard};

static WRITE_LOCK: Mutex<()> = Mutex::const_new(());

/// Wait for the data write lock from async code. The data is free again when
/// the guard drops.
pub async fn lock() -> MutexGuard<'static, ()> {
    WRITE_LOCK.lock().await
}

/// Wait for the data write lock from blocking code, such as a merge running
/// on its own thread or inside [`tokio::task::spawn_blocking`].
///
/// # Panics
///
/// Panics when called from inside an async task, as
/// [`tokio::sync::Mutex::blocking_lock`] does: blocking a runtime worker
/// there could stall the very request holding the lock. Move the call to
/// [`tokio::task::spawn_blocking`] instead.
pub fn lock_blocking() -> MutexGuard<'static, ()> {
    WRITE_LOCK.blocking_lock()
}

/// Request layer for [`crate::api::router`]: hold the data write lock for
/// the whole of every request whose method can change data.
///
/// `GET`, `HEAD`, and `OPTIONS` pass straight through. Every other method
/// waits, so a route added later with `post`, `put`, or `delete` is covered
/// without anyone remembering to add it here.
pub(crate) async fn hold_for_writes(request: Request, next: Next) -> Response {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return next.run(request).await;
    }
    let _guard = lock().await;
    next.run(request).await
}

//! Shared test helpers.
//!
//! Path/config resolution reads `XDG_DATA_HOME` / `XDG_CONFIG_HOME` at call
//! time, and env vars are process-global, so any test that mutates them MUST be
//! marked `#[serial_test::serial]` to avoid clobbering parallel tests.

#![allow(dead_code)]

use std::path::PathBuf;

use tempfile::TempDir;

/// A scoped, isolated data/config root for a single test.
///
/// Creating one points both `XDG_DATA_HOME` and `XDG_CONFIG_HOME` at a fresh
/// `TempDir`, so the test sees a clean filesystem state and never touches the
/// real user's home directory. The env vars stay set for the lifetime of the
/// returned guard (i.e. the whole test body); the `TempDir` is removed on drop.
pub struct TestEnv {
    _dir: TempDir,
    root: PathBuf,
}

impl TestEnv {
    /// Create a fresh isolated environment and point the XDG vars at it.
    pub fn new() -> Self {
        let dir = TempDir::new().expect("create tempdir");
        let root = dir.path().to_path_buf();
        // SAFETY: tests using this are serialized via `#[serial_test::serial]`,
        // so no other thread observes the env mutation concurrently.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &root);
            std::env::set_var("XDG_CONFIG_HOME", &root);
        }
        Self { _dir: dir, root }
    }

    /// The temp root directory used for both data and config.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
}

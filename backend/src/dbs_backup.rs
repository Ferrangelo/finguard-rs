//! Copy the whole `dbs` tree into a new folder under
//! `<data_home>/finguard/backups/` before a run changes or removes data.
//!
//! Two callers share it: the row ID migration in
//! [`crate::row_id_migration`], and a phone reset from the hub in
//! [`crate::sync_exchange`]. Each names its own folder suffix and reports a
//! failure as its own error, so the copier returns the failing path and the
//! cause and leaves the wrapping to the caller.
//!
//! Symbolic links are followed: a linked file or folder is copied as its
//! contents. A folder link that leads back into a folder the copy is already
//! inside, or into the backup itself, is skipped with a warning, so a link
//! loop cannot make the copy endless. A broken link is skipped with a
//! warning. Warnings name files and folders, never their contents.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::df_operations;
use crate::error::Error;
use crate::paths::get_backups_dir;

/// The path a backup was working on when it failed, and why.
#[derive(Debug)]
pub(crate) struct BackupFailure {
    /// The file or folder being read, created, or flushed.
    pub path: PathBuf,
    /// The underlying failure.
    pub source: Error,
}

fn fail_at(path: &Path, err: impl Into<Error>) -> BackupFailure {
    BackupFailure {
        path: path.to_path_buf(),
        source: err.into(),
    }
}

/// Copy the whole `dbs_root` tree into a new
/// `<backups>/<UTC timestamp>-<suffix>/` folder, flush it to disk, and return
/// its path. Never merges into an existing folder: a name collision gets a
/// numeric suffix instead. `label` starts each warning, such as
/// `"Row ID migration"`, so the user can tell which run wrote it.
///
/// # Errors
///
/// A [`BackupFailure`] naming the path when the backups folder cannot be
/// created, a file or folder cannot be read, copied, or flushed. A failure
/// naming `dbs_root` means the backups folder itself could not be made. The
/// partial copy stays where it is.
pub(crate) fn backup_dbs(
    dbs_root: &Path,
    suffix: &str,
    label: &str,
) -> std::result::Result<PathBuf, BackupFailure> {
    let backups_dir = get_backups_dir().map_err(|e| fail_at(dbs_root, e))?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut attempt = 1;
    let backup_dir = loop {
        let name = if attempt == 1 {
            format!("{stamp}-{suffix}")
        } else {
            format!("{stamp}-{suffix}-{attempt}")
        };
        let candidate = backups_dir.join(name);
        match std::fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => attempt += 1,
            Err(e) => return Err(fail_at(&candidate, e)),
        }
    };

    // The backup folder is in the set so a link to `backups/` cannot copy
    // the backup into itself.
    let mut open_folders = HashSet::new();
    for folder in [dbs_root, backup_dir.as_path()] {
        open_folders.insert(std::fs::canonicalize(folder).map_err(|e| fail_at(folder, e))?);
    }
    copy_dir_contents(dbs_root, &backup_dir, &mut open_folders, label)?;

    // The new folder's entry lives in `backups_dir`, whose own entry may be
    // new too.
    sync_dir(&backups_dir)?;
    if let Some(app_dir) = backups_dir.parent() {
        sync_dir(app_dir)?;
    }
    Ok(backup_dir)
}

/// Recursively copy everything inside `from` into the existing folder `to`,
/// flushing each copied file and folder to disk.
///
/// Symbolic links are followed. `open_folders` holds the canonical paths of
/// the folders being copied on the current path, plus the backup folder: a
/// folder link that resolves to one of them would copy forever, so it is
/// skipped with a warning. A broken link has no contents and is skipped with
/// a warning too.
fn copy_dir_contents(
    from: &Path,
    to: &Path,
    open_folders: &mut HashSet<PathBuf>,
    label: &str,
) -> std::result::Result<(), BackupFailure> {
    for entry in std::fs::read_dir(from).map_err(|e| fail_at(from, e))? {
        let entry = entry.map_err(|e| fail_at(from, e))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let metadata = match std::fs::metadata(&source) {
            Ok(metadata) => metadata,
            Err(err) => {
                let is_link = std::fs::symlink_metadata(&source)
                    .is_ok_and(|link| link.file_type().is_symlink());
                if !is_link {
                    return Err(fail_at(&source, err));
                }
                eprintln!(
                    "{label}: the backup skipped {}, a broken link.",
                    source.display()
                );
                continue;
            }
        };
        if metadata.is_dir() {
            let canonical = std::fs::canonicalize(&source).map_err(|e| fail_at(&source, e))?;
            if open_folders.contains(&canonical) {
                eprintln!(
                    "{label}: the backup skipped {}, a link back to a folder it is already \
                     copying.",
                    source.display()
                );
                continue;
            }
            std::fs::create_dir(&target).map_err(|e| fail_at(&target, e))?;
            open_folders.insert(canonical.clone());
            let copied = copy_dir_contents(&source, &target, open_folders, label);
            open_folders.remove(&canonical);
            copied?;
        } else {
            std::fs::copy(&source, &target).map_err(|e| fail_at(&source, e))?;
            std::fs::File::open(&target)
                .and_then(|file| file.sync_all())
                .map_err(|e| fail_at(&target, e))?;
        }
    }
    sync_dir(to)
}

/// Flush the entries of the folder `path` to disk, so a file created or
/// renamed in it survives a crash. See [`df_operations::sync_dir`].
fn sync_dir(path: &Path) -> std::result::Result<(), BackupFailure> {
    df_operations::sync_dir(path).map_err(|e| fail_at(path, e))
}

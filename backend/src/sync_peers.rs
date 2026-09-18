//! The devices this device is paired with for sync.
//!
//! Stored as `sync_peers.json` in the config directory, beside `device_id`
//! and for the same reason: a copied data folder must not carry a pairing to
//! another device, because a pairing is part of this device's identity, not
//! of its data.
//!
//! The file is one JSON object with a `peers` list. Each record holds the
//! peer's device id, its role, and when it was paired. Part 3b adds keys to
//! a record, so both the file and each record keep every field this version
//! does not know: a read followed by a rewrite writes them back unchanged.
//! Every write goes through a temporary file in the same folder, a flush,
//! and a rename, so a crash leaves either the old file or the new one.
//!
//! Whether any peer exists is what [`crate::sync_baseline::baseline_change_log`]
//! asks before recording the data again, which it must never do on a paired
//! device: see [`is_paired`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::get_config_dir;
use crate::error::{Error, Result};

const PEERS_FILE_NAME: &str = "sync_peers.json";

/// Serializes read, change, and rewrite of the peers file inside this
/// process, so two pairings finishing at once cannot drop each other's
/// record.
static PEERS_FILE_LOCK: Mutex<()> = Mutex::new(());

/// What a device is in the sync design: the desktop hub every phone syncs
/// with, or a phone.
///
/// Stored as a string, so a role a later version writes survives a read and
/// a rewrite as [`PeerRole::Unknown`]. An exchange refuses a peer whose role
/// is unknown; see [`crate::sync_exchange`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum PeerRole {
    /// The desktop that holds the reference copy of the data.
    Hub,
    /// A phone that syncs with the hub.
    Phone,
    /// A role this version does not know, kept as written.
    Unknown(String),
}

impl From<String> for PeerRole {
    fn from(value: String) -> Self {
        match value.as_str() {
            "hub" => PeerRole::Hub,
            "phone" => PeerRole::Phone,
            _ => PeerRole::Unknown(value),
        }
    }
}

impl From<PeerRole> for String {
    fn from(role: PeerRole) -> Self {
        match role {
            PeerRole::Hub => "hub".to_string(),
            PeerRole::Phone => "phone".to_string(),
            PeerRole::Unknown(value) => value,
        }
    }
}

/// One paired device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerRecord {
    /// The peer's device id, as its own `device_id` file holds it.
    pub device_id: String,
    /// What the peer is.
    pub role: PeerRole,
    /// When the pairing was recorded, in milliseconds since the Unix epoch.
    pub paired_at_ms: i64,
    /// Set on the hub, for a phone: that phone must reset from the hub
    /// before any ordinary exchange. [`crate::sync_exchange::pair_with_phone`]
    /// and [`crate::sync_exchange::repair_hub_log`] set it. Absent on the
    /// file when not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_required: Option<RequiredReset>,
    /// Set on a phone, for its hub: the id of the last reset from that hub
    /// that finished, as [`RequiredReset::id`] named it. The phone's hello
    /// carries it, and it is the hub's only proof that a required reset
    /// happened. Absent on the file when not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_reset: Option<String>,
    /// Every field this version does not know, kept so a rewrite does not
    /// drop what a later version stored, such as the peer's keys.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl PeerRecord {
    /// A record for a peer paired now, with no extra fields.
    pub fn new(device_id: impl Into<String>, role: PeerRole) -> Self {
        PeerRecord {
            device_id: device_id.into(),
            role,
            paired_at_ms: chrono::Utc::now().timestamp_millis(),
            reset_required: None,
            completed_reset: None,
            extra: Map::new(),
        }
    }
}

/// A reset the hub requires of one phone.
///
/// The id is fresh for every requirement, and the hub hands it out only in
/// the full log a reset applies ([`crate::sync_exchange::FullLog`]). So a
/// phone that reports it has finished a reset from a log issued after the
/// requirement: no stamp, count, or older reset can stand in for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredReset {
    /// A random id naming this requirement.
    pub id: String,
    /// While the phone has not proven the reset of its first pairing, that
    /// reset's id. The phone's data may then predate the pairing, and the
    /// hub takes no push from it. A hub repair replaces `id` and keeps this,
    /// so a phone that finished its first reset before the repair, and
    /// proves it with this id, may still push its later edits first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_pairing_id: Option<String>,
}

impl RequiredReset {
    /// The requirement of a first pairing: a fresh id, which is also the
    /// first pairing's id.
    pub fn first_pairing() -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        RequiredReset {
            first_pairing_id: Some(id.clone()),
            id,
        }
    }

    /// The requirement a hub repair sets: a fresh id, keeping the first
    /// pairing's id from `previous` while that reset is still unproven.
    pub fn after_repair(previous: Option<&RequiredReset>) -> Self {
        RequiredReset {
            id: uuid::Uuid::new_v4().to_string(),
            first_pairing_id: previous.and_then(|required| required.first_pairing_id.clone()),
        }
    }

    /// Whether a phone whose last finished reset is `last_reset` still owes
    /// the reset of its first pairing.
    pub fn first_pairing_pending(&self, last_reset: Option<&str>) -> bool {
        self.first_pairing_id
            .as_deref()
            .is_some_and(|first| last_reset != Some(first))
    }
}

/// The whole file, with its unknown top level fields.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PeersFile {
    #[serde(default)]
    peers: Vec<PeerRecord>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Return `<config_dir>/sync_peers.json`, creating its directory if
/// necessary.
pub fn sync_peers_path() -> Result<PathBuf> {
    Ok(get_config_dir()?.join(PEERS_FILE_NAME))
}

/// Every paired device, in the order they were first recorded. A missing
/// file means no peer.
///
/// # Errors
///
/// [`Error::SyncRefused`] when the file exists and cannot be read or is not
/// a peers file. The message names the file and says to remove it and pair
/// again; it carries no path and no file text.
pub fn load_peers() -> Result<Vec<PeerRecord>> {
    Ok(read_peers_file(&sync_peers_path()?)?.peers)
}

/// The record for `device_id`, or `None` when that device is not paired.
///
/// # Errors
///
/// As for [`load_peers`].
pub fn find_peer(device_id: &str) -> Result<Option<PeerRecord>> {
    Ok(load_peers()?
        .into_iter()
        .find(|peer| peer.device_id == device_id))
}

/// Whether this device has at least one sync peer.
///
/// # Errors
///
/// As for [`load_peers`]. A caller deciding whether a step is safe on an
/// unpaired device only must treat an error as paired: a peers file that
/// cannot be read may still name a peer.
pub fn is_paired() -> Result<bool> {
    Ok(!load_peers()?.is_empty())
}

/// Store `record`, replacing the record with the same device id or adding
/// it at the end. Fields of the old record that `record` does not carry in
/// [`PeerRecord::extra`] are kept, so a caller that knows only some fields
/// cannot erase the rest. Unknown top level fields of the file are kept too.
/// [`PeerRecord::reset_required`] and [`PeerRecord::completed_reset`] keep
/// their old value when `record` leaves them unset, so pairing a device again
/// never clears a reset the hub still requires.
///
/// # Errors
///
/// [`Error::InvalidArgument`] when the device id is empty or is this
/// device's own id, which would make the device its own peer. Otherwise the
/// errors of [`load_peers`], and [`Error::Io`] or [`Error::Json`] when the
/// new file cannot be written, flushed, or renamed. On a failure the old
/// file is still in place.
pub fn record_peer(record: PeerRecord) -> Result<()> {
    if record.device_id.trim().is_empty() {
        return Err(Error::InvalidArgument(
            "a sync peer needs a device id".to_string(),
        ));
    }
    if record.device_id == crate::sync::device_id()? {
        return Err(Error::InvalidArgument(format!(
            "device {} cannot be paired with itself",
            record.device_id
        )));
    }

    let _guard = PEERS_FILE_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let path = sync_peers_path()?;
    let mut file = read_peers_file(&path)?;
    match file
        .peers
        .iter_mut()
        .find(|peer| peer.device_id == record.device_id)
    {
        Some(existing) => {
            let mut extra = std::mem::take(&mut existing.extra);
            extra.extend(record.extra);
            let reset_required = record.reset_required.or(existing.reset_required.take());
            let completed_reset = record.completed_reset.or(existing.completed_reset.take());
            *existing = PeerRecord {
                device_id: record.device_id,
                role: record.role,
                paired_at_ms: record.paired_at_ms,
                reset_required,
                completed_reset,
                extra,
            };
        }
        None => file.peers.push(record),
    }
    write_peers_file(&path, &file)
}

/// Change the stored records with `change`, and rewrite the file when it
/// returns true. Unknown fields are kept as in [`record_peer`].
///
/// # Errors
///
/// The errors of [`load_peers`], and those of writing the file as in
/// [`record_peer`].
pub(crate) fn update_peers(change: impl FnOnce(&mut [PeerRecord]) -> bool) -> Result<()> {
    let _guard = PEERS_FILE_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let path = sync_peers_path()?;
    let mut file = read_peers_file(&path)?;
    if change(&mut file.peers) {
        write_peers_file(&path, &file)?;
    }
    Ok(())
}

/// Read the peers file. A missing file holds no peer.
///
/// A file that exists and cannot be read is an error that says how to
/// recover, because nothing else can: every pairing reads the file first,
/// so it would fail the same way forever. The message names the file by its
/// name only and gives the failure's kind, never the path or the file's
/// text, so it is safe to send to the other device.
fn read_peers_file(path: &Path) -> Result<PeersFile> {
    let unreadable = |why: String| {
        Error::SyncRefused(format!(
            "the sync peers file {PEERS_FILE_NAME} in this device's config folder cannot be read \
             ({why}). Remove that file and pair this device again with every device it syncs \
             with."
        ))
    };
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|err| {
            unreadable(format!(
                "{:?} error at column {}",
                err.classify(),
                err.column()
            ))
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(PeersFile::default()),
        Err(err) => Err(unreadable(format!("{:?}", err.kind()))),
    }
}

/// Write `file` through a temporary file and a rename, then flush the
/// folder, so a crash leaves the old file or the complete new one.
fn write_peers_file(path: &Path, file: &PeersFile) -> Result<()> {
    let temp_path = path.with_file_name(format!(".{PEERS_FILE_NAME}.tmp"));
    let written = (|| -> Result<()> {
        let mut temp = std::fs::File::create(&temp_path)?;
        temp.write_all(&serde_json::to_vec_pretty(file)?)?;
        temp.write_all(b"\n")?;
        temp.sync_all()?;
        std::fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if written.is_err() {
        // The write error is the one to report; the old file is untouched.
        let _ = std::fs::remove_file(&temp_path);
    }
    written?;
    if let Some(folder) = path.parent() {
        crate::df_operations::sync_dir(folder)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `HOME` at three folders
    /// in a fresh temp dir, so no test reads or writes the real config.
    ///
    /// # Safety
    ///
    /// `std::env::set_var` is not thread-safe; callers hold
    /// `#[serial_test::serial]`.
    fn with_temp_env() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        for name in ["data", "config", "home"] {
            std::fs::create_dir_all(dir.path().join(name)).expect("create a root");
        }
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path().join("data"));
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
            std::env::set_var("HOME", dir.path().join("home"));
        }
        dir
    }

    /// A device with no peers file is not paired, and recording a peer
    /// pairs it, in the config folder beside the device id.
    #[test]
    #[serial_test::serial]
    fn recording_a_peer_pairs_the_device() {
        let temp = with_temp_env();
        assert!(!is_paired().unwrap());
        assert!(load_peers().unwrap().is_empty());

        record_peer(PeerRecord::new("hub-1", PeerRole::Hub)).unwrap();

        assert!(is_paired().unwrap());
        let peer = find_peer("hub-1").unwrap().expect("the peer is stored");
        assert_eq!(peer.role, PeerRole::Hub);
        assert!(peer.paired_at_ms > 0);
        assert!(
            sync_peers_path()
                .unwrap()
                .starts_with(temp.path().join("config"))
        );
        assert_eq!(
            sync_peers_path().unwrap().parent(),
            crate::sync::device_id_path().unwrap().parent()
        );
        assert!(find_peer("someone-else").unwrap().is_none());
    }

    /// Fields a later version writes, in a record or at the top of the file,
    /// survive a read and a rewrite, and an unknown role is kept as written.
    #[test]
    #[serial_test::serial]
    fn unknown_fields_survive_a_rewrite() {
        let _temp = with_temp_env();
        let path = sync_peers_path().unwrap();
        std::fs::write(
            &path,
            br#"{"format":2,"peers":[{"device_id":"hub-1","role":"hub","paired_at_ms":5,"noise_key":"abc"},{"device_id":"tab-1","role":"tablet","paired_at_ms":6}]}"#,
        )
        .unwrap();

        record_peer(PeerRecord::new("phone-2", PeerRole::Phone)).unwrap();
        // Re-pairing the hub keeps its stored key.
        record_peer(PeerRecord::new("hub-1", PeerRole::Hub)).unwrap();

        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["format"], Value::from(2));
        assert_eq!(raw["peers"][0]["noise_key"], Value::from("abc"));
        assert_eq!(raw["peers"][1]["role"], Value::from("tablet"));
        assert_eq!(raw["peers"][2]["device_id"], Value::from("phone-2"));
        let peers = load_peers().unwrap();
        assert_eq!(peers.len(), 3);
        assert_eq!(peers[1].role, PeerRole::Unknown("tablet".to_string()));
        assert!(
            !path.with_file_name(".sync_peers.json.tmp").exists(),
            "the temporary file is renamed away"
        );
    }

    /// A device cannot pair with itself or with an empty id.
    #[test]
    #[serial_test::serial]
    fn a_device_is_not_its_own_peer() {
        let _temp = with_temp_env();
        let own = crate::sync::device_id().unwrap();

        let err = record_peer(PeerRecord::new(own, PeerRole::Hub)).expect_err("refused");
        assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
        let err = record_peer(PeerRecord::new(" ", PeerRole::Hub)).expect_err("refused");
        assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
        assert!(!is_paired().unwrap());
    }

    /// Pairing a device again keeps the reset its record still requires and
    /// the reset it last finished.
    #[test]
    #[serial_test::serial]
    fn pairing_again_keeps_the_reset_fields() {
        let _temp = with_temp_env();
        let mut record = PeerRecord::new("phone-1", PeerRole::Phone);
        let required = RequiredReset::first_pairing();
        record.reset_required = Some(required.clone());
        record.completed_reset = Some("done-1".to_string());
        record_peer(record).unwrap();

        record_peer(PeerRecord::new("phone-1", PeerRole::Phone)).unwrap();

        let peer = find_peer("phone-1").unwrap().unwrap();
        assert_eq!(peer.reset_required, Some(required));
        assert_eq!(peer.completed_reset.as_deref(), Some("done-1"));
    }

    /// A peers file that cannot be read makes every pairing fail with a
    /// message that names the file, says how to recover, and carries no path.
    #[test]
    #[serial_test::serial]
    fn an_unreadable_peers_file_says_how_to_recover() {
        let temp = with_temp_env();
        std::fs::write(sync_peers_path().unwrap(), b"{not json").unwrap();

        let err = record_peer(PeerRecord::new("hub-1", PeerRole::Hub)).expect_err("fails");

        let Error::SyncRefused(message) = &err else {
            panic!("expected a refusal, got {err}");
        };
        assert!(message.contains(PEERS_FILE_NAME), "{message}");
        assert!(message.contains("pair this device again"), "{message}");
        let root = temp.path().to_string_lossy().into_owned();
        assert!(!message.contains(&root), "{message}");
        assert!(!err.peer_safe_message().contains(&root));
        assert!(!message.contains("  "), "{message}");
        assert!(is_paired().is_err());
    }
}

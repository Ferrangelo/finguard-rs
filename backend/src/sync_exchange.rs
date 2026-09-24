//! One sync round between the desktop hub and a phone, and the safety checks
//! around it, with no networking. [`crate::sync_service`] carries the
//! messages defined here over the encrypted connection of
//! [`crate::sync_net`] and calls the functions below at each step; it adds
//! no sync logic of its own.
//!
//! # The round
//!
//! Every step below names the function its side calls. Each message is a
//! [`SyncMessage`], turned into bytes with [`SyncMessage::encode`] and back
//! with [`SyncMessage::decode`].
//!
//! 1. The phone sends a hello: [`phone_hello`]. It carries the phone's device
//!    id, a summary of its log ([`LogSummary`]), and its log's health
//!    ([`LogHealth`]).
//! 2. The hub checks it and answers with its own hello: [`hub_answer_hello`].
//!    The phone checks the answer with [`phone_read_hello`]. Both sides get
//!    the same [`RoundPlan`] from the two hellos.
//! 3. For [`RoundPlan::Exchange`], the phone pushes the entries the hub
//!    lacks: [`phone_push`].
//! 4. The hub applies them with [`hub_apply_push`], then replies with the
//!    entries the phone lacks: [`hub_reply`].
//! 5. The phone applies those: [`phone_apply_reply`].
//!
//! When a log is unreliable the round takes a repair path instead; see
//! [`RoundPlan`]. The phone shows what it holds with [`phone_reset_preview`]
//! before the user confirms, and then resets from the hub's whole log
//! ([`hub_full_log`], [`reset_phone_from_hub`]). An unreliable hub first
//! merges the phone's push, then repairs itself with [`repair_hub_log`],
//! which stores on the hub that every paired phone must reset, so a dropped
//! connection cannot turn the next round into an ordinary exchange.
//! First pairing uses the same reset, guarded on both sides so the hub never
//! receives the data the phone held before: [`pair_with_hub`] marks the
//! phone as due for a reset, and [`pair_with_phone`] stores on the hub that
//! it takes no push from that phone until the phone proves the reset.
//!
//! A phone proves a reset with its id. The hub issues a fresh id with every
//! reset it requires, sends it only inside the [`FullLog`] the reset
//! applies, and lifts the requirement when a reliable phone's hello reports
//! that id as its last finished reset.
//!
//! # No sync position is stored
//!
//! What a device lacks is computed from the two logs at every round, never
//! from a cursor. A batch whose apply fails stores nothing, so the next
//! round's summary still lacks it and it is sent again: nothing advances on
//! a failure because there is nothing to advance. This relies on one
//! invariant, which [`entries_missing_from`] documents.
//!
//! # Messages carry row values
//!
//! A push, a reply, and a full log hold whole rows: amounts, names, and
//! categories. Never print or log one. [`EntryBatch`] and [`SyncMessage`]
//! implement `Debug` by hand, with counts only, so a stray `{:?}` cannot leak
//! a row. A hello holds device ids, stamps, and counts, and is safe to log.
//!
//! # Errors and the wire
//!
//! Every error and report this module builds names files, counts, device
//! ids, and row ids only, never a value, a name, an amount, or a category.
//! Some errors from lower layers still carry an absolute path, which holds
//! the account name: [`Error::SyncLogLocked`], [`Error::RowIdMigration`], and
//! [`Error::SyncResetBackup`] from a reset. Others can quote a value:
//! [`Error::Polars`] and [`Error::Json`]. Part 3b must send the other device
//! [`Error::peer_safe_message`], never `to_string()`. The reports here are
//! for this device's own screen and are never sent; [`ResetReport`] names its
//! backup folder by name only all the same.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::dbs_backup;
use crate::error::{Error, Result};
use crate::merge_apply::{self, MergeReport};
use crate::paths::{get_dbs_root, get_sync_dir};
use crate::sync::{self, ChangeEntry, ChangeTable, Hlc, IncompleteReason, LogRead};
use crate::sync_baseline::{self, MarkerState};
use crate::sync_peers::{self, PeerRecord, PeerRole, RequiredReset};
use crate::write_lock;

/// The version of the messages in this module. Every message carries it, and
/// a device refuses a message with any other value. Raise it whenever a
/// message changes shape or meaning.
pub const PROTOCOL_VERSION: u32 = 2;

/// Suffix of the backup folder a phone reset writes, after the UTC
/// timestamp.
const RESET_BACKUP_SUFFIX: &str = "before-sync-reset";

// ------------------------------------------------------------------
// What a device holds
// ------------------------------------------------------------------

/// What a log holds, per device: the largest stamp this log holds from each
/// device that wrote any of its entries. Ordered by device id, so the same
/// log always gives the same summary.
pub type LogSummary = BTreeMap<String, Hlc>;

/// Summarize `entries`, a whole log, as the largest stamp per device.
pub fn summarize(entries: &[ChangeEntry]) -> LogSummary {
    let mut summary = LogSummary::new();
    for entry in entries {
        summary
            .entry(entry.device_id.clone())
            .and_modify(|stamp| *stamp = (*stamp).max(entry.stamp))
            .or_insert(entry.stamp);
    }
    summary
}

/// Every entry of `entries`, a whole local log, that a peer with `peer`'s
/// summary lacks, in log order: an entry whose device the summary does not
/// name, or whose stamp is larger than the summary's stamp for its device.
///
/// This is exact only under one invariant: **a log holds each device's
/// entries as a prefix of that device's history, in stamp order.** Then a
/// peer that holds stamp `s` from a device holds every earlier entry of that
/// device too, and one stamp per device says everything. The invariant holds
/// because of how entries get into a log:
///
/// - Local appends are monotonic: [`crate::sync::Hlc::next`] only moves
///   forwards.
/// - A remote batch is stored with
///   [`crate::sync::ChangeLog::append_remote_batch`] in
///   [`crate::sync::ChangeEntry::order_key`] order, and a batch is itself a
///   suffix per device, because this function builds it.
/// - A failed apply stores nothing, and a write that fails part way stores a
///   prefix of the batch.
///
/// A damaged line breaks it: the entry on that line is gone, and a summary
/// built from the lines around it claims a history with a hole in it. The
/// peer then never receives the lost entry. That is one reason an unreliable
/// log is repaired instead of synced; see [`log_health`].
pub fn entries_missing_from(entries: &[ChangeEntry], peer: &LogSummary) -> Vec<ChangeEntry> {
    entries
        .iter()
        .filter(|entry| {
            peer.get(&entry.device_id)
                .is_none_or(|held| entry.stamp > *held)
        })
        .cloned()
        .collect()
}

// ------------------------------------------------------------------
// Log health
// ------------------------------------------------------------------

/// One reason the local log does not describe this device's data, so sync
/// must not trust it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "problem", rename_all = "snake_case")]
pub enum LogProblem {
    /// Lines [`crate::sync::read_log`] could not read. Each one is a lost
    /// change. An unfinished final line is not counted here: see
    /// [`LogProblem::UnfinishedAppend`].
    DamagedLines {
        /// Every damaged line, including the oversized ones.
        corrupt: usize,
        /// The damaged lines that were too long to read at all.
        oversized: usize,
    },
    /// The log is marked incomplete ([`crate::sync::log_incomplete_path`]),
    /// for these reasons.
    Incomplete {
        /// Why, one entry per reason the marker names.
        reasons: Vec<IncompleteReason>,
    },
    /// The incomplete marker exists and cannot be read, so it may name any
    /// reason.
    IncompleteMarkerUnreadable,
    /// The log ends in an append that did not finish. The data layer
    /// appends only after its Parquet save succeeded, so that change reached
    /// the data and not the log. The next append turns the line into a
    /// damaged one, which is just as unreliable.
    UnfinishedAppend,
    /// There is no baseline marker, so the first recording of this data
    /// folder never finished.
    BaselineMissing,
    /// The baseline marker cannot be read.
    BaselineUnreadable,
    /// The baseline marker says rows were recorded and the log holds
    /// nothing: the log was deleted or replaced.
    LogGone,
}

impl fmt::Display for LogProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogProblem::DamagedLines { corrupt, oversized } => write!(
                f,
                "{corrupt} line(s) of the change log cannot be read ({oversized} of them too \
                 long), so the changes they held are lost"
            ),
            LogProblem::Incomplete { reasons } => {
                write!(f, "the change log is marked incomplete (")?;
                for (index, reason) in reasons.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", String::from(reason.clone()))?;
                }
                write!(f, ")")
            }
            LogProblem::UnfinishedAppend => write!(
                f,
                "the last change saved on this device did not finish reaching the change log"
            ),
            LogProblem::IncompleteMarkerUnreadable => write!(
                f,
                "the change log is marked incomplete, and the marker cannot be read"
            ),
            LogProblem::BaselineMissing => write!(
                f,
                "the first recording of this device's data never finished"
            ),
            LogProblem::BaselineUnreadable => write!(
                f,
                "the record of the first recording of this device's data cannot be read"
            ),
            LogProblem::LogGone => write!(
                f,
                "the change log is empty although this device recorded changes before"
            ),
        }
    }
}

/// Whether the local log can be trusted to describe this device's data, and
/// why not when it cannot. Holds counts and fixed words only, so it is safe
/// to log and to send to the other device, which it is, inside a
/// [`Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LogHealth {
    /// Every reason the log is unreliable. Empty for a reliable log.
    pub problems: Vec<LogProblem>,
    /// From [`crate::sync::LogRead::clock_ahead_of_wall_by`]: how far the
    /// newest stamp runs ahead of this device's wall clock, when that is
    /// more than a day. Reported, and on its own no reason to distrust the
    /// log: every entry is still there, only stamped late.
    pub clock_ahead_of_wall_by: Option<i64>,
}

impl LogHealth {
    /// Whether sync can trust this log: no problem was found.
    pub fn is_reliable(&self) -> bool {
        self.problems.is_empty()
    }

    /// Whether this device may be due for a reset from the hub, which
    /// [`pair_with_hub`] and a started [`reset_phone_from_hub`] mark. Such a
    /// device sends nothing: its data either predates the pairing, which the
    /// hub must never receive, or is half replaced.
    ///
    /// A marker that cannot be read, or names a reason this version does not
    /// know, counts as due too: either may hide a pending reset, and taking
    /// the device's data on a guess is the one mistake that cannot be undone.
    pub fn reset_pending(&self) -> bool {
        self.problems.iter().any(|problem| match problem {
            LogProblem::Incomplete { reasons } => reasons.iter().any(|reason| {
                matches!(
                    reason,
                    IncompleteReason::ResetPending | IncompleteReason::Unknown
                )
            }),
            LogProblem::IncompleteMarkerUnreadable => true,
            _ => false,
        })
    }
}

impl fmt::Display for LogHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.problems.is_empty() {
            write!(f, "The change log is reliable.")?;
        } else {
            write!(f, "The change log is not reliable: ")?;
            for (index, problem) in self.problems.iter().enumerate() {
                if index > 0 {
                    write!(f, "; ")?;
                }
                write!(f, "{problem}")?;
            }
            write!(f, ".")?;
        }
        if let Some(ahead) = self.clock_ahead_of_wall_by {
            write!(
                f,
                " Its newest change is dated about {} hours ahead of this device's clock.",
                ahead / (60 * 60 * 1000)
            )?;
        }
        Ok(())
    }
}

/// Judge whether the local log is reliable. Reads the log, the incomplete
/// marker, and the baseline marker; writes nothing.
///
/// # Errors
///
/// [`Error::Io`] when the log exists and cannot be read, or the sync folder
/// cannot be created. A marker that cannot be read is a problem in the
/// result, not an error.
pub fn log_health() -> Result<LogHealth> {
    health_of(&sync::read_log()?)
}

/// [`log_health`] for a log already read.
fn health_of(read: &LogRead) -> Result<LogHealth> {
    let mut problems = Vec::new();
    if !read.corrupt_lines.is_empty() {
        problems.push(LogProblem::DamagedLines {
            corrupt: read.corrupt_lines.len(),
            oversized: read.oversized_lines.len(),
        });
    }
    if read.unfinished_tail {
        problems.push(LogProblem::UnfinishedAppend);
    }
    match sync::read_log_incomplete() {
        Ok(Some(reasons)) => problems.push(LogProblem::Incomplete { reasons }),
        Ok(None) => {}
        Err(_) => problems.push(LogProblem::IncompleteMarkerUnreadable),
    }
    match sync_baseline::marker_state(read)? {
        MarkerState::Present => {}
        MarkerState::Missing => problems.push(LogProblem::BaselineMissing),
        MarkerState::Unreadable => problems.push(LogProblem::BaselineUnreadable),
        MarkerState::DescribesGoneLog => problems.push(LogProblem::LogGone),
    }
    Ok(LogHealth {
        problems,
        clock_ahead_of_wall_by: read.clock_ahead_of_wall_by,
    })
}

// ------------------------------------------------------------------
// Messages
// ------------------------------------------------------------------

/// The first message of a round, from each side: who is speaking, what its
/// log holds, and whether that log is reliable. Holds device ids, stamps,
/// and counts only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// [`PROTOCOL_VERSION`] of the sender.
    pub version: u32,
    /// The sender's device id.
    pub device_id: String,
    /// What the sender is.
    pub role: PeerRole,
    /// What the sender's log holds.
    pub summary: LogSummary,
    /// Whether the sender's log is reliable.
    pub health: LogHealth,
    /// Set only in the hub's answer: the hub requires this phone to reset
    /// before any ordinary exchange, because the phone was just paired or
    /// the hub repaired its log since. See [`PeerRecord::reset_required`].
    #[serde(default)]
    pub phone_must_reset: bool,
    /// Set only in the hub's answer, with `phone_must_reset`: the reset
    /// comes from pairing, so the hub takes no push first.
    #[serde(default)]
    pub phone_first_reset: bool,
    /// Set only in a phone's hello: the id of the last reset from its hub
    /// that finished ([`PeerRecord::completed_reset`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reset: Option<String>,
}

/// Log entries in log order: a push, a reply, or a hub's whole log.
///
/// Holds row values. `Debug` prints the count only; never log the entries.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryBatch {
    /// [`PROTOCOL_VERSION`] of the sender.
    pub version: u32,
    /// The entries, verbatim.
    pub entries: Vec<ChangeEntry>,
}

impl fmt::Debug for EntryBatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "EntryBatch {{ version: {}, entries: {} }}",
            self.version,
            self.entries.len()
        )
    }
}

impl EntryBatch {
    fn new(entries: Vec<ChangeEntry>) -> Self {
        EntryBatch {
            version: PROTOCOL_VERSION,
            entries,
        }
    }
}

/// A hub's whole log, for a phone reset.
///
/// Holds row values. `Debug` prints the count only; never log the entries.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct FullLog {
    /// [`PROTOCOL_VERSION`] of the sender.
    pub version: u32,
    /// The hub that sent it. A phone resets only from a paired hub.
    pub hub_device_id: String,
    /// The id of the reset the hub requires of this phone, if it requires
    /// one. The phone stores it once the reset finishes and reports it in
    /// its next hello, which is how the hub learns the reset happened.
    pub reset_id: Option<String>,
    /// The hub's entries, verbatim, in log order.
    pub entries: Vec<ChangeEntry>,
}

impl fmt::Debug for FullLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FullLog {{ version: {}, hub_device_id: {}, reset_id: {:?}, entries: {} }}",
            self.version,
            self.hub_device_id,
            self.reset_id,
            self.entries.len()
        )
    }
}

/// One message of a round, as it travels. The `kind` field names the
/// variant and every variant carries `version` at the top level, so a
/// receiver can refuse an unknown version before it tries to read the rest.
///
/// Holds row values in every variant but [`SyncMessage::Hello`]. `Debug`
/// prints the kind and counts only.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncMessage {
    /// Step 1 and 2: who is speaking and what its log holds.
    Hello(Hello),
    /// Step 3, phone to hub: the entries the hub lacks.
    Push(EntryBatch),
    /// Step 4, hub to phone: the entries the phone lacks.
    Reply(EntryBatch),
    /// Hub to phone, for a reset: the hub's whole log.
    FullLog(FullLog),
}

impl fmt::Debug for SyncMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncMessage::Hello(hello) => write!(f, "Hello({hello:?})"),
            SyncMessage::Push(batch) => write!(f, "Push({batch:?})"),
            SyncMessage::Reply(batch) => write!(f, "Reply({batch:?})"),
            SyncMessage::FullLog(full) => write!(f, "FullLog({full:?})"),
        }
    }
}

/// Just the version of a message, read before anything else.
#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

impl SyncMessage {
    /// The message as JSON bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Json`] when a value cannot be serialized, which the types
    /// here do not produce.
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Read a message from bytes another device sent.
    ///
    /// The version is read and checked first, so a message from a newer
    /// version fails as a version mismatch even when its body has changed
    /// shape.
    ///
    /// # Errors
    ///
    /// [`Error::SyncProtocol`] when the bytes are not a message, carry no
    /// version, or carry a version other than [`PROTOCOL_VERSION`]. The
    /// message gives the JSON error's category and position only, because
    /// the text of a JSON error can quote the value it could not read.
    pub fn decode(bytes: &[u8]) -> Result<SyncMessage> {
        let probe: VersionProbe = serde_json::from_slice(bytes).map_err(|err| {
            Error::SyncProtocol(format!(
                "the message has no readable protocol version ({:?} error at line {}, column {})",
                err.classify(),
                err.line(),
                err.column()
            ))
        })?;
        check_version(probe.version)?;
        serde_json::from_slice(bytes).map_err(|err| {
            Error::SyncProtocol(format!(
                "the message is not a sync message this version can read ({:?} error at line {}, \
                 column {})",
                err.classify(),
                err.line(),
                err.column()
            ))
        })
    }

    fn kind(&self) -> &'static str {
        match self {
            SyncMessage::Hello(_) => "hello",
            SyncMessage::Push(_) => "push",
            SyncMessage::Reply(_) => "reply",
            SyncMessage::FullLog(_) => "full log",
        }
    }

    fn unexpected(&self, expected: &str) -> Error {
        Error::SyncProtocol(format!(
            "expected a {expected} message, got a {} message",
            self.kind()
        ))
    }

    /// The hello this message carries.
    ///
    /// # Errors
    ///
    /// [`Error::SyncProtocol`] for any other kind of message.
    pub fn into_hello(self) -> Result<Hello> {
        match self {
            SyncMessage::Hello(hello) => Ok(hello),
            other => Err(other.unexpected("hello")),
        }
    }

    /// The push this message carries.
    ///
    /// # Errors
    ///
    /// [`Error::SyncProtocol`] for any other kind of message.
    pub fn into_push(self) -> Result<EntryBatch> {
        match self {
            SyncMessage::Push(batch) => Ok(batch),
            other => Err(other.unexpected("push")),
        }
    }

    /// The reply this message carries.
    ///
    /// # Errors
    ///
    /// [`Error::SyncProtocol`] for any other kind of message.
    pub fn into_reply(self) -> Result<EntryBatch> {
        match self {
            SyncMessage::Reply(batch) => Ok(batch),
            other => Err(other.unexpected("reply")),
        }
    }

    /// The full log this message carries.
    ///
    /// # Errors
    ///
    /// [`Error::SyncProtocol`] for any other kind of message.
    pub fn into_full_log(self) -> Result<FullLog> {
        match self {
            SyncMessage::FullLog(full) => Ok(full),
            other => Err(other.unexpected("full log")),
        }
    }
}

/// Refuse any version but [`PROTOCOL_VERSION`].
fn check_version(version: u32) -> Result<()> {
    if version == PROTOCOL_VERSION {
        return Ok(());
    }
    Err(Error::SyncProtocol(format!(
        "the other device speaks sync protocol version {version}, and this device speaks version \
         {PROTOCOL_VERSION}. Install the same app version on both devices."
    )))
}

// ------------------------------------------------------------------
// Pairing
// ------------------------------------------------------------------

/// On a phone: record `hub_device_id` as this phone's hub, with its Noise
/// static key `hub_static_key` (64 hex digits) and the `address` the phone
/// reaches it at, and mark this phone as due for a reset from it.
///
/// The mark comes first. A phone that records the peer and then stops,
/// before its reset, must not be able to run an ordinary exchange later,
/// which would push the data it held before pairing to the hub. The user
/// decided the hub never receives that data. The reset clears the mark.
///
/// # Errors
///
/// [`Error::Io`] when the mark cannot be written, and the errors of
/// [`sync_peers::record_peer`]. On an error after the mark the phone is
/// marked and not paired, which a later pairing repeats safely.
pub fn pair_with_hub(hub_device_id: &str, hub_static_key: &str, address: &str) -> Result<()> {
    sync::mark_log_incomplete(IncompleteReason::ResetPending)?;
    let mut record = PeerRecord::new(hub_device_id, PeerRole::Hub);
    record.static_key = Some(hub_static_key.to_string());
    record.address = Some(address.to_string());
    sync_peers::record_peer(record)
}

/// On the hub: record `phone_device_id`, with its Noise static key
/// `phone_static_key` (64 hex digits), as a paired phone that must reset
/// from this hub before anything else.
///
/// The requirement ([`PeerRecord::reset_required`], marked as a first
/// pairing) is the hub's own guard against taking the phone's data from
/// before the pairing: until the phone proves the reset, the hub plans a
/// reset without a push and [`hub_apply_push`] refuses. It does not depend
/// on the phone's marker, which can be lost or unreadable. Pairing the same
/// phone again issues a new requirement.
///
/// # Errors
///
/// The errors of [`sync_peers::record_peer`].
pub fn pair_with_phone(phone_device_id: &str, phone_static_key: &str) -> Result<()> {
    let mut record = PeerRecord::new(phone_device_id, PeerRole::Phone);
    record.static_key = Some(phone_static_key.to_string());
    record.reset_required = Some(RequiredReset::first_pairing());
    sync_peers::record_peer(record)
}

/// Refuse `hello` unless its version is this one's, it comes from another
/// device, and that device is paired with this one in the `role` it claims.
/// Returns the stored record of that device.
fn check_hello(hello: &Hello, role: PeerRole, own_id: &str) -> Result<PeerRecord> {
    check_version(hello.version)?;
    let wanted = match role {
        PeerRole::Hub => "hub",
        PeerRole::Phone => "phone",
        PeerRole::Unknown(_) => "known device",
    };
    if hello.role != role {
        return Err(Error::SyncProtocol(format!(
            "device {} did not introduce itself as a {wanted}",
            hello.device_id
        )));
    }
    if hello.device_id == own_id {
        return Err(Error::SyncRefused(format!(
            "the other device claims this device's own id {own_id}; two devices must never \
             share an id"
        )));
    }
    match sync_peers::find_peer(&hello.device_id)? {
        Some(peer) if peer.role == role => Ok(peer),
        Some(_) => Err(Error::SyncRefused(format!(
            "device {} is paired with this device, but not as a {wanted}",
            hello.device_id
        ))),
        None => Err(Error::SyncRefused(format!(
            "device {} is not paired with this device",
            hello.device_id
        ))),
    }
}

// ------------------------------------------------------------------
// The round
// ------------------------------------------------------------------

/// Which way a round goes, decided from the two hellos. Both sides compute
/// the same plan: [`hub_answer_hello`] and [`phone_read_hello`] return it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundPlan {
    /// Both logs are reliable: push, apply, reply, apply.
    Exchange,
    /// The hub is reliable and the phone must reset: its log is unreliable,
    /// or the hub requires a reset (see [`PeerRecord::reset_required`]).
    /// When `push_first` is set, the phone pushes what it can after the user
    /// confirms and the hub applies it; then the phone resets from the
    /// hub's whole log.
    PhoneReset {
        /// Whether the phone pushes before its reset. Not set when the phone
        /// may be due for a reset already (see [`LogHealth::reset_pending`])
        /// or the hub requires the reset of a first pairing: the phone's
        /// data then predates pairing, or is half replaced.
        push_first: bool,
    },
    /// The hub's log is unreliable. When `push_first` is set, the phone
    /// pushes what it can after the user confirms and the hub applies it.
    /// Then the hub repairs itself ([`repair_hub_log`]) and the phone resets
    /// from the repaired log.
    HubRepair {
        /// As for [`RoundPlan::PhoneReset`].
        push_first: bool,
    },
}

/// The plan for a round between `phone` and `hub`.
pub fn plan_round(phone: &Hello, hub: &Hello) -> RoundPlan {
    let push_first = !phone.health.reset_pending() && !hub.phone_first_reset;
    if !hub.health.is_reliable() {
        RoundPlan::HubRepair { push_first }
    } else if !phone.health.is_reliable() || hub.phone_must_reset {
        RoundPlan::PhoneReset { push_first }
    } else {
        RoundPlan::Exchange
    }
}

/// This device's log and its health, read once.
fn local_state() -> Result<(LogRead, LogHealth)> {
    let read = sync::read_log()?;
    let health = health_of(&read)?;
    Ok((read, health))
}

/// This device's hello in `role`. A phone's hello carries the id of the
/// last reset it finished from its hub.
fn local_hello(role: PeerRole) -> Result<Hello> {
    let (read, health) = local_state()?;
    let last_reset = match role {
        PeerRole::Phone => sync_peers::load_peers()?
            .into_iter()
            .find(|peer| peer.role == PeerRole::Hub)
            .and_then(|hub| hub.completed_reset),
        _ => None,
    };
    Ok(Hello {
        version: PROTOCOL_VERSION,
        device_id: sync::device_id()?,
        role,
        summary: summarize(&read.entries),
        health,
        phone_must_reset: false,
        phone_first_reset: false,
        last_reset,
    })
}

/// Step 1, on the phone: the hello that opens a round.
///
/// # Errors
///
/// [`Error::Io`] when the log cannot be read, the errors of
/// [`sync_peers::load_peers`], and those of [`crate::sync::device_id`].
pub fn phone_hello() -> Result<Hello> {
    local_hello(PeerRole::Phone)
}

/// Step 2, on the hub: check the phone's hello, and return the hub's own
/// hello to send back and the plan for this round.
///
/// When the hub requires a reset of this phone, the requirement is cleared
/// here once the phone's hello proves the reset: its log is reliable and it
/// reports the requirement's id as its last finished reset. Only a full log
/// issued after the requirement carries that id.
///
/// # Errors
///
/// [`Error::SyncProtocol`] for another protocol version or a sender that is
/// not a phone, [`Error::SyncRefused`] for a phone that is not paired with
/// this hub or claims the hub's own id, and the errors of [`phone_hello`].
pub fn hub_answer_hello(phone: &Hello) -> Result<(Hello, RoundPlan)> {
    let mut hub = local_hello(PeerRole::Hub)?;
    let peer = check_hello(phone, PeerRole::Phone, &hub.device_id)?;
    if let Some(required) = peer.reset_required {
        let last_reset = phone.last_reset.as_deref();
        let reset_done = phone.health.is_reliable() && last_reset == Some(required.id.as_str());
        if reset_done {
            clear_reset_required(&phone.device_id, &required.id)?;
        } else {
            hub.phone_must_reset = true;
            hub.phone_first_reset = required.first_pairing_pending(last_reset);
        }
    }
    let plan = plan_round(phone, &hub);
    Ok((hub, plan))
}

/// Clear the requirement `id` on `phone_device_id`'s record. A newer
/// requirement, set meanwhile, stays.
fn clear_reset_required(phone_device_id: &str, id: &str) -> Result<()> {
    sync_peers::update_peers(|peers| {
        let mut changed = false;
        for peer in peers.iter_mut() {
            if peer.device_id == phone_device_id
                && peer
                    .reset_required
                    .as_ref()
                    .is_some_and(|required| required.id == id)
            {
                peer.reset_required = None;
                changed = true;
            }
        }
        changed
    })
}

/// Step 2, on the phone: check the hub's answer to `own`, the hello this
/// phone sent, and return the plan for this round.
///
/// # Errors
///
/// As for [`hub_answer_hello`], with the roles swapped.
pub fn phone_read_hello(own: &Hello, hub: &Hello) -> Result<RoundPlan> {
    check_hello(hub, PeerRole::Hub, &own.device_id)?;
    Ok(plan_round(own, hub))
}

/// Step 3, on the phone: the entries the hub lacks, given its hello.
///
/// Runs on an unreliable log too, because a phone repair sends what it can
/// before the reset: the entries it can read are still the user's changes.
///
/// # Errors
///
/// [`Error::SyncRefused`] when this phone may be due for a reset (see
/// [`LogHealth::reset_pending`]) or the hub requires the reset of a first
/// pairing: the phone's data must not reach the hub. Also
/// [`Error::SyncProtocol`] for another protocol version, and [`Error::Io`]
/// when the log cannot be read.
pub fn phone_push(hub: &Hello) -> Result<EntryBatch> {
    check_version(hub.version)?;
    let (read, health) = local_state()?;
    if health.reset_pending() || hub.phone_first_reset {
        return Err(Error::SyncRefused(
            "this phone is due for a reset from the hub, so it sends nothing first".to_string(),
        ));
    }
    Ok(EntryBatch::new(entries_missing_from(
        &read.entries,
        &hub.summary,
    )))
}

/// Step 4, first half, on the hub: apply the phone's push with
/// [`merge_apply::apply_remote_batch`]. Runs on an unreliable hub too,
/// because a hub repair merges the phone's changes first.
///
/// Blocking: call it from a plain thread or from
/// [`tokio::task::spawn_blocking`], never inside an async task.
///
/// A failed apply stores no entry, so the next round's hello from the hub
/// still lacks them and the phone pushes them again. See the module docs.
///
/// # Errors
///
/// [`Error::SyncProtocol`] for another protocol version.
/// [`Error::SyncRefused`] when the phone is not paired with this hub, when
/// `phone`, the hello this round began with, says the phone may be due for
/// a reset, and while this hub's own record says the phone has not proven
/// the reset of its first pairing: a phone that claims a healthy log is
/// refused all the same. Otherwise the errors of
/// [`merge_apply::apply_remote_batch`].
pub fn hub_apply_push(phone: &Hello, push: &EntryBatch) -> Result<MergeReport> {
    check_version(phone.version)?;
    check_version(push.version)?;
    let peer = paired_phone(&phone.device_id)?;
    let first_pairing = peer
        .reset_required
        .as_ref()
        .is_some_and(|required| required.first_pairing_pending(phone.last_reset.as_deref()));
    if phone.health.reset_pending() || first_pairing {
        return Err(Error::SyncRefused(format!(
            "device {} is due for a reset from this hub, so its data is not taken",
            phone.device_id
        )));
    }
    merge_apply::apply_remote_batch(&push.entries)
}

/// The stored record of `device_id`, which must be a paired phone.
fn paired_phone(device_id: &str) -> Result<PeerRecord> {
    match sync_peers::find_peer(device_id)? {
        Some(peer) if peer.role == PeerRole::Phone => Ok(peer),
        _ => Err(Error::SyncRefused(format!(
            "device {device_id} is not paired with this device as a phone"
        ))),
    }
}

/// Step 4, second half, on the hub: the entries the phone lacks, given the
/// hello this round began with. Call it after [`hub_apply_push`] returned
/// `Ok`, so the phone's own entries are not sent back.
///
/// # Errors
///
/// [`Error::SyncRefused`] when either log is unreliable, or this hub
/// requires a reset of the phone: an unreliable hub log would hand the phone
/// a history with holes, and a phone in the other two cases resets instead.
/// Also [`Error::SyncProtocol`] for another protocol version, and
/// [`Error::Io`] when the log cannot be read.
pub fn hub_reply(phone: &Hello) -> Result<EntryBatch> {
    check_version(phone.version)?;
    if !phone.health.is_reliable() {
        return Err(Error::SyncRefused(format!(
            "device {} reports an unreliable change log, so it resets from this hub instead",
            phone.device_id
        )));
    }
    if paired_phone(&phone.device_id)?.reset_required.is_some() {
        return Err(Error::SyncRefused(format!(
            "this hub requires device {} to reset from it first",
            phone.device_id
        )));
    }
    let (read, health) = local_state()?;
    refuse_unreliable(&health, "this hub")?;
    Ok(EntryBatch::new(entries_missing_from(
        &read.entries,
        &phone.summary,
    )))
}

/// Step 5, on the phone: apply the hub's reply.
///
/// Blocking, like [`hub_apply_push`]. A failed apply stores nothing, so the
/// next round's hello still lacks those entries and the hub sends them
/// again.
///
/// # Errors
///
/// [`Error::SyncRefused`] when this phone's log is unreliable, because it
/// resets instead. [`Error::SyncProtocol`] for another protocol version, and
/// the errors of [`merge_apply::apply_remote_batch`].
pub fn phone_apply_reply(reply: &EntryBatch) -> Result<MergeReport> {
    check_version(reply.version)?;
    refuse_unreliable(&log_health()?, "this phone")?;
    merge_apply::apply_remote_batch(&reply.entries)
}

/// On the hub, for a reset of the phone that sent `phone`: the hub's whole
/// log, with the id of the reset the hub requires of that phone, if any.
///
/// # Errors
///
/// [`Error::SyncRefused`] when the phone is not paired with this hub, or the
/// hub's log is unreliable: repair it first with [`repair_hub_log`].
/// [`Error::SyncProtocol`] for another protocol version, and [`Error::Io`]
/// when the log cannot be read.
pub fn hub_full_log(phone: &Hello) -> Result<FullLog> {
    check_version(phone.version)?;
    let peer = paired_phone(&phone.device_id)?;
    let (read, health) = local_state()?;
    refuse_unreliable(&health, "this hub")?;
    Ok(FullLog {
        version: PROTOCOL_VERSION,
        hub_device_id: sync::device_id()?,
        reset_id: peer.reset_required.map(|required| required.id),
        entries: read.entries,
    })
}

fn refuse_unreliable(health: &LogHealth, who: &str) -> Result<()> {
    if health.is_reliable() {
        return Ok(());
    }
    Err(Error::SyncRefused(format!("{who} cannot sync: {health}")))
}

// ------------------------------------------------------------------
// Phone reset
// ------------------------------------------------------------------

/// What a phone holds before a reset deletes it, in counts. The phone shows
/// these before the user confirms. Holds no values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResetPreview {
    /// Rows per table, keyed by the table's name in the change log, such as
    /// `expenses` or `liquidity`, over every year. For `cashflow_income`,
    /// the income cells holding a value other than zero.
    pub rows_per_table: BTreeMap<String, usize>,
    /// Year folders in the data folder.
    pub year_folders: usize,
    /// Table files that could not be read, and so are not counted above.
    pub unreadable_files: usize,
    /// Local log entries the hub's summary lacks: changes the reset loses
    /// from the log. The backup still holds the data they produced.
    pub unsent_entries: usize,
    pub settings_entries: usize,
}

/// On the phone, before a reset: count what it holds, given the hub's hello.
/// Writes nothing.
///
/// # Errors
///
/// [`Error::SyncProtocol`] for another protocol version, and [`Error::Io`]
/// when the data folder or the log cannot be read.
pub fn phone_reset_preview(hub: &Hello) -> Result<ResetPreview> {
    check_version(hub.version)?;
    let counts = sync_baseline::count_data()?;
    let read = sync::read_log()?;
    Ok(ResetPreview {
        rows_per_table: counts.rows_per_table,
        year_folders: counts.year_folders,
        unreadable_files: counts.unreadable_files,
        unsent_entries: entries_missing_from(&read.entries, &hub.summary).len(),
        settings_entries: read
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.table,
                    ChangeTable::CategoryMappings
                        | ChangeTable::KnownCategories
                        | ChangeTable::CurrencySettings
                )
            })
            .count(),
    })
}

/// What a phone reset did. Holds counts and a folder name, no values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetReport {
    /// The name of the backup folder under `<data_home>/finguard/backups/`
    /// that holds the data as it was before the reset. The name only, never
    /// the absolute path.
    pub backup_folder: String,
    /// What applying the hub's log did.
    pub merge: MergeReport,
}

/// On the phone: replace all of its data and its log with the hub's, from
/// `full_log`, the hub's [`hub_full_log`]. This one flow covers first
/// pairing and the repair of an unreliable phone. Blocking, like
/// [`hub_apply_push`].
///
/// Refused unless `full_log` comes from a device this phone is paired with
/// as its hub. Then, holding the data write lock throughout, it:
///
/// 1. copies the whole `dbs` tree to
///    `<backups>/<UTC timestamp>-before-sync-reset/`, before anything else,
///    so a failed backup leaves the device exactly as it was;
/// 2. marks the log incomplete and removes the baseline marker, and flushes
///    the sync folder, all strictly and before any data goes, so from here
///    on a failure or a crash leaves the phone reporting unreliable, never
///    a healthy looking log over deleted data;
/// 3. removes the `dbs` tree's contents;
/// 4. empties the log through the open handle, keeping the clock;
/// 5. applies the hub's log with the same code a merge uses;
/// 6. writes the baseline marker, so the baseline never records this data
///    again, and records the full log's reset id on the hub's peer record,
///    which the next hello reports as proof of the reset;
/// 7. clears the incomplete marker.
///
/// The device id is untouched.
///
/// The clock stays at least where it was for as long as the process runs.
/// After a restart it recovers from the stored log, which can put it below a
/// stamp only a discarded entry held. Reissuing such a stamp is harmless:
/// the entry that held it was never sent, since the hub's summary lacked it
/// and it is counted in [`ResetPreview::unsent_entries`], so no device can
/// confuse the two changes.
///
/// A year folder that is a symbolic link loses the link, not the folder it
/// points to: that folder keeps the old data, and the backup holds a copy.
///
/// # Errors
///
/// [`Error::SyncProtocol`] for another protocol version and
/// [`Error::SyncRefused`] for a sender that is not this phone's hub, both
/// before anything changes. [`Error::SyncResetBackup`], naming the path,
/// when the backup fails; nothing else has changed then. [`Error::Io`] from
/// a marker, the folder flush, removing the data, or emptying the log, and
/// the errors of [`merge_apply::apply_remote_batch`]. After any error past
/// the backup the phone reports unreliable; run the reset again.
pub fn reset_phone_from_hub(full_log: &FullLog) -> Result<ResetReport> {
    check_version(full_log.version)?;
    for entry in &full_log.entries {
        sync::check_storable(entry)?;
    }
    match sync_peers::find_peer(&full_log.hub_device_id)? {
        Some(peer) if peer.role == PeerRole::Hub => {}
        _ => {
            return Err(Error::SyncRefused(format!(
                "device {} is not paired with this phone as its hub, so this phone does not \
                 reset from it",
                full_log.hub_device_id
            )));
        }
    }
    let guard = write_lock::lock_blocking();

    let dbs_root = get_dbs_root()?;
    let backup = dbs_backup::backup_dbs(&dbs_root, RESET_BACKUP_SUFFIX, "Sync reset").map_err(
        |failure| Error::SyncResetBackup {
            path: failure.path,
            source: Box::new(failure.source),
        },
    )?;

    sync::mark_log_incomplete(IncompleteReason::ResetPending)?;
    sync_baseline::remove_baseline_marker()?;
    crate::df_operations::sync_dir(&get_sync_dir()?)?;
    remove_contents(&dbs_root)?;

    let log = sync::shared_log()?;
    log.empty_for_reset()?;
    crate::config::reset_sync_settings()?;
    let merge = merge_apply::apply_remote_batch_holding(&guard, &full_log.entries)?;

    sync_baseline::write_marker_after_reset(merge.entries_stored)?;
    if let Some(reset_id) = &full_log.reset_id {
        sync_peers::update_peers(|peers| {
            for peer in peers.iter_mut() {
                if peer.device_id == full_log.hub_device_id {
                    peer.completed_reset = Some(reset_id.clone());
                }
            }
            true
        })?;
    }
    sync::clear_log_incomplete()?;
    Ok(ResetReport {
        backup_folder: backup
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        merge,
    })
}

/// Give every paired phone a fresh reset requirement, keeping the first
/// pairing's id where a phone has not proven that reset yet. A fresh id
/// cannot be proven by any reset that happened before this call.
fn require_phone_resets() -> Result<()> {
    sync_peers::update_peers(|peers| {
        let mut changed = false;
        for peer in peers.iter_mut().filter(|peer| peer.role == PeerRole::Phone) {
            peer.reset_required = Some(RequiredReset::after_repair(peer.reset_required.as_ref()));
            changed = true;
        }
        changed
    })
}

/// Remove everything inside `folder`, keeping the folder. A symbolic link is
/// removed as a link; the folder it points to is left alone.
fn remove_contents(folder: &std::path::Path) -> Result<()> {
    for entry in std::fs::read_dir(folder)? {
        let path = entry?.path();
        if std::fs::symlink_metadata(&path)?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    crate::df_operations::sync_dir(folder)?;
    Ok(())
}

// ------------------------------------------------------------------
// Hub repair
// ------------------------------------------------------------------

/// What a hub repair did, in counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubRepairReport {
    /// Log lines kept verbatim.
    pub lines_kept: usize,
    /// Log lines dropped because they could not be read.
    pub lines_dropped: usize,
    /// Rows the fresh baseline recorded.
    pub rows_recorded: usize,
    /// Cashflow income cells the fresh baseline recorded.
    pub income_cells_recorded: usize,
    /// Data files the fresh baseline could not read. Their rows are not in
    /// the log.
    pub files_skipped: usize,
    /// Always true: every phone must now reset from this hub, because the
    /// fresh baseline outranks everything they hold. The hub has stored this
    /// for each paired phone (see [`PeerRecord::reset_required`]); part 3b
    /// drives the reset.
    pub phone_must_reset: bool,
}

/// On the hub, after it applied the phone's push: make the hub's log
/// reliable again. Blocking, like [`hub_apply_push`].
///
/// Holding the data write lock throughout, it marks the log incomplete, then
/// rewrites the log to hold every readable entry, in order and verbatim,
/// without the damaged lines. Keeping those entries keeps the delete
/// history, so a generated recurring row the user deleted stays deleted
/// ([`crate::merge::generated_row_stays_deleted`]). It then records the
/// current data again as a fresh baseline, with stamps above everything in
/// the log, writes the baseline marker, stores a reset requirement for every
/// paired phone, and clears the incomplete marker.
///
/// The fresh baseline is the new truth: its stamps outrank every entry any
/// phone holds, so each phone must reset from this hub afterwards. A delete
/// the hub lost on a damaged line never reaches a phone that only exchanges,
/// and that phone would keep the row. So the requirement is stored, not only
/// reported: every paired phone's record gets a fresh
/// [`PeerRecord::reset_required`], and until that phone's hello reports that
/// requirement's id as a finished reset, [`hub_answer_hello`] plans
/// [`RoundPlan::PhoneReset`] and [`hub_reply`] refuses. A connection that
/// drops between the repair and the reset therefore cannot fall back to an
/// ordinary exchange, and a repair that recorded nothing still requires it.
///
/// # Errors
///
/// [`Error::SyncRefused`] when this device is paired with a hub, so it is a
/// phone: a phone repairs by resetting. [`Error::Io`] when a marker or the
/// log cannot be written, and the errors of the rewrite, the baseline, and
/// the peers file. After any error past the refusal the hub still reports
/// unreliable; run the repair again.
pub fn repair_hub_log() -> Result<HubRepairReport> {
    if sync_peers::load_peers()?
        .iter()
        .any(|peer| peer.role == PeerRole::Hub)
    {
        return Err(Error::SyncRefused(
            "this device is paired with a hub, so it resets from the hub instead of repairing \
             its own log"
                .to_string(),
        ));
    }
    let _guard = write_lock::lock_blocking();
    sync::mark_log_incomplete(IncompleteReason::RepairUnfinished)?;
    let log = sync::shared_log()?;
    let rewrite = log.rewrite_readable()?;
    let baseline = sync_baseline::record_fresh_baseline(&log)?;
    let settings = sync_baseline::record_current_settings(&log, None)?;
    sync_baseline::write_settings_marker_for_repair(&settings)?;
    // Before the incomplete marker goes, so a hub that stops in between
    // still reports unreliable and repairs again.
    require_phone_resets()?;
    sync::clear_log_incomplete()?;
    Ok(HubRepairReport {
        lines_kept: rewrite.kept,
        lines_dropped: rewrite.dropped,
        rows_recorded: baseline.rows_recorded,
        income_cells_recorded: baseline.income_cells_recorded,
        files_skipped: baseline.skipped_paths.len(),
        phone_must_reset: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use polars::prelude::*;

    use crate::df_operations::{
        Cashflow, DetailedExpenses, Liquidity, ROW_ID_COLUMN, RecurringExpenses,
    };
    use crate::paths::{LIQUIDITY_FILENAME, get_backups_dir, get_year_summary_path};
    use crate::sync::{ChangeOp, ChangeTable};
    use crate::sync_baseline::{baseline_change_log, baseline_marker_path};

    /// A static key and an address for pairings these tests make without a
    /// connection. No test here opens one, so neither is ever used.
    const TEST_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const TEST_ADDRESS: &str = "192.0.2.1:3112";

    /// Point `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `HOME` at three folders
    /// inside `dir`, and force offline FX mode, so no test reads or writes
    /// the real data or config or reaches the network. Two devices are two
    /// such folders, as in the tests of [`crate::merge_apply`].
    ///
    /// # Safety
    ///
    /// `std::env::set_var` is not thread-safe; callers hold
    /// `#[serial_test::serial]`.
    fn use_device(dir: &Path) {
        for name in ["data", "config", "home"] {
            std::fs::create_dir_all(dir.join(name)).expect("create a root");
        }
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.join("data"));
            std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
            std::env::set_var("HOME", dir.join("home"));
            std::env::set_var("FINGUARD_FX_OFFLINE", "1");
        }
    }

    /// Switch to `dir` and run the startup baseline, as the app does.
    fn start(dir: &Path) {
        use_device(dir);
        let report = baseline_change_log().expect("the startup baseline");
        assert!(report.needs_repair.is_none(), "{report}");
    }

    fn device_id_of(dir: &Path) -> String {
        use_device(dir);
        sync::device_id().unwrap()
    }

    fn log_entries() -> Vec<ChangeEntry> {
        sync::read_log().expect("read the change log").entries
    }

    fn local_summary() -> LogSummary {
        summarize(&log_entries())
    }

    fn append_to_log(bytes: &[u8]) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(sync::changelog_path().unwrap())
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    /// Carry a message the way part 3b will: as bytes, and back.
    fn wire(message: SyncMessage) -> SyncMessage {
        SyncMessage::decode(&message.encode().expect("encode")).expect("decode")
    }

    fn sorted(df: DataFrame, key_column: &str) -> DataFrame {
        df.sort([key_column], SortMultipleOptions::default())
            .unwrap()
    }

    /// The current device's data, in a form two devices can compare.
    fn snapshot() -> Vec<DataFrame> {
        vec![
            sorted(
                DetailedExpenses::new(2026, 3).unwrap().expense_df,
                ROW_ID_COLUMN,
            ),
            sorted(
                DetailedExpenses::new(2026, 4).unwrap().expense_df,
                ROW_ID_COLUMN,
            ),
            sorted(RecurringExpenses::new(2026).unwrap().df, ROW_ID_COLUMN),
            sorted(Liquidity::new(2026).unwrap().df, ROW_ID_COLUMN),
            Cashflow::new(2026).unwrap().df,
        ]
    }

    fn snapshot_of(dir: &Path) -> Vec<DataFrame> {
        use_device(dir);
        snapshot()
    }

    fn f64_cell(df: &DataFrame, column: &str) -> Option<f64> {
        df.column(column).unwrap().f64().unwrap().get(0)
    }

    /// A hub with some history: two March expenses, a recurring template, a
    /// liquidity account with one value, and an income cell.
    fn hub_with_history(hub: &Path) -> String {
        start(hub);
        let mut march = DetailedExpenses::new(2026, 3).unwrap();
        let rent = march
            .add_row("Rent", 1, 900.0, Some("Housing"), "EUR", Some("Rent"))
            .unwrap();
        march
            .add_row("Tea", 2, 3.0, Some("Out"), "EUR", Some("Cafe"))
            .unwrap();
        RecurringExpenses::new(2026)
            .unwrap()
            .add("Gym", 30.0, "EUR", "Health", "Gym")
            .unwrap();
        let mut liquidity = Liquidity::new(2026).unwrap();
        liquidity.add_asset("Bank", "Cash", "EUR").unwrap();
        liquidity.set_value("Bank", 1, 100.0).unwrap();
        Cashflow::new(2026)
            .unwrap()
            .set_income(1, "Salary", 2000.0)
            .unwrap();
        rent
    }

    /// Pair `hub` and `phone` and give the phone its first copy of the
    /// data, as first pairing does.
    fn pair_and_reset(hub: &Path, phone: &Path) {
        pair_both(hub, phone);
        reset_round(hub, phone, RoundPlan::PhoneReset { push_first: false });
    }

    #[test]
    #[serial_test::serial]
    fn reset_clears_phone_settings_before_applying_hub_settings() {
        let root = tempfile::tempdir().unwrap();
        let hub = root.path().join("hub");
        let phone = root.path().join("phone");
        start(&hub);
        start(&phone);
        use_device(&hub);
        crate::config::add_mapping("hub-only", "food", "cafe", true).unwrap();
        let mut row = serde_json::Map::new();
        row.insert("primary_category".into(), serde_json::Value::from("food"));
        row.insert("secondary_category".into(), serde_json::Value::from("cafe"));
        sync::shared_log()
            .unwrap()
            .append(
                ChangeTable::CategoryMappings,
                "hub-only",
                ChangeOp::Upsert { row },
            )
            .unwrap();
        use_device(&phone);
        crate::config::add_mapping("phone-only", "old", "old", true).unwrap();
        pair_and_reset(&hub, &phone);
        use_device(&phone);
        let mappings = crate::config::get_all_mappings().unwrap();
        assert!(mappings.contains_key("hub-only"));
        assert!(!mappings.contains_key("phone-only"));
    }

    #[test]
    #[serial_test::serial]
    fn paired_phone_receives_hub_settings_catchup_in_an_ordinary_exchange() {
        let root = tempfile::tempdir().unwrap();
        let hub = root.path().join("hub");
        let phone = root.path().join("phone");
        start(&hub);
        start(&phone);
        pair_and_reset(&hub, &phone);
        use_device(&hub);
        crate::config::add_mapping("upgrade-setting", "food", "other", true).unwrap();
        let mut row = serde_json::Map::new();
        row.insert("primary_category".into(), serde_json::Value::from("food"));
        row.insert(
            "secondary_category".into(),
            serde_json::Value::from("other"),
        );
        sync::shared_log()
            .unwrap()
            .append(
                ChangeTable::CategoryMappings,
                "upgrade-setting",
                ChangeOp::Upsert { row },
            )
            .unwrap();
        let round = exchange(&hub, &phone).unwrap();
        assert_eq!(round.replied, 1);
        use_device(&phone);
        assert!(
            crate::config::get_all_mappings()
                .unwrap()
                .contains_key("upgrade-setting")
        );
    }

    fn currency_repair_sequence(
        skip_currency_recording: bool,
    ) -> (crate::config::CurrencySettings, MergeReport) {
        let root = tempfile::tempdir().unwrap();
        let hub = root.path().join("hub");
        let phone = root.path().join("phone");
        start(&hub);
        start(&phone);
        pair_and_reset(&hub, &phone);
        exchange(&hub, &phone).expect("the initial ordinary exchange");

        use_device(&hub);
        let desired = crate::config::CurrencySettings {
            reference_currency: "USD".to_string(),
            current_month_rate_mode: crate::config::CurrentMonthRateMode::Live,
        };
        crate::config::set_currency_settings(&desired).unwrap();
        if skip_currency_recording {
            std::fs::write(
                crate::config::get_config_dir()
                    .unwrap()
                    .join("currency.json"),
                b"not json",
            )
            .unwrap();
        }
        crate::sync_baseline::settings_baseline_change_log().unwrap();
        assert_eq!(
            sync::read_log_incomplete().unwrap(),
            Some(vec![IncompleteReason::BaselineRefused])
        );
        reset_round(&hub, &phone, RoundPlan::HubRepair { push_first: true });
        use_device(&hub);
        crate::config::set_currency_settings(&desired).unwrap();
        for (field, value) in [
            ("reference_currency", serde_json::Value::from("USD")),
            ("current_month_rate_mode", serde_json::Value::from("live")),
        ] {
            sync::shared_log()
                .unwrap()
                .append(
                    ChangeTable::CurrencySettings,
                    field,
                    ChangeOp::Cell {
                        column: "value".to_string(),
                        value,
                    },
                )
                .unwrap();
        }

        let round = exchange(&hub, &phone).expect("the ordinary exchange after repair");
        assert!(round.replied > 0, "currency cells were not sent");
        use_device(&phone);
        (crate::config::get_currency_settings().unwrap(), round.phone)
    }

    #[test]
    #[serial_test::serial]
    fn skipped_currency_recording_is_repaired_by_cells() {
        let (settings, report) = currency_repair_sequence(true);

        assert_eq!(settings.reference_currency, "USD");
        assert_eq!(
            settings.current_month_rate_mode,
            crate::config::CurrentMonthRateMode::Live
        );
        assert_eq!(report.summary.unplaceable, 0);
    }

    #[test]
    #[serial_test::serial]
    fn recorded_currency_baseline_still_repairs_by_cells() {
        let (settings, report) = currency_repair_sequence(false);

        assert_eq!(settings.reference_currency, "USD");
        assert_eq!(
            settings.current_month_rate_mode,
            crate::config::CurrentMonthRateMode::Live
        );
        assert_eq!(report.summary.unplaceable, 0);
    }

    /// Pair `hub` and `phone` on both sides, without the first reset. Ends
    /// on the phone.
    fn pair_both(hub: &Path, phone: &Path) {
        let hub_id = device_id_of(hub);
        let phone_id = device_id_of(phone);
        use_device(hub);
        pair_with_phone(&phone_id, TEST_KEY).unwrap();
        use_device(phone);
        pair_with_hub(&hub_id, TEST_KEY, TEST_ADDRESS).unwrap();
    }

    /// A round that ends in a phone reset, following `expected`.
    fn reset_round(hub: &Path, phone: &Path, expected: RoundPlan) -> ResetReport {
        use_device(phone);
        let phone_hello = wire(SyncMessage::Hello(phone_hello().unwrap()))
            .into_hello()
            .unwrap();
        use_device(hub);
        let (hub_hello, plan) = hub_answer_hello(&phone_hello).unwrap();
        assert_eq!(plan, expected);
        let hub_hello = wire(SyncMessage::Hello(hub_hello)).into_hello().unwrap();
        use_device(phone);
        assert_eq!(phone_read_hello(&phone_hello, &hub_hello).unwrap(), plan);
        let push_first = match plan {
            RoundPlan::PhoneReset { push_first } | RoundPlan::HubRepair { push_first } => {
                push_first
            }
            RoundPlan::Exchange => panic!("not a reset round"),
        };
        if push_first {
            let push = wire(SyncMessage::Push(phone_push(&hub_hello).unwrap()))
                .into_push()
                .unwrap();
            use_device(hub);
            hub_apply_push(&phone_hello, &push).unwrap();
        }
        if matches!(plan, RoundPlan::HubRepair { .. }) {
            use_device(hub);
            repair_hub_log().unwrap();
        }
        use_device(hub);
        let full = wire(SyncMessage::FullLog(hub_full_log(&phone_hello).unwrap()))
            .into_full_log()
            .unwrap();
        use_device(phone);
        reset_phone_from_hub(&full).unwrap()
    }

    /// What one ordinary round moved.
    struct RoundCounts {
        pushed: usize,
        replied: usize,
        hub: MergeReport,
        phone: MergeReport,
    }

    /// One ordinary round, every message through [`wire`].
    fn exchange(hub: &Path, phone: &Path) -> Result<RoundCounts> {
        use_device(phone);
        let phone_hello = wire(SyncMessage::Hello(phone_hello()?)).into_hello()?;
        use_device(hub);
        let (hub_hello, plan) = hub_answer_hello(&phone_hello)?;
        assert_eq!(plan, RoundPlan::Exchange);
        let hub_hello = wire(SyncMessage::Hello(hub_hello)).into_hello()?;
        use_device(phone);
        assert_eq!(phone_read_hello(&phone_hello, &hub_hello)?, plan);
        let push = wire(SyncMessage::Push(phone_push(&hub_hello)?)).into_push()?;
        use_device(hub);
        let hub_report = hub_apply_push(&phone_hello, &push)?;
        let reply = wire(SyncMessage::Reply(hub_reply(&phone_hello)?)).into_reply()?;
        use_device(phone);
        let phone_report = phone_apply_reply(&reply)?;
        Ok(RoundCounts {
            pushed: push.entries.len(),
            replied: reply.entries.len(),
            hub: hub_report,
            phone: phone_report,
        })
    }

    /// The summary names each device's largest stamp, and an entry is
    /// missing from a peer when its device is unknown there or its stamp is
    /// larger.
    #[test]
    fn a_summary_and_what_a_peer_lacks() {
        let entry = |device: &str, ms: i64| ChangeEntry {
            stamp: Hlc { ms, counter: 0 },
            device_id: device.to_string(),
            table: ChangeTable::Recurring { year: 2026 },
            row_id: format!("{device}-{ms}"),
            origin: None,
            op: sync::ChangeOp::Delete,
        };
        let log = vec![entry("a", 1), entry("b", 5), entry("a", 3), entry("c", 2)];

        let summary = summarize(&log);
        assert_eq!(summary.len(), 3);
        assert_eq!(summary["a"], Hlc { ms: 3, counter: 0 });

        let peer = LogSummary::from([
            ("a".to_string(), Hlc { ms: 1, counter: 0 }),
            ("b".to_string(), Hlc { ms: 5, counter: 0 }),
        ]);
        let missing: Vec<String> = entries_missing_from(&log, &peer)
            .into_iter()
            .map(|entry| entry.row_id)
            .collect();
        assert_eq!(missing, vec!["a-3", "c-2"], "log order, nothing held");
        assert!(entries_missing_from(&log, &summary).is_empty());
    }

    /// A full round both ways leaves both devices with equal data and equal
    /// summaries, and a second round moves nothing.
    #[test]
    #[serial_test::serial]
    fn a_round_both_ways_leaves_equal_data_and_a_second_moves_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        let rent = hub_with_history(&hub);
        start(&phone);
        pair_and_reset(&hub, &phone);
        assert_eq!(snapshot_of(&phone), snapshot_of(&hub));

        // They drift apart.
        use_device(&hub);
        DetailedExpenses::new(2026, 3)
            .unwrap()
            .edit_row(&rent, None, None, Some(950.0), None, None, None)
            .unwrap();
        Liquidity::new(2026)
            .unwrap()
            .set_value("Bank", 3, 300.0)
            .unwrap();
        use_device(&phone);
        Liquidity::new(2026)
            .unwrap()
            .set_value("Bank", 5, 500.0)
            .unwrap();
        DetailedExpenses::new(2026, 4)
            .unwrap()
            .add_row("Book", 3, 12.0, Some("Culture"), "EUR", Some("Books"))
            .unwrap();
        Cashflow::new(2026)
            .unwrap()
            .set_income(2, "Other", 40.0)
            .unwrap();

        let first = exchange(&hub, &phone).expect("the first round");

        assert!(first.pushed > 0 && first.replied > 0);
        assert_eq!(first.hub.entries_stored, first.pushed);
        assert_eq!(first.phone.entries_stored, first.replied);
        let hub_data = snapshot_of(&hub);
        let hub_summary = local_summary();
        let phone_data = snapshot_of(&phone);
        assert_eq!(phone_data, hub_data);
        assert_eq!(local_summary(), hub_summary, "equal summaries");
        let liquidity = &phone_data[3];
        assert_eq!(f64_cell(liquidity, "03"), Some(300.0));
        assert_eq!(f64_cell(liquidity, "05"), Some(500.0));
        assert_eq!(
            phone_data[1].height(),
            1,
            "the phone's April row reached the hub"
        );

        let second = exchange(&hub, &phone).expect("the second round");

        assert_eq!(second.pushed, 0);
        assert_eq!(second.replied, 0);
        assert_eq!(second.hub.entries_stored + second.hub.files_written, 0);
        assert_eq!(second.phone.entries_stored + second.phone.files_written, 0);
        assert_eq!(snapshot_of(&phone), hub_data);
    }

    /// A push whose apply fails stores nothing on the hub, so the hub's next
    /// hello still lacks those entries and the phone sends the same ones
    /// again. Once the obstacle is gone the round completes.
    #[test]
    #[serial_test::serial]
    fn a_failed_apply_advances_nothing_and_is_sent_again() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        start(&phone);
        pair_and_reset(&hub, &phone);
        let phone_id = device_id_of(&phone);
        Liquidity::new(2026)
            .unwrap()
            .set_value("Bank", 5, 500.0)
            .unwrap();

        // A folder where the liquidity write puts its temporary file makes
        // the hub's apply fail.
        use_device(&hub);
        let hub_before = local_summary();
        let obstacle = get_year_summary_path(2026, LIQUIDITY_FILENAME)
            .unwrap()
            .with_file_name(format!(".{LIQUIDITY_FILENAME}.tmp"));
        std::fs::create_dir(&obstacle).unwrap();

        use_device(&phone);
        let hello = phone_hello().unwrap();
        use_device(&hub);
        let (hub_hello, _) = hub_answer_hello(&hello).unwrap();
        use_device(&phone);
        let push = phone_push(&hub_hello).unwrap();
        assert_eq!(push.entries.len(), 1);
        use_device(&hub);
        let err = hub_apply_push(&hello, &push).expect_err("the hub's write fails");

        assert!(matches!(err, Error::Io(_)), "{err}");
        assert_eq!(local_summary(), hub_before, "the hub advanced nothing");
        assert!(!local_summary().contains_key(&phone_id));

        use_device(&phone);
        let hello = phone_hello().unwrap();
        use_device(&hub);
        let (hub_hello, _) = hub_answer_hello(&hello).unwrap();
        use_device(&phone);
        assert_eq!(
            phone_push(&hub_hello).unwrap(),
            push,
            "the next round sends the same entries"
        );

        use_device(&hub);
        std::fs::remove_dir(&obstacle).unwrap();
        let round = exchange(&hub, &phone).expect("the round completes");
        assert_eq!(round.hub.entries_stored, 1);
        assert_eq!(snapshot_of(&hub), snapshot_of(&phone));
    }

    /// A fresh device whose startup baseline ran, for the health checks.
    fn healthy_device(root: &Path, name: &str) {
        start(&root.join(name));
        let health = log_health().unwrap();
        assert!(health.is_reliable(), "{health}");
    }

    /// Each cause makes the log unreliable on its own, and the health check
    /// names that cause and nothing else.
    #[test]
    #[serial_test::serial]
    fn each_health_cause_is_reported_on_its_own() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();

        healthy_device(root, "damaged");
        Liquidity::new(2026)
            .unwrap()
            .add_asset("Bank", "Cash", "EUR")
            .unwrap();
        append_to_log(b"{\"stamp\": not json\n");
        assert_eq!(
            log_health().unwrap().problems,
            vec![LogProblem::DamagedLines {
                corrupt: 1,
                oversized: 0
            }]
        );

        healthy_device(root, "unfinished");
        Liquidity::new(2026)
            .unwrap()
            .add_asset("Bank", "Cash", "EUR")
            .unwrap();
        append_to_log(b"{\"stamp\":{\"ms\":1,");
        assert_eq!(
            log_health().unwrap().problems,
            vec![LogProblem::UnfinishedAppend]
        );

        healthy_device(root, "incomplete");
        sync::mark_log_incomplete(IncompleteReason::AppendFailed).unwrap();
        assert_eq!(
            log_health().unwrap().problems,
            vec![LogProblem::Incomplete {
                reasons: vec![IncompleteReason::AppendFailed]
            }]
        );

        healthy_device(root, "no-marker");
        std::fs::remove_file(baseline_marker_path().unwrap()).unwrap();
        assert_eq!(
            log_health().unwrap().problems,
            vec![LogProblem::BaselineMissing]
        );

        healthy_device(root, "bad-marker");
        std::fs::write(baseline_marker_path().unwrap(), b"{\"rows_recorded\":").unwrap();
        assert_eq!(
            log_health().unwrap().problems,
            vec![LogProblem::BaselineUnreadable]
        );

        // Rows recorded before the baseline ran give the marker a non-zero
        // count, so an emptied log afterwards reads as a log that is gone.
        use_device(&root.join("gone"));
        Liquidity::new(2026)
            .unwrap()
            .add_asset("Bank", "Cash", "EUR")
            .unwrap();
        baseline_change_log().unwrap();
        assert!(log_health().unwrap().is_reliable());
        std::fs::write(sync::changelog_path().unwrap(), b"").unwrap();
        assert_eq!(log_health().unwrap().problems, vec![LogProblem::LogGone]);

        // A clock far ahead is reported and does not make the log
        // unreliable.
        healthy_device(root, "ahead");
        let ahead = ChangeEntry {
            stamp: Hlc {
                ms: 4_000_000_000_000,
                counter: 0,
            },
            device_id: "other".to_string(),
            table: ChangeTable::Recurring { year: 2026 },
            row_id: "row".to_string(),
            origin: None,
            op: sync::ChangeOp::Delete,
        };
        sync::shared_log()
            .unwrap()
            .append_remote_batch(&[&ahead])
            .unwrap();
        let health = log_health().unwrap();
        assert!(health.is_reliable(), "{health}");
        assert!(health.clock_ahead_of_wall_by.is_some());
    }

    /// A save whose record in the log fails marks the log incomplete, and
    /// the save itself still succeeds.
    #[test]
    #[serial_test::serial]
    fn a_failed_append_marks_the_log_incomplete() {
        let root = tempfile::tempdir().unwrap();
        healthy_device(root.path(), "device");
        let mut liquidity = Liquidity::new(2026).unwrap();
        liquidity.add_asset("Bank", "Cash", "EUR").unwrap();
        let entries_before = log_entries().len();

        // The log cannot carry NaN, so the save succeeds and its record
        // fails.
        liquidity
            .set_value("Bank", 1, f64::NAN)
            .expect("the save itself succeeds");

        assert_eq!(log_entries().len(), entries_before, "nothing was recorded");
        assert_eq!(
            sync::read_log_incomplete().unwrap(),
            Some(vec![IncompleteReason::AppendFailed])
        );
        assert!(!log_health().unwrap().is_reliable());
    }

    /// A phone that held data before its first pairing reports it in counts,
    /// sends none of it, and resets: the backup holds its old data, its data
    /// equals the hub's, its clock does not move back, and its log handle
    /// keeps appending to the log file.
    #[test]
    #[serial_test::serial]
    fn a_phone_with_data_resets_from_the_hub() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        let hub_id = device_id_of(&hub);

        // The phone's own data, before pairing, with a clock pushed far
        // ahead so a clock that moved back would show.
        start(&phone);
        let phone_id = sync::device_id().unwrap();
        sync::shared_log().unwrap().observe_remote(Hlc {
            ms: 4_000_000_000_000,
            counter: 0,
        });
        DetailedExpenses::new(2026, 3)
            .unwrap()
            .add_row("Lunch", 4, 11.0, Some("Food"), "EUR", Some("Out"))
            .unwrap();
        let mut liquidity = Liquidity::new(2026).unwrap();
        liquidity.add_asset("Wallet", "Cash", "EUR").unwrap();
        liquidity.set_value("Wallet", 2, 20.0).unwrap();
        crate::config::add_mapping("phone-setting", "food", "other", true).unwrap();
        let mut setting_row = serde_json::Map::new();
        setting_row.insert("primary_category".into(), serde_json::Value::from("food"));
        setting_row.insert(
            "secondary_category".into(),
            serde_json::Value::from("other"),
        );
        sync::shared_log()
            .unwrap()
            .append(
                ChangeTable::CategoryMappings,
                "phone-setting",
                ChangeOp::Upsert { row: setting_row },
            )
            .unwrap();
        let clock_before = log_entries().last().unwrap().stamp;
        let phone_entries = log_entries().len();

        use_device(&hub);
        pair_with_phone(&phone_id, TEST_KEY).unwrap();
        use_device(&phone);
        pair_with_hub(&hub_id, TEST_KEY, TEST_ADDRESS).unwrap();
        let hello = phone_hello().unwrap();
        assert!(hello.health.reset_pending());
        use_device(&hub);
        let (hub_hello, plan) = hub_answer_hello(&hello).unwrap();
        assert_eq!(plan, RoundPlan::PhoneReset { push_first: false });
        assert!(matches!(
            hub_apply_push(&hello, &EntryBatch::new(Vec::new())),
            Err(Error::SyncRefused(_))
        ));
        use_device(&phone);
        assert!(matches!(phone_push(&hub_hello), Err(Error::SyncRefused(_))));

        let preview = phone_reset_preview(&hub_hello).unwrap();
        assert_eq!(preview.rows_per_table.get("expenses"), Some(&1));
        assert_eq!(preview.rows_per_table.get("liquidity"), Some(&1));
        assert_eq!(preview.year_folders, 1);
        assert_eq!(preview.unreadable_files, 0);
        assert_eq!(preview.unsent_entries, phone_entries);
        assert_eq!(preview.settings_entries, 1);
        let log_path = sync::changelog_path().unwrap();

        use_device(&hub);
        let full = hub_full_log(&hello).unwrap();
        use_device(&phone);
        let report = reset_phone_from_hub(&full).unwrap();

        assert_eq!(report.merge.entries_stored, full.entries.len());
        assert!(report.backup_folder.ends_with("before-sync-reset"));
        let backup = get_backups_dir().unwrap().join(&report.backup_folder);
        assert!(
            backup
                .join("2026")
                .join("03_detailed_expenses.parquet")
                .exists()
        );
        assert!(backup.join("2026").join(LIQUIDITY_FILENAME).exists());
        let hub_data = snapshot_of(&hub);
        assert_eq!(
            snapshot_of(&phone),
            hub_data,
            "the phone holds the hub's data"
        );
        assert_eq!(sync::device_id().unwrap(), phone_id, "the id is kept");
        assert!(sync_peers::find_peer(&hub_id).unwrap().is_some());
        let health = log_health().unwrap();
        assert!(health.is_reliable(), "{health}");

        Liquidity::new(2026)
            .unwrap()
            .set_value("Bank", 6, 60.0)
            .unwrap();
        let entries = sync::read_log().unwrap().entries;
        assert_eq!(
            entries.len(),
            full.entries.len() + 1,
            "the new change reached the log file at {}",
            log_path.display()
        );
        let newest = entries.last().unwrap();
        assert_eq!(newest.device_id, phone_id);
        assert!(newest.stamp > clock_before, "the clock did not move back");

        // The phone is an ordinary peer now.
        let round = exchange(&hub, &phone).expect("an ordinary round");
        assert_eq!(round.pushed, 1);
        assert_eq!(snapshot_of(&hub), snapshot_of(&phone));
    }

    /// A hub whose log has a damaged line merges the phone's push, repairs
    /// itself, and the phone resets from it. The damaged line is gone, a
    /// deleted generated row stays deleted, the fresh baseline's stamps are
    /// above every older one, and the two devices end equal. A dropped
    /// connection between the repair and the reset still ends in a reset.
    #[test]
    #[serial_test::serial]
    fn a_hub_repair_after_a_damaged_line_ends_equal() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        // Generate March's gym row, then delete it.
        let rec = RecurringExpenses::new(2026).unwrap();
        let mut march = DetailedExpenses::new(2026, 3).unwrap();
        let plan = rec.plan_for_month(&march).unwrap();
        assert_eq!(plan.pending.len(), 1);
        rec.insert_resolved(&mut march, &plan.pending).unwrap();
        let generated = plan.pending[0].row_id.clone();
        DetailedExpenses::new(2026, 3)
            .unwrap()
            .delete_row(&generated)
            .unwrap();
        start(&phone);
        pair_and_reset(&hub, &phone);

        use_device(&phone);
        Liquidity::new(2026)
            .unwrap()
            .set_value("Bank", 7, 70.0)
            .unwrap();
        use_device(&hub);
        append_to_log(b"{\"stamp\": broken line\n");
        Liquidity::new(2026)
            .unwrap()
            .set_value("Bank", 8, 80.0)
            .unwrap();
        assert!(!log_health().unwrap().is_reliable());
        let old_max = sync::read_log().unwrap().max_stamp.unwrap();
        let hub_id = sync::device_id().unwrap();

        // The phone pushes first, the hub merges it, and the connection
        // drops right after the repair.
        use_device(&phone);
        let hello = phone_hello().unwrap();
        use_device(&hub);
        let (hub_hello, plan) = hub_answer_hello(&hello).unwrap();
        assert_eq!(plan, RoundPlan::HubRepair { push_first: true });
        assert!(matches!(hub_reply(&hello), Err(Error::SyncRefused(_))));
        assert!(matches!(hub_full_log(&hello), Err(Error::SyncRefused(_))));
        use_device(&phone);
        let push = phone_push(&hub_hello).unwrap();
        assert_eq!(push.entries.len(), 1);
        use_device(&hub);
        hub_apply_push(&hello, &push).unwrap();
        let repair = repair_hub_log().unwrap();

        assert_eq!(repair.lines_dropped, 1);
        assert!(repair.phone_must_reset);
        assert!(
            crate::paths::get_sync_dir()
                .unwrap()
                .join("settings_baseline_done")
                .exists()
        );
        let read = sync::read_log().unwrap();
        assert!(read.corrupt_lines.is_empty());
        assert!(
            !String::from_utf8_lossy(&std::fs::read(sync::changelog_path().unwrap()).unwrap())
                .contains("broken line"),
            "the damaged line is gone from the file"
        );
        assert_eq!(
            repair.lines_kept + repair.rows_recorded + repair.income_cells_recorded,
            read.entries.len() - 2
        );
        let march_table = ChangeTable::Expenses {
            year: 2026,
            month: 3,
        };
        assert!(crate::merge::generated_row_stays_deleted(
            &read.entries,
            &march_table,
            &generated
        ));
        let plan = RecurringExpenses::new(2026)
            .unwrap()
            .plan_for_month(&DetailedExpenses::new(2026, 3).unwrap())
            .unwrap();
        assert!(plan.pending.is_empty(), "apply does not bring the row back");
        assert_eq!(plan.skipped.len(), 1);
        let fresh = &read.entries[repair.lines_kept..];
        assert!(!fresh.is_empty());
        assert!(
            fresh
                .iter()
                .all(|entry| entry.stamp > old_max && entry.device_id == hub_id),
            "the fresh baseline is stamped above everything before it"
        );
        assert!(log_health().unwrap().is_reliable());
        // The handle was swapped: a new change reaches the file at the path.
        Liquidity::new(2026)
            .unwrap()
            .set_value("Bank", 9, 90.0)
            .unwrap();
        assert_eq!(
            sync::read_log().unwrap().entries.len(),
            read.entries.len() + 1
        );

        // Both logs are reliable now, and still the next round resets the
        // phone rather than exchanging.
        assert!(matches!(hub_reply(&hello), Err(Error::SyncRefused(_))));
        reset_round(&hub, &phone, RoundPlan::PhoneReset { push_first: true });

        let hub_data = snapshot_of(&hub);
        assert_eq!(snapshot_of(&phone), hub_data);
        assert_eq!(
            f64_cell(&hub_data[3], "07"),
            Some(70.0),
            "the phone's push survived"
        );
        assert_eq!(f64_cell(&hub_data[3], "09"), Some(90.0));

        // The reset cleared the requirement: the next round is ordinary.
        let round = exchange(&hub, &phone).expect("an ordinary round");
        assert_eq!(round.pushed + round.replied, 0);
        let phone_id = device_id_of(&phone);
        use_device(&hub);
        assert!(
            sync_peers::find_peer(&phone_id)
                .unwrap()
                .unwrap()
                .reset_required
                .is_none()
        );
    }

    /// A message from another protocol version, or with none, is refused
    /// with an error naming the versions, and so is a hello or a batch that
    /// carries one.
    #[test]
    #[serial_test::serial]
    fn another_protocol_version_is_refused() {
        let root = tempfile::tempdir().unwrap();
        healthy_device(root.path(), "device");
        let mut hello = phone_hello().unwrap();
        hello.version = PROTOCOL_VERSION + 1;
        let bytes = SyncMessage::Hello(hello.clone()).encode().unwrap();

        let err = SyncMessage::decode(&bytes).expect_err("refused");
        let Error::SyncProtocol(message) = &err else {
            panic!("expected a protocol error, got {err}");
        };
        assert!(
            message.contains(&format!("version {}", PROTOCOL_VERSION + 1)),
            "{message}"
        );
        assert!(
            message.contains(&format!("version {PROTOCOL_VERSION}")),
            "{message}"
        );

        // A newer body shape still fails as a version mismatch.
        let newer = br#"{"kind":"handshake_v2","version":3,"anything":[1,2]}"#;
        assert!(matches!(
            SyncMessage::decode(newer),
            Err(Error::SyncProtocol(message)) if message.contains("version 3")
        ));
        assert!(matches!(
            SyncMessage::decode(br#"{"kind":"push","entries":[]}"#),
            Err(Error::SyncProtocol(_))
        ));
        assert!(matches!(
            hub_answer_hello(&hello),
            Err(Error::SyncProtocol(_))
        ));
        let batch = EntryBatch {
            version: 0,
            entries: Vec::new(),
        };
        assert!(matches!(
            phone_apply_reply(&batch),
            Err(Error::SyncProtocol(_))
        ));
        let full = FullLog {
            version: 0,
            hub_device_id: "hub".to_string(),
            reset_id: None,
            entries: Vec::new(),
        };
        assert!(matches!(
            reset_phone_from_hub(&full),
            Err(Error::SyncProtocol(_))
        ));
        assert!(
            baseline_marker_path().unwrap().exists(),
            "a refused reset changes nothing"
        );
        assert!(matches!(
            SyncMessage::Push(batch).into_reply(),
            Err(Error::SyncProtocol(_))
        ));
    }

    /// A hello from a device that is not paired, or paired in another role,
    /// is refused, and a message's `Debug` output shows counts only.
    #[test]
    #[serial_test::serial]
    fn an_unpaired_device_is_refused_and_messages_print_counts_only() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        start(&phone);
        let hello = phone_hello().unwrap();

        use_device(&hub);
        assert!(matches!(
            hub_answer_hello(&hello),
            Err(Error::SyncRefused(message)) if message.contains(&hello.device_id)
        ));
        sync_peers::record_peer(PeerRecord::new(hello.device_id.clone(), PeerRole::Hub)).unwrap();
        assert!(matches!(
            hub_answer_hello(&hello),
            Err(Error::SyncRefused(_))
        ));

        assert!(matches!(hub_full_log(&hello), Err(Error::SyncRefused(_))));
        let full = FullLog {
            version: PROTOCOL_VERSION,
            hub_device_id: "hub".to_string(),
            reset_id: Some("id".to_string()),
            entries: log_entries(),
        };
        let printed = format!("{:?}", SyncMessage::FullLog(full.clone()));
        assert_eq!(
            printed,
            format!(
                "FullLog(FullLog {{ version: 2, hub_device_id: hub, reset_id: Some(\"id\"), \
                 entries: {} }})",
                full.entries.len()
            )
        );
        assert!(!printed.contains("Rent"));
    }

    /// The text sent to the other device never carries a path.
    #[test]
    fn the_peer_safe_message_carries_no_path() {
        let err = Error::SyncResetBackup {
            path: "/home/someone/.local/share/finguard/dbs/2026".into(),
            source: Box::new(Error::Io(std::io::Error::other("disk full"))),
        };
        assert!(err.to_string().contains("/home/someone"));
        assert!(!err.peer_safe_message().contains("/home"));
        let locked = Error::SyncLogLocked {
            path: "/home/someone/changelog.jsonl".into(),
        };
        assert!(!locked.peer_safe_message().contains("/home"));
        assert!(!locked.peer_safe_message().contains("  "));
        let refused = Error::SyncRefused("device x is not paired".to_string());
        assert_eq!(refused.peer_safe_message(), "device x is not paired");
    }

    /// An upsert with a column no expenses file has. A merge rejects the
    /// whole batch for it while preparing, before it writes anything.
    fn rejected_entry() -> ChangeEntry {
        let mut row = serde_json::Map::new();
        row.insert(ROW_ID_COLUMN.into(), "odd".into());
        row.insert("no_such_column".into(), 1.into());
        ChangeEntry {
            stamp: Hlc { ms: 1, counter: 0 },
            device_id: "hub-elsewhere".to_string(),
            table: ChangeTable::Expenses {
                year: 2026,
                month: 3,
            },
            row_id: "odd".to_string(),
            origin: None,
            op: sync::ChangeOp::Upsert { row },
        }
    }

    /// A reset that fails after it removed the data leaves the phone
    /// unreliable and still due for a reset, and running it again succeeds.
    #[test]
    #[serial_test::serial]
    fn a_reset_that_fails_after_removing_the_data_can_run_again() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        start(&phone);
        DetailedExpenses::new(2026, 3)
            .unwrap()
            .add_row("Lunch", 4, 11.0, Some("Food"), "EUR", Some("Out"))
            .unwrap();
        pair_both(&hub, &phone);
        let hello = phone_hello().unwrap();
        use_device(&hub);
        let good = hub_full_log(&hello).unwrap();
        let mut bad = good.clone();
        bad.entries.push(rejected_entry());

        use_device(&phone);
        let err = reset_phone_from_hub(&bad).expect_err("the apply is rejected");

        assert!(matches!(err, Error::MergeRejected(_)), "{err}");
        let health = log_health().unwrap();
        assert!(!health.is_reliable() && health.reset_pending(), "{health}");
        assert!(health.problems.contains(&LogProblem::BaselineMissing));
        assert!(
            DetailedExpenses::new(2026, 3).unwrap().expense_df.height() == 0,
            "the data was removed before the apply"
        );
        assert_eq!(
            std::fs::read_dir(get_backups_dir().unwrap())
                .unwrap()
                .count(),
            1
        );

        reset_phone_from_hub(&good).expect("the second run succeeds");

        assert!(log_health().unwrap().is_reliable());
        assert_eq!(snapshot_of(&phone), snapshot_of(&hub));
        let round = exchange(&hub, &phone).expect("an ordinary round");
        assert_eq!(round.pushed + round.replied, 0);
    }

    /// A hub repair that fails after the rewrite leaves the hub unreliable,
    /// and running it again succeeds.
    #[test]
    #[serial_test::serial]
    fn a_hub_repair_that_fails_after_the_rewrite_can_run_again() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        start(&phone);
        pair_and_reset(&hub, &phone);
        use_device(&hub);
        append_to_log(b"{\"stamp\": broken line\n");
        // A folder where the baseline marker goes makes the repair fail
        // after it rewrote the log and recorded the fresh baseline.
        let marker = baseline_marker_path().unwrap();
        std::fs::remove_file(&marker).unwrap();
        std::fs::create_dir(&marker).unwrap();

        let err = repair_hub_log().expect_err("the marker cannot be written");

        assert!(matches!(err, Error::Io(_)), "{err}");
        assert!(
            sync::read_log().unwrap().corrupt_lines.is_empty(),
            "rewritten"
        );
        let health = log_health().unwrap();
        assert!(
            health.problems.contains(&LogProblem::Incomplete {
                reasons: vec![IncompleteReason::RepairUnfinished]
            }),
            "{health}"
        );

        std::fs::remove_dir(&marker).unwrap();
        let repair = repair_hub_log().expect("the second run succeeds");

        assert_eq!(repair.lines_dropped, 0);
        assert!(log_health().unwrap().is_reliable());
        // The phone finished its first reset before the repair, and its
        // hello proves it, so it may push its later edits before this reset.
        reset_round(&hub, &phone, RoundPlan::PhoneReset { push_first: true });
        assert_eq!(snapshot_of(&phone), snapshot_of(&hub));
    }

    /// A phone whose incomplete marker cannot be read counts as due for a
    /// reset, sends nothing, and a reset over that marker succeeds.
    #[test]
    #[serial_test::serial]
    fn a_reset_succeeds_over_an_unreadable_marker() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        start(&phone);
        pair_both(&hub, &phone);
        let marker = sync::log_incomplete_path().unwrap();
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&marker).is_ok() {
            // Running as root: permissions cannot make the file unreadable.
            eprintln!("skipped: the marker stayed readable, probably running as root");
            return;
        }

        let health = log_health().unwrap();
        assert_eq!(
            health.problems,
            vec![LogProblem::IncompleteMarkerUnreadable]
        );
        assert!(health.reset_pending());
        reset_round(&hub, &phone, RoundPlan::PhoneReset { push_first: false });

        assert!(!marker.exists(), "the reset cleared the marker");
        assert!(log_health().unwrap().is_reliable());
        assert_eq!(snapshot_of(&phone), snapshot_of(&hub));
    }

    /// An unknown reason, or an unreadable marker, counts as a pending reset.
    #[test]
    fn an_unknown_or_unreadable_marker_counts_as_a_pending_reset() {
        let with = |problem| LogHealth {
            problems: vec![problem],
            clock_ahead_of_wall_by: None,
        };
        assert!(
            with(LogProblem::Incomplete {
                reasons: vec![IncompleteReason::Unknown]
            })
            .reset_pending()
        );
        assert!(with(LogProblem::IncompleteMarkerUnreadable).reset_pending());
        assert!(
            !with(LogProblem::Incomplete {
                reasons: vec![IncompleteReason::AppendFailed]
            })
            .reset_pending()
        );
    }

    /// The hub keeps its own record of a first pairing: a phone that claims
    /// a healthy log still gets a reset without a push, its push is refused,
    /// and pairing it again does not lift the requirement.
    #[test]
    #[serial_test::serial]
    fn the_hub_takes_no_push_before_the_first_reset() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        start(&phone);
        pair_both(&hub, &phone);
        let mut hello = phone_hello().unwrap();
        // As if the phone had lost its marker.
        hello.health.problems.clear();
        let phone_id = hello.device_id.clone();

        use_device(&hub);
        pair_with_phone(&phone_id, TEST_KEY).unwrap();
        let (hub_hello, plan) = hub_answer_hello(&hello).unwrap();

        assert_eq!(plan, RoundPlan::PhoneReset { push_first: false });
        assert!(hub_hello.phone_first_reset);
        assert!(matches!(
            hub_apply_push(&hello, &EntryBatch::new(Vec::new())),
            Err(Error::SyncRefused(_))
        ));
        assert!(matches!(hub_reply(&hello), Err(Error::SyncRefused(_))));
        use_device(&phone);
        std::fs::remove_file(sync::log_incomplete_path().unwrap()).unwrap();
        assert!(matches!(phone_push(&hub_hello), Err(Error::SyncRefused(_))));
    }

    /// A first pairing with an unreliable hub: the hub repairs, the phone
    /// sends nothing and resets, and the next round is ordinary.
    #[test]
    #[serial_test::serial]
    fn a_first_pairing_with_a_damaged_hub_repairs_without_a_push() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        append_to_log(b"{\"stamp\": broken line\n");
        start(&phone);
        DetailedExpenses::new(2026, 3)
            .unwrap()
            .add_row("Lunch", 4, 11.0, Some("Food"), "EUR", Some("Out"))
            .unwrap();
        pair_both(&hub, &phone);

        reset_round(&hub, &phone, RoundPlan::HubRepair { push_first: false });

        assert_eq!(snapshot_of(&phone), snapshot_of(&hub));
        assert_eq!(
            DetailedExpenses::new(2026, 3).unwrap().expense_df.height(),
            2,
            "the phone's own row never reached the hub"
        );
        let round = exchange(&hub, &phone).expect("an ordinary round");
        assert_eq!(round.pushed + round.replied, 0);
    }

    /// A repair whose fresh baseline records nothing, on a hub with no entry
    /// of its own, still requires every phone to reset, and a hello without
    /// that reset does not lift the requirement.
    #[test]
    #[serial_test::serial]
    fn a_repair_that_records_nothing_still_requires_a_reset() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        start(&hub);
        start(&phone);
        pair_and_reset(&hub, &phone);
        let round = exchange(&hub, &phone).expect("an ordinary round");
        assert_eq!(round.pushed + round.replied, 0);
        use_device(&hub);
        append_to_log(b"{\"stamp\": broken line\n");

        let repair = repair_hub_log().unwrap();

        assert_eq!(repair.rows_recorded + repair.income_cells_recorded, 0);
        assert_eq!(sync::read_log().unwrap().entries.len(), 2);
        for _ in 0..2 {
            use_device(&phone);
            let hello = phone_hello().unwrap();
            assert!(hello.health.is_reliable());
            use_device(&hub);
            let (_, plan) = hub_answer_hello(&hello).unwrap();
            assert_eq!(plan, RoundPlan::PhoneReset { push_first: true });
            assert!(matches!(hub_reply(&hello), Err(Error::SyncRefused(_))));
        }
        reset_round(&hub, &phone, RoundPlan::PhoneReset { push_first: true });
        exchange(&hub, &phone).expect("an ordinary round after the reset");
    }

    /// A reset whose backup fails changes nothing: the baseline marker, the
    /// data, and the incomplete marker stay as they were.
    #[test]
    #[serial_test::serial]
    fn a_failed_backup_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        start(&phone);
        DetailedExpenses::new(2026, 3)
            .unwrap()
            .add_row("Lunch", 4, 11.0, Some("Food"), "EUR", Some("Out"))
            .unwrap();
        pair_both(&hub, &phone);
        let hello = phone_hello().unwrap();
        use_device(&hub);
        let full = hub_full_log(&hello).unwrap();
        use_device(&phone);
        let backups = phone.join("data").join("finguard").join("backups");
        std::fs::write(&backups, b"a file where the backups folder goes").unwrap();
        let marker_before = std::fs::read(sync::log_incomplete_path().unwrap()).unwrap();

        let err = reset_phone_from_hub(&full).expect_err("the backup fails");

        assert!(matches!(err, Error::SyncResetBackup { .. }), "{err}");
        assert!(baseline_marker_path().unwrap().exists());
        assert_eq!(
            DetailedExpenses::new(2026, 3).unwrap().expense_df.height(),
            1
        );
        assert_eq!(
            std::fs::read(sync::log_incomplete_path().unwrap()).unwrap(),
            marker_before
        );
    }

    /// Only a paired hub's log resets a phone, and a device paired with a
    /// hub never repairs itself as one.
    #[test]
    #[serial_test::serial]
    fn the_destructive_steps_check_the_role() {
        let root = tempfile::tempdir().unwrap();
        let (hub, phone) = (root.path().join("hub"), root.path().join("phone"));
        hub_with_history(&hub);
        start(&phone);
        pair_both(&hub, &phone);
        let phone_id = sync::device_id().unwrap();
        let hello = phone_hello().unwrap();
        use_device(&hub);
        let mut full = hub_full_log(&hello).unwrap();
        full.hub_device_id = "not-my-hub".to_string();

        use_device(&phone);
        assert!(matches!(
            reset_phone_from_hub(&full),
            Err(Error::SyncRefused(_))
        ));
        full.hub_device_id = phone_id;
        assert!(matches!(
            reset_phone_from_hub(&full),
            Err(Error::SyncRefused(_))
        ));
        assert!(baseline_marker_path().unwrap().exists(), "nothing changed");
        assert!(matches!(repair_hub_log(), Err(Error::SyncRefused(_))));
        assert!(
            sync::read_log_incomplete().unwrap() == Some(vec![IncompleteReason::ResetPending]),
            "the refused repair marked nothing"
        );
    }
}

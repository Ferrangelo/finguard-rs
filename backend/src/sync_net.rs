//! The encrypted sync connection: framing, the pairing handshake, and the
//! handshake every later sync uses. It moves bytes only. It reads no data
//! file, and what the bytes mean is [`crate::sync_service`]'s business, so
//! every function here works over any byte stream: a TCP connection, or an
//! in-memory duplex in a test.
//!
//! # Wire format
//!
//! Every frame on the wire is a 2-byte big-endian length and that many bytes,
//! at most 65535, which is also the largest Noise message.
//!
//! Before the handshake, frames are plaintext. The phone speaks first with an
//! opening frame that names the protocol, its version, and the purpose,
//! [`Opening::Pair`] or [`Opening::Sync`]. Every plaintext frame the hub
//! sends is a status frame: a `0` byte and a payload to go on, or a `1` byte
//! and a short reason when it refuses. So the phone can show why at any
//! step, and a refusal never looks like handshake bytes.
//!
//! # Pairing: SPAKE2, then `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`
//!
//! 1. Phone: opening `pair`. Hub: status, refused when busy or when no
//!    pairing code is active.
//! 2. Phone: its SPAKE2 message, as side A. Hub: status carrying its own
//!    SPAKE2 message, as side B. Both use the code the desktop shows as the
//!    password, and the fixed identities [`SPAKE2_PHONE_ID`] and
//!    [`SPAKE2_HUB_ID`]. From here on the attempt counts against the code.
//! 3. Both finish SPAKE2 with 32 bytes that match only when both used the
//!    same code. Those bytes are the pre-shared key of an `XXpsk3`
//!    handshake: phone message 1, hub message 2 (in a status frame), phone
//!    message 3. `XX` sends both long-term static keys, encrypted.
//! 4. The hub reads message 3. With a different code its decryption fails,
//!    and the hub answers a refusal that says only that the code does not
//!    match. Otherwise it answers an empty status and both sides switch to
//!    transport mode.
//!
//! Why not the code itself as the pre-shared key: whoever records one
//! handshake could then try all million codes offline against message 3.
//! SPAKE2 allows one guess per connection, and an attacker who does not know
//! the code learns nothing from a recording.
//!
//! Why no HKDF over the SPAKE2 output: the output is already a SHA-256 hash
//! over the code, both identities, both SPAKE2 messages, and the shared
//! point, so it is uniform and bound to this exchange. Noise then feeds the
//! pre-shared key through its own HKDF (`MixKeyAndHash`), with the prologue
//! [`PAIR_PROLOGUE`] already in the handshake hash. Another HKDF would add a
//! dependency and no strength.
//!
//! # Every later sync: `Noise_KK_25519_ChaChaPoly_BLAKE2s`
//!
//! Both sides already hold each other's static key. The phone sends opening
//! `sync` and message 1. The hub tries message 1 against the key of each
//! paired phone, and only a key that decrypts it is accepted, so the phone's
//! device id never crosses the wire in clear. The hub answers message 2 in a
//! status frame, which the phone accepts only if it comes from the hub key
//! it recorded.
//!
//! # Transport
//!
//! An application message is sent as one Noise message holding its length,
//! 4 bytes big-endian, then as many Noise messages as it needs, each holding
//! at most [`MAX_CHUNK`] bytes. A receiver refuses a length above
//! [`Limits::max_message`] before reading any chunk.
//!
//! # Limits
//!
//! Before the handshake completes the hub reads at most
//! [`PRE_HANDSHAKE_BUDGET`] bytes in frames of at most
//! [`PRE_HANDSHAKE_FRAME_MAX`] bytes, and every read and write, before and
//! after, has a timeout. So a stranger on the network cannot make the hub
//! hold memory or wait forever.

use std::time::Duration;

use snow::{Builder, HandshakeState, TransportState};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::sync_keys::{KEY_LEN, StaticKeypair};

/// The Noise protocol of the pairing handshake.
pub const PAIR_NOISE_PARAMS: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
/// The Noise protocol of every sync after pairing.
pub const SYNC_NOISE_PARAMS: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";

/// Hashed into the pairing handshake, so its keys belong to this protocol
/// and version only.
pub const PAIR_PROLOGUE: &[u8] = b"finguard-sync/1 pair";
/// Hashed into every sync handshake, for the same reason.
pub const SYNC_PROLOGUE: &[u8] = b"finguard-sync/1 sync";
/// The phone's SPAKE2 identity. The phone is side A.
pub const SPAKE2_PHONE_ID: &[u8] = b"finguard-sync/1 phone";
/// The hub's SPAKE2 identity. The hub is side B.
pub const SPAKE2_HUB_ID: &[u8] = b"finguard-sync/1 hub";

const OPENING_PAIR: &[u8] = b"finguard-sync/1 open pair";
const OPENING_SYNC: &[u8] = b"finguard-sync/1 open sync";

/// The largest frame, and the largest Noise message.
pub const MAX_FRAME: usize = 65535;
/// The Poly1305 tag every encrypted Noise message carries.
const TAG_LEN: usize = 16;
/// The most application bytes one Noise transport message carries.
pub const MAX_CHUNK: usize = MAX_FRAME - TAG_LEN;
/// The largest frame the hub reads before the handshake completes. The
/// largest real one is `XXpsk3` message 3, 96 bytes with an empty payload.
pub const PRE_HANDSHAKE_FRAME_MAX: usize = 1024;
/// The most bytes, length prefixes included, the hub reads from one
/// connection before the handshake completes. Pairing needs about 250.
pub const PRE_HANDSHAKE_BUDGET: usize = 4096;
/// The largest application message: 64 MiB. A full log is the largest
/// message there is. The real data's full log was 390 entries on
/// 2026-09-18, a few hundred kilobytes of JSON, so this leaves room for more
/// than a hundred times that before a sync fails with a clear error, and it
/// still fits a phone's memory.
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// The longest refusal reason a status frame carries, in bytes.
const MAX_REASON: usize = 300;

/// Timeouts and the message cap, so tests can shorten them.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Each read and write until the handshake completes, and the first
    /// message after it.
    pub handshake: Duration,
    /// Each read and write after the handshake. Long enough for the other
    /// side to apply a large batch before it answers.
    pub session: Duration,
    /// The largest application message.
    pub max_message: usize,
}

impl Limits {
    /// The values the app uses.
    pub const DEFAULT: Limits = Limits {
        handshake: Duration::from_secs(10),
        session: Duration::from_secs(120),
        max_message: MAX_MESSAGE,
    };
}

/// What the phone opened a connection for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opening {
    /// A first pairing with a code.
    Pair,
    /// A sync with a paired hub.
    Sync,
}

/// How many bytes the hub may still read before the handshake completes.
#[derive(Debug)]
pub struct Budget {
    left: usize,
}

impl Budget {
    /// The full [`PRE_HANDSHAKE_BUDGET`].
    pub fn new() -> Budget {
        Budget {
            left: PRE_HANDSHAKE_BUDGET,
        }
    }
}

impl Default for Budget {
    fn default() -> Self {
        Budget::new()
    }
}

// ------------------------------------------------------------------
// Frames
// ------------------------------------------------------------------

fn timed_out(what: &str, limit: Duration) -> Error {
    Error::Network(format!(
        "the other device did not {what} within {} seconds",
        limit.as_secs()
    ))
}

fn io_error(err: std::io::Error) -> Error {
    if err.kind() == std::io::ErrorKind::UnexpectedEof {
        Error::Network("the other device closed the connection".to_string())
    } else {
        Error::Network(format!("the sync connection failed: {err}"))
    }
}

/// Write one frame. `bytes` must fit [`MAX_FRAME`].
async fn write_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    bytes: &[u8],
    limit: Duration,
) -> Result<()> {
    let len = u16::try_from(bytes.len()).map_err(|_| {
        Error::SyncProtocol(format!("a frame of {} bytes is too long", bytes.len()))
    })?;
    let write = async {
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(bytes).await?;
        stream.flush().await
    };
    tokio::time::timeout(limit, write)
        .await
        .map_err(|_| timed_out("take the data", limit))?
        .map_err(io_error)
}

/// Read one frame of at most `max_len` bytes. With a `budget`, the frame and
/// its prefix are charged to it, and a frame that would overdraw it is
/// refused before its body is read.
async fn read_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    max_len: usize,
    budget: Option<&mut Budget>,
    limit: Duration,
) -> Result<Vec<u8>> {
    let read = async {
        let mut prefix = [0u8; 2];
        stream.read_exact(&mut prefix).await.map_err(io_error)?;
        let len = usize::from(u16::from_be_bytes(prefix));
        if len > max_len {
            return Err(Error::SyncProtocol(format!(
                "the other device sent a frame of {len} bytes where at most {max_len} fit"
            )));
        }
        if let Some(budget) = budget {
            let cost = len + prefix.len();
            if cost > budget.left {
                return Err(Error::SyncProtocol(
                    "the other device sent too much data before the handshake".to_string(),
                ));
            }
            budget.left -= cost;
        }
        let mut bytes = vec![0u8; len];
        stream.read_exact(&mut bytes).await.map_err(io_error)?;
        Ok(bytes)
    };
    tokio::time::timeout(limit, read)
        .await
        .map_err(|_| timed_out("answer", limit))?
}

// ------------------------------------------------------------------
// Plaintext steps
// ------------------------------------------------------------------

/// On the phone: open a connection for `opening`.
async fn send_opening<S: AsyncWrite + Unpin>(
    stream: &mut S,
    opening: Opening,
    limits: &Limits,
) -> Result<()> {
    let bytes = match opening {
        Opening::Pair => OPENING_PAIR,
        Opening::Sync => OPENING_SYNC,
    };
    write_frame(stream, bytes, limits.handshake).await
}

/// On the hub: read what the phone opened this connection for.
///
/// # Errors
///
/// [`Error::SyncProtocol`] for anything but an opening of this version, and
/// [`Error::Network`] when the phone is silent or leaves.
pub async fn read_opening<S: AsyncRead + Unpin>(
    stream: &mut S,
    budget: &mut Budget,
    limits: &Limits,
) -> Result<Opening> {
    let bytes = read_frame(
        stream,
        PRE_HANDSHAKE_FRAME_MAX,
        Some(budget),
        limits.handshake,
    )
    .await?;
    match bytes.as_slice() {
        OPENING_PAIR => Ok(Opening::Pair),
        OPENING_SYNC => Ok(Opening::Sync),
        _ => Err(Error::SyncProtocol(
            "the other device does not speak finguard sync version 1".to_string(),
        )),
    }
}

/// On the hub: a status frame that lets the phone go on, carrying `payload`.
async fn send_go_on<S: AsyncWrite + Unpin>(
    stream: &mut S,
    payload: &[u8],
    limits: &Limits,
) -> Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 1);
    frame.push(0);
    frame.extend_from_slice(payload);
    write_frame(stream, &frame, limits.handshake).await
}

/// On the hub: the empty status frame that lets the phone go on after its
/// opening.
///
/// # Errors
///
/// [`Error::Network`] when the frame cannot be written in time.
pub async fn accept_opening<S: AsyncWrite + Unpin>(stream: &mut S, limits: &Limits) -> Result<()> {
    send_go_on(stream, &[], limits).await
}

/// On the hub: refuse with `reason`, cut to a short length. Best effort:
/// the connection closes next anyway, so a failed write is ignored.
pub async fn send_refusal<S: AsyncWrite + Unpin>(stream: &mut S, reason: &str, limits: &Limits) {
    let mut frame = vec![1];
    frame.extend_from_slice(truncate(reason, MAX_REASON).as_bytes());
    let _ = write_frame(stream, &frame, limits.handshake).await;
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// On the phone: read a status frame, and return its payload when the hub
/// lets the phone go on.
///
/// The reason in a refusal is unauthenticated text from the network, so it
/// is cut short and stripped of control characters before it reaches the
/// user.
async fn read_status<S: AsyncRead + Unpin>(stream: &mut S, limits: &Limits) -> Result<Vec<u8>> {
    let mut bytes = read_frame(stream, PRE_HANDSHAKE_FRAME_MAX, None, limits.handshake).await?;
    match bytes.first() {
        Some(0) => {
            bytes.remove(0);
            Ok(bytes)
        }
        Some(1) => {
            let reason: String = String::from_utf8_lossy(&bytes[1..])
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            Err(Error::SyncRefused(format!(
                "the desktop refused: {}",
                truncate(&reason, MAX_REASON)
            )))
        }
        _ => Err(Error::SyncProtocol(
            "the desktop sent an answer this version cannot read".to_string(),
        )),
    }
}

// ------------------------------------------------------------------
// Noise
// ------------------------------------------------------------------

fn noise_error(step: &str, err: snow::Error) -> Error {
    Error::SyncProtocol(format!("the sync handshake failed at {step}: {err}"))
}

fn builder<'a>(params: &str) -> Result<Builder<'a>> {
    let params = params
        .parse()
        .map_err(|err| noise_error("its setup", err))?;
    Ok(Builder::new(params))
}

/// Write the next handshake message, with an empty payload.
fn handshake_write(state: &mut HandshakeState, step: &str) -> Result<Vec<u8>> {
    let mut buffer = vec![0u8; PRE_HANDSHAKE_FRAME_MAX];
    let len = state
        .write_message(&[], &mut buffer)
        .map_err(|err| noise_error(step, err))?;
    buffer.truncate(len);
    Ok(buffer)
}

/// Read the next handshake message. Returns snow's error, so the caller
/// decides what a failed read means.
fn handshake_read(
    state: &mut HandshakeState,
    message: &[u8],
) -> std::result::Result<(), snow::Error> {
    let mut payload = vec![0u8; PRE_HANDSHAKE_FRAME_MAX];
    state.read_message(message, &mut payload).map(|_| ())
}

/// A finished handshake over `stream`: every application message is
/// encrypted, authenticated, and ordered.
pub struct SecureChannel<S> {
    stream: S,
    noise: TransportState,
    limits: Limits,
    remote_static: [u8; KEY_LEN],
}

impl<S> std::fmt::Debug for SecureChannel<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SecureChannel {{ remote: {} }}",
            crate::sync_keys::fingerprint(&self.remote_static)
        )
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> SecureChannel<S> {
    fn new(stream: S, state: HandshakeState, limits: &Limits) -> Result<SecureChannel<S>> {
        let remote_static: [u8; KEY_LEN] = state
            .get_remote_static()
            .and_then(|key| key.try_into().ok())
            .ok_or_else(|| {
                Error::SyncProtocol("the handshake finished without the other device's key".into())
            })?;
        let noise = state
            .into_transport_mode()
            .map_err(|err| noise_error("its end", err))?;
        Ok(SecureChannel {
            stream,
            noise,
            limits: *limits,
            remote_static,
        })
    }

    /// The other device's static public key, as the handshake proved it.
    pub fn remote_static(&self) -> &[u8; KEY_LEN] {
        &self.remote_static
    }

    /// Encrypt `plaintext`, at most [`MAX_CHUNK`] bytes, and send it as one
    /// frame.
    async fn send_noise(&mut self, plaintext: &[u8]) -> Result<()> {
        let mut buffer = vec![0u8; plaintext.len() + TAG_LEN];
        let len = self
            .noise
            .write_message(plaintext, &mut buffer)
            .map_err(|err| noise_error("encryption", err))?;
        write_frame(&mut self.stream, &buffer[..len], self.limits.session).await
    }

    /// Read one frame and decrypt it.
    async fn recv_noise(&mut self, limit: Duration) -> Result<Vec<u8>> {
        let frame = read_frame(&mut self.stream, MAX_FRAME, None, limit).await?;
        let mut plaintext = vec![0u8; frame.len()];
        let len = self
            .noise
            .read_message(&frame, &mut plaintext)
            .map_err(|_| {
                Error::SyncProtocol(
                    "a sync message failed its integrity check; the connection was altered"
                        .to_string(),
                )
            })?;
        plaintext.truncate(len);
        Ok(plaintext)
    }

    /// Send one application message of any size up to the cap.
    ///
    /// # Errors
    ///
    /// [`Error::SyncProtocol`] for a message above
    /// [`Limits::max_message`], before anything is sent, and
    /// [`Error::Network`] when the other side does not take it in time.
    pub async fn send(&mut self, message: &[u8]) -> Result<()> {
        if message.len() > self.limits.max_message {
            return Err(Error::SyncProtocol(format!(
                "a sync message of {} bytes is larger than the {} bytes allowed",
                message.len(),
                self.limits.max_message
            )));
        }
        let len = u32::try_from(message.len())
            .map_err(|_| Error::SyncProtocol("a sync message is too large".to_string()))?;
        self.send_noise(&len.to_be_bytes()).await?;
        for chunk in message.chunks(MAX_CHUNK) {
            self.send_noise(chunk).await?;
        }
        Ok(())
    }

    /// Receive one application message, waiting at most the session
    /// timeout for each part.
    ///
    /// # Errors
    ///
    /// [`Error::SyncProtocol`] for a length above the cap, a part that fails
    /// decryption, or parts that overrun the length. [`Error::Network`] for
    /// a timeout or a closed connection.
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        self.recv_within(self.limits.session).await
    }

    /// [`SecureChannel::recv`] with the shorter handshake timeout, for the
    /// first message after the handshake. A stranger replaying a recorded
    /// handshake cannot send it, so this keeps such a replay from holding
    /// the session for long.
    pub async fn recv_first(&mut self) -> Result<Vec<u8>> {
        self.recv_within(self.limits.handshake).await
    }

    async fn recv_within(&mut self, limit: Duration) -> Result<Vec<u8>> {
        let header = self.recv_noise(limit).await?;
        let header: [u8; 4] = header.as_slice().try_into().map_err(|_| {
            Error::SyncProtocol("a sync message started without its length".to_string())
        })?;
        let len = u32::from_be_bytes(header) as usize;
        if len > self.limits.max_message {
            return Err(Error::SyncProtocol(format!(
                "the other device announced a sync message of {len} bytes; at most {} are allowed",
                self.limits.max_message
            )));
        }
        let mut message = Vec::with_capacity(len);
        while message.len() < len {
            let chunk = self.recv_noise(self.limits.session).await?;
            if chunk.is_empty() || message.len() + chunk.len() > len {
                return Err(Error::SyncProtocol(
                    "a sync message's parts do not match its length".to_string(),
                ));
            }
            message.extend_from_slice(&chunk);
        }
        Ok(message)
    }
}

// ------------------------------------------------------------------
// Pairing
// ------------------------------------------------------------------

/// What the hub knows about pairing codes, as the handshake needs it.
///
/// An attempt starts when the hub has a phone's SPAKE2 message and needs the
/// code, and ends with [`PairingCodes::finish_attempt`], which the
/// handshake calls exactly once for every attempt it started, whatever
/// happens: success, a wrong code, or a phone that leaves half way.
pub trait PairingCodes: Send + Sync {
    /// The active code, or the reason there is none, for the phone.
    ///
    /// # Errors
    ///
    /// The refusal reason, such as no code or an expired one.
    fn begin_attempt(&self) -> std::result::Result<String, String>;
    /// Record how the attempt with `code`, as [`PairingCodes::begin_attempt`]
    /// returned it, ended. A code replaced meanwhile is not charged.
    fn finish_attempt(&self, code: &str, succeeded: bool);
}

/// Calls [`PairingCodes::finish_attempt`] once: with `true` if
/// [`AttemptGuard::succeed`] ran, and with `false` when dropped otherwise,
/// so an early return or a cancelled future counts as a failed attempt.
struct AttemptGuard<'a> {
    codes: &'a dyn PairingCodes,
    code: String,
    finished: bool,
}

impl AttemptGuard<'_> {
    fn succeed(mut self) {
        self.finished = true;
        self.codes.finish_attempt(&self.code, true);
    }
}

impl Drop for AttemptGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.codes.finish_attempt(&self.code, false);
        }
    }
}

/// The SPAKE2 output as a pre-shared key.
fn psk_from(spake_key: Vec<u8>) -> Result<[u8; 32]> {
    spake_key.try_into().map_err(|_| {
        Error::SyncProtocol("the pairing key exchange produced a key of the wrong size".into())
    })
}

#[derive(Clone, Copy)]
enum Spake2Side {
    Phone,
    Hub,
}

fn spake2_start(code: &str, side: Spake2Side) -> (Spake2<Ed25519Group>, Vec<u8>) {
    let password = Password::new(code.as_bytes());
    let phone_id = Identity::new(SPAKE2_PHONE_ID);
    let hub_id = Identity::new(SPAKE2_HUB_ID);
    match side {
        Spake2Side::Phone => Spake2::start_a(&password, &phone_id, &hub_id),
        Spake2Side::Hub => Spake2::start_b(&password, &phone_id, &hub_id),
    }
}

fn spake2_finish(spake: Spake2<Ed25519Group>, message: &[u8], peer: &str) -> Result<[u8; 32]> {
    psk_from(spake.finish(message).map_err(|err| {
        Error::SyncProtocol(format!("the {peer}'s pairing message is invalid: {err}"))
    })?)
}

#[cfg(test)]
fn pairing_psk_for_test(code: &str) -> Result<[u8; 32]> {
    let (phone, message_a) = spake2_start(code, Spake2Side::Phone);
    let (hub, message_b) = spake2_start(code, Spake2Side::Hub);
    let phone_psk = spake2_finish(phone, &message_b, "desktop")?;
    let hub_psk = spake2_finish(hub, &message_a, "phone")?;
    assert_eq!(phone_psk, hub_psk);
    Ok(phone_psk)
}

/// The phone's side of pairing: connect with `code` over `stream`, and
/// return the channel with the hub's static key in it. The caller records
/// the hub only after the exchange inside the channel succeeds.
///
/// # Errors
///
/// [`Error::SyncRefused`] with the hub's reason when it refuses, including a
/// wrong code. [`Error::SyncProtocol`] for bytes that do not follow the
/// protocol, and [`Error::Network`] for a timeout or a closed connection.
pub async fn pair_as_phone<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    code: &str,
    keys: &StaticKeypair,
    limits: &Limits,
) -> Result<SecureChannel<S>> {
    send_opening(&mut stream, Opening::Pair, limits).await?;
    read_status(&mut stream, limits).await?;

    let (spake, message_a) = spake2_start(code, Spake2Side::Phone);
    write_frame(&mut stream, &message_a, limits.handshake).await?;
    let message_b = read_status(&mut stream, limits).await?;
    let psk = spake2_finish(spake, &message_b, "desktop")?;

    let mut state = builder(PAIR_NOISE_PARAMS)?
        .local_private_key(keys.private())
        .and_then(|b| b.prologue(PAIR_PROLOGUE))
        .and_then(|b| b.psk(3, &psk))
        .and_then(|b| b.build_initiator())
        .map_err(|err| noise_error("its setup", err))?;
    let message_1 = handshake_write(&mut state, "message 1")?;
    write_frame(&mut stream, &message_1, limits.handshake).await?;
    let message_2 = read_status(&mut stream, limits).await?;
    handshake_read(&mut state, &message_2).map_err(|err| noise_error("message 2", err))?;
    let message_3 = handshake_write(&mut state, "message 3")?;
    write_frame(&mut stream, &message_3, limits.handshake).await?;
    read_status(&mut stream, limits).await?;
    SecureChannel::new(stream, state, limits)
}

/// The hub's side of pairing, after [`read_opening`] returned
/// [`Opening::Pair`] and the hub let the phone go on with
/// [`accept_opening`]. Returns the channel with the phone's static key in
/// it. Every attempt that reaches the code is reported to `codes`.
///
/// # Errors
///
/// [`Error::SyncRefused`] when `codes` has no code or the phone's code does
/// not match; the phone has been told. [`Error::SyncProtocol`] and
/// [`Error::Network`] as in [`pair_as_phone`].
pub async fn pair_as_hub<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    budget: &mut Budget,
    keys: &StaticKeypair,
    codes: &dyn PairingCodes,
    limits: &Limits,
) -> Result<SecureChannel<S>> {
    let message_a = read_frame(
        &mut stream,
        PRE_HANDSHAKE_FRAME_MAX,
        Some(budget),
        limits.handshake,
    )
    .await?;
    let code = match codes.begin_attempt() {
        Ok(code) => code,
        Err(reason) => {
            send_refusal(&mut stream, &reason, limits).await;
            return Err(Error::SyncRefused(reason));
        }
    };
    let attempt = AttemptGuard {
        codes,
        code: code.clone(),
        finished: false,
    };

    let (spake, message_b) = spake2_start(&code, Spake2Side::Hub);
    let psk = spake2_finish(spake, &message_a, "phone")?;
    send_go_on(&mut stream, &message_b, limits).await?;

    let mut state = builder(PAIR_NOISE_PARAMS)?
        .local_private_key(keys.private())
        .and_then(|b| b.prologue(PAIR_PROLOGUE))
        .and_then(|b| b.psk(3, &psk))
        .and_then(|b| b.build_responder())
        .map_err(|err| noise_error("its setup", err))?;
    let message_1 = read_frame(
        &mut stream,
        PRE_HANDSHAKE_FRAME_MAX,
        Some(budget),
        limits.handshake,
    )
    .await?;
    handshake_read(&mut state, &message_1).map_err(|err| noise_error("message 1", err))?;
    let message_2 = handshake_write(&mut state, "message 2")?;
    send_go_on(&mut stream, &message_2, limits).await?;
    let message_3 = read_frame(
        &mut stream,
        PRE_HANDSHAKE_FRAME_MAX,
        Some(budget),
        limits.handshake,
    )
    .await?;
    // The pre-shared key enters at message 3, so this is where a different
    // code shows. The reason says that and nothing more.
    if handshake_read(&mut state, &message_3).is_err() {
        send_refusal(&mut stream, WRONG_CODE, limits).await;
        return Err(Error::SyncRefused(
            "a phone tried to pair with a wrong code".to_string(),
        ));
    }
    attempt.succeed();
    send_go_on(&mut stream, &[], limits).await?;
    SecureChannel::new(stream, state, limits)
}

/// What the phone sees after a wrong code.
pub const WRONG_CODE: &str = "the pairing code does not match. Check the code on the desktop \
                              and try again; after 3 wrong codes the desktop needs a new one.";

// ------------------------------------------------------------------
// Sync
// ------------------------------------------------------------------

/// The phone's side of a sync connection to the hub whose static key is
/// `hub_key`.
///
/// # Errors
///
/// [`Error::SyncRefused`] when the hub refuses, for example because it is
/// busy or does not know this phone, or when the answer does not come from
/// `hub_key`. [`Error::SyncProtocol`] and [`Error::Network`] as in
/// [`pair_as_phone`].
pub async fn sync_as_phone<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    keys: &StaticKeypair,
    hub_key: &[u8; KEY_LEN],
    limits: &Limits,
) -> Result<SecureChannel<S>> {
    send_opening(&mut stream, Opening::Sync, limits).await?;
    read_status(&mut stream, limits).await?;
    let mut state = builder(SYNC_NOISE_PARAMS)?
        .local_private_key(keys.private())
        .and_then(|b| b.remote_public_key(hub_key))
        .and_then(|b| b.prologue(SYNC_PROLOGUE))
        .and_then(|b| b.build_initiator())
        .map_err(|err| noise_error("its setup", err))?;
    let message_1 = handshake_write(&mut state, "message 1")?;
    write_frame(&mut stream, &message_1, limits.handshake).await?;
    let message_2 = read_status(&mut stream, limits).await?;
    handshake_read(&mut state, &message_2).map_err(|_| {
        Error::SyncRefused(
            "the device at this address is not the desktop this phone paired with".to_string(),
        )
    })?;
    SecureChannel::new(stream, state, limits)
}

/// The hub's side of a sync connection, after [`read_opening`] returned
/// [`Opening::Sync`] and the hub let the phone go on with
/// [`accept_opening`]. `phones` pairs each paired phone's device id with its
/// static key. Returns the channel and the device id whose key the phone
/// proved.
///
/// # Errors
///
/// [`Error::SyncRefused`] when no paired phone's key fits; the phone has
/// been told. [`Error::SyncProtocol`] and [`Error::Network`] as in
/// [`pair_as_phone`].
pub async fn sync_as_hub<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    budget: &mut Budget,
    keys: &StaticKeypair,
    phones: &[(String, [u8; KEY_LEN])],
    limits: &Limits,
) -> Result<(SecureChannel<S>, String)> {
    let message_1 = read_frame(
        &mut stream,
        PRE_HANDSHAKE_FRAME_MAX,
        Some(budget),
        limits.handshake,
    )
    .await?;
    let mut matched = None;
    for (device_id, phone_key) in phones {
        let mut state = builder(SYNC_NOISE_PARAMS)?
            .local_private_key(keys.private())
            .and_then(|b| b.remote_public_key(phone_key))
            .and_then(|b| b.prologue(SYNC_PROLOGUE))
            .and_then(|b| b.build_responder())
            .map_err(|err| noise_error("its setup", err))?;
        // Message 1 ends in a payload encrypted under a key mixed from both
        // static keys, so it decrypts only with the sender's real key.
        if handshake_read(&mut state, &message_1).is_ok() {
            matched = Some((device_id.clone(), state));
            break;
        }
    }
    let Some((device_id, mut state)) = matched else {
        send_refusal(
            &mut stream,
            "this phone is not paired with this desktop. Pair it again.",
            limits,
        )
        .await;
        return Err(Error::SyncRefused(
            "a device that is not a paired phone tried to sync".to_string(),
        ));
    };
    let message_2 = handshake_write(&mut state, "message 2")?;
    send_go_on(&mut stream, &message_2, limits).await?;
    Ok((SecureChannel::new(stream, state, limits)?, device_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use tokio::io::{DuplexStream, duplex};
    use tokio::net::{TcpListener, TcpStream};

    /// A pairing code holder with no clock: a code, a count of failures, and
    /// a cap after which the code is gone.
    struct StubCodes {
        state: Mutex<(Option<String>, u32)>,
        successes: Mutex<u32>,
    }

    impl StubCodes {
        fn with(code: &str) -> StubCodes {
            StubCodes {
                state: Mutex::new((Some(code.to_string()), 0)),
                successes: Mutex::new(0),
            }
        }

        fn failures(&self) -> u32 {
            self.state.lock().unwrap().1
        }
    }

    impl PairingCodes for StubCodes {
        fn begin_attempt(&self) -> std::result::Result<String, String> {
            self.state
                .lock()
                .unwrap()
                .0
                .clone()
                .ok_or_else(|| "no pairing code is active".to_string())
        }

        fn finish_attempt(&self, _code: &str, succeeded: bool) {
            let mut state = self.state.lock().unwrap();
            if succeeded {
                state.0 = None;
                *self.successes.lock().unwrap() += 1;
            } else {
                state.1 += 1;
                if state.1 >= 3 {
                    state.0 = None;
                }
            }
        }
    }

    const FAST: Limits = Limits {
        handshake: Duration::from_secs(5),
        session: Duration::from_secs(5),
        max_message: 1024 * 1024,
    };

    /// The hub's side of a pairing, as the service runs it.
    async fn hub_pairs<S: AsyncRead + AsyncWrite + Unpin>(
        mut stream: S,
        keys: &StaticKeypair,
        codes: &StubCodes,
    ) -> Result<SecureChannel<S>> {
        let mut budget = Budget::new();
        assert_eq!(
            read_opening(&mut stream, &mut budget, &FAST).await?,
            Opening::Pair
        );
        accept_opening(&mut stream, &FAST).await?;
        pair_as_hub(stream, &mut budget, keys, codes, &FAST).await
    }

    /// The hub's side of a sync.
    async fn hub_syncs<S: AsyncRead + AsyncWrite + Unpin>(
        mut stream: S,
        keys: &StaticKeypair,
        phones: &[(String, [u8; KEY_LEN])],
    ) -> Result<(SecureChannel<S>, String)> {
        let mut budget = Budget::new();
        assert_eq!(
            read_opening(&mut stream, &mut budget, &FAST).await?,
            Opening::Sync
        );
        accept_opening(&mut stream, &FAST).await?;
        sync_as_hub(stream, &mut budget, keys, phones, &FAST).await
    }

    fn pipe() -> (DuplexStream, DuplexStream) {
        duplex(256 * 1024)
    }

    #[test]
    fn the_same_pairing_code_produces_a_fresh_psk_each_time() {
        let first = pairing_psk_for_test("042917").unwrap();
        let second = pairing_psk_for_test("042917").unwrap();
        assert_ne!(first, second);
    }

    /// With the right code both sides learn each other's static key, and
    /// messages of every size cross the channel both ways intact, including
    /// ones that need several chunks.
    #[tokio::test]
    async fn pairing_with_the_right_code_exchanges_keys_and_carries_messages() {
        let (phone_keys, hub_keys) = (
            StaticKeypair::generate().unwrap(),
            StaticKeypair::generate().unwrap(),
        );
        let codes = StubCodes::with("042917");
        let (phone_end, hub_end) = pipe();
        let (phone, hub) = tokio::join!(
            pair_as_phone(phone_end, "042917", &phone_keys, &FAST),
            hub_pairs(hub_end, &hub_keys, &codes)
        );
        let (mut phone, mut hub) = (phone.unwrap(), hub.unwrap());
        assert_eq!(phone.remote_static(), hub_keys.public());
        assert_eq!(hub.remote_static(), phone_keys.public());
        assert_eq!(*codes.successes.lock().unwrap(), 1);
        assert!(codes.begin_attempt().is_err(), "a code pairs once");

        for size in [
            1,
            MAX_CHUNK - 1,
            MAX_CHUNK,
            MAX_CHUNK + 1,
            3 * MAX_CHUNK + 7,
        ] {
            let message: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let (sent, received) = tokio::join!(phone.send(&message), hub.recv());
            sent.unwrap();
            assert_eq!(received.unwrap(), message, "{size} bytes, phone to hub");
            let (sent, received) = tokio::join!(hub.send(&message), phone.recv());
            sent.unwrap();
            assert_eq!(received.unwrap(), message, "{size} bytes, hub to phone");
        }
    }

    /// A wrong code fails on both sides, tells the phone only that the code
    /// does not match, and counts one failed attempt; three use the code up.
    #[tokio::test]
    async fn a_wrong_code_fails_and_three_use_the_code_up() {
        let (phone_keys, hub_keys) = (
            StaticKeypair::generate().unwrap(),
            StaticKeypair::generate().unwrap(),
        );
        let codes = StubCodes::with("111111");
        for attempt in 1..=3 {
            let (phone_end, hub_end) = pipe();
            let (phone, hub) = tokio::join!(
                pair_as_phone(phone_end, "222222", &phone_keys, &FAST),
                hub_pairs(hub_end, &hub_keys, &codes)
            );
            let phone_err = phone.expect_err("the phone fails");
            let Error::SyncRefused(message) = &phone_err else {
                panic!("expected a refusal, got {phone_err}");
            };
            assert!(message.contains("does not match"), "{message}");
            assert!(matches!(hub, Err(Error::SyncRefused(_))));
            assert_eq!(codes.failures(), attempt);
        }

        let (phone_end, hub_end) = pipe();
        let (phone, hub) = tokio::join!(
            pair_as_phone(phone_end, "111111", &phone_keys, &FAST),
            hub_pairs(hub_end, &hub_keys, &codes)
        );
        let message = phone.expect_err("the code is gone").to_string();
        assert!(message.contains("no pairing code is active"), "{message}");
        assert!(hub.is_err());
    }

    /// A phone that leaves after the key exchange started counts as a failed
    /// attempt, so disconnecting cannot give free guesses.
    #[tokio::test]
    async fn a_phone_that_leaves_half_way_uses_an_attempt() {
        let hub_keys = StaticKeypair::generate().unwrap();
        let codes = StubCodes::with("333333");
        let (mut phone_end, hub_end) = pipe();
        let phone = async move {
            send_opening(&mut phone_end, Opening::Pair, &FAST)
                .await
                .unwrap();
            read_status(&mut phone_end, &FAST).await.unwrap();
            let (_, message_a) = Spake2::<Ed25519Group>::start_a(
                &Password::new(b"000000"),
                &Identity::new(SPAKE2_PHONE_ID),
                &Identity::new(SPAKE2_HUB_ID),
            );
            write_frame(&mut phone_end, &message_a, FAST.handshake)
                .await
                .unwrap();
            read_status(&mut phone_end, &FAST).await.unwrap();
            drop(phone_end);
        };
        let (_, hub) = tokio::join!(phone, hub_pairs(hub_end, &hub_keys, &codes));
        assert!(matches!(hub, Err(Error::Network(_))), "{hub:?}");
        assert_eq!(codes.failures(), 1);
    }

    /// After pairing, a sync handshake finds the phone by its key among the
    /// paired phones, and carries messages.
    #[tokio::test]
    async fn a_sync_finds_the_phone_by_its_key() {
        let hub_keys = StaticKeypair::generate().unwrap();
        let phone_keys = StaticKeypair::generate().unwrap();
        let other = StaticKeypair::generate().unwrap();
        let phones = vec![
            ("other".to_string(), *other.public()),
            ("phone-1".to_string(), *phone_keys.public()),
        ];
        let (phone_end, hub_end) = pipe();
        let (phone, hub) = tokio::join!(
            sync_as_phone(phone_end, &phone_keys, hub_keys.public(), &FAST),
            hub_syncs(hub_end, &hub_keys, &phones)
        );
        let mut phone = phone.unwrap();
        let (mut hub, device_id) = hub.unwrap();
        assert_eq!(device_id, "phone-1");
        let (sent, received) = tokio::join!(phone.send(b"hello"), hub.recv_first());
        sent.unwrap();
        assert_eq!(received.unwrap(), b"hello");
    }

    /// A phone whose key the hub does not hold is refused and told so.
    #[tokio::test]
    async fn a_sync_from_an_unknown_key_is_refused() {
        let hub_keys = StaticKeypair::generate().unwrap();
        let stranger = StaticKeypair::generate().unwrap();
        let paired = StaticKeypair::generate().unwrap();
        let phones = vec![("phone-1".to_string(), *paired.public())];
        let (phone_end, hub_end) = pipe();
        let (phone, hub) = tokio::join!(
            sync_as_phone(phone_end, &stranger, hub_keys.public(), &FAST),
            hub_syncs(hub_end, &hub_keys, &phones)
        );
        let message = phone.expect_err("refused").to_string();
        assert!(message.contains("not paired"), "{message}");
        assert!(matches!(hub, Err(Error::SyncRefused(_))));
    }

    /// A device holding another key than the hub the phone paired with
    /// cannot complete the phone's sync handshake: it cannot even read
    /// message 1, which is encrypted to the paired hub's key.
    #[tokio::test]
    async fn a_device_with_another_key_cannot_answer_a_phone() {
        let hub_keys = StaticKeypair::generate().unwrap();
        let impostor = StaticKeypair::generate().unwrap();
        let phone_keys = StaticKeypair::generate().unwrap();
        let phones = vec![("phone-1".to_string(), *phone_keys.public())];
        let (phone_end, hub_end) = pipe();
        let (phone, hub) = tokio::join!(
            sync_as_phone(phone_end, &phone_keys, hub_keys.public(), &FAST),
            hub_syncs(hub_end, &impostor, &phones)
        );
        assert!(matches!(phone, Err(Error::SyncRefused(_))), "{phone:?}");
        assert!(hub.is_err());
    }

    /// The hub refuses a frame above the pre-handshake limit before reading
    /// its body, and a stream of small frames once the budget is spent.
    #[tokio::test]
    async fn the_hub_reads_little_before_the_handshake() {
        let (mut phone_end, mut hub_end) = pipe();
        phone_end.write_all(&[0xff, 0xff]).await.unwrap();
        let err = read_opening(&mut hub_end, &mut Budget::new(), &FAST)
            .await
            .expect_err("too long");
        assert!(matches!(err, Error::SyncProtocol(_)), "{err}");

        let mut budget = Budget::new();
        let (mut phone_end, mut hub_end) = pipe();
        let frame = vec![0u8; PRE_HANDSHAKE_FRAME_MAX];
        let writer = async {
            for _ in 0..5 {
                let _ = write_frame(&mut phone_end, &frame, FAST.handshake).await;
            }
        };
        let reader = async {
            let mut result = Ok(Vec::new());
            for _ in 0..5 {
                result = read_frame(
                    &mut hub_end,
                    PRE_HANDSHAKE_FRAME_MAX,
                    Some(&mut budget),
                    FAST.handshake,
                )
                .await;
                if result.is_err() {
                    break;
                }
            }
            result
        };
        let (_, result) = tokio::join!(writer, reader);
        assert!(matches!(result, Err(Error::SyncProtocol(_))), "{result:?}");
    }

    /// A silent peer times out instead of holding the connection.
    #[tokio::test]
    async fn a_silent_peer_times_out() {
        let (_phone_end, mut hub_end) = pipe();
        let limits = Limits {
            handshake: Duration::from_millis(50),
            ..FAST
        };
        let err = read_opening(&mut hub_end, &mut Budget::new(), &limits)
            .await
            .expect_err("timed out");
        assert!(matches!(err, Error::Network(_)), "{err}");
    }

    /// A message above the cap is refused by the sender before it sends
    /// anything, and by the receiver from the length alone.
    #[tokio::test]
    async fn messages_above_the_cap_are_refused() {
        let (phone_keys, hub_keys) = (
            StaticKeypair::generate().unwrap(),
            StaticKeypair::generate().unwrap(),
        );
        let codes = StubCodes::with("123456");
        let (phone_end, hub_end) = pipe();
        let (phone, hub) = tokio::join!(
            pair_as_phone(phone_end, "123456", &phone_keys, &FAST),
            hub_pairs(hub_end, &hub_keys, &codes)
        );
        let (mut phone, mut hub) = (phone.unwrap(), hub.unwrap());
        let too_big = vec![0u8; FAST.max_message + 1];
        assert!(matches!(
            phone.send(&too_big).await,
            Err(Error::SyncProtocol(_))
        ));

        // A receiver with a smaller cap refuses the announced length.
        hub.limits.max_message = 100;
        let (_, received) = tokio::join!(phone.send(&[0u8; 101]), hub.recv());
        assert!(
            matches!(received, Err(Error::SyncProtocol(_))),
            "{received:?}"
        );
    }

    /// Pairing and a sync over a real local TCP connection, with a stub hub
    /// that echoes one message and touches no data.
    #[tokio::test]
    async fn pairing_and_sync_work_over_tcp() {
        let hub_keys = StaticKeypair::generate().unwrap();
        let phone_keys = StaticKeypair::generate().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hub_public = *hub_keys.public();

        let hub = tokio::spawn(async move {
            let codes = StubCodes::with("654321");
            let (stream, _) = listener.accept().await.unwrap();
            let mut channel = hub_pairs(stream, &hub_keys, &codes).await.unwrap();
            let phone_key = *channel.remote_static();
            let message = channel.recv_first().await.unwrap();
            channel.send(&message).await.unwrap();

            let (stream, _) = listener.accept().await.unwrap();
            let phones = vec![("phone-1".to_string(), phone_key)];
            let (mut channel, device_id) = hub_syncs(stream, &hub_keys, &phones).await.unwrap();
            let message = channel.recv_first().await.unwrap();
            channel.send(&message).await.unwrap();
            device_id
        });

        let stream = TcpStream::connect(address).await.unwrap();
        let mut channel = pair_as_phone(stream, "654321", &phone_keys, &FAST)
            .await
            .unwrap();
        assert_eq!(channel.remote_static(), &hub_public);
        let big: Vec<u8> = (0..200_000).map(|i| (i % 7) as u8).collect();
        channel.send(&big).await.unwrap();
        assert_eq!(channel.recv().await.unwrap(), big);

        let stream = TcpStream::connect(address).await.unwrap();
        let mut channel = sync_as_phone(stream, &phone_keys, &hub_public, &FAST)
            .await
            .unwrap();
        channel.send(b"round").await.unwrap();
        assert_eq!(channel.recv().await.unwrap(), b"round");
        assert_eq!(hub.await.unwrap(), "phone-1");
    }
}

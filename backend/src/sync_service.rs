//! Sync as the app runs it: the desktop's listener and pairing code, the
//! phone's pairing and Sync now, and the status both show. It carries the
//! messages of [`crate::sync_exchange`] over the channel
//! [`crate::sync_net`] builds, and adds no sync logic of its own: every
//! decision about data is a call into [`crate::sync_exchange`].
//!
//! # Roles
//!
//! A build for Android is a phone and every other build is the hub; see
//! [`role`]. The hub listens and shows pairing codes; the phone pairs and
//! syncs. Each refuses the other's actions with [`Error::SyncRefused`].
//!
//! # The desktop listens only while its Sync page is open
//!
//! The page calls [`heartbeat`] while it is open. The first call binds the
//! sync port, `0.0.0.0:3112` unless `FINGUARD_SYNC_HOST` or
//! `FINGUARD_SYNC_PORT` say otherwise, and the listener closes about
//! [`LISTEN_GRACE`] after the last call. A port that cannot be bound is
//! reported in [`ListenerStatus::bind_error`], never as a failed request, so
//! the rest of the API is unaffected.
//!
//! # Messages inside the channel
//!
//! Each application message is a `WireMessage`: one tag byte, then the
//! body. A [`crate::sync_exchange::SyncMessage`] travels as its own JSON
//! bytes, so its version check runs first. Pairing, after the handshake:
//! the hub sends its identity, the phone answers with its own, the hub
//! records the phone and says done, and only then does the phone record the
//! hub. A round, after the sync handshake:
//!
//! 1. Phone: hello. Hub: hello, and both now hold the same
//!    [`RoundPlan`].
//! 2. For [`RoundPlan::Exchange`]: phone push; hub applies it, sends its
//!    counts as a push report, then its reply; the phone applies the reply.
//! 3. For a reset plan without the user's confirmation: the phone counts
//!    what it holds and sends done, and nothing changes on either side.
//!    With the confirmation: the phone pushes first when the plan says so
//!    (and gets a push report), or sends proceed; a hub that must repair
//!    does so; the hub sends its full log; the phone resets from it.
//!
//! Only [`Error::peer_safe_message`] crosses the wire. Local logs get
//! counts, device ids, and error kinds, never a value.
//!
//! # Locking
//!
//! Nothing here holds the data write lock while it waits on the network. The
//! functions of [`crate::sync_exchange`] that change data take the lock
//! themselves, blocking, so every call into that module runs through
//! [`tokio::task::spawn_blocking`].

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::error::{Error, Result};
use crate::merge_apply::MergeReport;
use crate::sync;
use crate::sync_exchange::{
    self, EntryBatch, FullLog, Hello, LogHealth, ResetPreview, RoundPlan, SyncMessage,
};
use crate::sync_keys::{self, KEY_LEN, StaticKeypair};
use crate::sync_net::{self, Budget, Limits, Opening, PairingCodes, SecureChannel};
use crate::sync_peers::{self, PeerRecord, PeerRole};
use crate::write_lock;

/// The sync port when `FINGUARD_SYNC_PORT` is not set.
pub const DEFAULT_SYNC_PORT: u16 = 3112;
/// How long the listener stays open after the last heartbeat.
pub const LISTEN_GRACE: Duration = Duration::from_secs(60);
/// How long a pairing code is valid.
pub const PAIR_CODE_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// Failed attempts after which a pairing code is withdrawn.
pub const PAIR_CODE_ATTEMPTS: u32 = 3;
/// The longest a connection on the hub may last, whatever it does.
const SESSION_LIMIT: Duration = Duration::from_secs(15 * 60);

// ------------------------------------------------------------------
// Role
// ------------------------------------------------------------------

/// What this device is in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncRole {
    /// The desktop, which listens and holds the reference copy.
    Hub,
    /// A phone, which pairs with one hub and starts every sync.
    Phone,
}

impl SyncRole {
    /// `"hub"` or `"phone"`, as the API and the peers file spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            SyncRole::Hub => "hub",
            SyncRole::Phone => "phone",
        }
    }

    fn device_word(self) -> &'static str {
        match self {
            SyncRole::Hub => "desktop",
            SyncRole::Phone => "phone",
        }
    }
}

/// 0: no override, 1: hub, 2: phone.
static ROLE_OVERRIDE: AtomicU8 = AtomicU8::new(0);

/// This device's role: [`SyncRole::Phone`] in a build for Android,
/// [`SyncRole::Hub`] in every other build, unless
/// [`override_role_for_tests`] set another.
pub fn role() -> SyncRole {
    match ROLE_OVERRIDE.load(Ordering::Relaxed) {
        1 => SyncRole::Hub,
        2 => SyncRole::Phone,
        _ if cfg!(target_os = "android") => SyncRole::Phone,
        _ => SyncRole::Hub,
    }
}

/// Make this process act as `role`, or as its build's role again with
/// `None`. For tests only: a desktop test that plays the phone needs it,
/// and nothing in the app calls it.
#[doc(hidden)]
pub fn override_role_for_tests(role: Option<SyncRole>) {
    let value = match role {
        None => 0,
        Some(SyncRole::Hub) => 1,
        Some(SyncRole::Phone) => 2,
    };
    ROLE_OVERRIDE.store(value, Ordering::Relaxed);
}

/// Refuse `action` unless this device is `wanted`.
fn require_role(wanted: SyncRole, action: &str) -> Result<()> {
    let actual = role();
    if actual == wanted {
        return Ok(());
    }
    Err(Error::SyncRefused(format!(
        "{action} works only on the {}, and this device is the {}",
        wanted.device_word(),
        actual.device_word()
    )))
}

// ------------------------------------------------------------------
// One session at a time
// ------------------------------------------------------------------

static SESSION_BUSY: AtomicBool = AtomicBool::new(false);

/// The right to run the one sync session, pairing or round, this device
/// allows at a time. Released on drop.
struct SessionSlot;

impl SessionSlot {
    fn try_take() -> Option<SessionSlot> {
        SESSION_BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| SessionSlot)
    }
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        SESSION_BUSY.store(false, Ordering::Release);
    }
}

const BUSY: &str = "another sync is running on this device; try again in a moment";

// ------------------------------------------------------------------
// Pairing codes
// ------------------------------------------------------------------

/// The hub's pairing code, if one is active. Pure state: every method takes
/// the time, so the rules are testable without waiting.
#[derive(Debug, Default)]
struct PairCodeSlot {
    active: Option<ActiveCode>,
}

#[derive(Debug)]
struct ActiveCode {
    code: String,
    expires_at: Instant,
    expires_at_ms: i64,
    failures: u32,
}

impl PairCodeSlot {
    const fn new() -> PairCodeSlot {
        PairCodeSlot { active: None }
    }

    /// Replace any active code with `code`, valid for
    /// [`PAIR_CODE_LIFETIME`] from `now`.
    fn issue(&mut self, code: String, now: Instant, now_ms: i64) -> PairCodeIssued {
        let lifetime_ms = PAIR_CODE_LIFETIME.as_millis() as i64;
        let issued = PairCodeIssued {
            code: code.clone(),
            expires_at_ms: now_ms + lifetime_ms,
            attempts_allowed: PAIR_CODE_ATTEMPTS,
        };
        self.active = Some(ActiveCode {
            code,
            expires_at: now + PAIR_CODE_LIFETIME,
            expires_at_ms: issued.expires_at_ms,
            failures: 0,
        });
        issued
    }

    /// The active code at `now`, dropping an expired one.
    fn current(&mut self, now: Instant) -> std::result::Result<&ActiveCode, String> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.expires_at <= now)
        {
            self.active = None;
            return Err(
                "the pairing code expired. Show a new code on the desktop and try again."
                    .to_string(),
            );
        }
        self.active.as_ref().ok_or_else(|| {
            "no pairing code is active on the desktop. Show a new code there and try again."
                .to_string()
        })
    }

    /// When the active code expires, in milliseconds since the Unix epoch.
    fn expires_at_ms(&mut self, now: Instant) -> Option<i64> {
        self.current(now).ok().map(|active| active.expires_at_ms)
    }

    /// Record the end of an attempt with `code`. Success uses the code up;
    /// the [`PAIR_CODE_ATTEMPTS`]th failure withdraws it. An attempt with a
    /// code that has been replaced changes nothing.
    fn finish(&mut self, code: &str, succeeded: bool) {
        let Some(active) = self.active.as_mut().filter(|active| active.code == code) else {
            return;
        };
        if succeeded {
            self.active = None;
            return;
        }
        active.failures += 1;
        if active.failures >= PAIR_CODE_ATTEMPTS {
            self.active = None;
        }
    }
}

/// A fresh 6-digit code from the operating system's random generator. The
/// modulo bias of 64 random bits over a million values is below 1e-13.
fn random_code() -> Result<String> {
    let mut bytes = [0u8; 8];
    sync_keys::fill_random(&mut bytes)?;
    Ok(format!("{:06}", u64::from_be_bytes(bytes) % 1_000_000))
}

/// A pairing code as the desktop's page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairCodeIssued {
    /// Six digits.
    pub code: String,
    /// When the code stops working, in milliseconds since the Unix epoch.
    pub expires_at_ms: i64,
    /// Wrong codes after which this code stops working.
    pub attempts_allowed: u32,
}

/// [`PairingCodes`] over the hub's shared state.
struct HubCodes;

impl PairingCodes for HubCodes {
    fn begin_attempt(&self) -> std::result::Result<String, String> {
        hub_state()
            .pair_code
            .current(Instant::now())
            .map(|active| active.code.clone())
    }

    fn finish_attempt(&self, code: &str, succeeded: bool) {
        hub_state().pair_code.finish(code, succeeded);
    }
}

/// On the hub: show a new pairing code, replacing any active one.
///
/// # Errors
///
/// [`Error::SyncRefused`] on a phone, and [`Error::Io`] when the random
/// generator fails.
pub fn new_pair_code() -> Result<PairCodeIssued> {
    require_role(SyncRole::Hub, "Showing a pairing code")?;
    let code = random_code()?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    Ok(hub_state().pair_code.issue(code, Instant::now(), now_ms))
}

// ------------------------------------------------------------------
// The hub's listener
// ------------------------------------------------------------------

/// The hub's shared state. A static rather than router state for the same
/// reason as [`crate::write_lock`]: every router copy shares one listener.
struct HubState {
    last_heartbeat: Option<Instant>,
    /// The bound port while the listener runs.
    listening_on: Option<u16>,
    /// A heartbeat is binding right now, so another one does not bind too.
    starting: bool,
    bind_error: Option<String>,
    pair_code: PairCodeSlot,
}

static HUB: Mutex<HubState> = Mutex::new(HubState {
    last_heartbeat: None,
    listening_on: None,
    starting: false,
    bind_error: None,
    pair_code: PairCodeSlot::new(),
});

fn hub_state() -> MutexGuard<'static, HubState> {
    HUB.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The hub's listener, as its Sync page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerStatus {
    /// Whether the sync port is open now.
    pub listening: bool,
    /// The port it listens on, or would.
    pub port: u16,
    /// A guess at the address a phone on the same network can reach; see
    /// [`address_hint`].
    pub address_hint: Option<String>,
    /// Why the last attempt to listen failed, such as a port in use.
    pub bind_error: Option<String>,
    /// When the active pairing code expires, if one is active.
    pub pair_code_expires_at_ms: Option<i64>,
}

/// Where the listener binds: `FINGUARD_SYNC_HOST`, default `0.0.0.0`, and
/// `FINGUARD_SYNC_PORT`, default [`DEFAULT_SYNC_PORT`]. Read at every bind.
fn sync_bind_address() -> std::result::Result<SocketAddr, String> {
    let host = std::env::var("FINGUARD_SYNC_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let host: IpAddr = host
        .trim()
        .parse()
        .map_err(|_| "FINGUARD_SYNC_HOST is not an IP address".to_string())?;
    let port = match std::env::var("FINGUARD_SYNC_PORT") {
        Ok(port) => port
            .trim()
            .parse()
            .map_err(|_| "FINGUARD_SYNC_PORT is not a port number".to_string())?,
        Err(_) => DEFAULT_SYNC_PORT,
    };
    Ok(SocketAddr::new(host, port))
}

/// A guess at the address a phone on the same network uses to reach this
/// desktop's sync port, for the page to show as a guess, never as a fact.
///
/// It asks the operating system which local address it would use to reach
/// a public address, by connecting a UDP socket to one; connecting a UDP
/// socket sends nothing. On a machine with several networks that can be the
/// wrong one. Inside Docker it is the container's own address, which a phone
/// cannot reach: the user has to use the host's address with the published
/// port instead. A bind address set in `FINGUARD_SYNC_HOST` wins when it
/// names one address.
pub fn address_hint(bind: SocketAddr) -> Option<String> {
    if !bind.ip().is_unspecified() {
        return Some(bind.to_string());
    }
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("8.8.8.8", 80)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_unspecified() || ip.is_loopback() {
        return None;
    }
    Some(SocketAddr::new(ip, bind.port()).to_string())
}

pub(crate) fn listener_remaining() -> Duration {
    hub_state()
        .last_heartbeat
        .map(|beat| (beat + LISTEN_GRACE).saturating_duration_since(Instant::now()))
        .unwrap_or_default()
}

/// The listener's state now.
fn listener_status() -> ListenerStatus {
    let bind = sync_bind_address();
    let mut state = hub_state();
    let (port, bind_error) = match (&bind, state.listening_on) {
        (_, Some(port)) => (port, state.bind_error.clone()),
        (Ok(address), None) => (address.port(), state.bind_error.clone()),
        (Err(reason), None) => (DEFAULT_SYNC_PORT, Some(reason.clone())),
    };
    let listening = state.listening_on.is_some();
    let pair_code_expires_at_ms = state.pair_code.expires_at_ms(Instant::now());
    drop(state);
    let address_hint = bind
        .ok()
        .and_then(|address| address_hint(SocketAddr::new(address.ip(), port)));
    ListenerStatus {
        listening,
        port,
        address_hint,
        bind_error,
        pair_code_expires_at_ms,
    }
}

/// On the hub: the Sync page's heartbeat. Keeps the listener open for
/// [`LISTEN_GRACE`] from now, and opens it when it is closed.
///
/// Binding is the only step that waits, and it does not wait on another
/// device. A failure to bind is reported in the result, not as an error, and
/// the next heartbeat tries again.
///
/// # Errors
///
/// [`Error::SyncRefused`] on a phone, which never listens.
pub async fn heartbeat() -> Result<ListenerStatus> {
    require_role(SyncRole::Hub, "Listening for phones")?;
    {
        let mut state = hub_state();
        state.last_heartbeat = Some(Instant::now());
        if state.listening_on.is_some() || state.starting {
            drop(state);
            return Ok(listener_status());
        }
        state.starting = true;
    }

    let bound = match sync_bind_address() {
        Ok(address) => TcpListener::bind(address)
            .await
            .map_err(|err| format!("cannot listen on {address}: {err}"))
            .and_then(|listener| {
                let port = listener
                    .local_addr()
                    .map_err(|err| format!("cannot read the sync port: {err}"))?
                    .port();
                Ok((listener, SocketAddr::new(address.ip(), port)))
            }),
        Err(reason) => Err(reason),
    };
    {
        let mut state = hub_state();
        state.starting = false;
        match bound {
            Ok((listener, bind_address)) => {
                state.listening_on = Some(bind_address.port());
                state.bind_error = None;
                println!("Sync: listening for phones on port {}", bind_address.port());
                tokio::spawn(accept_loop(listener));
                tokio::spawn(start_discovery(bind_address));
            }
            Err(reason) => {
                eprintln!("Sync: {reason}");
                state.bind_error = Some(reason);
            }
        }
    }
    Ok(listener_status())
}

async fn start_discovery(bind_address: SocketAddr) {
    let identity = tokio::task::spawn_blocking(|| {
        let key = sync_keys::local_keypair()?;
        Ok::<_, Error>((sync::device_id()?, key.fingerprint()))
    })
    .await;
    let Ok(Ok((device_id, fingerprint))) = identity else {
        eprintln!("Sync discovery: cannot load desktop identity");
        return;
    };
    crate::sync_discovery::serve_until_listener_closes(bind_address, device_id, fingerprint).await;
}

/// Accept connections until [`LISTEN_GRACE`] has passed since the last
/// heartbeat, serving each on its own task.
async fn accept_loop(listener: TcpListener) {
    let permits = Arc::new(Semaphore::new(8));
    loop {
        let wait = {
            let mut state = hub_state();
            let deadline = state
                .last_heartbeat
                .map(|beat| beat + LISTEN_GRACE)
                .unwrap_or_else(Instant::now);
            let wait = deadline.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                state.listening_on = None;
                // Closed while the state is still locked, so a heartbeat that
                // sees the listener gone can bind the port at once.
                drop(listener);
                println!("Sync: stopped listening for phones");
                return;
            }
            wait
        };
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, from)) => {
                    let Ok(permit) = permits.clone().try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    tokio::spawn(async move {
                        let _permit = permit;
                        serve_connection(stream, from).await;
                    });
                }
                Err(err) => {
                    eprintln!("Sync: accepting a connection failed: {err}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            },
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// Serve one connection to the sync port, within [`SESSION_LIMIT`].
async fn serve_connection(stream: TcpStream, from: SocketAddr) {
    let limits = Limits::DEFAULT;
    match tokio::time::timeout(SESSION_LIMIT, hub_session(stream, &limits)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => eprintln!("Sync: the connection from {from} ended: {}", log_text(&err)),
        Err(_) => eprintln!(
            "Sync: the connection from {from} ran longer than {} minutes and was closed",
            SESSION_LIMIT.as_secs() / 60
        ),
    }
}

/// Every paired phone that has a static key, with that key.
fn paired_phone_keys() -> Result<Vec<(String, [u8; KEY_LEN])>> {
    Ok(sync_peers::load_peers()?
        .into_iter()
        .filter(|peer| peer.role == PeerRole::Phone)
        .filter_map(|peer| {
            let key = sync_keys::key_from_hex(peer.static_key.as_deref()?)?;
            Some((peer.device_id, key))
        })
        .collect())
}

/// Refuse the phone with `reason` and fail with `err`.
async fn refuse<S: AsyncWrite + Unpin>(
    stream: &mut S,
    reason: &str,
    err: Error,
    limits: &Limits,
) -> Result<()> {
    sync_net::send_refusal(stream, reason, limits).await;
    Err(err)
}

/// One connection on the hub: the opening, then pairing or a round.
async fn hub_session<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    limits: &Limits,
) -> Result<()> {
    let mut budget = Budget::new();
    let opening = sync_net::read_opening(&mut stream, &mut budget, limits).await?;
    let keys = match blocking(sync_keys::local_keypair).await {
        Ok(keys) => keys,
        Err(err) => {
            let reason = "the desktop cannot use its sync key; its log has the details";
            return refuse(&mut stream, reason, err, limits).await;
        }
    };
    match opening {
        Opening::Pair => {
            sync_net::accept_opening(&mut stream, limits).await?;
            let codes = PairAttemptCodes::new();
            let channel = sync_net::pair_as_hub(stream, &mut budget, &keys, &codes, limits).await?;
            hub_pairing(channel).await
        }
        Opening::Sync => {
            let phones = match blocking(paired_phone_keys).await {
                Ok(phones) => phones,
                Err(err) => {
                    let reason = err.peer_safe_message();
                    return refuse(&mut stream, &reason, err, limits).await;
                }
            };
            sync_net::accept_opening(&mut stream, limits).await?;
            let (channel, device_id) =
                sync_net::sync_as_hub(stream, &mut budget, &keys, &phones, limits).await?;
            let Some(_slot) = SessionSlot::try_take() else {
                let result = Err(Error::SyncRefused(BUSY.to_string()));
                let mut channel = channel;
                report_to_peer(&mut channel, &result).await;
                return result;
            };
            hub_round(channel, device_id).await
        }
    }
}

/// Acquires the session slot after the phone has supplied its SPAKE2 message.
/// The slot remains held for the whole pairing handshake and channel session.
struct PairAttemptCodes {
    slot: Mutex<Option<SessionSlot>>,
}

impl PairAttemptCodes {
    fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }
}

impl PairingCodes for PairAttemptCodes {
    fn begin_attempt(&self) -> std::result::Result<String, String> {
        let Some(slot) = SessionSlot::try_take() else {
            return Err(BUSY.to_string());
        };
        let code = HubCodes.begin_attempt();
        if code.is_ok() {
            *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(slot);
        }
        code
    }

    fn finish_attempt(&self, code: &str, succeeded: bool) {
        HubCodes.finish_attempt(code, succeeded);
    }
}

/// The hub's side of pairing inside the channel.
async fn hub_pairing<S: AsyncRead + AsyncWrite + Unpin>(
    mut channel: SecureChannel<S>,
) -> Result<()> {
    let result = async {
        let own_id = blocking(sync::device_id).await?;
        send(&mut channel, &WireMessage::identity(own_id, PeerRole::Hub)).await?;
        let phone = match recv(&mut channel, "phone").await? {
            WireMessage::Identity(identity) => identity,
            other => return Err(other.unexpected("the phone's identity")),
        };
        phone.check(PeerRole::Phone)?;
        let key = sync_keys::to_hex(channel.remote_static());
        let phone_id = phone.device_id.clone();
        blocking(move || sync_exchange::pair_with_phone(&phone_id, &key)).await?;
        send(&mut channel, &WireMessage::Done).await?;
        println!(
            "Sync: paired phone {} (key {})",
            phone.device_id,
            sync_keys::fingerprint(channel.remote_static())
        );
        Ok(())
    }
    .await;
    report_to_peer(&mut channel, &result).await;
    result
}

/// The hub's side of a round with `device_id`, whose key the handshake
/// proved.
async fn hub_round<S: AsyncRead + AsyncWrite + Unpin>(
    mut channel: SecureChannel<S>,
    device_id: String,
) -> Result<()> {
    let result = hub_round_steps(&mut channel, &device_id).await;
    report_to_peer(&mut channel, &result).await;
    match &result {
        Ok(last) => {
            println!("Sync: {}", last.summary());
            record_last(last.clone());
        }
        Err(err) => record_last(LastSync::failed(Some(device_id), err)),
    }
    result.map(|_| ())
}

async fn hub_round_steps<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<S>,
    device_id: &str,
) -> Result<LastSync> {
    let bytes = channel.recv_first().await?;
    let hello = match WireMessage::decode(&bytes)? {
        WireMessage::Sync(message) => message.into_hello()?,
        WireMessage::Error(text) => return Err(peer_stopped("phone", &text)),
        other => return Err(other.unexpected("a hello")),
    };
    if hello.device_id != device_id {
        check_plain_device_id(&hello.device_id)?;
        return Err(Error::SyncRefused(format!(
            "the phone introduced itself as device {}, but its key belongs to device {device_id}",
            hello.device_id
        )));
    }
    let phone_hello = hello.clone();
    let (hub_hello, plan) = blocking(move || sync_exchange::hub_answer_hello(&phone_hello)).await?;
    send(channel, &WireMessage::Sync(SyncMessage::Hello(hub_hello))).await?;

    let mut last = LastSync::new(Some(device_id.to_string()), SyncOutcome::Exchanged, plan);
    let push_first = match plan {
        RoundPlan::Exchange => {
            let push = match recv(channel, "phone").await? {
                WireMessage::Sync(message) => message.into_push()?,
                other => return Err(other.unexpected("a push")),
            };
            hub_take_push(channel, &hello, push, &mut last).await?;
            let phone_hello = hello.clone();
            let reply = blocking(move || sync_exchange::hub_reply(&phone_hello)).await?;
            last.counts.sent = reply.entries.len();
            send(channel, &WireMessage::Sync(SyncMessage::Reply(reply))).await?;
            return Ok(last);
        }
        RoundPlan::PhoneReset { push_first } | RoundPlan::HubRepair { push_first } => push_first,
    };

    match recv(channel, "phone").await? {
        WireMessage::Done => {
            last.outcome = SyncOutcome::ResetNeeded;
            return Ok(last);
        }
        WireMessage::Sync(message) if push_first => {
            hub_take_push(channel, &hello, message.into_push()?, &mut last).await?;
        }
        WireMessage::Proceed if !push_first => {}
        other => {
            let expected = if push_first { "a push" } else { "proceed" };
            return Err(other.unexpected(expected));
        }
    }
    if matches!(plan, RoundPlan::HubRepair { .. }) {
        let report = blocking(sync_exchange::repair_hub_log).await?;
        println!(
            "Sync: repaired this desktop's change log: {} lines kept, {} dropped, {} rows and {} \
             income cells recorded again, {} files skipped",
            report.lines_kept,
            report.lines_dropped,
            report.rows_recorded,
            report.income_cells_recorded,
            report.files_skipped
        );
    }
    let phone_hello = hello.clone();
    let full = blocking(move || sync_exchange::hub_full_log(&phone_hello)).await?;
    last.counts.sent = full.entries.len();
    send(channel, &WireMessage::Sync(SyncMessage::FullLog(full))).await?;
    last.outcome = SyncOutcome::ResetDone;
    Ok(last)
}

/// Apply the phone's push and send the phone the counts.
async fn hub_take_push<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<S>,
    hello: &Hello,
    push: EntryBatch,
    last: &mut LastSync,
) -> Result<()> {
    last.counts.received = push.entries.len();
    let phone_hello = hello.clone();
    let report = blocking(move || sync_exchange::hub_apply_push(&phone_hello, &push)).await?;
    last.counts.add_merge(&report);
    send(channel, &WireMessage::PushReport(PeerCounts::from(&report))).await
}

// ------------------------------------------------------------------
// The phone
// ------------------------------------------------------------------

/// What a pairing recorded on the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairResult {
    /// The hub's device id.
    pub hub_device_id: String,
    /// The fingerprint of the hub's static key, to compare with the one the
    /// desktop shows.
    pub hub_key_fingerprint: String,
    /// The address the phone paired through and will sync with.
    pub address: String,
}

/// The pairing code as the phone must send it: exactly 6 digits, ignoring
/// spaces around and inside.
fn normalize_code(code: &str) -> Result<String> {
    let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() == 6 && digits.chars().all(|c| c.is_ascii_digit()) {
        return Ok(digits);
    }
    Err(Error::InvalidArgument(
        "the pairing code is the 6 digits the desktop shows".to_string(),
    ))
}

/// The address to connect to: `host:port`, or a host alone with
/// [`DEFAULT_SYNC_PORT`]. An IPv6 address alone is taken as a host.
fn normalize_address(address: &str) -> Result<String> {
    let address = address.trim();
    if address.is_empty() || address.len() > 255 || address.chars().any(char::is_whitespace) {
        return Err(Error::InvalidArgument(
            "the desktop's address is its IP address or name, optionally with :port".to_string(),
        ));
    }
    if address.parse::<SocketAddr>().is_ok() {
        return Ok(address.to_string());
    }
    if let Ok(ip) = address.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, DEFAULT_SYNC_PORT).to_string());
    }
    if address.contains(':') {
        return Ok(address.to_string());
    }
    Ok(format!("{address}:{DEFAULT_SYNC_PORT}"))
}

/// Connect to `address` within the handshake timeout.
async fn connect(address: &str, limits: &Limits) -> Result<TcpStream> {
    let unreachable = |why: String| {
        Error::Network(format!(
            "cannot reach the desktop at {address}: {why}. Check that its Sync page is open and \
             that both devices are on the same network, and that the desktop firewall allows \
             inbound TCP on the sync port."
        ))
    };
    match tokio::time::timeout(limits.handshake, TcpStream::connect(address)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(err)) => Err(unreachable(err.to_string())),
        Err(_) => Err(unreachable(format!(
            "no answer within {} seconds",
            limits.handshake.as_secs()
        ))),
    }
}

/// On the phone: pair with the desktop at `address` using the `code` it
/// shows, in one call, because the code enters the very first message of
/// the connection.
///
/// The phone records the hub only after the hub has recorded the phone, and
/// marks itself due for a reset, so its first Sync now shows what it holds
/// and asks before anything is replaced.
///
/// # Errors
///
/// [`Error::SyncRefused`] on the hub, when this phone already has a hub,
/// when another sync runs, and for every refusal from the desktop, a wrong
/// code included. [`Error::InvalidArgument`] for a malformed code or
/// address. [`Error::Network`] when the desktop cannot be reached or stops
/// answering, and [`Error::SyncProtocol`] for a desktop that does not speak
/// this protocol.
pub async fn pair(address: &str, code: &str) -> Result<PairResult> {
    require_role(SyncRole::Phone, "Pairing with a desktop")?;
    let code = normalize_code(code)?;
    let address = normalize_address(address)?;
    let _slot = SessionSlot::try_take().ok_or_else(|| Error::SyncRefused(BUSY.to_string()))?;
    let (keys, own_id, has_hub) = blocking(|| {
        let has_hub = sync_peers::load_peers()?
            .iter()
            .any(|peer| peer.role == PeerRole::Hub);
        Ok((sync_keys::local_keypair()?, sync::device_id()?, has_hub))
    })
    .await?;
    if has_hub {
        return Err(Error::SyncRefused(
            "this phone is already paired with a desktop. Unpair it first, then pair again."
                .to_string(),
        ));
    }

    let limits = Limits::DEFAULT;
    let stream = connect(&address, &limits).await?;
    let mut channel = sync_net::pair_as_phone(stream, &code, &keys, &limits).await?;
    let result = async {
        let hub = match recv(&mut channel, "desktop").await? {
            WireMessage::Identity(identity) => identity,
            other => return Err(other.unexpected("the desktop's identity")),
        };
        hub.check(PeerRole::Hub)?;
        if hub.device_id == own_id {
            return Err(Error::SyncRefused(
                "the desktop claims this phone's own device id".to_string(),
            ));
        }
        send(
            &mut channel,
            &WireMessage::identity(own_id.clone(), PeerRole::Phone),
        )
        .await?;
        match recv(&mut channel, "desktop").await? {
            WireMessage::Done => {}
            other => return Err(other.unexpected("the desktop's confirmation")),
        }
        Ok(hub)
    }
    .await;
    report_to_peer(&mut channel, &result).await;
    let hub = result?;

    let hub_key = sync_keys::to_hex(channel.remote_static());
    let hub_id = hub.device_id.clone();
    let stored_address = address.clone();
    blocking(move || {
        // The reset mark and the peer record change what a data save and a
        // merge must do, so they are written under the data lock, but only
        // after the network part is over.
        let _guard = write_lock::lock_blocking();
        sync_exchange::pair_with_hub(&hub_id, &hub_key, &stored_address)
    })
    .await?;
    Ok(PairResult {
        hub_device_id: hub.device_id,
        hub_key_fingerprint: sync_keys::fingerprint(channel.remote_static()),
        address,
    })
}

/// How a Sync now ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// An ordinary exchange ran both ways.
    Exchanged,
    /// The round needs a reset of the phone, and the user has not confirmed
    /// it. Nothing changed on either side.
    ResetNeeded,
    /// The phone reset from the hub, after the hub repaired itself if the
    /// plan said so.
    ResetDone,
    /// The round failed; see [`LastSync::error`].
    Failed,
}

impl SyncOutcome {
    /// The name the API uses.
    pub fn as_str(self) -> &'static str {
        match self {
            SyncOutcome::Exchanged => "exchanged",
            SyncOutcome::ResetNeeded => "reset_needed",
            SyncOutcome::ResetDone => "reset_done",
            SyncOutcome::Failed => "failed",
        }
    }
}

/// The name the API uses for `plan`.
pub fn plan_name(plan: RoundPlan) -> &'static str {
    match plan {
        RoundPlan::Exchange => "exchange",
        RoundPlan::PhoneReset { .. } => "phone_reset",
        RoundPlan::HubRepair { .. } => "hub_repair",
    }
}

/// What a merge on the other device did with the changes this one sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerCounts {
    /// Changes that decide part of the other device's data.
    pub applied: usize,
    /// Changes stored there but set aside.
    pub skipped: usize,
    /// Changes the other device could not place.
    pub unplaceable: usize,
    /// Changes the other device already had.
    pub already_known: usize,
}

impl From<&MergeReport> for PeerCounts {
    fn from(report: &MergeReport) -> Self {
        PeerCounts {
            applied: report.summary.applied,
            skipped: report.summary.skipped,
            unplaceable: report.summary.unplaceable,
            already_known: report.summary.already_known,
        }
    }
}

/// What one round moved, in counts, from this device's point of view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncCounts {
    /// Log entries this device sent: its push, or the hub's reply or full
    /// log.
    pub sent: usize,
    /// Log entries this device received.
    pub received: usize,
    /// Received entries that decide part of this device's data.
    pub applied: usize,
    /// Received entries stored but set aside.
    pub skipped: usize,
    /// Received entries this device could not place.
    pub unplaceable: usize,
    /// On the phone: what the hub did with the phone's push, when it pushed.
    pub peer: Option<PeerCounts>,
}

impl SyncCounts {
    fn add_merge(&mut self, report: &MergeReport) {
        self.applied += report.summary.applied;
        self.skipped += report.summary.skipped;
        self.unplaceable += report.summary.unplaceable;
    }
}

/// The result of the last round on this device, kept in memory until the
/// process ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastSync {
    /// When the round ended, in milliseconds since the Unix epoch.
    pub finished_at_ms: i64,
    /// The other device, when the round got far enough to know it.
    pub peer_device_id: Option<String>,
    /// How it ended.
    pub outcome: SyncOutcome,
    /// The plan both sides agreed on, when they got that far.
    pub plan: Option<RoundPlan>,
    /// What moved.
    pub counts: SyncCounts,
    /// For a failed round: what went wrong, naming no value.
    pub error: Option<String>,
}

impl LastSync {
    fn new(peer_device_id: Option<String>, outcome: SyncOutcome, plan: RoundPlan) -> LastSync {
        LastSync {
            finished_at_ms: chrono::Utc::now().timestamp_millis(),
            peer_device_id,
            outcome,
            plan: Some(plan),
            counts: SyncCounts::default(),
            error: None,
        }
    }

    fn failed(peer_device_id: Option<String>, err: &Error) -> LastSync {
        LastSync {
            finished_at_ms: chrono::Utc::now().timestamp_millis(),
            peer_device_id,
            outcome: SyncOutcome::Failed,
            plan: None,
            counts: SyncCounts::default(),
            error: Some(log_text(err)),
        }
    }

    /// One line for the log, with counts only.
    fn summary(&self) -> String {
        format!(
            "{} with {} ({}): sent {}, received {}, applied {}, set aside {}, unplaceable {}",
            self.outcome.as_str(),
            self.peer_device_id
                .as_deref()
                .unwrap_or("an unknown device"),
            self.plan.map(plan_name).unwrap_or("no plan"),
            self.counts.sent,
            self.counts.received,
            self.counts.applied,
            self.counts.skipped,
            self.counts.unplaceable
        )
    }
}

static LAST_SYNC: Mutex<Option<LastSync>> = Mutex::new(None);

fn record_last(last: LastSync) {
    *LAST_SYNC.lock().unwrap_or_else(PoisonError::into_inner) = Some(last);
}

/// The result of the last round on this device since the process started.
pub fn last_sync() -> Option<LastSync> {
    LAST_SYNC
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// What Sync now did.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncNowResult {
    /// How it ended.
    pub outcome: SyncOutcome,
    /// The plan both sides agreed on.
    pub plan: RoundPlan,
    /// What moved.
    pub counts: SyncCounts,
    /// For [`SyncOutcome::ResetNeeded`]: what the reset would replace, in
    /// counts, for the user to confirm.
    pub reset_preview: Option<ResetPreview>,
    /// For [`SyncOutcome::ResetDone`]: the backup folder's name under the
    /// data folder's `backups`, holding the phone's data from before.
    pub backup_folder: Option<String>,
}

/// On the phone: run one round with the paired hub.
///
/// When the round needs a reset of the phone and `confirm_reset` is false,
/// the phone only counts what it holds, tells the hub it stops, and returns
/// [`SyncOutcome::ResetNeeded`] with those counts: nothing changes on either
/// device. With `confirm_reset` it performs the reset.
///
/// # Errors
///
/// [`Error::SyncRefused`] on the hub, when this phone has no hub, when
/// another sync runs, and when the desktop refuses or stops. The errors of
/// the [`crate::sync_exchange`] steps, and [`Error::Network`] and
/// [`Error::SyncProtocol`] as for [`pair`]. The result, success or failure,
/// is also kept for [`last_sync`].
pub async fn sync_now(confirm_reset: bool) -> Result<SyncNowResult> {
    require_role(SyncRole::Phone, "Sync now")?;
    let _slot = SessionSlot::try_take().ok_or_else(|| Error::SyncRefused(BUSY.to_string()))?;
    let hub = blocking(|| {
        sync_peers::load_peers()
            .map(|peers| peers.into_iter().find(|peer| peer.role == PeerRole::Hub))
    })
    .await;
    let hub_id = hub
        .as_ref()
        .ok()
        .and_then(|hub| hub.as_ref().map(|hub| hub.device_id.clone()));
    let result = match hub {
        Ok(Some(hub)) => phone_round(hub, confirm_reset).await,
        Ok(None) => Err(Error::SyncRefused(
            "this phone is not paired with a desktop. Pair it first.".to_string(),
        )),
        Err(err) => Err(err),
    };
    match &result {
        Ok(done) => {
            let mut last = LastSync::new(hub_id, done.outcome, done.plan);
            last.counts = done.counts.clone();
            println!("Sync: {}", last.summary());
            record_last(last);
        }
        Err(err) => {
            eprintln!("Sync: Sync now failed: {}", log_text(err));
            record_last(LastSync::failed(hub_id, err));
        }
    }
    result
}

async fn phone_round(hub: PeerRecord, confirm_reset: bool) -> Result<SyncNowResult> {
    let pair_again = |what: &str| {
        Error::SyncRefused(format!(
            "this phone's record of its desktop has no {what}. Unpair the desktop and pair again."
        ))
    };
    let hub_key = hub
        .static_key
        .as_deref()
        .and_then(sync_keys::key_from_hex)
        .ok_or_else(|| pair_again("valid key"))?;
    let address = hub.address.clone().ok_or_else(|| pair_again("address"))?;
    let keys = blocking(sync_keys::local_keypair).await?;

    let limits = Limits::DEFAULT;
    let stream = connect(&address, &limits).await?;
    let mut channel = sync_net::sync_as_phone(stream, &keys, &hub_key, &limits).await?;
    let result = phone_round_steps(&mut channel, &hub.device_id, confirm_reset).await;
    report_to_peer(&mut channel, &result).await;
    result
}

async fn phone_round_steps<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<S>,
    hub_id: &str,
    confirm_reset: bool,
) -> Result<SyncNowResult> {
    let own = blocking(sync_exchange::phone_hello).await?;
    send(channel, &WireMessage::Sync(SyncMessage::Hello(own.clone()))).await?;
    let hub_hello = match recv(channel, "desktop").await? {
        WireMessage::Sync(message) => message.into_hello()?,
        other => return Err(other.unexpected("a hello")),
    };
    if hub_hello.device_id != hub_id {
        check_plain_device_id(&hub_hello.device_id)?;
        return Err(Error::SyncRefused(format!(
            "the desktop introduced itself as device {}, but this phone is paired with device \
             {hub_id}",
            hub_hello.device_id
        )));
    }
    let (own_copy, hub_copy) = (own.clone(), hub_hello.clone());
    let plan = blocking(move || sync_exchange::phone_read_hello(&own_copy, &hub_copy)).await?;
    let mut result = SyncNowResult {
        outcome: SyncOutcome::Exchanged,
        plan,
        counts: SyncCounts::default(),
        reset_preview: None,
        backup_folder: None,
    };

    let push_first = match plan {
        RoundPlan::Exchange => {
            phone_push(channel, &hub_hello, &mut result.counts).await?;
            let reply = match recv(channel, "desktop").await? {
                WireMessage::Sync(message) => message.into_reply()?,
                other => return Err(other.unexpected("a reply")),
            };
            result.counts.received = reply.entries.len();
            let report = blocking(move || sync_exchange::phone_apply_reply(&reply)).await?;
            result.counts.add_merge(&report);
            return Ok(result);
        }
        RoundPlan::PhoneReset { push_first } | RoundPlan::HubRepair { push_first } => push_first,
    };

    if !confirm_reset {
        let hub_copy = hub_hello.clone();
        let preview = blocking(move || sync_exchange::phone_reset_preview(&hub_copy)).await?;
        send(channel, &WireMessage::Done).await?;
        result.outcome = SyncOutcome::ResetNeeded;
        result.reset_preview = Some(preview);
        return Ok(result);
    }
    if push_first {
        phone_push(channel, &hub_hello, &mut result.counts).await?;
    } else {
        send(channel, &WireMessage::Proceed).await?;
    }
    let full: FullLog = match recv(channel, "desktop").await? {
        WireMessage::Sync(message) => message.into_full_log()?,
        other => return Err(other.unexpected("the desktop's full log")),
    };
    result.counts.received = full.entries.len();
    let report = blocking(move || sync_exchange::reset_phone_from_hub(&full)).await?;
    result.counts.add_merge(&report.merge);
    result.outcome = SyncOutcome::ResetDone;
    result.backup_folder = Some(report.backup_folder);
    Ok(result)
}

/// Push what the hub lacks, and read the hub's counts for it.
async fn phone_push<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<S>,
    hub_hello: &Hello,
    counts: &mut SyncCounts,
) -> Result<()> {
    let hub_copy = hub_hello.clone();
    let push = blocking(move || sync_exchange::phone_push(&hub_copy)).await?;
    counts.sent = push.entries.len();
    send(channel, &WireMessage::Sync(SyncMessage::Push(push))).await?;
    match recv(channel, "desktop").await? {
        WireMessage::PushReport(peer) => {
            counts.peer = Some(peer);
            Ok(())
        }
        other => Err(other.unexpected("the desktop's counts")),
    }
}

// ------------------------------------------------------------------
// Status and unpairing
// ------------------------------------------------------------------

/// Everything the Sync page shows.
#[derive(Debug, Clone)]
pub struct SyncStatus {
    /// This device's role.
    pub role: SyncRole,
    /// This device's id.
    pub device_id: String,
    /// The fingerprint of this device's static key.
    pub key_fingerprint: String,
    /// The paired devices, or empty when [`SyncStatus::peers_error`] is set.
    pub peers: Vec<PeerRecord>,
    /// Why the peers file cannot be read, with the way to recover.
    pub peers_error: Option<String>,
    /// The local log's health, unless it cannot be judged.
    pub log_health: Option<LogHealth>,
    /// Why the log's health cannot be judged.
    pub log_health_error: Option<String>,
    /// On the hub: the listener.
    pub listener: Option<ListenerStatus>,
    /// The last round since the process started.
    pub last_sync: Option<LastSync>,
    /// On the hub: settings files that could not be recorded. Empty on a phone
    /// and when all settings were recorded.
    pub settings_sync_pending: Vec<String>,
}

/// This device's sync status. Blocking: it reads the log and the config
/// files, so call it from [`tokio::task::spawn_blocking`]. It creates the
/// static key if there is none yet.
///
/// # Errors
///
/// The errors of [`crate::sync::device_id`] and
/// [`sync_keys::local_keypair`]. A peers file or a log that cannot be read is
/// reported in the result instead.
pub fn status() -> Result<SyncStatus> {
    let role = role();
    let keys: StaticKeypair = sync_keys::local_keypair()?;
    let (peers, peers_error) = match sync_peers::load_peers() {
        Ok(peers) => (peers, None),
        Err(err) => (Vec::new(), Some(log_text(&err))),
    };
    let (log_health, log_health_error) = match sync_exchange::log_health() {
        Ok(health) => (Some(health), None),
        Err(err) => (None, Some(log_text(&err))),
    };
    Ok(SyncStatus {
        role,
        device_id: sync::device_id()?,
        key_fingerprint: keys.fingerprint(),
        peers,
        peers_error,
        log_health,
        log_health_error,
        listener: (role == SyncRole::Hub).then(listener_status),
        last_sync: last_sync(),
        settings_sync_pending: if role == SyncRole::Hub {
            crate::sync_baseline::settings_baseline_pending().unwrap_or_default()
        } else {
            Vec::new()
        },
    })
}

/// Unpair `device_id` on this side. The other device keeps its record until
/// it unpairs too; a sync between the two is refused meanwhile.
///
/// # Errors
///
/// [`Error::NotFound`] when `device_id` is not paired, and the errors of
/// [`sync_peers::remove_peer`].
pub fn unpair(device_id: &str) -> Result<()> {
    if sync_peers::remove_peer(device_id)? {
        println!("Sync: unpaired device {device_id}");
        Ok(())
    } else {
        Err(Error::NotFound(format!(
            "device {device_id} is not paired with this device"
        )))
    }
}

// ------------------------------------------------------------------
// Messages inside the channel
// ------------------------------------------------------------------

/// Who a device says it is, inside the pairing channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DeviceIdentity {
    version: u32,
    device_id: String,
    role: PeerRole,
}

impl DeviceIdentity {
    /// Refuse an identity of another version, another role than `role`, or
    /// a device id that is not a plain id.
    fn check(&self, role: PeerRole) -> Result<()> {
        if self.version != sync_exchange::PROTOCOL_VERSION {
            return Err(Error::SyncProtocol(format!(
                "the other device speaks sync protocol version {}, and this device speaks version \
                 {}. Install the same app version on both devices.",
                self.version,
                sync_exchange::PROTOCOL_VERSION
            )));
        }
        if self.role != role {
            return Err(Error::SyncRefused(format!(
                "the other device is not a {}",
                String::from(role)
            )));
        }
        check_plain_device_id(&self.device_id)
    }
}

fn check_plain_device_id(device_id: &str) -> Result<()> {
    let plain = !device_id.is_empty()
        && device_id.len() <= 64
        && device_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    if plain {
        Ok(())
    } else {
        Err(Error::SyncProtocol(
            "the other device sent a device id that is not a plain id".to_string(),
        ))
    }
}

/// One application message inside the channel.
///
/// Every variant but [`WireMessage::Sync`] holds only ids, counts, and
/// fixed words. `Debug` goes through [`SyncMessage`]'s, which prints counts
/// only.
#[derive(Debug)]
enum WireMessage {
    /// A message of [`crate::sync_exchange`].
    Sync(SyncMessage),
    /// The sender stopped, for this reason, which is a
    /// [`Error::peer_safe_message`].
    Error(String),
    /// The sender has finished its part: the hub after recording a pairing,
    /// or the phone ending a round that needs a reset it has not confirmed.
    Done,
    /// The phone confirms a reset without pushing first.
    Proceed,
    /// The sender's identity, while pairing.
    Identity(DeviceIdentity),
    /// What the hub did with the phone's push.
    PushReport(PeerCounts),
}

/// The longest error text sent or shown from the other device, in chars.
const MAX_PEER_ERROR: usize = 500;

impl WireMessage {
    fn identity(device_id: String, role: PeerRole) -> WireMessage {
        WireMessage::Identity(DeviceIdentity {
            version: sync_exchange::PROTOCOL_VERSION,
            device_id,
            role,
        })
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let (tag, body) = match self {
            WireMessage::Sync(message) => (1u8, message.encode()?),
            WireMessage::Error(text) => (
                2,
                text.chars()
                    .take(MAX_PEER_ERROR)
                    .collect::<String>()
                    .into_bytes(),
            ),
            WireMessage::Done => (3, Vec::new()),
            WireMessage::Proceed => (4, Vec::new()),
            WireMessage::Identity(identity) => (5, serde_json::to_vec(identity)?),
            WireMessage::PushReport(counts) => (6, serde_json::to_vec(counts)?),
        };
        let mut bytes = Vec::with_capacity(body.len() + 1);
        bytes.push(tag);
        bytes.extend_from_slice(&body);
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<WireMessage> {
        let unreadable = || {
            Error::SyncProtocol("the other device sent a message this version cannot read".into())
        };
        let (tag, body) = bytes.split_first().ok_or_else(unreadable)?;
        Ok(match tag {
            1 => WireMessage::Sync(SyncMessage::decode(body)?),
            2 => WireMessage::Error(
                String::from_utf8_lossy(body)
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_PEER_ERROR)
                    .collect(),
            ),
            3 if body.is_empty() => WireMessage::Done,
            4 if body.is_empty() => WireMessage::Proceed,
            5 => WireMessage::Identity(serde_json::from_slice(body).map_err(|_| unreadable())?),
            6 => WireMessage::PushReport(serde_json::from_slice(body).map_err(|_| unreadable())?),
            _ => return Err(unreadable()),
        })
    }

    fn kind(&self) -> &'static str {
        match self {
            WireMessage::Sync(SyncMessage::Hello(_)) => "a hello",
            WireMessage::Sync(SyncMessage::Push(_)) => "a push",
            WireMessage::Sync(SyncMessage::Reply(_)) => "a reply",
            WireMessage::Sync(SyncMessage::FullLog(_)) => "a full log",
            WireMessage::Error(_) => "an error",
            WireMessage::Done => "done",
            WireMessage::Proceed => "proceed",
            WireMessage::Identity(_) => "an identity",
            WireMessage::PushReport(_) => "a push report",
        }
    }

    fn unexpected(&self, expected: &str) -> Error {
        Error::SyncProtocol(format!(
            "expected {expected} from the other device, got {}",
            self.kind()
        ))
    }
}

async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<S>,
    message: &WireMessage,
) -> Result<()> {
    channel.send(&message.encode()?).await
}

/// Receive the next message. An error message from `peer` becomes
/// [`Error::SyncRefused`] naming it.
async fn recv<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<S>,
    peer: &str,
) -> Result<WireMessage> {
    match WireMessage::decode(&channel.recv().await?)? {
        WireMessage::Error(text) => Err(peer_stopped(peer, &text)),
        other => Ok(other),
    }
}

fn peer_stopped(peer: &str, text: &str) -> Error {
    Error::SyncRefused(format!("the {peer} stopped the sync: {text}"))
}

/// After a failed step, tell the other device why, in its peer-safe form.
/// Skipped when the connection itself failed, since nothing would arrive,
/// and when the other device reported the failure itself. Best effort: the
/// connection closes next either way.
async fn report_to_peer<S: AsyncRead + AsyncWrite + Unpin, T>(
    channel: &mut SecureChannel<S>,
    result: &Result<T>,
) {
    let Err(err) = result else { return };
    if matches!(err, Error::Network(_)) {
        return;
    }
    let _ = send(channel, &WireMessage::Error(err.peer_safe_message())).await;
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

/// Run blocking `work`, such as a step of [`crate::sync_exchange`], on
/// tokio's blocking pool. The steps that change data take the write lock
/// with a blocking call, which panics inside an async task.
async fn blocking<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work).await.map_err(|err| {
        Error::Io(std::io::Error::other(format!(
            "a sync step stopped unexpectedly: {err}"
        )))
    })?
}

/// `err` for this device's own log: the full text of the variants that name
/// only files, counts, ids, and positions, and only the kind of the others,
/// whose text can quote a value, a name, or a category.
fn log_text(err: &Error) -> String {
    match err {
        Error::Io(_)
        | Error::Network(_)
        | Error::SyncProtocol(_)
        | Error::SyncRefused(_)
        | Error::MergeRejected(_)
        | Error::SyncLogLocked { .. }
        | Error::NoHomeDir => err.to_string(),
        Error::Json(_) => "a JSON error (its text is not logged, as it can quote data)".into(),
        Error::Polars(_) => {
            "a data table error (its text is not logged, as it can quote data)".into()
        }
        Error::InvalidArgument(_) | Error::NotFound(_) | Error::AlreadyExists(_) => {
            "a data error (its text is not logged, as it can quote data)".into()
        }
        Error::RowIdsMissing(_) => "a table without row ids".into(),
        Error::SyncResetBackup { path, .. } => {
            format!(
                "the backup before a sync reset failed for {}",
                path.display()
            )
        }
        Error::RowIdMigration { path, .. } => {
            format!("the row ID migration failed for {}", path.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point every filesystem base directory at a fresh folder before a test
    /// drives a service path that loads sync keys or peers.
    fn with_temp_env() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            std::env::set_var("HOME", dir.path());
            std::env::set_var("FINGUARD_FX_OFFLINE", "1");
        }
        dir
    }

    /// A code works until it is used, withdrawn after three failures, and
    /// gone after five minutes; a failure with a replaced code costs the new
    /// one nothing.
    #[test]
    fn pairing_codes_follow_their_rules() {
        let start = Instant::now();
        let mut slot = PairCodeSlot::new();
        assert!(slot.current(start).is_err(), "no code at first");

        slot.issue("123456".to_string(), start, 0);
        assert_eq!(slot.current(start).unwrap().code, "123456");
        slot.finish("123456", true);
        assert!(slot.current(start).is_err(), "a code pairs once");

        slot.issue("123456".to_string(), start, 0);
        slot.finish("123456", false);
        slot.finish("123456", false);
        assert!(slot.current(start).is_ok(), "two failures leave the code");
        slot.finish("123456", false);
        assert!(
            slot.current(start).is_err(),
            "the third failure withdraws it"
        );

        let issued = slot.issue("654321".to_string(), start, 1_000);
        assert_eq!(issued.expires_at_ms, 1_000 + 5 * 60 * 1000);
        slot.finish("111111", false);
        slot.finish("111111", false);
        slot.finish("111111", false);
        assert!(
            slot.current(start).is_ok(),
            "another code's failures cost nothing"
        );
        assert_eq!(slot.expires_at_ms(start), Some(issued.expires_at_ms));
        let message = slot
            .current(start + PAIR_CODE_LIFETIME)
            .expect_err("expired");
        assert!(message.contains("expired"), "{message}");
        assert!(slot.current(start).is_err(), "an expired code is dropped");
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..50 {
            let code = random_code().unwrap();
            assert_eq!(code.len(), 6);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
        }
        assert_eq!(normalize_code(" 012 345 ").unwrap(), "012345");
        assert!(normalize_code("12345").is_err());
        assert!(normalize_code("12345a").is_err());
    }

    #[test]
    fn addresses_get_the_default_port() {
        assert_eq!(normalize_address("192.0.2.4").unwrap(), "192.0.2.4:3112");
        assert_eq!(
            normalize_address(" 192.0.2.4:4000 ").unwrap(),
            "192.0.2.4:4000"
        );
        assert_eq!(normalize_address("fe80::1").unwrap(), "[fe80::1]:3112");
        assert_eq!(normalize_address("desk.local").unwrap(), "desk.local:3112");
        assert_eq!(normalize_address("desk.local:9").unwrap(), "desk.local:9");
        assert!(normalize_address("").is_err());
        assert!(normalize_address("a b").is_err());
    }

    #[test]
    #[serial_test::serial]
    fn listener_remaining_is_zero_after_grace() {
        hub_state().last_heartbeat = Some(Instant::now() - LISTEN_GRACE - Duration::from_secs(1));
        assert!(listener_remaining().is_zero());
        hub_state().last_heartbeat = None;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn discovery_responder_exits_after_grace() {
        hub_state().last_heartbeat = Some(Instant::now() - LISTEN_GRACE - Duration::from_secs(1));
        let bind = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            crate::sync_discovery::serve_until_listener_closes(bind, "id".into(), "fp".into()),
        )
        .await;
        assert!(result.is_ok());
        hub_state().last_heartbeat = None;
    }

    /// Every message survives encoding, and an error text loses control
    /// characters on the way in.
    #[test]
    fn wire_messages_round_trip() {
        let messages = [
            WireMessage::Done,
            WireMessage::Proceed,
            WireMessage::identity("abc-123".to_string(), PeerRole::Hub),
            WireMessage::PushReport(PeerCounts {
                applied: 1,
                skipped: 2,
                unplaceable: 3,
                already_known: 4,
            }),
            WireMessage::Error("stopped".to_string()),
        ];
        for message in messages {
            let decoded = WireMessage::decode(&message.encode().unwrap()).unwrap();
            assert_eq!(format!("{decoded:?}"), format!("{message:?}"));
        }
        let decoded = WireMessage::decode(b"\x02bad\x1b[31m text").unwrap();
        assert_eq!(format!("{decoded:?}"), r#"Error("bad[31m text")"#);
        assert!(WireMessage::decode(b"").is_err());
        assert!(WireMessage::decode(b"\x09").is_err());
        assert!(WireMessage::decode(b"\x03x").is_err());
    }

    #[test]
    fn identities_are_checked() {
        let good = DeviceIdentity {
            version: sync_exchange::PROTOCOL_VERSION,
            device_id: "0b6f-11".to_string(),
            role: PeerRole::Hub,
        };
        assert!(good.check(PeerRole::Hub).is_ok());
        assert!(matches!(
            good.check(PeerRole::Phone),
            Err(Error::SyncRefused(_))
        ));
        let odd = DeviceIdentity {
            device_id: "../x".to_string(),
            ..good.clone()
        };
        assert!(odd.check(PeerRole::Hub).is_err());
        let newer = DeviceIdentity {
            version: sync_exchange::PROTOCOL_VERSION + 1,
            ..good
        };
        assert!(matches!(
            newer.check(PeerRole::Hub),
            Err(Error::SyncProtocol(_))
        ));
    }

    /// Each role refuses the other's actions, and the default role of a
    /// desktop build is the hub.
    #[tokio::test]
    #[serial_test::serial]
    async fn each_role_refuses_the_others_actions() {
        override_role_for_tests(None);
        assert_eq!(role(), SyncRole::Hub);
        assert!(matches!(sync_now(false).await, Err(Error::SyncRefused(_))));
        assert!(matches!(
            pair("192.0.2.1", "123456").await,
            Err(Error::SyncRefused(_))
        ));

        override_role_for_tests(Some(SyncRole::Phone));
        let listen = heartbeat().await;
        let code = new_pair_code();
        override_role_for_tests(None);
        assert!(matches!(listen, Err(Error::SyncRefused(_))));
        assert!(matches!(code, Err(Error::SyncRefused(_))));
    }

    /// A sync port already in use is reported by the heartbeat as a bind
    /// error, and the heartbeat itself succeeds.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_taken_port_is_reported_not_raised() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        unsafe {
            std::env::set_var("FINGUARD_SYNC_HOST", "127.0.0.1");
            std::env::set_var("FINGUARD_SYNC_PORT", port.to_string());
        }
        let status = heartbeat().await;
        unsafe {
            std::env::remove_var("FINGUARD_SYNC_HOST");
            std::env::remove_var("FINGUARD_SYNC_PORT");
        }
        let status = status.expect("the heartbeat itself succeeds");
        assert!(!status.listening);
        assert_eq!(status.port, port);
        assert!(status.bind_error.is_some(), "{status:?}");
        assert_eq!(status.address_hint, Some(format!("127.0.0.1:{port}")));
    }

    /// A busy device does not reveal its busy state to an unauthenticated phone.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_second_session_is_refused() {
        let _temp = with_temp_env();
        let slot = SessionSlot::try_take().expect("free");
        let (mut phone_end, hub_end) = tokio::io::duplex(4096);
        let limits = Limits {
            handshake: Duration::from_secs(5),
            ..Limits::DEFAULT
        };
        let hub = tokio::spawn(async move { hub_session(hub_end, &limits).await });
        let keys = StaticKeypair::generate().unwrap();
        let phone = sync_net::sync_as_phone(&mut phone_end, &keys, &[9u8; KEY_LEN], &limits).await;
        let message = phone.expect_err("refused").to_string();
        assert!(message.contains("not paired"), "{message}");
        assert!(matches!(hub.await.unwrap(), Err(Error::SyncRefused(_))));
        drop(slot);
        assert!(SessionSlot::try_take().is_some(), "released on drop");
    }
}

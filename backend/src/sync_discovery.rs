//! Discovery of an open desktop on the local network.
//!
//! The phone sends one exact UDP broadcast query. A desktop answers directly
//! to the sender while its TCP sync listener is alive. The datagram contains
//! only the address, device id, and key fingerprint needed before pairing.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

use crate::error::{Error, Result};
use crate::sync_service::{self, SyncRole};

/// The query accepted by a desktop. Matching is deliberately exact.
pub const DISCOVERY_QUERY: &[u8] = b"finguard-sync/1 discover";
/// How long a phone listens after its broadcast.
pub const DISCOVERY_WINDOW: Duration = Duration::from_secs(3);

/// The three values a desktop publishes for pairing verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryReply {
    pub address: String,
    pub device_id: String,
    pub key_fingerprint: String,
}

fn discovery_port() -> u16 {
    std::env::var("FINGUARD_SYNC_PORT")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(sync_service::DEFAULT_SYNC_PORT)
}

/// Answer discovery queries until the TCP listener's heartbeat grace expires.
pub async fn serve_until_listener_closes(
    bind: SocketAddr,
    device_id: String,
    key_fingerprint: String,
) {
    let socket = match UdpSocket::bind(bind).await {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("Sync discovery: cannot listen on {bind}: {err}");
            return;
        }
    };
    let mut buffer = [0u8; 256];
    loop {
        let remaining = sync_service::listener_remaining();
        if remaining.is_zero() {
            return;
        }
        let (length, from) =
            match tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await {
                Ok(Ok(received)) => received,
                Ok(Err(err)) => {
                    eprintln!("Sync discovery: receiving a query failed: {err}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
                Err(_) => {
                    if sync_service::listener_remaining().is_zero() {
                        return;
                    }
                    continue;
                }
            };
        if &buffer[..length] != DISCOVERY_QUERY {
            continue;
        }
        let local_ip = match local_ip_for(from) {
            Ok(ip) => ip,
            Err(err) => {
                eprintln!("Sync discovery: cannot determine reply address: {err}");
                continue;
            }
        };
        let reply = DiscoveryReply {
            address: SocketAddr::new(local_ip, bind.port()).to_string(),
            device_id: device_id.clone(),
            key_fingerprint: key_fingerprint.clone(),
        };
        let Ok(bytes) = serde_json::to_vec(&reply) else {
            continue;
        };
        let _ = socket.send_to(&bytes, from).await;
    }
}

fn local_ip_for(destination: SocketAddr) -> Result<IpAddr> {
    let unspecified = if destination.is_ipv6() {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    } else {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    };
    let socket = std::net::UdpSocket::bind(SocketAddr::new(unspecified, 0))
        .map_err(|err| Error::Network(format!("cannot choose discovery interface: {err}")))?;
    socket
        .connect(destination)
        .map_err(|err| Error::Network(format!("cannot choose discovery interface: {err}")))?;
    socket
        .local_addr()
        .map(|local| local.ip())
        .map_err(|err| Error::Network(format!("cannot read discovery address: {err}")))
}

/// Broadcast once and collect distinct desktop replies. No reply is normal.
pub async fn probe() -> Result<Vec<DiscoveryReply>> {
    if sync_service::role() != SyncRole::Phone {
        return Err(Error::SyncRefused(
            "Discovering desktops works only on the phone".to_string(),
        ));
    }
    let port = discovery_port();
    probe_at(SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), port)).await
}

async fn probe_at(target: SocketAddr) -> Result<Vec<DiscoveryReply>> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))
        .await
        .map_err(|err| Error::Network(format!("cannot open discovery socket: {err}")))?;
    socket
        .set_broadcast(true)
        .map_err(|err| Error::Network(format!("cannot enable discovery broadcast: {err}")))?;
    if let Err(err) = socket.send_to(DISCOVERY_QUERY, target).await {
        eprintln!("Sync discovery: cannot send broadcast query: {err}");
        return Ok(Vec::new());
    }

    let deadline = tokio::time::Instant::now() + DISCOVERY_WINDOW;
    let mut replies = Vec::new();
    let mut buffer = [0u8; 2048];
    while let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        if remaining.is_zero() {
            break;
        }
        let Ok(Ok((length, _))) =
            tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await
        else {
            break;
        };
        let Ok(reply) = serde_json::from_slice::<DiscoveryReply>(&buffer[..length]) else {
            continue;
        };
        if !replies.contains(&reply) {
            replies.push(reply);
        }
    }
    Ok(replies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn query_requires_exact_bytes() {
        assert_eq!(DISCOVERY_QUERY, b"finguard-sync/1 discover");
        assert_ne!(DISCOVERY_QUERY, b"finguard-sync/1 discover\n");
    }

    #[test]
    fn reply_round_trips_without_extra_fields() {
        let reply = DiscoveryReply {
            address: "127.0.0.1:3112".to_string(),
            device_id: "desktop".to_string(),
            key_fingerprint: "abcd".to_string(),
        };
        let value: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&reply).unwrap()).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 3);
        assert_eq!(
            serde_json::from_value::<DiscoveryReply>(value).unwrap(),
            reply
        );
    }

    #[tokio::test]
    #[serial]
    async fn probe_collects_distinct_replies() {
        sync_service::override_role_for_tests(Some(SyncRole::Phone));
        let port = std::net::UdpSocket::bind("0.0.0.0:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        unsafe { std::env::set_var("FINGUARD_SYNC_PORT", port.to_string()) };
        let responder = UdpSocket::bind(("0.0.0.0", port)).await.unwrap();
        let task = tokio::spawn(async move {
            let mut query = [0u8; 128];
            let (_, from) = responder.recv_from(&mut query).await.unwrap();
            let reply = DiscoveryReply {
                address: "192.0.2.1:3112".to_string(),
                device_id: "one".to_string(),
                key_fingerprint: "aaaa".to_string(),
            };
            let other = DiscoveryReply {
                address: "192.0.2.2:3112".to_string(),
                device_id: "two".to_string(),
                key_fingerprint: "bbbb".to_string(),
            };
            for item in [reply.clone(), reply, other] {
                responder
                    .send_to(&serde_json::to_vec(&item).unwrap(), from)
                    .await
                    .unwrap();
            }
        });
        let replies = probe_at(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .await
            .unwrap();
        task.await.unwrap();
        assert_eq!(replies.len(), 2);
        sync_service::override_role_for_tests(None);
    }
}

//! Discovery exchange and route role checks over loopback sockets.

use std::time::Duration;

use finguard_rs_backend::{api, sync_discovery, sync_service};
use tokio::net::UdpSocket;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_answers_queries_and_phone_route_returns_a_list() {
    let root = tempfile::tempdir().unwrap();
    for name in ["data", "config", "home"] {
        std::fs::create_dir_all(root.path().join(name)).unwrap();
    }
    unsafe {
        std::env::set_var("XDG_DATA_HOME", root.path().join("data"));
        std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
        std::env::set_var("HOME", root.path().join("home"));
        std::env::set_var("FINGUARD_FX_OFFLINE", "1");
        std::env::set_var("FINGUARD_SYNC_HOST", "127.0.0.1");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::env::set_var("FINGUARD_SYNC_PORT", port.to_string());
    }
    sync_service::override_role_for_tests(Some(sync_service::SyncRole::Hub));
    let listener = sync_service::heartbeat().await.unwrap();
    let port = listener.port;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut bytes = [0u8; 2048];
    let mut reply_length = None;
    for _ in 0..20 {
        socket
            .send_to(sync_discovery::DISCOVERY_QUERY, ("127.0.0.1", port))
            .await
            .unwrap();
        if let Ok(Ok((length, _))) =
            tokio::time::timeout(Duration::from_millis(100), socket.recv_from(&mut bytes)).await
        {
            reply_length = Some(length);
            break;
        }
    }
    socket.send_to(b"junk", ("127.0.0.1", port)).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.recv_from(&mut [0u8; 64]))
            .await
            .is_err(),
        "junk datagrams must not receive replies"
    );
    let length = reply_length.expect("discovery responder did not become ready");
    let reply: sync_discovery::DiscoveryReply = serde_json::from_slice(&bytes[..length]).unwrap();
    assert_eq!(reply.address, format!("127.0.0.1:{port}"));
    assert!(!reply.device_id.is_empty());
    assert!(!reply.key_fingerprint.is_empty());

    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_address = http_listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(http_listener, api::router()).await.unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let hub_response = client
        .post(format!("http://{http_address}/api/sync/discover"))
        .send()
        .await
        .unwrap();
    assert_eq!(hub_response.status(), reqwest::StatusCode::CONFLICT);

    sync_service::override_role_for_tests(Some(sync_service::SyncRole::Phone));
    let phone_response = client
        .post(format!("http://{http_address}/api/sync/discover"))
        .send()
        .await
        .unwrap();
    assert_eq!(phone_response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        phone_response
            .json::<Vec<sync_discovery::DiscoveryReply>>()
            .await
            .unwrap(),
        []
    );
    server.abort();
    sync_service::override_role_for_tests(None);
}

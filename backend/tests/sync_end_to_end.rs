//! Pairing and sync end to end, over real sockets, between two devices in
//! two processes.
//!
//! The hub is the real `finguard_rs_backend` binary, started as a child
//! process. The phone is this test process: it serves `api::router()` on a
//! local port, as the desktop binary would, with the role override set to
//! phone. Each device has its own scratch `XDG_DATA_HOME`, `XDG_CONFIG_HOME`,
//! and `HOME`, and both run with `FINGUARD_FX_OFFLINE=1`, so nothing here
//! reads or writes the default paths, which in the dev container hold the
//! user's real records, and nothing reaches the network beyond 127.0.0.1.
//!
//! Environment variables are process global, which is why the two devices
//! need two processes. This file holds one test so nothing else in this
//! process changes them.

use std::net::TcpListener as StdListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use finguard_rs_backend::{api, row_id_migration, sync_baseline, sync_service};

/// Kills the hub process when dropped, so a failed assertion does not leave
/// it running.
struct HubProcess(Child);

impl Drop for HubProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Reserve two distinct ephemeral ports while selecting them, so the second
/// lookup cannot return the first port.
fn distinct_free_ports() -> (u16, u16) {
    let first = StdListener::bind("127.0.0.1:0").unwrap();
    let first_port = first.local_addr().unwrap().port();
    loop {
        let second = StdListener::bind("127.0.0.1:0").unwrap();
        let second_port = second.local_addr().unwrap().port();
        if second_port != first_port {
            return (first_port, second_port);
        }
    }
}

/// `data`, `config`, and `home` folders for one device under `dir`.
fn device_dirs(dir: &Path) {
    for name in ["data", "config", "home"] {
        std::fs::create_dir_all(dir.join(name)).unwrap();
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap()
}

/// Send `method` to `url` with an optional JSON body, and return the status
/// and the body as JSON (`Value::Null` for an empty body).
async fn call(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = client.request(method, url);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|err| panic!("{url}: {err}"));
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, value)
}

async fn post_ok(client: &reqwest::Client, url: &str, body: Value) -> Value {
    let (status, value) = call(client, reqwest::Method::POST, url, Some(body)).await;
    assert_eq!(status, 200, "POST {url}: {value}");
    value
}

async fn get_ok(client: &reqwest::Client, url: &str) -> Value {
    let (status, value) = call(client, reqwest::Method::GET, url, None).await;
    assert_eq!(status, 200, "GET {url}: {value}");
    value
}

fn expense(month: u32, day: u32, name: &str, amount: f64) -> Value {
    json!({
        "id": "", "year": 2026, "month": month, "day": day, "name": name,
        "amount": amount, "currency": "EUR", "primary": "Food", "secondary": "Groceries"
    })
}

/// Everything the test compares between the devices, in a form where equal
/// data gives equal values: expenses sorted by id.
async fn snapshot(client: &reqwest::Client, base: &str) -> Value {
    let mut months = Vec::new();
    for month in [3, 4] {
        let list = get_ok(
            client,
            &format!("{base}/api/expenses?year=2026&month={month}"),
        )
        .await;
        let mut expenses = list["expenses"].as_array().unwrap().clone();
        expenses.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        months.push(Value::Array(expenses));
    }
    json!({
        "expenses": months,
        "liquidity": get_ok(client, &format!("{base}/api/liquidity?year=2026")).await,
        "income": get_ok(client, &format!("{base}/api/cashflow/income?year=2026")).await,
    })
}

/// Wait until the hub answers HTTP, or fail after a while.
async fn wait_for(client: &reqwest::Client, base: &str, hub: &mut HubProcess) {
    let start = Instant::now();
    loop {
        if let Ok(response) = client.get(format!("{base}/api/years")).send().await
            && response.status().is_success()
        {
            return;
        }
        if let Some(status) = hub.0.try_wait().unwrap() {
            panic!("the hub exited early: {status}");
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the hub never answered"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn status_without_host(address: &str) -> u16 {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /api/years HTTP/1.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    response
        .split(|byte| *byte == b' ')
        .nth(1)
        .and_then(|status| std::str::from_utf8(status).ok())
        .and_then(|status| status.lines().next())
        .and_then(|status| status.parse().ok())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_pairs_resets_and_syncs_with_a_hub_process() {
    let root = tempfile::tempdir().unwrap();
    let (hub_dir, phone_dir) = (root.path().join("hub"), root.path().join("phone"));
    device_dirs(&hub_dir);
    device_dirs(&phone_dir);
    let client = client();

    // The hub: the real binary, bound to 127.0.0.1 only.
    let (hub_port, sync_port) = distinct_free_ports();
    let mut hub = HubProcess(
        Command::new(env!("CARGO_BIN_EXE_finguard_rs_backend"))
            .env("XDG_DATA_HOME", hub_dir.join("data"))
            .env("XDG_CONFIG_HOME", hub_dir.join("config"))
            .env("HOME", hub_dir.join("home"))
            .env("FINGUARD_FX_OFFLINE", "1")
            .env("FINGUARD_HOST", "127.0.0.1")
            .env("FINGUARD_PORT", hub_port.to_string())
            .env("FINGUARD_SYNC_HOST", "127.0.0.1")
            .env("FINGUARD_SYNC_PORT", sync_port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the hub"),
    );
    let hub_url = format!("http://127.0.0.1:{hub_port}");
    wait_for(&client, &hub_url, &mut hub).await;

    let cross_origin = client
        .get(format!("{hub_url}/api/years"))
        .header("Origin", "https://untrusted.example")
        .send()
        .await
        .unwrap();
    assert!(
        !cross_origin
            .headers()
            .contains_key("access-control-allow-origin")
    );
    let rebinding = client
        .get(format!("{hub_url}/api/years"))
        .header("Host", "attacker.example")
        .send()
        .await
        .unwrap();
    assert_eq!(rebinding.status(), reqwest::StatusCode::FORBIDDEN);
    for host in [
        format!("localhost:{hub_port}"),
        format!("backend:{hub_port}"),
    ] {
        let response = client
            .get(format!("{hub_url}/api/years"))
            .header("Host", host)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
    }

    // The phone: this process, pointed at its own folders before anything
    // reads them, started the way the Android app starts.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", phone_dir.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", phone_dir.join("config"));
        std::env::set_var("HOME", phone_dir.join("home"));
        std::env::set_var("FINGUARD_FX_OFFLINE", "1");
    }
    sync_service::override_role_for_tests(Some(sync_service::SyncRole::Phone));
    row_id_migration::migrate_row_ids().unwrap();
    sync_baseline::baseline_change_log().unwrap();
    let phone_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let phone_url = format!("http://{}", phone_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(phone_listener, api::router()).await });
    assert_eq!(status_without_host(&phone_url[7..]).await, 200);

    // Seed the hub.
    for (day, name, amount) in [(1, "Rent", 900.0), (2, "Tea", 3.5)] {
        post_ok(
            &client,
            &format!("{hub_url}/api/expenses"),
            expense(3, day, name, amount),
        )
        .await;
    }
    let liquidity = json!({"year": 2026, "name": "Bank", "category": "Cash", "currency": "EUR"});
    post_ok(&client, &format!("{hub_url}/api/liquidity"), liquidity).await;
    let cell = json!({"id": "Bank", "year": 2026, "month": 1, "value": 100.0});
    post_ok(&client, &format!("{hub_url}/api/liquidity/cell"), cell).await;
    let income = json!({"year": 2026, "month": 1, "category": "Salary", "amount": 2000.0});
    post_ok(&client, &format!("{hub_url}/api/cashflow/income"), income).await;
    post_ok(
        &client,
        &format!("{hub_url}/api/mappings"),
        json!({"id": "", "match_str": "coffee", "primary": "Food", "secondary": "Cafe"}),
    )
    .await;
    post_ok(
        &client,
        &format!("{hub_url}/api/categories/primary"),
        json!({"kind": "primary", "name": "Travel"}),
    )
    .await;
    let (status, value) = call(
        &client,
        reqwest::Method::PUT,
        &format!("{hub_url}/api/settings/currency"),
        Some(json!({"reference_currency": "USD", "current_month_rate_mode": "live"})),
    )
    .await;
    assert_eq!(status, 200, "PUT currency: {value}");

    // Each side refuses the other's routes.
    let (status, _) = call(
        &client,
        reqwest::Method::POST,
        &format!("{phone_url}/api/sync/listen"),
        None,
    )
    .await;
    assert_eq!(status, 409);
    let (status, _) = call(
        &client,
        reqwest::Method::POST,
        &format!("{hub_url}/api/sync/now"),
        None,
    )
    .await;
    assert_eq!(status, 409);

    // The hub listens and shows a code.
    let listen = post_ok(&client, &format!("{hub_url}/api/sync/listen"), json!({})).await;
    assert_eq!(listen["listening"], true, "{listen}");
    assert_eq!(listen["port"], sync_port);
    let code = post_ok(&client, &format!("{hub_url}/api/sync/pair-code"), json!({})).await;
    let code = code["code"].as_str().unwrap().to_string();
    let address = format!("127.0.0.1:{sync_port}");
    let pair_url = format!("{phone_url}/api/sync/pair");

    // Three wrong codes fail, and then the right one is gone too.
    let wrong = format!("{:06}", (code.parse::<u32>().unwrap() + 1) % 1_000_000);
    for _ in 0..3 {
        let body = json!({"address": address, "code": wrong});
        let (status, value) = call(&client, reqwest::Method::POST, &pair_url, Some(body)).await;
        assert_eq!(status, 409, "{value}");
        assert!(
            value["error"].as_str().unwrap().contains("does not match"),
            "{value}"
        );
    }
    let status_after = get_ok(&client, &format!("{hub_url}/api/sync/status")).await;
    assert_eq!(
        status_after["listener"]["pair_code_expires_at_ms"],
        Value::Null
    );
    let body = json!({"address": address, "code": code});
    let (status, value) = call(&client, reqwest::Method::POST, &pair_url, Some(body)).await;
    assert_eq!(status, 409, "{value}");
    assert!(
        value["error"].as_str().unwrap().contains("no pairing code"),
        "{value}"
    );

    // A new code pairs, and both sides show the other's key.
    let code = post_ok(&client, &format!("{hub_url}/api/sync/pair-code"), json!({})).await;
    let body = json!({"address": address, "code": code["code"]});
    let paired = post_ok(&client, &pair_url, body).await;
    let hub_status = get_ok(&client, &format!("{hub_url}/api/sync/status")).await;
    let phone_status = get_ok(&client, &format!("{phone_url}/api/sync/status")).await;
    assert_eq!(paired["hub_key_fingerprint"], hub_status["key_fingerprint"]);
    assert_eq!(
        hub_status["peers"][0]["device_id"],
        phone_status["device_id"]
    );
    assert_eq!(
        hub_status["peers"][0]["key_fingerprint"],
        phone_status["key_fingerprint"]
    );
    assert_eq!(
        phone_status["peers"][0]["address"],
        Value::from(address.clone())
    );
    assert_eq!(phone_status["listener"], Value::Null);

    // The first sync needs a reset and changes nothing until confirmed.
    let now_url = format!("{phone_url}/api/sync/now");
    let first = post_ok(&client, &now_url, json!({})).await;
    assert_eq!(first["outcome"], "reset_needed", "{first}");
    assert_eq!(first["plan"], "phone_reset");
    assert_eq!(first["push_first"], false);
    assert!(
        first["reset_preview"]["rows_per_table"].is_object(),
        "{first}"
    );
    let reset = post_ok(&client, &now_url, json!({"confirm_reset": true})).await;
    assert_eq!(reset["outcome"], "reset_done", "{reset}");
    assert!(reset["counts"]["received"].as_u64().unwrap() > 0, "{reset}");
    assert_eq!(
        snapshot(&client, &hub_url).await,
        snapshot(&client, &phone_url).await
    );
    assert_eq!(
        get_ok(&client, &format!("{hub_url}/api/mappings")).await,
        get_ok(&client, &format!("{phone_url}/api/mappings")).await
    );
    assert_eq!(
        get_ok(&client, &format!("{hub_url}/api/categories")).await,
        get_ok(&client, &format!("{phone_url}/api/categories")).await
    );
    assert_eq!(
        get_ok(&client, &format!("{hub_url}/api/settings/currency")).await,
        get_ok(&client, &format!("{phone_url}/api/settings/currency")).await
    );

    // Both sides edit, and an ordinary sync brings them together.
    post_ok(
        &client,
        &format!("{hub_url}/api/expenses"),
        expense(3, 9, "Bread", 2.0),
    )
    .await;
    let cell = json!({"id": "Bank", "year": 2026, "month": 2, "value": 150.0});
    post_ok(&client, &format!("{hub_url}/api/liquidity/cell"), cell).await;
    post_ok(
        &client,
        &format!("{phone_url}/api/expenses"),
        expense(4, 3, "Book", 12.0),
    )
    .await;
    let income = json!({"year": 2026, "month": 2, "category": "Other", "amount": 40.0});
    post_ok(&client, &format!("{phone_url}/api/cashflow/income"), income).await;
    post_ok(
        &client,
        &format!("{phone_url}/api/mappings"),
        json!({"id": "", "match_str": "tea", "primary": "Food", "secondary": "Cafe"}),
    )
    .await;
    let (status, value) = call(
        &client,
        reqwest::Method::PUT,
        &format!("{phone_url}/api/settings/currency"),
        Some(json!({"reference_currency": "GBP", "current_month_rate_mode": "previous_month_end"})),
    )
    .await;
    assert_eq!(status, 200, "PUT phone currency: {value}");
    post_ok(
        &client,
        &format!("{hub_url}/api/categories/secondary"),
        json!({"kind": "secondary", "name": "Travel Cafe"}),
    )
    .await;

    let round = post_ok(&client, &now_url, json!({})).await;
    assert_eq!(round["outcome"], "exchanged", "{round}");
    assert!(round["counts"]["sent"].as_u64().unwrap() > 0, "{round}");
    assert!(round["counts"]["received"].as_u64().unwrap() > 0, "{round}");
    assert!(round["counts"]["peer"].is_object(), "{round}");
    assert_eq!(
        snapshot(&client, &hub_url).await,
        snapshot(&client, &phone_url).await
    );
    assert_eq!(
        get_ok(&client, &format!("{hub_url}/api/mappings")).await,
        get_ok(&client, &format!("{phone_url}/api/mappings")).await
    );
    assert_eq!(
        get_ok(&client, &format!("{hub_url}/api/categories")).await,
        get_ok(&client, &format!("{phone_url}/api/categories")).await
    );
    assert_eq!(
        get_ok(&client, &format!("{hub_url}/api/settings/currency")).await,
        get_ok(&client, &format!("{phone_url}/api/settings/currency")).await
    );

    // A second sync has nothing to move.
    let again = post_ok(&client, &now_url, json!({})).await;
    assert_eq!(again["outcome"], "exchanged", "{again}");
    assert_eq!(again["counts"]["sent"], 0, "{again}");
    assert_eq!(again["counts"]["received"], 0, "{again}");

    let hub_status = get_ok(&client, &format!("{hub_url}/api/sync/status")).await;
    assert_eq!(
        hub_status["last_sync"]["outcome"], "exchanged",
        "{hub_status}"
    );
    assert_eq!(hub_status["log_health"]["reliable"], true, "{hub_status}");

    // Unpairing on the hub makes the phone's next sync fail with a refusal.
    let phone_id = phone_status["device_id"].as_str().unwrap();
    let url = format!("{hub_url}/api/sync/peers/{phone_id}");
    let (status, _) = call(&client, reqwest::Method::DELETE, &url, None).await;
    assert_eq!(status, 200);
    let (status, value) = call(&client, reqwest::Method::POST, &now_url, Some(json!({}))).await;
    assert_eq!(status, 409, "{value}");
    assert!(
        value["error"].as_str().unwrap().contains("not paired"),
        "{value}"
    );

    drop(hub);
}

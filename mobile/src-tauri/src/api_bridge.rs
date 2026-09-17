//! Serves the backend's HTTP API inside this process, over Tauri's IPC.
//!
//! The app opens no network port, because the backend has no authentication
//! and any other app on the phone could otherwise call it. A custom URI
//! scheme cannot replace the port either: Android's WebView hands a custom
//! scheme request no body, so every POST, PUT, and DELETE would arrive empty.
//! IPC does carry a body, so the page's `fetch` is replaced by
//! `src/api_shim.js`, which sends each `/api/...` call to the
//! [`api_request`] command below. That command runs the call against the same
//! [`axum::Router`] the desktop binary serves.

use std::collections::HashMap;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, Method, Request, StatusCode};
use serde::Serialize;
use tauri::plugin::{Builder as PluginBuilder, TauriPlugin};
use tauri::{Runtime, State};
use tokio::sync::watch;
use tokio::time::timeout;
use tower::ServiceExt;

/// How long an API call waits for startup before it answers 503.
///
/// The budget is deliberately generous: the row ID migration reads every
/// Parquet file in every year folder, which can take seconds on a phone, and
/// tripping this on an ordinary slow start would replace a working load with
/// an error. What it catches is a startup that never finishes at all, for
/// example a call into the Android context whose reply never arrives, since
/// `run_mobile_plugin` blocks on that reply with no timeout of its own.
/// Without this budget every `fetch` on the page would stay pending forever
/// and the user would see a loading state with no message.
///
/// A timeout does not cancel startup. The startup task keeps running and
/// publishes its result when it gets one, so a reload after the message
/// succeeds.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// The embedded backend, once startup has decided whether it can serve.
pub enum ApiBackend {
  /// The row ID migration finished, so the router may touch the data files.
  Ready(axum::Router),
  /// Startup failed and no request may reach the data. Every API call
  /// answers with this message instead.
  Unavailable(String),
}

/// Publishes the startup result to every waiting API call. Held by the
/// startup task, which owns it until it has a result to send.
pub struct ApiStartup(watch::Sender<Option<ApiBackend>>);

/// The managed state each API call waits on. Cheap to clone, and holding it
/// costs nothing until a call arrives.
pub struct ApiReady(watch::Receiver<Option<ApiBackend>>);

/// Create the readiness gate: put [`ApiReady`] in Tauri's managed state
/// before the page can call anything, and give [`ApiStartup`] to the task
/// that sets the data paths and runs the migration.
///
/// The gate exists because a call can arrive before startup finishes. Tauri
/// builds the window declared in `tauri.conf.json` before it runs the `setup`
/// hook, the page loads and IPC arrives on threads of their own, and the
/// migration reads every Parquet file in every year folder before it can
/// decide it has nothing to write. Without the gate an early call would fail
/// with Tauri's "state not managed" error, or worse read a file the migration
/// is still rewriting, and the page never retries a failed load.
pub fn startup_channel() -> (ApiStartup, ApiReady) {
  let (sender, receiver) = watch::channel(None);
  (ApiStartup(sender), ApiReady(receiver))
}

impl ApiStartup {
  /// Publish the startup result and wake every call waiting for it.
  pub fn publish(self, backend: ApiBackend) {
    // The only failure is every receiver being gone, which means the app is
    // shutting down and nothing is waiting for this.
    let _ = self.0.send(Some(backend));
  }
}

/// One response for the page's patched `fetch` to rebuild.
///
/// The body is text because every route in this API answers with JSON or
/// nothing, and a string costs far less over IPC than a JSON array of bytes.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiResponse {
  status: u16,
  content_type: Option<String>,
  body: String,
}

/// Run one API call against the embedded router, waiting first for startup
/// to finish.
///
/// `path` carries the query string, and `headers` and `body` come from the
/// page's `fetch` arguments. Returns `Result` only because Tauri requires it
/// for an async command that borrows managed state: every failure, including
/// a failed startup or a malformed request, comes back as an [`ApiResponse`]
/// with an HTTP status and a `{ "error": ... }` body, never as `Err`.
#[tauri::command]
pub async fn api_request(
  ready: State<'_, ApiReady>,
  method: String,
  path: String,
  headers: HashMap<String, String>,
  body: Option<String>,
) -> Result<ApiResponse, String> {
  let router = match wait_for_router(&ready).await {
    Ok(router) => router,
    Err((status, message)) => return Ok(error_response(status, &message)),
  };

  let request = match build_request(&method, &path, &headers, body) {
    Ok(request) => request,
    Err(message) => return Ok(error_response(StatusCode::BAD_REQUEST, &message)),
  };

  let response = match router.oneshot(request).await {
    Ok(response) => response,
    Err(err) => {
      let message = format!("The API router could not handle {method} {path}: {err}");
      return Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, &message));
    }
  };

  let status = response.status().as_u16();
  let content_type = response
    .headers()
    .get(header::CONTENT_TYPE)
    .and_then(|value| value.to_str().ok())
    .map(str::to_owned);

  // No size limit: these bodies are built in this process by the handlers of
  // this same app, and a chart or a full year of expenses can be large.
  let bytes = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
    Ok(bytes) => bytes,
    Err(err) => {
      let message = format!("The API response to {method} {path} could not be read: {err}");
      return Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, &message));
    }
  };

  match String::from_utf8(bytes.into()) {
    Ok(body) => Ok(ApiResponse {
      status,
      content_type,
      body,
    }),
    Err(_) => {
      let message = format!("The API response to {method} {path} is not text.");
      Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, &message))
    }
  }
}

/// Register the `fetch` shim as a webview initialization script.
///
/// Add this on `tauri::Builder`, not inside the `setup` hook: Tauri creates
/// the windows declared in `tauri.conf.json` before it calls `setup`, and a
/// window only gets the initialization scripts of the plugins registered
/// before it was built.
///
/// The plugin carries no command, because a plugin command needs a
/// permission in `capabilities/`; [`api_request`] is an app command instead,
/// which a local page may call without one.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
  PluginBuilder::new("finguard-api")
    .js_init_script(include_str!("api_shim.js"))
    .build()
}

/// Wait until startup has published its result, then hand back the router,
/// or the status and message to answer this call with.
///
/// Waiting is what keeps a call that arrives during startup working: the
/// desktop binary gets the same effect for free, because requests sit in the
/// listen queue while it migrates (see `backend/src/main.rs`).
async fn wait_for_router(ready: &ApiReady) -> Result<axum::Router, (StatusCode, String)> {
  let mut receiver = ready.0.clone();
  let published = match timeout(STARTUP_TIMEOUT, receiver.wait_for(Option::is_some)).await {
    Ok(Ok(published)) => published,
    // The channel closed without a result, so the startup task unwound
    // before it could publish, most likely a panic below `start_backend`.
    Ok(Err(_)) => {
      return Err((
        StatusCode::INTERNAL_SERVER_ERROR,
        "The app's backend did not finish starting. Restart the app.".to_owned(),
      ));
    }
    Err(_) => {
      return Err((
        StatusCode::SERVICE_UNAVAILABLE,
        "The app is still starting. Reload in a moment.".to_owned(),
      ));
    }
  };

  match published.as_ref() {
    Some(ApiBackend::Ready(router)) => Ok(router.clone()),
    Some(ApiBackend::Unavailable(message)) => {
      Err((StatusCode::INTERNAL_SERVER_ERROR, message.clone()))
    }
    // `wait_for` returns only once the value is `Some`.
    None => Err((
      StatusCode::INTERNAL_SERVER_ERROR,
      "The app's backend did not report whether it can serve. Restart the app.".to_owned(),
    )),
  }
}

/// Build the `{ "error": ... }` body every backend handler returns for a
/// failure (see `backend/src/http_error.rs`), so the page reports the message
/// the same way for a backend error and for a failure of this bridge.
fn error_response(status: StatusCode, message: &str) -> ApiResponse {
  ApiResponse {
    status: status.as_u16(),
    content_type: Some("application/json".to_owned()),
    body: serde_json::json!({ "error": message }).to_string(),
  }
}

/// Turn the page's `fetch` arguments into a request for the router, or
/// return the message to show for an argument the HTTP types reject.
fn build_request(
  method: &str,
  path: &str,
  headers: &HashMap<String, String>,
  body: Option<String>,
) -> Result<Request<Body>, String> {
  let method = Method::from_bytes(method.as_bytes())
    .map_err(|_| format!("{method:?} is not an HTTP method."))?;

  let mut builder = Request::builder().method(method).uri(path);
  for (name, value) in headers {
    let header_name =
      HeaderName::from_bytes(name.as_bytes()).map_err(|_| format!("{name:?} is not a header."))?;
    let header_value = HeaderValue::from_str(value)
      .map_err(|_| format!("The {name:?} header has an unusable value."))?;
    builder = builder.header(header_name, header_value);
  }

  builder
    .body(body.map(Body::from).unwrap_or_else(Body::empty))
    .map_err(|err| format!("The API request for {path} is malformed: {err}"))
}

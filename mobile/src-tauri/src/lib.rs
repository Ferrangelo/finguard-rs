//! The Android app: the finguard backend and the finguard page in one
//! process, with no network port between them. [`api_bridge`] carries the
//! page's API calls over Tauri's IPC.

mod api_bridge;

use api_bridge::ApiBackend;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(api_bridge::init())
    .invoke_handler(tauri::generate_handler![api_bridge::api_request])
    .setup(|app| {
      // The gate goes in first. Tauri has already built the window declared
      // in tauri.conf.json by the time this hook runs, so the page can call
      // the API while the startup task below is still working, and
      // `api_request` needs something to wait on from the first call.
      let (startup, ready) = api_bridge::startup_channel();
      app.manage(ready);

      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }

      // `spawn_blocking`, because the migration and the change log baseline
      // each scan every Parquet file in every year folder and must not hold
      // up the event loop.
      let handle = app.handle().clone();
      tauri::async_runtime::spawn_blocking(move || {
        startup.publish(start_backend(&handle));
      });
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}

/// Point `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `HOME` at the app's own data
/// directory, creating that directory first. On failure, returns the message
/// to answer every API call with, so the page can show it; the app keeps
/// running rather than dying before it draws anything.
///
/// Two reasons this runs first in the startup task, before the migration and
/// before the router exists:
///
/// - `set_var` writes process global state, which is only safe while nothing
///   else reads the environment. The backend resolves its data and config
///   folders from these three variables on every call, not once at startup,
///   and the readiness gate in [`api_bridge`] holds every API call until this
///   task publishes its result, so no backend call can read them first.
/// - The override is what keeps a build of this app on a development machine
///   away from the real finguard data folder. In the dev container that
///   folder holds the user's real financial records.
fn redirect_backend_paths(app: &tauri::AppHandle) -> Result<(), String> {
  let data_dir = app
    .path()
    .app_data_dir()
    .map_err(|err| format!("The app has no data directory: {err}"))?;
  std::fs::create_dir_all(&data_dir)
    .map_err(|err| format!("The app cannot create its data directory: {err}"))?;

  std::env::set_var("XDG_DATA_HOME", &data_dir);
  std::env::set_var("XDG_CONFIG_HOME", &data_dir);
  std::env::set_var("HOME", &data_dir);
  Ok(())
}

/// Redirect the backend's paths, run the row ID migration, record the change
/// log baseline, then build the router those two clear the way for.
///
/// The order is required. Every handler that loads a synced table rejects a
/// file without row IDs, so the migration has to finish before the first
/// request, and the baseline records rows by ID, so it has to follow the
/// migration. A failed step leaves the app serving its message and touching
/// no data file.
///
/// The baseline has to run here, at the first start, not whenever it seems
/// convenient. It records what a data folder already holds, and it does that
/// once: after the phone records its own first change, adding this call would
/// come too late, and every row entered before it would stay out of the log
/// and out of every sync. On a phone that starts empty, which is how the
/// first pairing is meant to go, it records nothing and costs nothing.
fn start_backend(app: &tauri::AppHandle) -> ApiBackend {
  if let Err(message) = redirect_backend_paths(app) {
    log::error!("{message}");
    return ApiBackend::Unavailable(message);
  }

  match finguard_rs_backend::row_id_migration::migrate_row_ids() {
    Ok(report) => log::info!("{report}"),
    Err(err) => {
      log::error!("Row ID migration failed: {err}");
      return ApiBackend::Unavailable(format!(
        "The app cannot use its data: the row ID migration failed: {err}"
      ));
    }
  }

  match finguard_rs_backend::sync_baseline::baseline_change_log() {
    Ok(report) => log::info!("{report}"),
    Err(err) => {
      log::error!("Change log baseline failed: {err}");
      return ApiBackend::Unavailable(format!(
        "The app cannot use its data: recording what it already holds failed: {err}"
      ));
    }
  }

  ApiBackend::Ready(finguard_rs_backend::api::router())
}

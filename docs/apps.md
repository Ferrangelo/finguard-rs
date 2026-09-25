# The apps

FinGuard is one Rust backend used by two apps: a desktop app and an Android phone app.
Both talk to the same JSON API and read the same kind of data; they differ in how the
API reaches the page and in how they get filled with data. See `docs/storage.md` for
where each one keeps its files and `docs/sync.md` for how they exchange data.

## Desktop

The desktop app runs two processes: the Rust backend (`backend/`) and the React
frontend (`frontend/`). The browser talks to the frontend, and the frontend's `/api`
proxy forwards requests to the backend over HTTP on `127.0.0.1:3111`.

## Phone (Android)

The phone app (`mobile/`, built with Tauri 2) runs the same backend as a library inside
one process, with no network port between the page and the backend. The page's calls to
`/api/...` travel over Tauri's IPC channel instead of a socket
(`mobile/src-tauri/src/lib.rs`, `mobile/src-tauri/src/api_bridge.rs`). Its release
package id is `dev.finguard.app`.

## Pages

The app has five pages, plus a set of sub-tabs on Expenses:

- **Expenses**: Detailed, Summary, Recurring, and Mappings sub-tabs. Detailed lists and
  edits individual expenses; Summary shows category totals and charts; Recurring
  manages payment templates and applies them to a month; Mappings defines rules that
  assign a category to an expense name automatically.
- **Cashflow**: monthly income by category, with spending and savings computed from
  expenses.
- **NetWorth**: investment holdings and prices, liquidity (bank and cash accounts), and
  credits and debts.
- **Categories**: manage primary and secondary category names.
- **Sync**: pair and exchange data with another device. See `docs/sync.md`.

A year selector and a month selector at the top control which period every page shows.

## Currency conversion

Exchange rates come from Frankfurter, a free, keyless API over European Central Bank
reference rates. Rates are cached on disk (`fx_rates.json`), and each date is looked up
over the network only once. Converting an amount to the reference currency you never
went online for needs no lookup at all: the reference currency always resolves to
exactly 1.0. A rate the app cannot resolve is an error, never silently 1.0, because a
plausible wrong number is worse than a visible failure.

A completed month's rate is frozen at that month's last published rate. The month still
in progress is priced by a setting you choose: either last month's closing rate, or the
latest rate published so far.

## Recurring expenses

A recurring template is applied to a chosen month, which creates one expense row per
template in that month, dated the first day of the month. The generated row is then an
ordinary expense row: editing or deleting it works like any other row. If you delete a
generated row and apply the template to that month again, the deleted row does not come
back.

## Themes

Seven themes ship with the app, switched from the header and kept as a saved
preference. See `frontend/THEME_SYSTEM.md` for how the theme system is built before
changing it.

## Desktop versus phone

|               | Desktop                                                                             | Phone                                                                |
| ------------- | ----------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| Role in sync  | Hub: listens for a phone, issues pairing codes                                      | Pairs with a hub, starts each sync round                             |
| Data location | `$XDG_DATA_HOME/finguard/` and `$XDG_CONFIG_HOME/finguard/` (see `docs/storage.md`) | The app's private data directory, used for both data and config      |
| App id        | (desktop images, no package id)                                                     | `dev.finguard.app` (release), `dev.finguard.app.debug` (debug build) |
| Cloud backup  | Whatever your desktop backs up, outside the app                                     | Off (`android:allowBackup="false"`); the desktop is the backup       |

## Limitations

- Exchange rates need the internet the first time each date is looked up; cached rates
  work offline afterwards, and a reference-currency amount never needs a lookup.
- Investment prices are entered by hand each month; the app fetches no market data.
- There is no authentication, which is why the API and web UI ports stay on loopback
  by default.
- There is no CSV or spreadsheet import or export, and sync between devices only
  happens when a page presses Sync now, never automatically.

## Technology stack

Backend (Rust, edition 2024): Axum for the HTTP API, Tokio as the async runtime, Polars
for the Parquet dataframes, Serde for JSON, `thiserror` for the crate's error type.

Frontend (TypeScript and React): TanStack Start as the meta-framework, TanStack Router
for file-based routing, React Hook Form for forms, Tailwind CSS for styling, Recharts
for charts, Vite as the dev server and bundler.

## Project structure

```
finguard-rs/
├── backend/                # Rust backend
│   ├── src/
│   │   ├── main.rs         # Process startup only: bind, migrate, serve
│   │   ├── api.rs          # Routes, handlers, and JSON DTOs
│   │   └── (library modules)
│   ├── Dockerfile
│   └── Cargo.toml
├── frontend/                # React frontend
│   ├── src/
│   │   ├── routes/          # Page routes
│   │   ├── components/      # UI components
│   │   └── services/        # API client and shared types
│   ├── Dockerfile
│   └── package.json
├── mobile/                  # Android app (Tauri 2)
│   └── src-tauri/
├── .github/workflows/       # CI: image builds, APK releases
├── docs/                    # This documentation
├── docker-compose.yml       # Pulls the published images
├── docker-compose_build_dev.yml  # Builds the images locally
├── run.sh                   # Starts backend and frontend from source
├── run-podman.sh            # Runs the published images under rootless Podman
└── README.md
```

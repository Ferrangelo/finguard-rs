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

## Entering data

### Expenses

On Expenses > Detailed, the **Add expense** button opens a form with Name, Day, **Amount (math
ok)**, Currency, Primary category, and Secondary category. Name and a valid Amount are
required; Day must be a day that exists in the selected month (day 31 in a 30-day month is
rejected). Amount accepts a math expression, e.g. `10+5.5`, evaluated when you save. Primary
and Secondary use a combobox of known categories, or you can type a new one. While adding (not
while editing), typing a name that exactly matches a saved mapping (see Category mappings
below) fills in Primary and Secondary for you; you can still change them before saving.

### Recurring expenses

On Expenses > Recurring, the **Add recurring template** panel takes the same fields as an
expense minus Day (Name, Amount, Currency, Primary, Secondary); a template has no day of its
own. The **Apply to `<month>` `<year>`** button creates one expense row per template in the
selected month, dated the first day of that month, and lands them in the Detailed tab like any
other expense: editing or deleting a generated row works the same way. Applying again in a
month that already has a template's row skips it. If you delete a generated row yourself and
apply the template to that month again, the app remembers the deletion and leaves it out,
listing it instead so you can add it back on purpose.

### Category mappings

On Expenses > Mappings, the **Add mapping rule** panel takes a **Match substring
(case-insensitive)** field plus a Primary and, optionally, a Secondary category. Despite the
field's name, the match is compared to the whole expense name, not a substring inside it: a
rule for "lidl" applies to an expense named "Lidl" or " lidl " (matching ignores case and
surrounding spaces), but not to "Lidl Supermarket". A mapping only affects expenses you add
afterward; it never changes categories on expenses already saved. Primary and Secondary are
stored lower-case, so a rule's categories can show in a different case than you typed.

### Investments

On Net Worth > Investments, **Add asset** opens a **New investment asset** panel: Asset name,
Category (Stocks/ETF, Commodities, or Bonds), Currency (defaults to your reference currency),
and an optional Link. Once created, the asset gets a row with three views, **Holdings
(quantity)**, **Prices**, and **Value (quantity × price)**, each with one editable cell per
month of the selected year; Holdings and Prices cells accept a math expression like the expense
Amount field. Prices are entered in the asset's own currency, set in the row's Currency column
in the Prices view; changing that currency only affects prices you enter afterward, it does not
relabel or convert months already saved. The Value view converts quantity × price into your
reference currency using that month's own exchange rate, and shows a dash where no rate could
be resolved. Edit turns the row's Name, Category, Currency, and Link into editable fields;
deleting the asset removes every month's data for it.

### Liquidity

On Net Worth > Liquidity & Debts, the Liquidity section's **Add** button creates a row with a
Name, a Category (Bank/Broker account, Cash, or Other), and a Currency. Each row has one
editable cell per month of the selected year, accepting a math expression like the other
amount fields. The section's Total row converts every row's own currency into your reference
currency using that calendar month's own rate, and marks a month with an asterisk when some
currency's rate could not be resolved. The Credits & Debts section on the same tab has its own
**Add** button and works the same way, but has no Category, and colors a row's monthly balance
to show a debt (negative) or a credit (positive).

### Cashflow

The Cashflow page shows one row per income category (Salary, Interests Bank account, Dividendi
e Cedole, Other) with one editable cell per month, accepting a math expression. Income has no
currency of its own: whatever you enter is saved directly in your reference currency. Spending,
Saving, and Saving % are computed automatically underneath: Spending sums that month's expenses
(already converted to the reference currency), Saving is Income minus Spending, and Saving % is
Saving divided by Income.

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

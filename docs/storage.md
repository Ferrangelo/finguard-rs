# Where the data lives

All facts below come from the backend source, not from a running copy of the data.

## Folders

- Data: `$XDG_DATA_HOME/finguard/` (default `~/.local/share/finguard/`).
- Config: `$XDG_CONFIG_HOME/finguard/` (default `~/.config/finguard/`).

Docker and Podman map these to `/data/finguard` and `/config/finguard` inside the
container, then bind-mount them from the host paths in `.env` or `run-podman.sh`.

On the phone, both `XDG_DATA_HOME` and `XDG_CONFIG_HOME` point at the app's own private
data directory, so data and config files share one `finguard/` folder there
(`redirect_backend_paths` in `mobile/src-tauri/src/lib.rs`).

## Data folder tree

```
finguard/
├── fx_rates.json
├── dbs/
│   └── <year>/
│       ├── 01_detailed_expenses.parquet
│       ├── ...
│       ├── 12_detailed_expenses.parquet
│       ├── recurring_expenses.parquet
│       ├── cashflow.parquet
│       ├── investments.parquet
│       ├── investments_prices.parquet
│       ├── liquidity.parquet
│       ├── credits_debts.parquet
│       ├── primaries.parquet      (legacy, see below)
│       └── secondaries.parquet    (legacy, see below)
├── sync/
│   ├── changelog.jsonl
│   ├── baseline_done
│   ├── settings_baseline_done
│   └── log_incomplete           (present only while the log needs repair)
└── backups/
    └── <UTC timestamp>-<reason>/
```

Backup reasons: `before-row-ids` (the startup row-id migration), `before-sync-reset` (a
phone resetting from the hub), `before-expense-date-repair` (the `repair_expense_dates`
maintenance tool). Each backup is a full copy of the `dbs/` tree taken before the
change, so it can be restored by hand if needed.

## Config folder tree

```
finguard/
├── category_mappings.json
├── known_categories.json
├── currency.json
├── device_id
├── sync_peers.json
└── sync_static_key        (file mode 0600)
```

## Parquet schemas

Every synced table (all but `cashflow.parquet`, `primaries.parquet`, and
`secondaries.parquet`) carries a `row_id` String column, always last.

| File (in `dbs/<year>/`)                    | One row is                                                 | Columns                                                                                                                                                                                                          |
| ------------------------------------------ | ---------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `MM_detailed_expenses.parquet`             | One expense in that month                                  | `expense_name` String, `expense_date` Date, `expense_amount` Float64, `currency` String, `primary_category` String, `secondary_category` String, `row_id` String                                                 |
| `recurring_expenses.parquet`               | One recurring template                                     | `expense_name` String, `expense_day` Int64, `expense_amount` Float64, `currency` String, `primary_category` String, `secondary_category` String, `row_id` String                                                 |
| `cashflow.parquet`                         | One category row (fixed set, see below)                    | `category` String, `01`..`12` Float64. No `row_id`: the category name is the row's key                                                                                                                           |
| `investments.parquet`                      | One holding's monthly quantities                           | `asset_name` String, `category` String, `link` String, `currency` String, `01`..`12` Float64, `row_id` String                                                                                                    |
| `investments_prices.parquet`               | One holding's monthly prices                               | `asset_name` String, `category` String, `link` String, `01`..`12` Float64, `01_currency`..`12_currency` String, `row_id` String (the same id as that asset's row in `investments.parquet`)                       |
| `liquidity.parquet`                        | One liquidity account's monthly balances                   | `asset_name` String, `category` String, `currency` String, `01`..`12` Float64, `row_id` String                                                                                                                   |
| `credits_debts.parquet`                    | One credit or debt's monthly balances                      | `name` String, `currency` String, `01`..`12` Float64, `row_id` String                                                                                                                                            |
| `primaries.parquet`, `secondaries.parquet` | A category total, as of whenever the file was last written | Legacy. No longer written or read for totals; category totals are computed from the monthly expense files instead. Still keyed by a `primary_category` or `secondary_category` column, matching which file it is |

Row labels that are code constants, not user data, so they are safe to name here:
cashflow rows are `Salary`, `Interests Bank account`, `Dividendi e Cedole`, `Other`
(income categories) plus `Income`, `Spending`, `Saving`, `Saving %` (derived rows; only
`Income` is still recomputed, the other three persist only for files saved by an older
version). Investment categories are `Stocks/ETF`, `Commodities`, `Bonds`. Liquidity
categories are `Bank/Broker account`, `Cash`, `Other`.

Never list an actual user category name, amount, or description here or anywhere else.

## Row ids

Every `row_id` is a random UUID v4. A recurring template's generated row (the expense
row that applying a template creates in a given month) instead gets a derived id,
`<template row_id>:<YYYY>-<MM>`, so the same generated row keeps the same id if the
template is applied again.

The startup row-id migration adds a missing `row_id` to a file that predates this
column, after first copying the whole `dbs/` tree into a `before-row-ids` backup. A
handler that meets a file still missing ids returns an error asking for a restart,
rather than assigning ids that were never saved.

## JSON files

`category_mappings.json`: an object keyed by lower-cased expense name, each value
`{ "primary_category": <name>, "secondary_category": <name> }`.

`known_categories.json`: `{ "primary": [<name>, ...], "secondary": [<name>, ...] }`.

`currency.json`: `{ "reference_currency": <code>, "current_month_rate_mode": <mode> }`.

`fx_rates.json`: always EUR-based regardless of the reference currency,
`{ "base": "EUR", "rates": { "<date>": { "<code>": <rate>, ... } } }`, plus bookkeeping
fields (`aliases`, `latest_date`, `latest_fetched_at`) that an older cache file loads
without.

`device_id`: the file holds this device's id as plain text.

`sync_peers.json`: `{ "peers": [ { "device_id": <id>, "role": <role>, "paired_at_ms":
<ms>, "static_key": <key or absent>, "address": <address or absent>, ... } ] }`.

`sync_static_key`: this device's private sync key, 64 lowercase hex digits and a
newline.

One line of `sync/changelog.jsonl`: a JSON object with `stamp` (`{ "ms": <int>,
"counter": <int> }`), `device_id`, the table it changed (flattened, for example
`{ "table": "Expenses", "year": <year>, "month": <month> }`), `row_id`, an optional
`origin` (present only for a row a recurring template generated), and what happened to
the row: `{ "op": "upsert", "row": { <full row, example key> "primary_category":
"<name>", ... } }`, `{ "op": "delete" }`, or `{ "op": "cell", "column": <name>, "value":
<value> }`. Use a placeholder like `<name>` for any example value; never a real one.

## Format history

- A monthly expense file saved by this version drops `expense_in_ref_currency`,
  `fx_rate`, and `rate_date` if it still has them; those figures are computed at read
  time from `expense_amount` and `currency` instead. Once a file has been saved this
  way, an older build that expects those columns can no longer read it.
- A legacy `currency` value of `"E"` is read as `"EUR"`.
- An `expense_date` column stored as a non-Date type (for example `Int64`, from an
  older file) is cast to `Date` on load.

## Parquet writes

Two write paths exist. `write_parquet` creates the file directly; it is what an
ordinary API request uses to save an edit, so a crash mid-write can leave a partial
file. `write_parquet_atomic` writes to a temporary file in the same folder, flushes it,
and renames it over the target, so a crash leaves either the old file or the complete
new one; the startup row-id migration, an incoming sync merge, and the expense date
repair use it, because these are writers that change a file the user did not just edit.

## Backups

The app takes its own backups automatically before the three operations listed above
(row-id migration, sync reset, expense-date repair), each a full copy of `dbs/` under
`backups/<timestamp>-<reason>/`. Nothing automatic backs up the config folder or
copies data off the machine. For your own copy, stop the app and copy the whole data
and config folders elsewhere.

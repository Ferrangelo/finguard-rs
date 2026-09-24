# FinGuard-rs

No app exists that accomplishes everything my spreadsheets handled in a simple way, so I built my own.

Personal finance management with a modern web dashboard.
It monitors investments, expenses, cashflow, liquidity, and net worth. Everything is stored locally as Parquet files with zero cloud dependencies.

It follows a scheme similar to [Mr Rip spreadsheets](https://retireinprogress.com/how-i-track-my-finances-using-spreadsheets-part-1-why-and-what/).

Backend in rust, the typescript + React frontend.

## Features

- **Investment Tracking**: Monitor investments (stocks/ETFs, commodities, bonds) over time with price and quantity history.
- **Expense Tracking**: Add, edit, delete, and filter monthly expenses with automatic categorization via configurable rules.
- **Summary Dashboards**: Monthly and cumulative expense breakdowns by primary/secondary category with interactive charts.
- **Cashflow**: Track salary, interest, dividends, and other income alongside spending. Automatically computes savings and savings rate.
- **Net Worth**: Keep track of all assets: liquidity, credit/debts, investments (stocks/ETFs, commodities, bonds).
- **Recurring Expenses**: Define payment templates and apply them to any month; instances are stored per-month separately. Supported via the Expenses → Recurring sub-tab.
- **Currency Conversion**: Enter expenses and investments in different currencies and see everything added up in one currency you choose, using the exchange rate for each item's own date.
- **Local-First**: All data is stored locally in Parquet files with no external dependencies.
- **Modern UI**: Built with React, Tailwind CSS, and interactive charts (Recharts).
- **Themes**: Seven predefined themes with persistent user preference.

![Demo Animation](sshots/sshots_animation.gif)

## Installation and usage

### Option A: Install and run with docker

Download the `docker-compose.yml` file and cd into the directory where this file is stored.

Before starting the service the user should set `PUID` and `PGID` so files created by the container are owned by the host user. Do one of the following:

- Export in your current shell (temporary for this session):

```
export PUID=$(id -u)
export PGID=$(id -g)
docker compose up -d
```

- Or create a persistent `.env` file next to `docker-compose.yml`. To auto-create it with the current UID/GID:

```
printf 'PUID=%s\nPGID=%s\n' "$(id -u)" "$(id -g)" > .env
```

The same `.env` file can also pin the host directories the backend data and config are mounted from, through `FINGUARD_DATA_DIR` and `FINGUARD_CONFIG_DIR`. See `.env.example` in the repository for every variable the compose file reads.

Finally install with (this will pull the latest image if it doesn't already exist locally):

```
docker compose up -d
```

Notes:

- With no `.env` entry, the bind mounts fall back to `${HOME}`, which is expanded in the environment of the process that runs `docker compose`. Under `sudo` that is `/root`, so the mounts point at `/root/.local/share/finguard` and `/root/.config/finguard`, which your own account cannot read. Set `FINGUARD_DATA_DIR` and `FINGUARD_CONFIG_DIR` in `.env` to make the paths explicit:

```
printf 'FINGUARD_DATA_DIR=%s/.local/share/finguard\nFINGUARD_CONFIG_DIR=%s/.config/finguard\n' "$HOME" "$HOME" >> .env
```

#### Update docker image

To update, pull the latest image:

```bash
# Pull images for services defined in docker-compose.yml
docker compose pull
```

Then run it

```bash
# Start using the pulled image and recreate the container
docker compose up -d --no-build --force-recreate
```

The UI is available at `http://localhost:5173` as soon as the container starts.

```bash
# stop and remove
docker compose down
```

#### Run the same stack with rootless Podman

`run-podman.sh` starts the same two `ghcr.io/ferrangelo/finguard-rs-backend` and `ghcr.io/ferrangelo/finguard-rs-frontend` images without Docker. Like `docker-compose.yml`, it is a single self-contained file, so downloading just `run-podman.sh` and running it is enough. It pulls both images, creates a user-defined `finguard` network so the frontend container can resolve `backend` by name, and mounts `~/.local/share/finguard` and `~/.config/finguard` from the account that runs it. `FINGUARD_PORT` (default `3111`) and `FRONTEND_PORT` (default `5173`) are read from the environment. The script sets `PUID=0` and `PGID=0` on purpose, the opposite of the Docker case: rootless Podman maps container UID 0 to your host user, while any other value lands in the subuid range at 100000 and up, where you cannot read the files it writes.

Cloning the repository is one way to get the script:

```bash
git clone https://github.com/Ferrangelo/finguard-rs.git
cd finguard-rs
./run-podman.sh
```

### Option B: Build and run from source

#### Prerequisites

- **Rust** 1.70+ (install from [rustup.rs](https://rustup.rs/))
- **Node.js** 18+ and **npm**

```bash
# Clone the repository
git clone https://github.com/Ferrangelo/finguard-rs.git
cd finguard-rs
```

```bash
# Run both backend and frontend
./run.sh
```

The UI will be available at `http://localhost:5173`.

### Navigate the Dashboard

The interface has three main tabs:

| Tab           | What it does                                                                                                                                                                                                        |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Expenses**  | View, add, edit, delete, and filter detailed monthly expenses. Switch to the _Summary_ sub-tab for category breakdowns and charts. The _Mappings_ sub-tab lets you define automatic expense-name-to-category rules. |
| **Cashflow**  | Enter monthly income by category (salary, interest, dividends, other). Spending and savings are auto-calculated from expense data.                                                                                  |
| **Net Worth** | Track investment holdings and prices, bank/broker liquidity, and credits/debts. View allocation pie charts and evolution over time. The Total sub-tab lets you pick which currency every total is added up in, view the figures in a different currency, and choose whether the month still in progress is priced at last month's closing rate or the latest rate available. |

Use the **year** and **month** selectors at the top to switch between periods. All data refreshes automatically.

## Data Storage

Data is stored in **Parquet files** (via Polars) in local XDG-compliant directories:

- **Expense & financial data**: `$XDG_DATA_HOME/finguard/` (default: `~/.local/share/finguard/`)
- **Category mappings and settings**: `$XDG_CONFIG_HOME/finguard/` (default: `~/.config/finguard/`)

Directory layout:

```
finguard/
├── fx_rates.json                  # Exchange rates looked up so far, kept so each date is only looked up once
└── dbs/
    └── 2026/
        ├── 01_detailed_expenses.parquet   # January expenses
        ├── 02_detailed_expenses.parquet   # February expenses
        ├── ...
        ├── primaries.parquet              # Primary category totals as of when this file was last saved
        ├── secondaries.parquet            # Secondary category totals as of when this file was last saved
        ├── cashflow.parquet               # Monthly income/spending/savings
        ├── investments.parquet            # Investment holdings
        ├── investments_prices.parquet     # Investment prices
        ├── liquidity.parquet              # Bank accounts & cash
        └── credits_debts.parquet          # Credits/debts
```

The app keeps a `currency.json` file next to the category-mapping files: it holds the currency you want totals shown in, and how to price the month that is still in progress.

`primaries.parquet` and `secondaries.parquet` are no longer kept up to date. The app now adds up category totals from the monthly expense files each time it needs them, so these two files are left untouched from whenever they were last written and should not be relied on.

> **Note on switching app versions**: monthly expense files saved by this version no longer keep a saved copy of each expense already converted to your chosen currency; the app works that out each time it shows the figures. Older expense files still open normally, but once one of them is saved again by this version, an older version of the app can no longer open it.

## Limitations

- **Currency exchange needs the internet the first time**: the app converts expenses and investments into the currency you have chosen for totals, using the exchange rate published for each item's own date. It only goes online the first time it needs a given date's rate; after that the rate is saved and reused. If every amount you enter is already in that one currency, it never needs to go online.
- **No automatic price updates**: investment prices must be entered manually each month.
- **No authentication or multi-user support**
- **No data import/export**: no CSV, bank-statement, or spreadsheet import; no export functionality (however the parquet files are always saved to disk).

## Technology Stack

### Backend (Rust)

- **Axum**: async HTTP framework
- **Tokio**: async runtime
- **Polars**: fast DataFrame library
- **Serde**: JSON serialization
- **Parquet**: efficient columnar storage

### Frontend (React)

- **TanStack Start**: React meta-framework
- **TanStack Router**: client-side routing
- **React Hook Form**: form management
- **Tailwind CSS**: utility-first styling
- **Recharts**: interactive charts
- **TypeScript**: type-safe JavaScript
- **Vite**: lightning-fast dev server

## Project Structure

```
finguard-rs/
├── backend/        # Rust backend
|   ├── src/
│   │   ├── main.rs  # Axum web server & API handlers
│   │   └── (library modules)
│   ├── Dockerfile
│   └── Cargo.toml   # Rust dependencies
│
├── frontend/        # React frontend (TanStack Start)
│   ├── src/
│   │   ├── components/  # UI components
│   │   ├── routes/      # Page routes
│   │   └── services/    # API client
│   ├── Dockerfile
│   └── package.json
├── run.sh               # Unified startup script
├── run-podman.sh        # Runs the published images under rootless Podman
└── README.md            # This file
```

## References

**This is a rewrite in Rust of the original Python project [Finguard](https://github.com/Ferrangelo/finguard).** It features a modern React frontend and a performant Axum backend. All data processing uses [Polars](https://pola.rs/) for efficiency and Parquet for storage.

- **Original Python Project**: [finguard](https://github.com/Ferrangelo/finguard)
- **Frontend Framework**: [TanStack Start](https://tanstack.com/start/latest)
- **Web Framework**: [Axum](https://github.com/tokio-rs/axum)
- **Data Processing**: [Polars](https://pola.rs/)

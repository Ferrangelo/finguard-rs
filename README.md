# FinGuard-rs

Personal finance management tool with a modern web dashboard for tracking investments, expenses, cashflow, liquidity, and net worth. Everything is stored locally as Parquet files with zero cloud dependencies.

It follows a scheme similar to [Mr Rip spreadsheets](https://retireinprogress.com/how-i-track-my-finances-using-spreadsheets-part-1-why-and-what/). 

**This is a rewrite in Rust of the original Python project [Finguard](https://github.com/Ferrangelo/finguard).** It features a modern React frontend and a performant Axum backend. All data processing uses [Polars](https://pola.rs/) for efficiency and Parquet for storage.

## Quick Start

```bash
# Clone/navigate to project
cd /home/anferrar/Projects/finguard_rs

# Run both backend and frontend
./run.sh
```

The application will be available at `http://localhost:5173` (Vite dev server).

Alternatively, run them separately:
```bash
# Terminal 1: Rust backend (runs on :3111)
cargo run

# Terminal 2: React frontend
cd finguard-zen && npm run dev
```

## Features

- **Investment Tracking**: Monitor investments (stocks/ETFs, commodities, bonds) over time with price and quantity history.
- **Expense Tracking**: Add, edit, delete, and filter monthly expenses with automatic categorization via configurable rules.
- **Summary Dashboards**: Monthly and cumulative expense breakdowns by primary/secondary category with interactive charts.
- **Cashflow**: Track salary, interest, dividends, and other income alongside spending. Automatically computes savings and savings rate.
- **Net Worth**: Keep track of all assets: liquidity, credit/debts, investments (stocks/ETFs, commodities, bonds).
- **Recurring Expenses**: Define payment templates and apply them to any month; instances are stored per-month separately. Supported via the Expenses → Recurring sub-tab.
- **Local-First**: All data is stored locally in Parquet files with no external dependencies.
- **Modern UI**: Built with React, Tailwind CSS, and interactive charts (Recharts).
- **Themes**: Seven predefined themes with persistent user preference.

## Installation

### Prerequisites
- **Rust** 1.70+ (install from [rustup.rs](https://rustup.rs/))
- **Node.js** 18+ and **npm** (or **Bun**)

### Setup

```bash
# Clone the repository
git clone https://github.com/Ferrangelo/finguard_rs.git
cd finguard_rs

# Install frontend dependencies
cd finguard-zen
npm install
cd ..
```

No additional installation needed for the backend—`cargo run` handles everything.

## Usage

### Start the Application

Option 1: Run both at once (recommended for development)
```bash
./run.sh
```

Option 2: Run separately (two terminals)
```bash
# Terminal 1: Backend
cargo run

# Terminal 2: Frontend
cd finguard-zen && npm run dev
```

The UI will be available at `http://localhost:5173`.

### Navigate the Dashboard

The interface has three main tabs:

| Tab | What it does |
|-----|-------------|
| **Expenses** | View, add, edit, delete, and filter detailed monthly expenses. Switch to the *Summary* sub-tab for category breakdowns and charts. The *Mappings* sub-tab lets you define automatic expense-name-to-category rules. |
| **Cashflow** | Enter monthly income by category (salary, interest, dividends, other). Spending and savings are auto-calculated from expense data. |
| **Net Worth** | Track investment holdings and prices, bank/broker liquidity, and credits/debts. View allocation pie charts and evolution over time. |

Use the **year** and **month** selectors at the top to switch between periods. All data refreshes automatically.

## Data Storage

Data is stored in **Parquet files** (via Polars) in local XDG-compliant directories:

- **Expense & financial data**: `$XDG_DATA_HOME/finguard/` (default: `~/.local/share/finguard/`)
- **Category mappings**: `$XDG_CONFIG_HOME/finguard/` (default: `~/.config/finguard/`)

Directory layout per year:

```
dbs/
└── 2026/
    ├── 01_detailed_expenses.parquet   # January expenses
    ├── 02_detailed_expenses.parquet   # February expenses
    ├── ...
    ├── primaries.parquet              # Cumulative primary category summary
    ├── secondaries.parquet            # Cumulative secondary category summary
    ├── cashflow.parquet               # Monthly income/spending/savings
    ├── investments.parquet            # Investment holdings
    ├── investments_prices.parquet     # Investment prices
    ├── liquidity.parquet              # Bank accounts & cash
    └── credits_debts.parquet          # Credits/debts
```


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
- **TanStack Query**: data fetching & caching
- **React Hook Form**: form management
- **Tailwind CSS**: utility-first styling
- **Recharts**: interactive charts
- **TypeScript**: type-safe JavaScript
- **Vite**: lightning-fast dev server

## Limitations

- **No currency exchange**: all amounts are assumed to be in a single currency.
- **No automatic price updates**: investment prices must be entered manually each month.
- **No authentication or multi-user support**
- **No data import/export**: no CSV, bank-statement, or spreadsheet import; no export functionality (however the parquet files are always saved to disk).
- **Limited mobile experience**

## API

The backend exposes a REST API on `http://localhost:3111/api` with 40+ endpoints covering:

- **Expenses**: CRUD operations, filtering, category totals
- **Recurring Payments**: template management and application
- **Categories**: primary and secondary category management
- **Auto-Categorization Rules**: name-to-category mappings
- **Income**: monthly cashflow tracking
- **Investments**: asset portfolios with price history
- **Liquidity**: bank accounts and cash balances
- **Credits & Debts**: loan and credit card tracking

The frontend dev server (`localhost:5173`) automatically proxies `/api/*` requests to the backend.

## Project Structure

```
finguard_rs/
├── src/
│   ├── main.rs          # Axum web server & API handlers
│   └── (library modules)
├── finguard-zen/        # React frontend (TanStack Start)
│   ├── src/
│   │   ├── components/  # UI components
│   │   ├── routes/      # Page routes
│   │   └── services/    # API client
│   └── package.json
├── Cargo.toml           # Rust dependencies
├── run.sh               # Unified startup script
└── README.md            # This file
```

## References

- **Original Python Project**: `/home/anferrar/Projects/finguard`
- **Frontend Framework**: [TanStack Start](https://tanstack.com/start/latest)
- **Web Framework**: [Axum](https://github.com/tokio-rs/axum)
- **Data Processing**: [Polars](https://pola.rs/)

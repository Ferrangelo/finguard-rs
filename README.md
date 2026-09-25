# FinGuard-rs

A local-first personal finance tracker. It follows a scheme similar to
[Mr Rip's spreadsheets](https://retireinprogress.com/how-i-track-my-finances-using-spreadsheets-part-1-why-and-what/):
expenses, investments, cashflow, and net worth, with everything stored on your own disk as Parquet
files. A desktop app and an Android app share the same data and sync over the local
network, with no cloud account involved.

## Features

- Detailed and recurring **expense tracking**, with automatic categorization rules
- **Cashflow**: monthly income against spending, with savings computed automatically
- **Investment** tracking: quantity and price per month, in each asset's own currency, with the computed value shown in your reference currency
- **Net worth**: investments, liquidity, and credits and debts, tracked over time
- **Currency conversion**, using the exchange rate for each item's own date
- An Android app that syncs with the desktop over the local network
- All data stored locally as Parquet files, with no cloud dependency

![Demo Animation](sshots/sshots_animation.gif)

## Using the app

- **Expenses**: add one from Expenses > Detailed. Amount can take a math expression, e.g. `10 + 5.5/2`.
- **Recurring expenses**: set up a template on Expenses > Recurring, then apply it to a month
  to generate that month's expense rows.
- **Category mappings**: on Expenses > Mappings, a rule fills in a category automatically
  when a new expense's name matches it.
- **Investments**: track quantity and price per month on Net Worth > Investments, one row
  per asset.
- **Liquidity**: track a cash or bank balance per month on Net Worth > Liquidity & Debts.
- **Cashflow**: enter income by category on the Cashflow page; spending and saving are
  computed from your expenses.

See [`docs/apps.md`](docs/apps.md) for the full steps, fields, and behavior of each one.

## Quick start

**Desktop**, with Docker Compose:

1. Download `docker-compose.yml` and `.env.example`, then copy the second to `.env` and
   edit it.
2. Run `docker compose up -d`.
3. Open `http://localhost:5173`.

The compose file runs both containers on the host network so a phone can find and sync
with this desktop; see [Letting the phone reach a Docker
desktop](docs/install.md#letting-the-phone-reach-a-docker-desktop) if you copied an
older compose file.

Full steps, other ways to run the desktop app, and the Android app are in
[`docs/install.md`](docs/install.md).

**Android**: download the latest release from
[GitHub Releases](https://github.com/Ferrangelo/finguard-rs/releases), or add the
repository to [Obtainium](https://github.com/ImranR98/Obtainium) for automatic updates.

See [`docs/install.md`](docs/install.md).

## Documentation

- [`docs/install.md`](docs/install.md): installing, running, and updating the desktop
  and Android apps.
- [`docs/apps.md`](docs/apps.md): what each app does, the pages, and the current
  limitations.
- [`docs/sync.md`](docs/sync.md): how the desktop and a phone sync, and why it works
  this way.
- [`docs/storage.md`](docs/storage.md): where the data lives on disk and what the
  files contain.

## Limitations

- No authentication: the API and web UI stay on loopback by default.
- No CSV or spreadsheet import or export.
- Sync only runs when a Sync page presses Sync now, never automatically.

## License

AGPL-3.0. See [`LICENSE`](LICENSE).

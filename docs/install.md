# Install and run

FinGuard has two apps that share one backend: a desktop app (Rust backend plus a React
web UI, run with Docker or Podman, or built from source) and an Android app (arm64
only). See `docs/apps.md` for how they fit together and `docs/sync.md` for connecting a
phone to a desktop.

## Desktop with Docker Compose

1. Download `docker-compose.yml` and `.env.example` from the repository root into an
   empty folder.
2. Copy `.env.example` to `.env` in that same folder.
3. Edit `.env`: set `FINGUARD_DATA_DIR` and `FINGUARD_CONFIG_DIR` to real paths, for
   example `/home/youruser/.local/share/finguard` and `/home/youruser/.config/finguard`.
   Set `PUID` and `PGID` to your user and group id (`id -u` and `id -g`). Leaving
   `FINGUARD_DATA_DIR` and `FINGUARD_CONFIG_DIR` unset falls back to `${HOME}`, expanded
   by the process that runs `docker compose`. Under `sudo` that process's `HOME` is
   `/root`, so the mounts land under `/root` and your own account cannot read them. Set
   both variables explicitly instead of relying on `sudo`.
4. Run `docker compose up -d`.
5. Open `http://localhost:5173`.

To update: run `docker compose pull`, then `docker compose up -d --force-recreate`. To
stop: run `docker compose down`.

## Desktop with rootless Podman

`run-podman.sh` runs the same two published images without Docker.

1. Download `run-podman.sh` from the repository root.
2. Run `./run-podman.sh`.
3. Open `http://localhost:5173`.

The script sets `PUID=0` and `PGID=0`. This is the opposite of the Docker Compose case
on purpose: rootless Podman maps container UID 0 to your host user, so files the
container writes come out owned by you. Any other value lands in the subuid range
(100000 and up), where your account cannot read them.

`FINGUARD_PORT` (default `3111`), `FRONTEND_PORT` (default `5173`), and
`FINGUARD_SYNC_PORT` (default `3112`) can be set as environment variables before running
the script.

To update: run `./run-podman.sh` again. It pulls the latest images and recreates both
containers.

## Desktop from source

Prerequisites:

- Rust 1.88 or newer (the backend uses edition 2024 and a let-chain, which needs 1.88).
  Install from `https://rustup.rs/`.
- Node.js 20.19 or newer, or Node.js 22.12 or newer, and npm.

Steps:

1. Clone the repository: `git clone https://github.com/Ferrangelo/finguard-rs.git`.
2. Enter the folder: `cd finguard-rs`.
3. Run `./run.sh`.
4. Open `http://localhost:5173`.

`run.sh` starts the Rust backend (`cargo run`) and the frontend (`npm run dev`)
together. Both bind to `127.0.0.1` by default.

## Ports and network

| Port | What                        | Binds to                                                   |
| ---- | --------------------------- | ---------------------------------------------------------- |
| 3111 | Backend API                 | loopback only (`127.0.0.1`), no authentication             |
| 5173 | Web UI                      | loopback only (`127.0.0.1`), no authentication             |
| 3112 | Sync listener (TCP and UDP) | every address, so a phone on the same network can reach it |

3111 and 5173 stay on loopback because neither the API nor the `/api` dev proxy has
authentication. 3112 has to be reachable from other devices, because that is the port a
phone connects to and discovers over during pairing and sync; see `docs/sync.md`.

If your desktop runs `./run.sh` directly (not in Docker or Podman) and a firewall
blocks inbound connections, allow port 3112 for both TCP and UDP. With ufw, run:

```
sudo ufw allow 3112/tcp
sudo ufw allow 3112/udp
```

Environment variables the backend reads:

- `FINGUARD_HOST`, `FINGUARD_PORT`: the API bind address and port (default
  `127.0.0.1:3111`).
- `FINGUARD_SYNC_HOST`, `FINGUARD_SYNC_PORT`: the sync listener's bind address and port
  (default `0.0.0.0:3112`).
- `FINGUARD_ALLOWED_HOSTS`: a comma-separated list of extra `Host` header values the API
  accepts. Without it, the API accepts `localhost`, `127.0.0.1`, `::1`, `backend` (the
  Docker Compose service name), and the address in `FINGUARD_HOST` when that address is
  not the unspecified `0.0.0.0`.
- `FINGUARD_FX_OFFLINE`: set to any value to stop every exchange rate network call.
  Currency conversion then works from the cached rates only.
- `FINGUARD_FX_QUIET`: set to `1` to silence the exchange rate log lines.

The frontend dev server reads `FINGUARD_DEV_HOST` to bind somewhere other than
`127.0.0.1`.

## Updating the desktop app

- Docker Compose: `docker compose pull`, then `docker compose up -d --force-recreate`.
- Podman script: rerun `./run-podman.sh`.

Published images rebuild when a push to `main` changes files under `backend/` or `frontend/` (excluding Markdown files), when the workflow file itself changes, or when manually triggered (`.github/workflows/docker-publish.yml`). They are tagged `latest` and with the commit SHA.

## Android app

The Android app is arm64 only.

1. Download the latest `finguard-X.Y.Z-arm64.apk` from
   `https://github.com/Ferrangelo/finguard-rs/releases`.
2. Install it.
3. To get updates automatically, add `https://github.com/Ferrangelo/finguard-rs` as an
   app in Obtainium instead of installing the file by hand. Obtainium then offers an
   update for each new tagged release.

The app starts empty. Open its Sync page and follow `docs/sync.md` to pair it with a
desktop and fill it with your data. The app does not use Android's automatic cloud
backup: `android:allowBackup="false"` in its manifest turns that off on purpose, because
the desktop is the backup.

### Verifying a downloaded APK

Every release APK is signed with the same key. Its certificate SHA-256 fingerprint is:

```
429f9ff74413fac95c433e812054935e29920edced6786451a816ff65ac4c669
```

To check a downloaded APK against it, run `apksigner verify --print-certs` on the file
(part of the Android SDK build tools) and compare the `Signer #1 certificate SHA-256
digest:` line. The two must match, ignoring case and the colons some tools print between
byte pairs.

## Building the Android app from source

Toolchain versions, pinned in `.github/workflows/release-apk.yml`:

- tauri-cli 2.11.4
- Android NDK 30.0.16248370
- Android platform android-36
- Android build-tools 35.0.0
- JDK 21
- Node.js 26

With that toolchain installed, run `cargo tauri android build` from the `mobile/`
folder. A debug build installs as a separate app, `dev.finguard.app.debug`, alongside
the release app `dev.finguard.app`, with its own data and its own pairing.

## How releases are built

A tag named `vX.Y.Z` must match the `version` field in
`mobile/src-tauri/tauri.conf.json`. Pushing that tag runs
`.github/workflows/release-apk.yml`, which has three jobs: the first builds an unsigned
APK with no access to the signing key, the second signs and verifies it, and the third
attaches the signed APK and its `.sha256` file to a GitHub Release named after the tag.
Release tags are only pushed on commits already on `main`.

A manual run of the same workflow (`workflow_dispatch`, from `main`) builds and signs
the same APK but uploads it only as a workflow run artifact, without creating a release.

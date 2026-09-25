#!/usr/bin/env bash
# Run the finguard-rs stack (docker-compose.yml equivalent) under rootless Podman.
set -euo pipefail

FINGUARD_PORT="${FINGUARD_PORT:-3111}"
FRONTEND_PORT="${FRONTEND_PORT:-5173}"
FINGUARD_SYNC_PORT="${FINGUARD_SYNC_PORT:-3112}"

# Rootless Podman maps container UID 0 to the host user, so PUID/PGID of 0 make
# the entrypoint chown the bind mounts to files this host user owns. Any other
# value lands in the subuid range (100000+) and becomes unreadable from the host.
PUID="${PUID:-0}"
PGID="${PGID:-0}"

BACKEND_IMAGE="ghcr.io/ferrangelo/finguard-rs-backend:latest"
FRONTEND_IMAGE="ghcr.io/ferrangelo/finguard-rs-frontend:latest"
DATA_DIR="${HOME}/.local/share/finguard"
CONFIG_DIR="${HOME}/.config/finguard"

command -v podman >/dev/null || { echo "podman is not installed" >&2; exit 1; }

mkdir -p "${DATA_DIR}" "${CONFIG_DIR}"

podman pull "${BACKEND_IMAGE}"
podman pull "${FRONTEND_IMAGE}"

podman rm -f finguard-rs-frontend finguard-rs-backend 2>/dev/null || true

# Host networking lets the phone's UDP discovery reach the backend and lets the
# backend advertise the desktop's real address instead of a private container
# address. FINGUARD_HOST keeps the API on loopback, since the host network would
# otherwise expose it, and it has no login.
podman run -d \
    --name finguard-rs-backend \
    --network host \
    --restart unless-stopped \
    -e PUID="${PUID}" \
    -e PGID="${PGID}" \
    -e FINGUARD_HOST=127.0.0.1 \
    -e FINGUARD_PORT="${FINGUARD_PORT}" \
    -e FINGUARD_SYNC_PORT="${FINGUARD_SYNC_PORT}" \
    -e XDG_DATA_HOME=/data \
    -e XDG_CONFIG_HOME=/config \
    -v "${DATA_DIR}:/data/finguard" \
    -v "${CONFIG_DIR}:/config/finguard" \
    "${BACKEND_IMAGE}"

# VITE_API_URL is the Vite dev server's /api proxy target, resolved inside the
# frontend container, not by the browser. The command override keeps the dev
# server, which has no login, on loopback instead of the image's default
# all-interfaces bind, and sets its port explicitly because host networking has
# no port remapping to fall back on.
podman run -d \
    --name finguard-rs-frontend \
    --network host \
    --restart unless-stopped \
    -e VITE_API_URL="http://127.0.0.1:${FINGUARD_PORT}" \
    "${FRONTEND_IMAGE}" \
    npm run dev -- --host 127.0.0.1 --port "${FRONTEND_PORT}"

echo
echo "Frontend: http://localhost:${FRONTEND_PORT}"
echo "Backend:  http://localhost:${FINGUARD_PORT}"
echo "Logs:     podman logs -f finguard-rs-backend"
echo "Stop:     podman stop finguard-rs-frontend finguard-rs-backend"

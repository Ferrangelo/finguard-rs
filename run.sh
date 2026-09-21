#!/bin/bash

# Run the Rust backend and Node.js frontend in parallel

echo "Starting finguard_rs development environment..."
echo ""

# Inside the dev container the host browser reaches the app through the
# container's published ports, so both servers must bind all interfaces.
# FINGUARD_HOST and FINGUARD_DEV_HOST are the backend's and frontend's own
# opt-ins; on the host neither is set here and both keep their loopback
# defaults. An explicit value the caller exported always wins.
if [ -f /run/.containerenv ]; then
    echo "Dev container detected: binding backend and frontend to 0.0.0.0."
    export FINGUARD_HOST="${FINGUARD_HOST:-0.0.0.0}"
    export FINGUARD_DEV_HOST="${FINGUARD_DEV_HOST:-0.0.0.0}"
fi

# Start the Rust backend in the background
echo "Starting Rust backend (cargo run)..."
cd backend && cargo run --bin finguard_rs_backend &
RUST_PID=$!

# Give the backend a moment to start
sleep 2

# Start the Node.js frontend
echo "Starting Node.js frontend (npm run dev)..."
cd frontend && npm run dev &
FRONTEND_PID=$!

echo ""
echo "Both services are running:"
echo "  - Rust backend (PID: $RUST_PID)"
echo "  - Node.js frontend (PID: $FRONTEND_PID)"
echo ""
echo "Press Ctrl+C to stop all services"

# Wait for both processes and handle interrupts gracefully
trap "kill $RUST_PID $FRONTEND_PID 2>/dev/null; echo 'Services stopped'; exit 0" INT TERM

wait

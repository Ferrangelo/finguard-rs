#!/bin/bash

# Run the Rust backend and Node.js frontend in parallel

echo "Starting finguard_rs development environment..."
echo ""

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

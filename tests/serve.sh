#!/usr/bin/env bash
# Start a local HTTP server for the WebView integration test page.
# Usage:
#   ./tests/serve.sh            # server only, port 8787
#   ./tests/serve.sh --browser  # also launch the browser example
set -euo pipefail

PORT=8787
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
URL="http://localhost:$PORT/webview-integrations.html"

PIDS=()

stop() {
    echo ""
    echo "Stopping..."
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    exit 0
}
trap stop INT TERM

cd "$SCRIPT_DIR"
python3 -m http.server "$PORT" --bind 127.0.0.1 &>/dev/null &
PIDS+=($!)
echo "Test server: $URL"

if [[ "${1:-}" == "--browser" ]]; then
    echo "Building and launching browser example..."
    cd "$ROOT"
    cargo run --example browser --features browser-example 2>&1 &
    PIDS+=($!)
fi

echo "Press Ctrl-C to stop."
wait

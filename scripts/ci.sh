#!/usr/bin/env bash
# Cross-platform build & test runner
# Usage:
#   ./scripts/ci.sh build    # Build on both platforms
#   ./scripts/ci.sh test     # Test on both platforms
#   ./scripts/ci.sh linux    # Linux only
#   ./scripts/ci.sh windows  # Windows only
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

DOCKER_IMAGE="libkrun-dev"
DOCKER_TAG="latest"
WIN_HOST="mukai@192.168.0.5"
WIN_PROJECT_DIR="C:\\Users\\mukai\\libkrun-dev\\libkrun"
WIN_VCENV="C:\\Users\\mukai\\libkrun-dev\\vcenv.bat"

# --- Docker helpers ---

docker_ensure_image() {
    if ! docker image inspect "${DOCKER_IMAGE}:${DOCKER_TAG}" >/dev/null 2>&1; then
        echo "[linux] Building Docker image..."
        docker build -t "${DOCKER_IMAGE}:${DOCKER_TAG}" "$PROJECT_DIR"
    fi
}

docker_run() {
    docker run --rm \
        -v "${PROJECT_DIR}:/work" \
        -e KRUN_INIT_BINARY_PATH=/tmp/fake_init \
        "${DOCKER_IMAGE}:${DOCKER_TAG}" \
        bash -c "touch /tmp/fake_init && $1"
}

# --- Windows helpers ---

win_sync() {
    echo "[windows] Syncing sources..."
    ssh -o BatchMode=yes "$WIN_HOST" "powershell -NoProfile -Command \"Remove-Item -Recurse -Force '${WIN_PROJECT_DIR}' -ErrorAction SilentlyContinue\""
    scp -r "$PROJECT_DIR" "${WIN_HOST}:$(dirname "$WIN_PROJECT_DIR")/libkrun"
}

win_run() {
    ssh -o BatchMode=yes "$WIN_HOST" "cd ${WIN_PROJECT_DIR} && $1"
}

# --- Actions ---

build_linux() {
    docker_ensure_image
    echo "[linux] cargo check (default features)..."
    docker_run "cd /work && cargo check 2>&1"
    echo "[linux] cargo check OK"
}

build_windows() {
    win_sync
    echo "[windows] cargo check (default features)..."
    win_run "cargo check 2>&1"
    echo "[windows] cargo check OK"
}

test_linux() {
    docker_ensure_image
    echo "[linux] cargo test..."
    docker_run "cd /work && cargo test 2>&1"
    echo "[linux] cargo test OK"
}

test_windows() {
    win_sync
    echo "[windows] cargo test..."
    win_run "cargo test 2>&1"
    echo "[windows] cargo test OK"
}

# --- Main ---

ACTION="${1:-build}"

case "$ACTION" in
    build)
        build_linux &
        PID_LINUX=$!
        build_windows &
        PID_WIN=$!
        wait $PID_LINUX && echo "[linux] BUILD PASSED" || echo "[linux] BUILD FAILED"
        wait $PID_WIN && echo "[windows] BUILD PASSED" || echo "[windows] BUILD FAILED"
        ;;
    test)
        test_linux &
        PID_LINUX=$!
        test_windows &
        PID_WIN=$!
        wait $PID_LINUX && echo "[linux] TEST PASSED" || echo "[linux] TEST FAILED"
        wait $PID_WIN && echo "[windows] TEST PASSED" || echo "[windows] TEST FAILED"
        ;;
    linux)
        build_linux
        ;;
    windows)
        build_windows
        ;;
    *)
        echo "Usage: $0 {build|test|linux|windows}"
        exit 1
        ;;
esac

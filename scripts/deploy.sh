#!/usr/bin/env bash
# Comptrol one-command deploy: build + tests + in-place binary swap + quick check.
#
# The executable name and deploy path work on Windows, macOS, and Linux.
# Windows cannot overwrite a running executable, so the old file is moved
# aside first. On macOS/Linux the same rename keeps the replacement atomic.
#
# Usage:
#   bash scripts/deploy.sh              # build + test + swap
#   bash scripts/deploy.sh --respawn    # Windows: restart a Comptrol HTTP sidecar
#   bash scripts/deploy.sh --no-test    # build + swap only (fast iterate)
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET_DIR="target-buffy"
RESPAWN=0
RUN_TESTS=1
case "${OSTYPE:-$(uname -s)}" in
  msys*|mingw*|cygwin*|Windows_NT)
    PLATFORM=windows
    EXE_SUFFIX=.exe
    ;;
  darwin*)
    PLATFORM=macos
    EXE_SUFFIX=
    ;;
  linux*)
    PLATFORM=linux
    EXE_SUFFIX=
    ;;
  *)
    echo "unsupported deployment platform: ${OSTYPE:-$(uname -s)}" >&2
    exit 2
    ;;
esac
for arg in "$@"; do
  case "$arg" in
    --respawn) RESPAWN=1 ;;
    --no-test) RUN_TESTS=0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

if [ "$RESPAWN" = "1" ] && [ "$PLATFORM" != "windows" ]; then
  echo "--respawn is currently supported only on Windows; restart the Comptrol HTTP sidecar manually." >&2
  exit 2
fi

echo "== build (CARGO_TARGET_DIR=$TARGET_DIR) =="
CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release -p comptrol

if [ "$RUN_TESTS" = "1" ]; then
  echo "== tests =="
  CARGO_TARGET_DIR="$TARGET_DIR" cargo test -p comptrol --lib
fi

echo "== deploy $PLATFORM binary =="
STAMP="$(date +%Y%m%d-%H%M%S)"
BIN_NAME="comptrol${EXE_SUFFIX}"
OLD="target/debug/$BIN_NAME"
BUILT="$TARGET_DIR/release/$BIN_NAME"
BACKUP="$OLD.prev-$STAMP"
mkdir -p target/debug
if [ -f "$OLD" ]; then
  mv "$OLD" "$BACKUP"
fi
mv "$BUILT" "$OLD"
if [ -f "$BACKUP" ]; then
  echo "   previous binary kept at: $BACKUP"
fi

if [ "$RESPAWN" = "1" ]; then
  echo "== respawn serve-http sidecar (port 7317) =="
  PIDS="$(netstat -ano | grep ':7317' | grep LISTENING | awk '{print $NF}' | sort -u || true)"
  if [ -n "$PIDS" ]; then
    for pid in $PIDS; do
      NAME="$(tasklist //FI "PID eq $pid" 2>/dev/null | awk 'NR==4{print $1}' || true)"
      if [ "$NAME" = "comptrol.exe" ]; then
        echo "   killing stale serve-http PID $pid"
        taskkill //PID "$pid" //F >/dev/null 2>&1 || true
      else
        echo "   PID $pid on 7317 is '$NAME' (not comptrol) - leaving it alone"
      fi
    done
  else
    echo "   no serve-http listener running (MCP will spawn one on demand)"
  fi
  echo "   note: MCP stdio processes pick up the new binary on next client reload"
fi

echo "== smoke check =="
VERSION="$("$OLD" version 2>/dev/null || echo unknown)"
echo "   deployed binary: $VERSION"

# P6.3 (T8): the Rust side deploys in one command, but a browser extension
# only updates when Chrome is told to. Stage the package and detect whether
# the deployed copy actually differs, so the user is told to reload exactly
# when a reload is needed instead of discovering it later as a stale worker.
echo "== stage browser extension =="
if command -v node >/dev/null 2>&1; then
  node scripts/sync-browser-bridge-package.js >/dev/null

  SRC_DIR="extensions/comptrol-browser-bridge"
  STAGE_DIR="${HOME}/.comptrol/browser-extensions"
  if [ -d "$STAGE_DIR" ]; then
    # Sync the real package tree. The service worker lives at src/ (earlier
    # revisions compared a root-level service_worker.js that never existed,
    # so worker changes silently never staged and the reload notice never
    # fired). A reload is only needed when a file Chrome loads into the
    # extension context changed: the manifest, the service worker, popups, or
    # the offscreen documents. Host-side files (native_host.py, the .bat
    # wrapper, host manifests) are re-read at every native host launch.
    CHANGED=""
    RELOAD=""
    for src_file in $(cd "$SRC_DIR" && find . -type f \
        ! -path '*/__pycache__/*' ! -name '*.pyc' ! -name '*.pyo' ! -name '*.md' | sort); do
      file="${src_file#./}"
      file_changed=0
      for staged in "$STAGE_DIR"/*/; do
        [ -d "$staged" ] || continue
        dst_file="$staged$file"
        if [ ! -f "$dst_file" ] || ! cmp -s "$SRC_DIR/$file" "$dst_file"; then
          file_changed=1
          mkdir -p "$(dirname "$dst_file")"
          # Keep the staged copy current so the next deploy is a no-op.
          cp "$SRC_DIR/$file" "$dst_file"
        fi
      done
      if [ "$file_changed" = 1 ]; then
        CHANGED="$CHANGED $file"
        case "$file" in
          manifest.json|src/*|offscreen/*) RELOAD="$RELOAD $file" ;;
        esac
      fi
    done
    if [ -n "$CHANGED" ]; then
      echo "   extension files staged:$CHANGED"
      if [ -n "$RELOAD" ]; then
        echo "   extension worker changed:$RELOAD"
        echo "   ACTION REQUIRED: open chrome://extensions, find Comptrol, click Reload."
        echo "   The Rust side is already live; only the extension worker needs the reload."
      else
        echo "   staged host-side files only (the native host re-reads them at its next launch; no reload needed)"
      fi
    else
      echo "   extension is already staged and identical (no reload needed)"
    fi
  else
    echo "   no staged extension directory yet; run 'comptrol setup' to install it"
  fi
else
  echo "   node not found; skipped extension staging"
fi

echo "deploy complete."

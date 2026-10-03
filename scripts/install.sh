#!/usr/bin/env bash
set -euo pipefail

# Determine repository root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

echo "==> Building agentctl (release)..."
cargo build --release -p agentctl "$@"

# Target installation directory
DEST_DIR="${HOME}/.local/bin"
mkdir -p "${DEST_DIR}"

TARGET_BIN="${REPO_ROOT}/target/release/agentctl"
DEST_BIN="${DEST_DIR}/agentctl"

echo "==> Installing agentctl to ${DEST_BIN}..."
cp "${TARGET_BIN}" "${DEST_BIN}"

# On macOS, Apple Silicon Gatekeeper will SIGKILL (-9) binaries modified or replaced
# in-place unless an ad-hoc or valid codesign is applied.
if [[ "$(uname -s)" == "Darwin" ]]; then
    echo "==> Applying ad-hoc codesign for macOS..."
    codesign --force --deep --sign - "${DEST_BIN}"
    codesign -v "${DEST_BIN}"
fi

echo "==> Verification:"
"${DEST_BIN}" --version
echo "==> Successfully installed and verified agentctl at ${DEST_BIN}"

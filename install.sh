#!/usr/bin/env bash
set -euo pipefail

# gdsync - Git-Aware Realtime Google Drive Sync
# One-line installer script

REPO="sadabx/gdsync"
BINARY_NAME="gdsync"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

info() {
    printf "${BLUE}[INFO]${NC} %s\n" "$1"
}

success() {
    printf "${GREEN}[OK]${NC} %s\n" "$1"
}

warn() {
    printf "${RED}[WARN]${NC} %s\n" "$1"
}

error() {
    printf "${RED}[ERROR]${NC} %s\n" "$1" >&2
    exit 1
}

# 1. Platform Detection
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
if [ "$OS" != "linux" ]; then
    error "gdsync is currently designed and optimized for Linux (inotify support required)."
fi

ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|amd64)
        TARGET_ARCH="x86_64"
        ;;
    aarch64|arm64)
        TARGET_ARCH="aarch64"
        ;;
    *)
        error "Unsupported architecture: $ARCH. gdsync supports x86_64 and aarch64."
        ;;
esac

# 2. Determine installation directory
if [ "$(id -u)" -eq 0 ]; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="${HOME}/.local/bin"
fi
mkdir -p "$INSTALL_DIR"

info "Target installation path: ${INSTALL_DIR}/${BINARY_NAME}"

# 3. Check for download utilities
if command -v curl >/dev/null 2>&1; then
    DOWNLOAD_CMD="curl -fsSL"
elif command -v wget >/dev/null 2>&1; then
    DOWNLOAD_CMD="wget -qO-"
else
    error "Neither 'curl' nor 'wget' was found. Please install one to continue."
fi

# 4. Attempt release binary download from GitHub
TMP_DIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

info "Checking for latest release of ${REPO}..."
DOWNLOADED=0

RELEASE_URL="https://github.com/${REPO}/releases/latest/download/gdsync-linux-${TARGET_ARCH}.tar.gz"

if curl -sI --fail "$RELEASE_URL" >/dev/null 2>&1; then
    info "Downloading prebuilt binary for ${TARGET_ARCH}..."
    if curl -fsSL "$RELEASE_URL" | tar -xz -C "$TMP_DIR" 2>/dev/null; then
        if [ -f "${TMP_DIR}/${BINARY_NAME}" ]; then
            install -m 755 "${TMP_DIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
            DOWNLOADED=1
        fi
    fi
fi

# 5. Fallback: If no binary release asset exists yet, build with cargo if available
if [ "$DOWNLOADED" -eq 0 ]; then
    if command -v cargo >/dev/null 2>&1; then
        info "Precompiled release binary not found for ${TARGET_ARCH}. Building via cargo..."
        cargo install --git "https://github.com/${REPO}.git" gdsync-cli --root "${TMP_DIR}/cargo_build"
        install -m 755 "${TMP_DIR}/cargo_build/bin/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
        DOWNLOADED=1
    else
        error "Precompiled binary is not yet available and 'cargo' is not installed.\nPlease install Rust via https://rustup.rs or check https://github.com/${REPO}/releases"
    fi
fi

# 6. Verify installation
if [ -x "${INSTALL_DIR}/${BINARY_NAME}" ]; then
    success "Successfully installed ${BINARY_NAME} to ${INSTALL_DIR}/${BINARY_NAME}"
else
    error "Installation failed: executable not found in ${INSTALL_DIR}"
fi

# 7. Check PATH
case ":$PATH:" in
    *":${INSTALL_DIR}:"*)
        ;;
    *)
        warn "${INSTALL_DIR} is not in your PATH."
        printf "Add it to your shell configuration (e.g. ~/.bashrc or ~/.zshrc):\n"
        printf "  export PATH=\"%s:\$PATH\"\n\n" "$INSTALL_DIR"
        ;;
esac

printf "\nRun '${BOLD}gdsync${NC}' to start the interactive environment.\n"

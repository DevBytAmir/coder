#!/usr/bin/env sh
# Installs a prebuilt coder binary from GitHub releases: detects OS/arch, downloads
# the matching asset and its SHA256SUMS.txt, verifies the checksum, installs to
# ~/.local/bin, and checks for podman.
#
# Usage: curl -fsSL <raw-url-to-this-script> | sh
# Override the version with: CODER_VERSION=0.1.0 sh install.sh

set -eu

REPO="DevBytAmir/coder"
INSTALL_DIR="${CODER_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

detect_os() {
    case "$(uname -s)" in
        Linux) echo linux ;;
        Darwin) echo macos ;;
        *) die "unsupported OS: $(uname -s) (Windows: use install.ps1 instead)" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64 | amd64) echo x86_64 ;;
        arm64 | aarch64) echo arm64 ;;
        *) die "unsupported architecture: $(uname -m)" ;;
    esac
}

OS=$(detect_os)
ARCH=$(detect_arch)
ASSET="coder-${OS}-${ARCH}"

if [ -n "${CODER_VERSION:-}" ]; then
    TAG="v${CODER_VERSION}"
else
    say "looking up the latest release..."
    TAG=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" |
        grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
    [ -n "$TAG" ] || die "could not determine the latest release tag"
fi

BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

say "downloading ${ASSET} (${TAG})..."
curl -fsSL "${BASE_URL}/${ASSET}" -o "${WORK_DIR}/${ASSET}" ||
    die "failed to download ${ASSET} for ${TAG}; check that a release for your platform exists"
curl -fsSL "${BASE_URL}/SHA256SUMS.txt" -o "${WORK_DIR}/SHA256SUMS.txt" ||
    die "failed to download SHA256SUMS.txt for ${TAG}"

say "verifying checksum..."
EXPECTED=$(grep " ${ASSET}\$" "${WORK_DIR}/SHA256SUMS.txt" | cut -d' ' -f1)
[ -n "$EXPECTED" ] || die "${ASSET} is not listed in SHA256SUMS.txt"
if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL=$(sha256sum "${WORK_DIR}/${ASSET}" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL=$(shasum -a 256 "${WORK_DIR}/${ASSET}" | cut -d' ' -f1)
else
    die "neither sha256sum nor shasum is available to verify the download"
fi
[ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch for ${ASSET}: expected ${EXPECTED}, got ${ACTUAL}"

mkdir -p "$INSTALL_DIR"
install -m 755 "${WORK_DIR}/${ASSET}" "${INSTALL_DIR}/coder"
say "installed coder to ${INSTALL_DIR}/coder"

case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        say ""
        say "warning: ${INSTALL_DIR} is not on your PATH. Add this to your shell profile:"
        say "  export PATH=\"${INSTALL_DIR}:\$PATH\""
        ;;
esac

say ""
if command -v podman >/dev/null 2>&1; then
    say "podman: found ($(podman --version))"
else
    say "podman: not found. coder needs it to run sandboxed agents. Install it with:"
    case "$OS" in
        linux)
            say "  sudo pacman -S podman      # Arch"
            say "  sudo apt install podman    # Debian/Ubuntu"
            say "  sudo dnf install podman    # Fedora"
            ;;
        macos)
            say "  brew install podman && podman machine init && podman machine start"
            ;;
    esac
fi

say ""
say "Next steps:"
say "  coder build --fetch   # or --source-dir <path> if you have a checkout"
say "  coder init"

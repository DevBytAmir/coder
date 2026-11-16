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

# The podman install command for this Linux distro, read from os-release ($1, default
# /etc/os-release), or nothing if the distro isn't recognized.
podman_install_hint() {
    ids=$(
        [ -r "${1:-/etc/os-release}" ] && . "${1:-/etc/os-release}"
        printf '%s %s' "${ID:-}" "${ID_LIKE:-}"
    )
    case " $ids " in
        *" arch "*) echo "sudo pacman -S podman" ;;
        *" fedora "* | *" rhel "* | *" centos "*) echo "sudo dnf install podman" ;;
        *" debian "* | *" ubuntu "*) echo "sudo apt install podman" ;;
        *" suse "* | *" opensuse "*) echo "sudo zypper install podman" ;;
        *" alpine "*) echo "sudo apk add podman" ;;
    esac
}

OS=$(detect_os)
ARCH=$(detect_arch)
ASSET="coder-${OS}-${ARCH}"

if [ -n "${CODER_VERSION:-}" ]; then
    TAG="v${CODER_VERSION}"
else
    say "looking up the latest release..."
    # The web URL redirects to .../releases/tag/<tag>. Unlike api.github.com, it isn't
    # limited to 60 anonymous requests an hour per IP, which shared IPs and CI hit.
    LATEST_URL=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest") ||
        die "could not look up the latest release of ${REPO} on GitHub"
    TAG=${LATEST_URL##*/tag/}
    case "$TAG" in
        v[0-9]*) ;;
        *) die "could not determine the latest release tag (got ${LATEST_URL})" ;;
    esac
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
            HINT=$(podman_install_hint)
            if [ -n "$HINT" ]; then
                say "  $HINT"
            else
                say "  sudo pacman -S podman      # Arch"
                say "  sudo apt install podman    # Debian/Ubuntu"
                say "  sudo dnf install podman    # Fedora"
            fi
            ;;
        macos)
            say "  brew install podman && podman machine init && podman machine start"
            ;;
    esac
fi

say ""
say "Next steps:"
say "  coder build   # downloads this version's container files the first time"
say "  coder init"

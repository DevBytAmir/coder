#!/usr/bin/env bash
# Sets coder's version everywhere it is declared, or checks that they all agree.
#
# Usage:
#   scripts/bump-version.sh <X.Y.Z>   set the version in every file listed below
#   scripts/bump-version.sh --check   exit 1 if any of them disagrees with Cargo.toml
#
# CHANGELOG.md is left alone: write its release section by hand.

set -euo pipefail
cd "$(dirname "$0")/.."

MANIFESTS=(
    plugins/coder/.claude-plugin/plugin.json
    plugins/coder/.codex-plugin/plugin.json
)
JSON_VERSION='"version": *"[^"]*"'

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

# The first match of the extended regex $2 in file $1, or nothing.
first_match() { grep -m1 -oE "$2" "$1" || true; }

# Replace the first match of the extended regex $2 in file $1 with $3.
replace_first() {
    awk -v re="$2" -v rep="$3" '
        !done && match($0, re) {
            $0 = substr($0, 1, RSTART - 1) rep substr($0, RSTART + RLENGTH)
            done = 1
        }
        { print }
    ' "$1" >"$1.tmp" && mv "$1.tmp" "$1"
}

cargo_version() { first_match Cargo.toml '^version = "[^"]*"' | cut -d'"' -f2; }
lock_version() { awk '/^name = "coder"$/ { getline; print; exit }' Cargo.lock | cut -d'"' -f2; }
json_version() { first_match "$1" "$JSON_VERSION" | sed -E 's/.*"([^"]*)"$/\1/'; }
readme_version() { first_match README.md 'badge/version-[0-9.]+-' | sed -E 's/badge\/version-(.*)-/\1/'; }

check() {
    local expected status=0
    expected=$(cargo_version)
    [ -n "$expected" ] || die "no version found in Cargo.toml"
    report() {
        if [ "$2" = "$expected" ]; then
            printf '  ok        %-44s %s\n' "$1" "$2"
        else
            printf '  MISMATCH  %-44s %s (expected %s)\n' "$1" "${2:-<none>}" "$expected"
            status=1
        fi
    }
    report Cargo.toml "$expected"
    report Cargo.lock "$(lock_version)"
    for manifest in "${MANIFESTS[@]}"; do
        report "$manifest" "$(json_version "$manifest")"
    done
    report "README.md (badge)" "$(readme_version)"
    return "$status"
}

case "${1:-}" in
    --check)
        check || die "versions disagree; run scripts/bump-version.sh $(cargo_version)"
        ;;
    [0-9]*.[0-9]*.[0-9]*)
        new=$1
        [[ $new =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "expected a version like 1.2.3, got $new"
        replace_first Cargo.toml '^version = "[^"]*"' "version = \"$new\""
        for manifest in "${MANIFESTS[@]}"; do
            replace_first "$manifest" "$JSON_VERSION" "\"version\": \"$new\""
        done
        replace_first README.md 'badge/version-[0-9.]+-' "badge/version-$new-"
        cargo update --workspace --offline --quiet
        check
        printf '\nnext: add a [%s] section to CHANGELOG.md, commit, and tag v%s\n' "$new" "$new"
        ;;
    *)
        die "usage: scripts/bump-version.sh <X.Y.Z> | --check"
        ;;
esac

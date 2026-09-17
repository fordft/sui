#!/bin/sh
# sui installer — checksum-verified wrapper around the cargo-dist installer.
#
#   curl --proto '=https' --tlsv1.2 -LsSf \
#     https://github.com/fordft/sui/releases/latest/download/install.sh | sh
#
#   SUI_TAG=v0.3.0 ... | sh        pin a specific release
#   sh install.sh --force          skip the foreign-`sui` prompt
#
# Steps: detect platform → fetch archive + .sha256 sidecar → verify →
# delegate to the release's sui-installer.sh against the verified local
# copy → install to ~/.sui/bin (never clobbers another `sui`).
set -eu

APP_DIR="$HOME/.sui/bin"
REPO="fordft/sui"
TAG="${SUI_TAG:-latest}"
FORCE=0
[ "${1:-}" = "--force" ] && FORCE=1

say() { echo "install.sh: $*" >&2; }
die() { say "error: $*"; exit 1; }

# ── platform ─────────────────────────────────────────────────────────
os="$(uname -s)"; arch="$(uname -m)"
case "$os/$arch" in
    Darwin/arm64)  ARCHIVE="sui-aarch64-apple-darwin.tar.xz" ;;
    Darwin/x86_64) ARCHIVE="sui-x86_64-apple-darwin.tar.xz" ;;
    Linux/x86_64)  ARCHIVE="sui-x86_64-unknown-linux-gnu.tar.xz" ;;
    Linux/aarch64) ARCHIVE="sui-aarch64-unknown-linux-gnu.tar.xz" ;;
    *) die "unsupported platform $os/$arch (musl/Alpine and Windows are not packaged; use WSL2 on Windows)" ;;
esac

if [ "$TAG" = "latest" ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
else
    BASE="https://github.com/$REPO/releases/download/$TAG"
fi

# ── foreign-binary collision check ───────────────────────────────────
existing="$(command -v sui 2>/dev/null || true)"
case "$existing" in
    ""|"$APP_DIR"/*) ;;
    *)
        say "warning: '$existing' already provides a 'sui' command (e.g. the"
        say "Mysten Sui toolchain). We install only to $APP_DIR — it will"
        say "not be touched, but 'sui' may still resolve to it on PATH."
        if [ "$FORCE" -eq 0 ] && [ -t 0 ]; then
            printf "continue anyway? [y/N] " >&2
            read -r a
            case "$a" in y|Y) ;; *) exit 1 ;; esac
        fi
        ;;
esac

# ── download + verify ────────────────────────────────────────────────
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

say "fetching $ARCHIVE + checksum ($BASE)"
curl --proto '=https' --tlsv1.2 -LsSf -O "$BASE/$ARCHIVE" || die "download failed"
curl --proto '=https' --tlsv1.2 -LsSf -O "$BASE/$ARCHIVE.sha256" || die "checksum download failed"
curl --proto '=https' --tlsv1.2 -LsSf -O "$BASE/sui-installer.sh" || die "installer download failed"

if command -v sha256sum >/dev/null 2>&1; then
    ( cd "$tmp" && sha256sum -c "$ARCHIVE.sha256" ) || die "checksum mismatch — aborting"
elif command -v shasum >/dev/null 2>&1; then
    ( cd "$tmp" && shasum -a 256 -c "$ARCHIVE.sha256" ) || die "checksum mismatch — aborting"
else
    die "no sha256sum/shasum available to verify the archive"
fi

# ── delegate to the real installer over the verified local copy ──────
say "checksum ok — installing to $APP_DIR"
SUI_DOWNLOAD_URL="file://$tmp" SUI_INSTALL_DIR="$APP_DIR" sh "$tmp/sui-installer.sh" \
    || die "install failed (existing install untouched)"

say "installed: $APP_DIR/sui  ($APP_DIR/sui-mission, $APP_DIR/sui-certify)"
say "launch:    $APP_DIR/sui"
case ":${PATH}:" in
    *":$APP_DIR:"*) ;;
    *) say "PATH:      export PATH=\"$APP_DIR:\$PATH\"" ;;
esac

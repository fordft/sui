#!/bin/sh
# Explicit installation for Macs without a notarized SUI release. Verify the
# release checksum and signatures before removing quarantine from SUI only.
set -eu

say() { printf 'install-desktop.sh: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

[ "$#" -le 1 ] || die 'usage: sh install-desktop.sh [absolute installation directory]'
prefix="${1:-$HOME/Applications}"
case "$prefix" in /*) ;; *) die 'installation directory must be absolute' ;; esac
destination="$prefix/SUI Desktop.app"
case "$(uname -s)/$(uname -m)" in
    Darwin/arm64) arch=arm64; cpu=arm64 ;;
    Darwin/x86_64) arch=x64; cpu=x86_64 ;;
    *) die 'Desktop installer requires an Apple Silicon or Intel Mac' ;;
esac

repo='https://github.com/fordft/sui/releases'
tag="${SUI_TAG:-latest}"
if [ "$tag" = latest ]; then
    latest=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fLsS \
        --connect-timeout 15 --max-time 60 -o /dev/null -w '%{url_effective}' "$repo/latest") \
        || die 'could not resolve the latest release'
    tag="${latest##*/}"
fi
case "$tag" in *[!v0-9.]*) die 'SUI_TAG contains invalid characters' ;; esac
printf '%s\n' "$tag" | LC_ALL=C grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' \
    || die 'SUI_TAG must be a stable release tag such as v0.11.6'
version="${tag#v}"
asset="SUI.Desktop-$version-$arch.dmg"
base="$repo/download/$tag"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/sui-desktop.XXXXXX")
mountpoint="$tmp/mounted"
mounted=0
stage=''
cleanup() {
    if [ -n "$stage" ]; then
        if [ -d "$stage/previous.app" ] && [ ! -e "$destination" ]; then
            mv "$stage/previous.app" "$destination" || return
        fi
        rm -rf "$stage"
    fi
    if [ "$mounted" -eq 1 ]; then
        hdiutil detach "$mountpoint" >/dev/null 2>&1 || return
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

say "fetching $asset and its SHA-256 checksum"
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fLsS \
    --connect-timeout 15 --max-time 60 --max-filesize 1048576 \
    -o "$tmp/checksums" "$base/SUI-Desktop-$version-SHA256SUMS.txt" \
    || die 'checksum download failed; use a release with Desktop assets'
expected=$(awk -v name="$asset" '$2 == name { print $1 }' "$tmp/checksums")
[ "${#expected}" -eq 64 ] || die 'release checksum is missing or ambiguous'
case "$expected" in *[!0-9a-f]*) die 'release checksum is invalid' ;; esac
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fLsS \
    --connect-timeout 15 --max-time 600 --max-filesize 1073741824 \
    -o "$tmp/$asset" "$base/$asset" || die 'DMG download failed'
actual=$(shasum -a 256 "$tmp/$asset")
[ "${actual%% *}" = "$expected" ] || die 'checksum mismatch; existing installation untouched'

mkdir "$mountpoint"
hdiutil attach -readonly -nobrowse -mountpoint "$mountpoint" "$tmp/$asset" >/dev/null
mounted=1
bundle="$mountpoint/SUI Desktop.app"
[ "$(plutil -extract CFBundleIdentifier raw -o - "$bundle/Contents/Info.plist")" = dev.sui.desktop ] \
    || die 'download does not contain the SUI Desktop app'
lipo "$bundle/Contents/MacOS/SUI Desktop" -verify_arch "$cpu"
lipo "$bundle/Contents/Resources/sui" -verify_arch "$cpu"
codesign --verify --deep --strict "$bundle"
codesign --verify --strict "$bundle/Contents/Resources/sui"

[ ! -L "$destination" ] || die 'refusing to replace a symbolic link'
if [ -e "$destination" ]; then
    [ "$(plutil -extract CFBundleIdentifier raw -o - "$destination/Contents/Info.plist")" = dev.sui.desktop ] \
        || die 'refusing to replace an app other than SUI Desktop'
fi
mkdir -p "$prefix"
stage=$(mktemp -d "$prefix/.sui-desktop.XXXXXX")
ditto "$bundle" "$stage/SUI Desktop.app"
codesign --verify --deep --strict "$stage/SUI Desktop.app"
say 'removing download quarantine from the verified SUI app only'
xattr -dr com.apple.quarantine "$stage/SUI Desktop.app"
[ "$("$stage/SUI Desktop.app/Contents/Resources/sui" --version)" = "sui $version" ] \
    || die 'bundled SUI could not run or has the wrong version; existing installation untouched'
if [ -e "$destination" ]; then mv "$destination" "$stage/previous.app"; fi
mv "$stage/SUI Desktop.app" "$destination"
say "installed $version at $destination"
say "launch with: open \"$destination\""

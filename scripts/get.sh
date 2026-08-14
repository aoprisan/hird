#!/bin/sh
#
# Install a prebuilt hird release binary — no Rust toolchain required.
#
#   curl -fsSL https://raw.githubusercontent.com/aoprisan/hird/main/scripts/get.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/aoprisan/hird/main/scripts/get.sh | sh -s -- --install-skill
#
# Downloads the release asset for this machine's platform, verifies it
# against the release's own checksum, and hands the binary its `--install`,
# so the result is byte-for-byte what scripts/install.sh produces: a
# standalone snapshot at ~/.local/bin/hird. Arguments are forwarded to
# `hird --install`, which is how `--install-skill` works here too.
#
# HIRD_VERSION pins a release tag (default: the latest release):
#
#   HIRD_VERSION=v0.1.0 sh scripts/get.sh
#
# Building from source instead is scripts/install.sh, which needs a Rust
# toolchain and nothing here.

set -eu

repo="aoprisan/hird"

say() { printf '%s\n' "$*" >&2; }
die() {
    say "get.sh: $*"
    exit 1
}

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO- "$1"
    else
        die "neither curl nor wget is available to download with"
    fi
}

os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
    Linux/x86_64) target=x86_64-unknown-linux-musl ;;
    Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    Darwin/arm64) target=aarch64-apple-darwin ;;
    *) die "no prebuilt binary for $os/$arch; scripts/install.sh builds from source (needs a Rust toolchain)" ;;
esac

tag="${HIRD_VERSION:-}"
if [ -z "$tag" ]; then
    tag="$(fetch "https://api.github.com/repos/$repo/releases/latest" |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)" || true
    [ -n "$tag" ] || die "could not resolve the latest release; pin one with HIRD_VERSION=v0.x.y"
fi

asset="hird-$tag-$target.tar.gz"
url="https://github.com/$repo/releases/download/$tag/$asset"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading $asset"
fetch "$url" >"$tmp/$asset" || die "release $tag has no asset $asset ($url)"
fetch "$url.sha256" >"$tmp/$asset.sha256" || die "release $tag has no checksum for $asset"

cd "$tmp"
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$asset.sha256" >/dev/null || die "checksum mismatch on $asset"
elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$asset.sha256" >/dev/null || die "checksum mismatch on $asset"
else
    say "neither sha256sum nor shasum is available; skipping checksum verification"
fi

tar -xzf "$asset"
./hird --install "$@"

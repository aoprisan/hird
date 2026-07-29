#!/usr/bin/env bash
#
# Build a release snapshot, install it into ~/.local/bin, then remove the
# release-profile artifacts. Extra arguments are forwarded to hird, so
# `scripts/install.sh --install-skill` installs the bundled skill in the same
# run.

set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cleanup_release() {
    local status=$?
    trap - EXIT
    if ! cargo clean --release --locked; then
        if (( status == 0 )); then
            status=1
        fi
    fi
    exit "$status"
}
trap cleanup_release EXIT

cargo build --release --locked

target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
if [[ "$target_dir" != /* ]]; then
    target_dir="$repo_root/$target_dir"
fi

"$target_dir/release/hird" --install "$@"

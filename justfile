# hird — development tasks.
# `just` with no arguments lists what is available.

default:
    @just --list

# Everything CI would check: formatting, lints, and the full test suite.
check: fmt-check lint test

build:
    cargo build

release:
    cargo build --release

test:
    cargo test --all-targets

# Just the fast in-process tests, without spawning the binary.
test-unit:
    cargo test --lib

lint:
    cargo clippy --all-targets -- -D warnings

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

# Install `hird` into ~/.cargo/bin.
install:
    cargo install --path . --locked

docs:
    cargo doc --no-deps --open

# Where this machine's database lives.
db-path:
    cargo run --quiet -- db-path

# Run the TUI against a throwaway database seeded with sample tasks.
demo:
    #!/usr/bin/env bash
    set -euo pipefail
    db="$(mktemp -d)/hird.db"
    export HIRD_DB="$db"
    cargo build --quiet
    bin="target/debug/hird"
    "$bin" add "Port the config loader to serde" --priority 3 \
        --body "Keep the env-var precedence. Tests in tests/config.rs must pass." >/dev/null
    "$bin" add "Fix the flaky renderer test" >/dev/null
    "$bin" add "Write the release notes" --priority -1 >/dev/null
    "$bin" mem add "Integration tests need HIRD_DB set or they touch the real database" \
        --tags testing >/dev/null
    echo "demo database: $db"
    "$bin" tui

# Delete build artifacts.
clean:
    cargo clean

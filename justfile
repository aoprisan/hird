# hird — development tasks.
# `just` with no arguments lists what is available.

default:
    @just --list

# Everything CI would check: formatting, lints, the test suite and the site.
check: fmt-check lint test site-check

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

# Open the static usage guide — the same files GitHub Pages publishes.
site:
    #!/usr/bin/env bash
    set -euo pipefail
    for opener in xdg-open open; do
        if command -v "$opener" >/dev/null; then exec "$opener" docs/index.html; fi
    done
    echo "open docs/index.html in a browser"

# Check the site's own links and assets, the way CI does.
site-check:
    .github/scripts/check-docs-links.sh

# Run the example scripts end to end against throwaway databases.
examples: build
    #!/usr/bin/env bash
    set -euo pipefail
    export HIRD_BIN="$PWD/target/debug/hird"
    ./examples/manual-dispatch.sh
    ./examples/swarm-plan.sh
    ./examples/plan-file.sh
    ./examples/witness.sh
    ./examples/exhibit.sh
    ./examples/tenure.sh
    ./examples/footing.sh
    ./examples/review.sh
    ./examples/verdict.sh
    ./examples/dispatch-hook.sh
    ./examples/protocol.sh

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
        --path "src/config.rs" --path "tests/config.rs" \
        --body "Keep the env-var precedence. Tests in tests/config.rs must pass." >/dev/null
    "$bin" add "Fix the flaky renderer test" --path "src/tui/**" >/dev/null
    "$bin" add "Write the release notes" --priority -1 --needs 1,2 >/dev/null
    # Someone is already in the config loader, so the board has a live overlap
    # and one blocked task to show off.
    "$bin" add "Audit the config tests" --path "tests/**" >/dev/null
    # Linked to task 1, which declares tests/config.rs — so opening task 4
    # (tests/**) shows recall carrying this fact across, with its provenance.
    "$bin" mem add "Integration tests need HIRD_DB set or they touch the real database" \
        --tags testing --task 1 >/dev/null
    # Anchored to files that really exist in this checkout, so the Memory tab
    # has a footing to show — `f` there filters to whatever has moved since.
    "$bin" mem add "The witness only fingerprints what git already calls dirty" \
        --tags witness --path src/witness.rs >/dev/null
    "$bin" mem add "Every migration is numbered and applied on open" \
        --tags schema --path src/db.rs >/dev/null
    echo "demo database: $db"
    "$bin" tui

# Delete build artifacts.
clean:
    cargo clean

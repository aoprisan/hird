# Repository Guidelines

## Project Structure & Module Organization

`hird` is a Rust 2021 binary and library. `src/main.rs` starts the application, while `src/lib.rs` exposes the main layers: domain types in `model.rs`, SQLite setup and migrations in `db.rs`, and typed data access in `repo/`. Keep SQL inside `src/repo/`; the CLI (`cli.rs`), MCP server (`mcp.rs`), and TUI (`tui/`) should call that layer instead. Shared formatting, identity, configuration, and error handling live in their named modules. Integration tests are in `tests/`, example configuration is in `examples/`, and `DESIGN.md` records architecture and state-machine decisions.

## Build, Test, and Development Commands

Use the `justfile` as the normal entry point:

- `just build` compiles a debug binary.
- `just test` runs `cargo test --all-targets`.
- `just test-unit` runs only fast library tests.
- `just check` runs formatting checks, Clippy with warnings denied, and all tests—the same gates as CI.
- `just demo` launches the TUI with a seeded temporary database.
- `just install` installs the locked local crate into Cargo’s bin directory.

The project requires Rust 1.88 or newer. Run `just fmt` before submitting changes.

## Coding Style & Naming Conventions

Follow standard `rustfmt` output (four-space indentation). Use `snake_case` for modules, functions, variables, and test names; use `PascalCase` for types and traits; use `SCREAMING_SNAKE_CASE` for constants. Prefer small typed repository methods over SQL or database details leaking into front ends. Preserve the task transition rules documented in `DESIGN.md`. Clippy must pass with `-D warnings`.

## Testing Guidelines

Place focused unit tests beside their module under `#[cfg(test)]`; add end-to-end CLI or stdio MCP behavior to `tests/cli.rs` or `tests/mcp_stdio.rs`. Name tests as behavioral statements, such as `claim_is_atomic_across_connections`. Use `tests/support::Sandbox` so tests operate on temporary config, project, and database paths rather than user data. There is no numeric coverage threshold, but new behavior and regressions should have tests.

## Commit & Pull Request Guidelines

History uses concise, imperative milestone subjects such as `M4: the TUI — queue board and memory browser`. Continue that style when a change belongs to a milestone; otherwise use a short, scoped summary. Keep commits cohesive. Pull requests should explain the user-visible effect, note design or schema implications, link relevant issues, and report `just check` results. Include screenshots for TUI changes and update `README.md`, `DESIGN.md`, or `examples/config.toml` when behavior or configuration changes.

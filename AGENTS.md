# AGENTS.md
Guide for coding agents working in `fast-movie-dl`.

## Project overview
- Language: Rust 2021
- Crate: single binary CLI (`fast-movie-dl`)
- Entry point: `src/main.rs`
- Runtime dependency: `aria2c` on `PATH`
- Key modules:
  - `src/cli.rs` (clap command/arg model)
  - `src/doctor.rs` (environment checks)
  - `src/auth.rs` (prompt + keychain credential store)
  - `src/probe.rs` (protocol candidate resolution + probing)
  - `src/planner.rs` (transfer planning + aria2 args)
  - `src/runner.rs` (process execution + streaming output)
  - `src/errors.rs` (app-level domain errors)

## Cursor and Copilot rules
Checked the following locations:
- `.cursorrules`
- `.cursor/rules/`
- `.github/copilot-instructions.md`

Result: no Cursor/Copilot rule files currently exist in this repository.
If they are added later, treat them as mandatory and fold them into this file.

## Build, lint, test commands

### Setup / environment
```bash
rustup show
brew install aria2
cargo run -- doctor
```

### Build
```bash
cargo build
cargo build --release
```

Release binary:
```bash
./target/release/fast-movie-dl
```

### Format
```bash
cargo fmt --all
cargo fmt --all -- --check
```

### Lint
```bash
cargo clippy --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

### Test all
```bash
cargo test
```

### Discover tests
```bash
cargo test -- --list
```

### Run a single test (important)
Single test by path:
```bash
cargo test planner::tests::chooses_connection_count
```

Single exact test only:
```bash
cargo test probe::tests::filters_forced_protocol -- --exact
```

Single exact test with output:
```bash
cargo test probe::tests::resolves_auto_candidates_with_dedup -- --exact --nocapture
```

Substring match (quick targeting):
```bash
cargo test resolves_auto_candidates
```

### Suggested pre-PR validation
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Style and coding guidelines

### Imports
- Keep imports explicit; avoid wildcard imports.
- Order groups as `std` -> external crates -> `crate::...`.
- Prefer targeted imports, e.g. `anyhow::{Context, Result}`.

### Formatting
- Let `rustfmt` decide layout.
- Keep functions focused and readable.
- Use helper functions when branching starts to sprawl.

### Types
- Model domain behavior with structs/enums (`TransferPlan`, `Protocol`, `RunOutcome`).
- Derive traits intentionally (`Debug`, `Clone`, `Copy`, `Eq`, `PartialEq`, `Hash`).
- Use `Path`/`PathBuf` for file paths.
- Use `Option<T>` for optional state; use `Result<T, E>` for fallible flows.

### Naming
- Types/enums/traits: `PascalCase`.
- Functions/modules/variables: `snake_case`.
- Constants: `UPPER_SNAKE_CASE`.
- Prefer descriptive names that reflect behavior.

### Error handling
- Use `anyhow::Result<T>` for orchestration and command flows.
- Add context on IO/process/parse failures with `.context(...)` and `.with_context(...)`.
- Use `thiserror` for domain error enums (`AppError`) where stable variants matter.
- Prefer explicit early returns and `bail!` for invalid states.
- Avoid `unwrap()` in production code.
- `expect(...)` is acceptable in tests for setup assumptions.

### Control flow
- Favor early exits for guard conditions.
- Keep `match` arms explicit and local.
- Keep side effects in dedicated modules (`runner`, `auth`, `planner`).

### CLI conventions
- Use `clap` derive-based argument definitions.
- Keep CLI help text concise and user-facing.
- Prefer strongly-typed enums (`ValueEnum`) over free-form strings.

### Security and secrets
- Never print credentials.
- Redact password-like CLI args before logging or dry-run output.
- Keep credential persistence behind `CredentialStore` abstraction.

### Testing
- Prefer colocated unit tests under `#[cfg(test)]` blocks.
- Keep tests deterministic and focused on pure logic.
- Avoid network-coupled tests in the default suite.

### Dependencies
- Add crates only when justified.
- Prefer small, focused dependencies.
- Do not add async runtimes/frameworks unless explicitly needed.

## Implementation notes for agents
- `main.rs` wires command routing and top-level flow.
- `probe.rs` decides protocol candidates and optional throughput probing.
- `planner.rs` converts choices into deterministic aria2 command args.
- `runner.rs` executes aria2 and streams combined logs.
- `auth.rs` manages prompt + keychain read/write/clear behavior.

## Change policy
- Keep edits minimal and scoped.
- Preserve existing behavior unless task requires a behavior change.
- When adding CLI options, update `src/cli.rs`, `src/main.rs`, and `README.md`.
- When changing transfer logic, validate with dry-run:
```bash
cargo run -- download "https://example.com/movie.mkv" --dry-run
```

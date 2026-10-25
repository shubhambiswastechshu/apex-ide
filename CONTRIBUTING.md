# Contributing to Apex IDE

Thanks for helping. The bar for this project is a solid foundation: correct
and tested beats feature count.

## Setup

1. Install Rust stable with [rustup](https://rustup.rs) (MSRV is 1.92), plus
   the `rustfmt` and `clippy` components:
   `rustup component add rustfmt clippy`.
2. Install [uv](https://docs.astral.sh/uv/) and make sure it is on `PATH`.
3. Linux only: install the Slint/winit system libraries listed in the
   [README](README.md#prerequisites).
4. Build and run: `cargo run -p apex-ide`.

## Before opening a pull request

Run the same checks as CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must pass on Windows, macOS and Linux.

## Code rules

- **Respect the dependency direction:** `apex-ide` -> feature crates ->
  `apex-core`. Feature crates never depend on `apex-ide` or on Slint. See
  [ARCHITECTURE.md](ARCHITECTURE.md).
- **Errors:** each library crate has its own `thiserror` enum. No
  `unwrap`/`expect` where failure is possible at runtime; return an error.
  `anyhow` is for the binary only.
- **Cross-platform:** use `Path`/`PathBuf`; isolate OS differences behind
  `cfg!(windows)` / `#[cfg(...)]` in one function.
- **Threads:** never block the UI thread. Do slow work on a worker and send
  results back over a channel or `slint::invoke_from_event_loop`.
- **Docs:** every public item has a doc comment saying what it does and when
  it fails.
- **Tests:** test real behaviour. Use `tempfile` for anything that touches
  the file system; do not depend on network access or on tools that may be
  missing (skip gracefully if `uv`/`python` is not installed).
- **Dependencies:** prefer what is already in `[workspace.dependencies]`. A
  new dependency goes only into the crate that needs it, with a short
  justification in the pull request.
- **Scope:** keep changes small and focused; one concern per pull request.

## Commits and pull requests

- Write commit messages in the imperative ("Add venv discovery").
- Describe what changed and how you tested it in the pull request.
- Update `README.md` / `ARCHITECTURE.md` when you change behaviour or
  structure they describe. Do not mark roadmap items as done until they are
  merged and tested.

## Licensing

By contributing you agree that your contribution is licensed under the
project's MIT licence. Note the open Slint licence decision described in
[ARCHITECTURE.md](ARCHITECTURE.md#11-slint-licence).

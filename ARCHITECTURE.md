# Apex IDE architecture

This document describes how the workspace is organised and the rules that
keep it maintainable. It describes the intended design; where something is
not built yet it says so. Each crate's rustdoc (`cargo doc --workspace
--open`) is the source of truth for its current public API.

## 1. Workspace layout

Apex IDE is a single Cargo workspace. Shared dependency versions live in the
root `Cargo.toml` under `[workspace.dependencies]`; crates opt in with
`dep.workspace = true`.

```text
            +--------------------------------------------+
            |            apex-ide (crates/apex-app)       |
            |   Slint UI, app state, threads, wiring      |
            +--------------------------------------------+
               |        |        |       |        |      |
               v        v        v       v        v      v
        apex-python apex-editor apex-lsp apex-themes apex-plugins apex-db
               |                                      |
               v                                      v
            +--------------------------------------------+
            |                  apex-core                  |
            |   settings, project model, file tree        |
            +--------------------------------------------+
```

## 2. Dependency direction

Dependencies point one way: **app -> feature crates -> core**.

- `apex-core` depends on no other Apex crate.
- Feature crates (`apex-python`, `apex-editor`, `apex-lsp`, `apex-themes`,
  `apex-plugins`, `apex-db`) may depend on `apex-core`, and only on each
  other when there is a clear, acyclic reason. Today none of them do.
- **Feature crates never depend on `apex-app`, and never on Slint.** They are
  plain Rust libraries that can be unit-tested headless and reused by other
  front ends (a CLI, tests, a future mobile shell).
- Only `apex-app` knows about the UI. It translates between UI types
  (`SharedString`, `ModelRc`, Slint callbacks) and the libraries' own types.

### Library conventions

- Each library crate has its own `thiserror` error enum and a
  `Result<T>` alias; `anyhow` is only used in the binary.
- No `unwrap`/`expect` on paths that can fail at runtime (I/O, parsing,
  process spawning, user input). Tests may unwrap.
- Paths are `std::path::Path`/`PathBuf`, never strings glued with `/`.
  Platform differences (for example `.venv\Scripts\python.exe` versus
  `.venv/bin/python`) are handled with `cfg!(windows)` or `#[cfg(...)]` in
  one place, not scattered.
- Public items have doc comments; behaviour has unit tests, using `tempfile`
  for anything touching the file system.

## 3. Threads and events

Slint's event loop runs on the **UI thread**, and Slint components may only
be touched there. Anything that can block (file system walks, spawning `uv`
or Python, talking to a language server, database queries) runs on
**worker threads**.

```text
  UI thread (Slint event loop)               worker threads
  ----------------------------               --------------
  callback (e.g. "Run")  ---- command ---->  spawn process / query / LSP
                                                    |
  update UI models  <---- event (channel) ----------+
        ^
        |  slint::invoke_from_event_loop / Weak::upgrade_in_event_loop
```

- Workers report back by sending typed events over `crossbeam-channel`
  channels (or, for one-shot results, by posting a closure with
  `slint::invoke_from_event_loop` / `Weak::upgrade_in_event_loop`).
- Libraries expose blocking or channel-based APIs and do not spawn the UI's
  callbacks themselves; the app decides which thread calls them.
- Long-running processes (language servers, the program being run) stream
  output line by line as events so the UI stays responsive.

The current app already follows this for "Run": the Python process runs on a
spawned thread and the result is posted back to the event loop. Moving the
remaining blocking work (project creation with `uv`) off the UI thread is
part of the foundation work.

## 4. Crate responsibilities

| Crate          | Owns                                                                 | Does not own                    |
| -------------- | -------------------------------------------------------------------- | ------------------------------- |
| `apex-core`    | Settings (load/save, defaults, config dir), project model, file tree | Python specifics, UI            |
| `apex-python`  | Interpreter discovery, venv creation via `uv`, templates, runner     | Editing, UI                     |
| `apex-editor`  | Text buffer (`ropey`), tree-sitter parsing and highlight spans        | Colours (themes map spans)      |
| `apex-lsp`     | Language server process, JSON-RPC framing, LSP requests/notifications | Choosing what to show in the UI |
| `apex-themes`  | Theme data model, built-in themes, loading user themes                | Rendering                       |
| `apex-plugins` | Plugin manifests, permission model, discovery and registry           | Running untrusted code (yet)    |
| `apex-db`      | Driver abstraction, SQLite driver, driver metadata and logos         | Result grid UI                  |
| `apex-ide`     | Slint UI, app state, threads, wiring the crates together             | Business logic                  |

## 5. Python and project model

- A **project** is a root directory plus settings (interpreter, run
  configurations) stored with the project.
- **Interpreters** are discovered from `uv`, the `PATH` and well-known
  locations; a project normally uses its own `.venv`.
- **New project flow:** name + location + Python version -> create folder ->
  `uv venv --python <version> .venv` -> write template files (plain script
  first; Django and data-science templates later) -> open the project.
- The **runner** executes a file or module with the project interpreter in
  the project root and streams stdout/stderr and the exit code back.

## 6. Plugin model

Status: manifest and permissions are the foundation; executing plugin code is
future work.

- **Manifest.** Each plugin is a directory with a TOML manifest that declares
  at least an id, a display name, a `semver` version, the Apex API version it
  targets, and the permissions it requests. Manifests are validated on load;
  invalid ones are reported, never silently ignored.
- **Permissions.** Plugins get nothing by default. Capabilities such as
  reading the project, writing files, spawning processes or using the network
  must be declared in the manifest and granted by the user (the Plugins panel
  shows what each plugin requests; grants are stored in `plugins.toml`).
  Built-in plugins ship with the IDE and get what they declare. File access
  is further confined to the plugin's own folder and the open project.
- **Discovery.** Plugins are discovered from `<data dir>/ApexIDE/plugins`
  (see `apex_core::paths::plugins_dir`; later also per project); duplicates
  and incompatible API versions are rejected with clear errors.
- **Runtime (future).** Plugin code will run in a sandboxed WebAssembly
  runtime using the **component model** (WIT-defined host interfaces), so
  plugins are portable across platforms and can only reach the host through
  interfaces matching their granted permissions. No runtime dependency is
  taken until that work starts.

## 7. Theme model

- A **theme** has an id, a name, a kind (light or dark), a palette of UI
  colours (background, foreground, accent, panel, border, selection, ...) and
  a mapping from syntax token kinds to styles (colour, bold, italic).
- Built-in light and dark themes ship in the binary; user themes are JSON
  files in `<data dir>/ApexIDE/themes` (`apex_core::paths::themes_dir`, the
  single source of truth for the location). Active plugins can contribute
  theme files too; they appear in the picker while the plugin is active.
- `apex-editor` produces highlight spans tagged with token kinds; the app
  maps them to colours through the active theme. The editor never hard-codes
  colours.
- Colours are parsed and validated (for example `#RRGGBB` / `#RRGGBBAA`) when
  a theme loads, so a broken theme fails fast with a useful error.

## 8. Database driver model

- A **driver** describes one database engine: id, display name, logo and
  connection parameters it needs, and implements a common interface: connect,
  list schemas/tables/columns, run a query, return rows as typed values.
- **SQLite** (via `rusqlite` with the bundled library) is the first driver
  because it needs no server and can be tested with `tempfile`.
- PostgreSQL, MySQL/MariaDB, SQL Server, MongoDB, Redis, DuckDB and others
  are planned as additional drivers; each lands with its own tests.
- **Logos** are bundled assets (SVG) keyed by driver id, so the UI can show
  the right logo in the connection list and explorer without network access.
  Logos must respect each project's trademark guidelines.
- Credentials are never logged; storing secrets should use the OS keychain
  rather than plain-text settings.

## 9. LSP model

- `apex-lsp` locates a server binary on the `PATH` (with `which`), preferring
  `basedpyright-langserver` and falling back to `pyright-langserver`, and
  starts it with `--stdio`.
- Messages are JSON-RPC 2.0 framed with `Content-Length` headers; types come
  from `lsp-types` and documents are identified by `file://` URLs built with
  `url`.
- A reader thread parses incoming messages and sends responses,
  notifications (diagnostics, logs) and server requests as events over a
  channel; the app consumes them on the UI thread.
- Lifecycle: `initialize` -> `initialized` -> document sync
  (`didOpen`/`didChange`/`didClose`) -> requests (completion, hover,
  definition) -> `shutdown` -> `exit`.
- If no server is installed, the IDE keeps working without language
  features and tells the user how to install one.

## 10. Debugging and notebooks (future)

- **Debugger:** a Debug Adapter Protocol client in its own crate, talking to
  `debugpy`. It will reuse the same process/channel patterns as `apex-lsp`.
- **Notebooks:** `.ipynb` parsing plus a Jupyter kernel client. Not started.

## 11. Slint licence

Slint is available under three licences:

1. **GNU GPLv3** — free, but a distributed application that links Slint must
   itself be distributed under GPLv3-compatible terms with source available.
2. **Slint Royalty-free licence** — free for desktop, mobile and web
   applications (not embedded devices), with conditions such as attributing
   Slint (for example an "About Slint" notice or badge). Check the current
   terms at <https://slint.dev/terms-and-conditions>.
3. **Commercial licence** — paid, for cases neither of the above fits.

Apex IDE's own code is declared MIT in `Cargo.toml`. That is compatible with
either free Slint option, but the choice changes the obligations attached to
the **built binary**. The project must pick one of these licences, and meet
its conditions, before distributing any build. Until then, builds are for
local development only.

## 12. Continuous integration

`.github/workflows/ci.yml` runs on every push to `main`/`master` and on pull
requests, on `windows-latest`, `macos-latest` and `ubuntu-latest`:
`cargo fmt --all --check` (Linux), `cargo clippy --workspace --all-targets --
-D warnings` and `cargo test --workspace`. The workflow token is read-only
(`permissions: contents: read`).

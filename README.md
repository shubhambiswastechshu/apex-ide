# Apex IDE

**A fast, memory-light Python IDE in the spirit of PyCharm, written in pure Rust with a native [Slint](https://slint.dev) UI.**

No Electron, no webview, no JVM. Apex IDE is built for people who want a real
IDE on an ordinary laptop: it idles at roughly **30–70 MB of RAM** where
JVM-based IDEs commonly use a gigabyte or more.

> **Status: early preview (v0.1).** It is usable for day-to-day Python editing
> and running, but it is young software. Expect rough edges, and please report
> them as issues.

## Highlights

### Editor
- Custom native code editor: live tree-sitter syntax colouring, line numbers,
  code folding, indent guides, current-line highlight, bracket matching,
  minimap scroll bar.
- Fast on big files: in a 20,000-line file a keystroke costs about 1 ms and
  pasting 5,000 lines about 1 ms (release build).
- Autocomplete that appears instantly, using Python keywords, builtins and
  names from the file. When a language server is installed, its suggestions
  are merged in as they arrive. Tab or Enter accepts the highlighted item.
- Snippets in VS Code snippet format: Python (`def`, `class`, `for`, `try`,
  `with`, dataclass, main guard…), Django (model, view, URL pattern) and data
  science (pandas `read_csv`, matplotlib plot), with Tab to jump between
  placeholders.
- Problems from basedpyright or pyright, shown as squiggles and in a Problems
  panel.
- Find, go to line, toggle comment, duplicate line, smart Python indentation,
  auto-closing brackets, undo/redo, clipboard.
- Zoom from 50% to 300% (Ctrl+= / Ctrl+- / Ctrl+0 / Ctrl+wheel); 100% is a
  13 px Cascadia Code.

### Python projects
- **New Project** works like PyCharm's. Pick a template (Pure Python, Django,
  Data Science or Data Analytics) and Apex creates the folder, a
  [uv](https://docs.astral.sh/uv/) virtual environment, the starter files and
  the packages.
- Interpreter discovery covers PATH, the Windows `py` launcher, uv-managed
  Pythons, conda and project `.venv`s.
- **Missing packages:** imports that aren't installed are underlined, and a
  banner offers **Install all** (via uv, with pip as the fallback). It also
  checks `requirements.txt` and `pyproject.toml`.
- Run configurations, Run (Shift+F10) and Stop, live output, and input to the
  running program.

### Workspace
- Visual Studio 2026-inspired design with the VS Blue, VS Dark and VS Light
  themes, plus Apex Dark, Light, Midnight and High Contrast. All are
  compact by default.
- A Project Explorer you can dock left or right and resize, with file-type
  icons and lazy loading. Alt+1 toggles it.
- Editor tabs that split right or down; drag a tab into another split or
  out into its own window.
- Menus for every action, with shortcuts. The keymap can be customised in
  `keymap.toml`, and conflicts are detected.
- Command palette (Ctrl+Shift+P), Search Everywhere (double Shift), Go to File
  (Ctrl+Shift+O), Find in Files (Ctrl+Shift+F), and a Ctrl+Tab file switcher.

### Databases
- A catalogue of 25 databases with their logos: PostgreSQL, MySQL, MariaDB,
  SQL Server, Oracle, SQLite, DuckDB, ClickHouse, Snowflake, BigQuery,
  Redshift, Databricks, MongoDB, Redis, Cassandra, Elasticsearch, Neo4j,
  InfluxDB, CockroachDB, Supabase, Firestore, DynamoDB, Cosmos DB, Hive and
  Trino.
- A working **SQLite console** to connect, browse tables, run queries (with
  cancel) and view results in a grid. The other drivers are planned.

### Plugins
- Plugins are described by an `apex-plugin.toml` manifest. They can add
  commands, themes, languages, snippets and database drivers.
- Plugins only get the permissions the user grants, and file access is
  scoped to the plugin and the open project.
- Built-in Rust plugins work today. The runtime is designed so a sandboxed
  WASM runtime can be added later.

## Quick start

### Prerequisites
- Rust stable (1.92+) via [rustup](https://rustup.rs)
- [uv](https://docs.astral.sh/uv/getting-started/installation/) on your `PATH`
- Optional, for diagnostics and smarter completion:
  `uv tool install basedpyright`
- Linux only: Slint/winit system libraries, for example on Debian/Ubuntu:
  ```sh
  sudo apt-get install -y libxkbcommon-dev libxkbcommon-x11-dev \
    libfontconfig1-dev libwayland-dev libxcb-shape0-dev libxcb-xfixes0-dev
  ```

### Build and run
```sh
git clone https://github.com/shubhambiswastechshu/apex-ide.git
cd apex-ide
cargo run -p apex-ide --release
```

Useful flags:
- `apex-ide --selftest` runs the non-UI flows end to end (it creates a
  project, edits, runs, highlights, queries SQLite) and exits 0 or 1.
- `apex-ide --screenshot out.png <welcome|new-project|workspace> [project]`
  renders a screen off-screen with the software renderer.

## Keyboard shortcuts (defaults)

| Action | Shortcut |
|---|---|
| Command palette | Ctrl+Shift+P |
| Search Everywhere | Shift, Shift |
| Go to File | Ctrl+Shift+O |
| Find / Find in Files | Ctrl+F / Ctrl+Shift+F |
| Go to Line | Ctrl+G |
| Autocomplete | Ctrl+Space |
| Toggle comment / duplicate line | Ctrl+/ / Ctrl+D |
| Save / Save All | Ctrl+S / Ctrl+Shift+S |
| Run / Stop | Shift+F10 / Ctrl+F2 |
| Project Explorer / Output / Problems | Alt+1 / Alt+4 / Alt+6 |
| Switch open files | Ctrl+Tab |
| Next / previous tab | Ctrl+PageDown / Ctrl+PageUp |
| Zoom in / out / reset | Ctrl+= / Ctrl+- / Ctrl+0 |
| Settings | Ctrl+Alt+S |
| Keyboard shortcuts | Ctrl+K Ctrl+S |

## Architecture

```text
crates/
  apex-app      binary "apex-ide": Slint UI, editor view, wiring, event loop
  apex-core     settings, project model, lazy file tree, keymap, fuzzy search
  apex-python   interpreters, venvs (uv), templates, runner, package checks
  apex-editor   rope buffer, undo, tree-sitter highlighting, completion, snippets
  apex-lsp      language server client (JSON-RPC over stdio)
  apex-themes   theme model, built-in themes, VS Code theme import
  apex-plugins  plugin manifests, permissions, registry, commands
  apex-db       database catalogue + logos, SQLite driver
```

The UI thread only renders and handles input. File indexing, search, runs,
package installs, language-server traffic and database queries all run on
worker threads, which send results back to the UI thread over channels. See
[ARCHITECTURE.md](ARCHITECTURE.md) for details.

## Tech stack

| Concern | Choice |
|---|---|
| Language | Rust 2021 |
| UI | Slint 1.18 (native rendering, no webview) |
| Text | `ropey` rope buffer |
| Highlighting | `tree-sitter` + `tree-sitter-python` (incremental) |
| Language intelligence | LSP: basedpyright / pyright / pylsp / ruff |
| Python tooling | `uv` (pip fallback) |
| Databases | `rusqlite` (bundled SQLite) |
| Fonts | Cascadia Code, Inter (bundled, OFL); Segoe UI on Windows |

## Roadmap
- Debugger (DAP + debugpy)
- Jupyter notebooks
- More database drivers (PostgreSQL, MySQL first)
- Hover and signature help, go to definition, rename
- Integrated terminal and Git
- WASM plugin runtime
- macOS/Linux packaging; mobile later

## Quality
About 450 unit and integration tests. CI runs `cargo fmt`, `clippy -D
warnings` and `cargo test` on Windows, macOS and Linux.

## Contributing
Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License
Apex IDE's own source code is released under the [MIT License](LICENSE).

Apex IDE is built with [Slint](https://slint.dev), which has its own licence
options (GPLv3, a royalty-free licence or a commercial licence) that apply to
distributed binaries; see [ARCHITECTURE.md](ARCHITECTURE.md#11-slint-licence).
The bundled fonts (Cascadia Code, Inter) use the SIL Open Font License. The
database logos come from [Simple Icons](https://simpleicons.org) (CC0). Brand
names and logos are trademarks of their respective owners.

//! Missing packages in the editor: squiggles under unresolved imports, the
//! "Package requirements ... are not installed" banner, and installing them
//! off the UI thread with progress in the Run tool window.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use apex_python::packages::{self, Environment, ImportRef, MissingPackage};

use crate::state::Workspace;

/// An unresolved import: line, start and end column (chars) and the
/// package that provides it.
pub type Squiggle = (usize, usize, usize, String);

/// Package state of an open project.
#[derive(Debug, Default)]
pub struct PackageState {
    /// What the project environment has installed (`None`: no venv found).
    pub env: Option<Environment>,
    /// The project interpreter, if any.
    pub python: Option<PathBuf>,
    /// Per file: buffer revision and the scan result.
    files: HashMap<PathBuf, (u64, Vec<ImportRef>, Vec<MissingPackage>)>,
    /// Packages the user chose to ignore.
    pub ignored: BTreeSet<String>,
    /// An install is running.
    pub installing: bool,
    /// Modules the language server reported as unresolved, per file.
    pub lsp_missing: HashMap<PathBuf, BTreeSet<String>>,
    /// Declared requirements (read once per scan).
    requirements: Option<Vec<String>>,
    /// Whether a module is part of the project, per module name.
    local: HashMap<String, bool>,
    /// Whether a render may re-scan imports (set when typing pauses; scans
    /// are O(file) and skipped while typing).
    pub scan_allowed: bool,
}

impl PackageState {
    /// Re-reads the environment of `python` (cheap: lists `site-packages`).
    pub fn rescan(&mut self, python: Option<PathBuf>) {
        self.env = python
            .as_deref()
            .and_then(crate::workspace::venv_root_of)
            .and_then(|venv| Environment::scan(&venv));
        self.python = python;
        self.files.clear();
        self.requirements = None;
        self.local.clear();
    }

    /// Forgets results for a closed file.
    pub fn forget(&mut self, path: &Path) {
        self.files.remove(path);
        self.lsp_missing.remove(path);
    }
}

/// Extracts the module from a basedpyright/pyright "missing import"
/// message: `Import "requests" could not be resolved`.
pub fn module_from_diagnostic(message: &str) -> Option<String> {
    let rest = message.strip_prefix("Import \"")?;
    let name = rest.split('"').next()?;
    let top = name.split('.').next()?;
    (!top.is_empty() && !top.starts_with('.')).then(|| top.to_owned())
}

/// Updates the scan of the active tab and returns its squiggles as
/// `(line, start col, end col)` (chars) plus the packages to install.
pub fn active_missing(ws: &mut Workspace) -> (Vec<Squiggle>, Vec<MissingPackage>) {
    let active = ws.active;
    missing_for(ws, active)
}

/// Like [`active_missing`] for tab `index` of the workspace.
pub fn missing_for(
    ws: &mut Workspace,
    index: Option<usize>,
) -> (Vec<Squiggle>, Vec<MissingPackage>) {
    let root = ws.project.root.clone();
    let Some(tab) = ws.tabs.get(index.unwrap_or(usize::MAX)) else {
        return (Vec::new(), Vec::new());
    };
    let state = &mut ws.packages;
    let Some(env) = state.env.as_ref() else {
        return (Vec::new(), Vec::new());
    };
    if !tab.is_python() {
        return (Vec::new(), Vec::new());
    }
    let rev = tab.buffer.revision();
    let local = &mut state.local;
    let mut is_local = |m: &str| {
        *local
            .entry(m.to_owned())
            .or_insert_with(|| packages::is_local_module(&root, m))
    };
    let cached = state.files.get(&tab.path);
    let fresh = cached.is_some_and(|(r, _, _)| *r == rev);
    let (refs, mut pkgs) = match cached.filter(|_| fresh || !state.scan_allowed) {
        Some((_, refs, pkgs)) => (refs.clone(), pkgs.clone()),
        None => {
            let requirements = state
                .requirements
                .get_or_insert_with(|| packages::declared_requirements(&root));
            let (refs, pkgs) =
                packages::find_missing_with(&tab.buffer.text(), env, requirements, &mut is_local);
            state
                .files
                .insert(tab.path.clone(), (rev, refs.clone(), pkgs.clone()));
            (refs, pkgs)
        }
    };
    if let Some(extra) = state.lsp_missing.get(&tab.path) {
        for m in extra {
            if packages::is_stdlib(m) || env.has_module(m) || is_local(m) {
                continue;
            }
            let pypi = packages::pypi_name(m);
            if !pkgs
                .iter()
                .any(|p| packages::normalize(&p.pypi) == packages::normalize(&pypi))
            {
                pkgs.push(MissingPackage {
                    pypi,
                    module: Some(m.clone()),
                });
            }
        }
    }
    let ignored = &state.ignored;
    pkgs.retain(|p| !ignored.contains(&packages::normalize(&p.pypi)));
    let shown: BTreeSet<String> = pkgs.iter().filter_map(|p| p.module.clone()).collect();
    let squiggles = refs
        .into_iter()
        .filter(|r| shown.contains(&r.module))
        .map(|r| {
            (
                r.line,
                r.col_start,
                r.col_end,
                packages::pypi_name(&r.module),
            )
        })
        .collect();
    (squiggles, pkgs)
}

/// Banner text for missing packages (empty when none).
pub fn banner_text(pkgs: &[MissingPackage]) -> String {
    match pkgs.len() {
        0 => String::new(),
        1 => format!("Package requirement '{}' is not installed", pkgs[0].pypi),
        n if n <= 4 => format!(
            "Package requirements {} are not installed",
            pkgs.iter()
                .map(|p| format!("'{}'", p.pypi))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        n => format!(
            "Package requirements {} and {} more are not installed",
            pkgs[..3]
                .iter()
                .map(|p| format!("'{}'", p.pypi))
                .collect::<Vec<_>>()
                .join(", "),
            n - 3
        ),
    }
}

/// Installs `names` into the environment of `python` on a worker thread,
/// streaming tool output; reports through `sink`.
pub fn install(python: &Path, names: Vec<String>, sink: crate::events::EventSink) {
    let python = python.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("pip-install".into())
        .spawn(move || {
            let out_sink = sink.clone();
            let mut on_output = |line: &str| {
                out_sink.send(crate::events::AppEvent::Editor(
                    super::EditorEvent::InstallOutput(line.to_owned()),
                ));
            };
            let mut options = apex_python::ToolOptions {
                on_output: Some(&mut on_output),
                ..apex_python::ToolOptions::default()
            };
            let result = apex_python::install_packages_with(&python, &names, &mut options)
                .map_err(|e| e.to_string());
            sink.send(crate::events::AppEvent::Editor(
                super::EditorEvent::InstallDone(result),
            ));
        });
    if let Err(e) = spawned {
        tracing::warn!(%e, "install thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_messages_name_modules() {
        assert_eq!(
            module_from_diagnostic("Import \"requests.adapters\" could not be resolved"),
            Some("requests".into())
        );
        assert_eq!(module_from_diagnostic("something else"), None);
    }

    #[test]
    fn banner_lists_packages() {
        let p = |n: &str| MissingPackage {
            pypi: n.into(),
            module: None,
        };
        assert_eq!(banner_text(&[]), "");
        assert_eq!(
            banner_text(&[p("numpy")]),
            "Package requirement 'numpy' is not installed"
        );
        assert_eq!(
            banner_text(&[p("a"), p("b")]),
            "Package requirements 'a', 'b' are not installed"
        );
        assert!(banner_text(&[p("a"), p("b"), p("c"), p("d"), p("e")]).contains("2 more"));
    }
}

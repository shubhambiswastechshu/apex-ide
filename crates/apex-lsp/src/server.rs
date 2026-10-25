//! Describing language servers ([`ServerSpec`]) and finding Python ones on this machine.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use lsp_types::Uri;

/// How to launch a language server that speaks LSP over stdio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// Executable to run (an absolute path or a name resolved through `PATH`).
    pub command: PathBuf,
    /// Command-line arguments, e.g. `["--stdio"]`.
    pub args: Vec<String>,
    /// Workspace root sent in `initialize`; when it is a `file://` URI the
    /// server is also started with that directory as its working directory.
    pub root_uri: Option<Uri>,
    /// Extra environment variables for the server process (for example
    /// `VIRTUAL_ENV`, or a `PATH` with the project's venv first).
    pub env: Vec<(OsString, OsString)>,
}

impl ServerSpec {
    /// A spec that runs `command` with no arguments, root or extra environment.
    pub fn new(command: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            root_uri: None,
            env: Vec::new(),
        }
    }

    /// Replaces the argument list.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the workspace root.
    #[must_use]
    pub fn with_root_uri(mut self, root_uri: Uri) -> Self {
        self.root_uri = Some(root_uri);
        self
    }

    /// Adds an environment variable for the server process.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// A short name for the UI, derived from the executable's file stem
    /// (e.g. `pyright-langserver`).
    pub fn display_name(&self) -> String {
        self.command
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.command.to_string_lossy().into_owned())
    }
}

/// Known Python language servers in order of preference: `(executable, args)`.
pub const PYTHON_SERVERS: &[(&str, &[&str])] = &[
    ("basedpyright-langserver", &["--stdio"]),
    ("pyright-langserver", &["--stdio"]),
    ("pylsp", &[]),
    ("ruff", &["server"]),
];

/// Finds the Python language servers available on `PATH`, best first.
///
/// Preference: `basedpyright-langserver`, `pyright-langserver`, `pylsp`,
/// then `ruff server`. Returns an empty list when none is installed. The
/// returned specs have no root; add one with [`ServerSpec::with_root_uri`].
pub fn find_python_servers() -> Vec<ServerSpec> {
    find_python_servers_in(&[])
}

/// Like [`find_python_servers`], but searches `dirs` (for example a project
/// venv's `Scripts`/`bin` directory) before `PATH`.
pub fn find_python_servers_in(dirs: &[PathBuf]) -> Vec<ServerSpec> {
    find_with(|name| lookup_in_dirs(name, dirs).or_else(|| which::which(name).ok()))
}

/// The scripts directory of a virtual environment: `Scripts` on Windows, `bin` elsewhere.
pub fn venv_bin_dir(venv: &Path) -> PathBuf {
    venv.join(if cfg!(windows) { "Scripts" } else { "bin" })
}

fn lookup_in_dirs(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if dirs.is_empty() {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let paths = std::env::join_paths(dirs).ok()?;
    which::which_in(name, Some(paths), cwd).ok()
}

fn find_with(lookup: impl Fn(&str) -> Option<PathBuf>) -> Vec<ServerSpec> {
    PYTHON_SERVERS
        .iter()
        .filter_map(|(name, args)| {
            lookup(name).map(|path| ServerSpec::new(path).with_args(args.iter().copied()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_order_and_args() {
        let found = find_with(|name| match name {
            "ruff" | "pylsp" | "pyright-langserver" => Some(PathBuf::from(format!("/x/{name}"))),
            _ => None,
        });
        let names: Vec<_> = found.iter().map(ServerSpec::display_name).collect();
        assert_eq!(names, ["pyright-langserver", "pylsp", "ruff"]);
        assert_eq!(found[0].args, ["--stdio"]);
        assert!(found[1].args.is_empty());
        assert_eq!(found[2].args, ["server"]);
        assert!(found.iter().all(|s| s.root_uri.is_none()));
    }

    #[test]
    fn tolerates_no_servers() {
        assert!(find_with(|_| None).is_empty());
    }

    #[test]
    fn real_lookup_does_not_panic() {
        // Whatever is installed on this machine, discovery must just return a list.
        let _ = find_python_servers();
    }

    #[test]
    fn finds_servers_in_a_venv_dir_first() {
        let venv = tempfile::tempdir().unwrap();
        let bin = venv_bin_dir(venv.path());
        std::fs::create_dir_all(&bin).unwrap();
        let file_name = if cfg!(windows) { "pylsp.exe" } else { "pylsp" };
        let exe = bin.join(file_name);
        std::fs::write(&exe, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let found = find_python_servers_in(std::slice::from_ref(&bin));
        let pylsp = found
            .iter()
            .find(|s| s.display_name() == "pylsp")
            .expect("pylsp from the venv dir");
        assert!(pylsp.command.starts_with(&bin), "{:?}", pylsp.command);
    }

    #[test]
    fn spec_builder() {
        let root = "file:///proj".parse::<Uri>().unwrap();
        let spec = ServerSpec::new("pylsp")
            .with_args(["-v"])
            .with_root_uri(root.clone())
            .with_env("VIRTUAL_ENV", "/proj/.venv");
        assert_eq!(spec.args, ["-v"]);
        assert_eq!(spec.root_uri, Some(root));
        assert_eq!(spec.env.len(), 1);
        assert_eq!(spec.display_name(), "pylsp");
    }
}

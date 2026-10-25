//! Python interpreter discovery.
//!
//! [`discover_interpreters`] gathers candidates from several sources (a
//! project's `.venv`, `VIRTUAL_ENV`, `CONDA_PREFIX`, `PATH`, the Windows `py`
//! launcher and `uv python list`), probes each for its version, and returns a
//! de-duplicated, sorted list. Every source is optional: missing tools are
//! skipped silently.
//!
//! The text parsers ([`parse_version_output`], [`parse_py_launcher_list`],
//! [`parse_uv_python_list`]) are pure functions so they can be unit tested
//! against captured tool output.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{PythonError, Result};
use crate::process::{command, output_with_timeout};
use crate::venv::{find_uv, VENV_DIR_NAME};

/// How long a single probe (version query, tool listing) may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The snippet used to ask an interpreter for its version.
const VERSION_SNIPPET: &str = "import sys;print(sys.version_info[:3])";

/// Where an interpreter came from, ordered from most to least project-specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InterpreterKind {
    /// A virtual environment (has a `pyvenv.cfg`).
    Venv,
    /// A conda environment (has a `conda-meta` directory).
    Conda,
    /// A Python installation managed by `uv`.
    Uv,
    /// A Python found through the Windows `py` launcher.
    PyLauncher,
    /// Any other Python found on `PATH` or in a well-known location.
    System,
}

impl InterpreterKind {
    /// A short lowercase label, e.g. `"venv"`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Venv => "venv",
            Self::Conda => "conda",
            Self::Uv => "uv",
            Self::PyLauncher => "py launcher",
            Self::System => "system",
        }
    }

    /// Lower is preferred when the same interpreter is found twice.
    fn rank(self) -> u8 {
        match self {
            Self::Venv => 0,
            Self::Conda => 1,
            Self::Uv => 2,
            Self::PyLauncher => 3,
            Self::System => 4,
        }
    }
}

impl fmt::Display for InterpreterKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A working Python interpreter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interpreter {
    /// Path to the executable (as found, not canonicalized).
    pub path: PathBuf,
    /// `(major, minor, micro)` as reported by the interpreter.
    pub version: (u8, u8, u8),
    /// Where it came from.
    pub kind: InterpreterKind,
    /// A label for UI lists, e.g. `Python 3.12.4 (venv: myproject)`.
    pub display_name: String,
}

impl Interpreter {
    /// The version as `"3.12.4"`.
    pub fn version_string(&self) -> String {
        let (major, minor, micro) = self.version;
        format!("{major}.{minor}.{micro}")
    }

    /// Whether this interpreter satisfies a request such as `"3"`, `"3.12"`
    /// or `"3.12.4"` (component-wise prefix match).
    pub fn matches_version(&self, request: &str) -> bool {
        let wanted: Vec<&str> = request.trim().split('.').collect();
        if wanted.is_empty() || wanted.len() > 3 {
            return false;
        }
        let have = [self.version.0, self.version.1, self.version.2];
        wanted
            .iter()
            .zip(have)
            .all(|(w, h)| w.parse::<u8>().is_ok_and(|w| w == h))
    }
}

/// Discovers every working Python interpreter on this machine.
///
/// When `project_root` is given, its `.venv` / `venv` interpreter is included
/// (and listed first). The result is de-duplicated by canonical path and
/// sorted: virtual environments first, then conda, uv, py-launcher and
/// system interpreters, newest version first within each group.
///
/// This spawns processes and can take a moment; call it off the UI thread.
pub fn discover_interpreters(project_root: Option<&Path>) -> Vec<Interpreter> {
    let candidates = dedupe_candidates(collect_candidates(project_root));
    let mut found: Vec<Interpreter> = thread::scope(|scope| {
        let handles: Vec<_> = candidates
            .into_iter()
            .map(|(path, kind)| scope.spawn(move || probe(&path, kind)))
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok().flatten())
            .collect()
    });
    found.sort_by(compare_interpreters);
    found
}

/// Probes a single, user-chosen interpreter path (e.g. "Add interpreter...").
pub fn inspect_interpreter(path: &Path) -> Result<Interpreter> {
    if !is_executable_file(path) {
        return Err(PythonError::NotAnInterpreter(path.to_path_buf()));
    }
    probe(path, InterpreterKind::System)
        .ok_or_else(|| PythonError::NotAnInterpreter(path.to_path_buf()))
}

/// Picks the best interpreter matching a version request, preferring
/// non-virtual-environment interpreters (useful as a venv base).
pub fn find_matching<'a>(
    interpreters: &'a [Interpreter],
    request: Option<&str>,
) -> Option<&'a Interpreter> {
    let matches = |i: &&Interpreter| request.is_none_or(|r| i.matches_version(r));
    interpreters
        .iter()
        .filter(|i| i.kind != InterpreterKind::Venv)
        .find(matches)
        .or_else(|| interpreters.iter().find(matches))
}

/// Runs `python` to learn its version, falling back to `--version`.
///
/// Returns `None` if it cannot be run or its output is unrecognised.
pub fn query_version(python: &Path) -> Option<(u8, u8, u8)> {
    let mut cmd = command(python);
    cmd.args(["-c", VERSION_SNIPPET]);
    if let Some(v) = output_with_timeout(cmd, PROBE_TIMEOUT)
        .filter(|o| o.status.success())
        .and_then(|o| parse_version_output(&String::from_utf8_lossy(&o.stdout)))
    {
        return Some(v);
    }
    let mut cmd = command(python);
    cmd.arg("--version");
    let out = output_with_timeout(cmd, PROBE_TIMEOUT).filter(|o| o.status.success())?;
    // Python 2 printed its version on stderr.
    parse_version_output(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| parse_version_output(&String::from_utf8_lossy(&out.stderr)))
}

/// Parses `(3, 12, 4)` (from `sys.version_info[:3]`) or `Python 3.12.4`
/// (from `--version`; pre-release suffixes such as `rc1` are ignored).
pub fn parse_version_output(output: &str) -> Option<(u8, u8, u8)> {
    let text = output.trim();
    if let Some(inner) = text.strip_prefix('(') {
        let inner = inner.split(')').next()?;
        let mut parts = inner.split(',').map(|p| p.trim().parse::<u8>());
        let major = parts.next()?.ok()?;
        let minor = parts.next()?.ok()?;
        let micro = parts.next().and_then(|p| p.ok()).unwrap_or(0);
        return Some((major, minor, micro));
    }
    text.split_whitespace()
        .filter(|tok| tok.starts_with(|c: char| c.is_ascii_digit()) && tok.contains('.'))
        .find_map(parse_dotted)
}

/// Parses `3.12.4`, `3.13.0rc1` or `3.12` into a version triple.
fn parse_dotted(token: &str) -> Option<(u8, u8, u8)> {
    let mut parts = token.split('.').map(|p| {
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u8>().ok()
    });
    let major = parts.next()??;
    let minor = parts.next()??;
    let micro = parts.next().flatten().unwrap_or(0);
    Some((major, minor, micro))
}

/// Parses the output of `py -0p` (Windows Python launcher).
///
/// Handles both the modern format (` -V:3.12 *   C:\...\python.exe`) and the
/// legacy one (` -3.12-64   C:\...\python.exe *`).
pub fn parse_py_launcher_list(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with('-') {
                return None;
            }
            let (_tag, rest) = line.split_once(char::is_whitespace)?;
            let path = rest
                .trim()
                .trim_start_matches('*')
                .trim_end_matches('*')
                .trim();
            looks_like_path(path).then(|| PathBuf::from(path))
        })
        .collect()
}

/// Parses the output of `uv python list --only-installed`.
///
/// Each line is `<key> <path>`; lines whose second column is not a path
/// (e.g. `<download available>`) are skipped, and for symlinks
/// (`<link> -> <target>`) the link itself is returned.
pub fn parse_uv_python_list(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| {
            let (_key, rest) = line.trim().split_once(char::is_whitespace)?;
            let rest = rest.trim();
            let path = rest.split(" -> ").next()?.trim();
            looks_like_path(path).then(|| PathBuf::from(path))
        })
        .collect()
}

fn looks_like_path(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('<') && (s.contains('/') || s.contains('\\'))
}

/// Classifies an interpreter by its location on disk, if recognisable.
///
/// `<root>/bin/python` or `<root>\Scripts\python.exe` with a
/// `<root>/pyvenv.cfg` is a venv; a `conda-meta` folder next to the
/// interpreter or its parent marks conda; a path containing `uv/python`
/// is uv-managed.
pub fn classify_path(path: &Path) -> Option<InterpreterKind> {
    if venv_root_of(path).is_some() {
        return Some(InterpreterKind::Venv);
    }
    let parent = path.parent()?;
    let is_conda = parent.join("conda-meta").is_dir()
        || parent
            .parent()
            .is_some_and(|gp| gp.join("conda-meta").is_dir());
    if is_conda {
        return Some(InterpreterKind::Conda);
    }
    let components: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    if components
        .windows(2)
        .any(|w| w[0] == "uv" && w[1] == "python")
    {
        return Some(InterpreterKind::Uv);
    }
    None
}

/// Returns the venv root if `python` lives in a virtual environment.
fn venv_root_of(python: &Path) -> Option<&Path> {
    let root = python.parent()?.parent()?;
    root.join("pyvenv.cfg").is_file().then_some(root)
}

/// Gathers raw candidate paths with the kind implied by their source.
fn collect_candidates(project_root: Option<&Path>) -> Vec<(PathBuf, InterpreterKind)> {
    let mut out = Vec::new();

    if let Some(root) = project_root {
        for dir in [VENV_DIR_NAME, "venv"] {
            out.push((
                crate::venv::python_in_venv(&root.join(dir)),
                InterpreterKind::Venv,
            ));
        }
    }
    if let Some(venv) = env::var_os("VIRTUAL_ENV") {
        out.push((
            crate::venv::python_in_venv(Path::new(&venv)),
            InterpreterKind::Venv,
        ));
    }
    if let Some(prefix) = env::var_os("CONDA_PREFIX") {
        let prefix = PathBuf::from(prefix);
        let python = if cfg!(windows) {
            prefix.join("python.exe")
        } else {
            prefix.join("bin").join("python")
        };
        out.push((python, InterpreterKind::Conda));
    }

    let names: &[&str] = if cfg!(windows) {
        &["python", "python3"]
    } else {
        &["python3", "python"]
    };
    for name in names {
        if let Ok(paths) = which::which_all(name) {
            out.extend(paths.map(|p| (p, InterpreterKind::System)));
        }
    }
    if !cfg!(windows) {
        // GUI apps on macOS do not inherit the shell PATH.
        for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            out.push((Path::new(dir).join("python3"), InterpreterKind::System));
        }
    }

    if cfg!(windows) {
        let mut cmd = command("py");
        cmd.arg("-0p");
        if let Some(o) = output_with_timeout(cmd, PROBE_TIMEOUT).filter(|o| o.status.success()) {
            let text = String::from_utf8_lossy(&o.stdout);
            out.extend(
                parse_py_launcher_list(&text)
                    .into_iter()
                    .map(|p| (p, InterpreterKind::PyLauncher)),
            );
        }
    }

    if let Some(uv) = find_uv() {
        let mut cmd = command(uv);
        cmd.args(["python", "list", "--only-installed"]);
        if let Some(o) = output_with_timeout(cmd, PROBE_TIMEOUT).filter(|o| o.status.success()) {
            let text = String::from_utf8_lossy(&o.stdout);
            out.extend(
                parse_uv_python_list(&text)
                    .into_iter()
                    .map(|p| (p, InterpreterKind::System)),
            );
        }
    }
    out
}

/// Drops missing files, refines kinds from on-disk layout, and keeps one
/// entry per canonical location (the best-ranked kind wins). Order of first
/// appearance is preserved.
fn dedupe_candidates(
    candidates: Vec<(PathBuf, InterpreterKind)>,
) -> Vec<(PathBuf, InterpreterKind)> {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut best: HashMap<PathBuf, (PathBuf, InterpreterKind)> = HashMap::new();
    for (path, hint) in candidates {
        let Some(key) = dedupe_key(&path) else {
            continue;
        };
        let kind = match classify_path(&path) {
            Some(k) if k.rank() < hint.rank() => k,
            _ => hint,
        };
        match best.get_mut(&key) {
            Some(existing) => {
                if kind.rank() < existing.1.rank() {
                    existing.1 = kind;
                }
            }
            None => {
                order.push(key.clone());
                best.insert(key, (path, kind));
            }
        }
    }
    order
        .into_iter()
        .filter_map(|key| best.remove(&key))
        .collect()
}

/// Identity of an interpreter for de-duplication, or `None` if it does not
/// exist.
///
/// A venv's `bin/python` is often a symlink to the base interpreter, so venv
/// interpreters are keyed by their (canonical) venv root instead.
pub(crate) fn dedupe_key(path: &Path) -> Option<PathBuf> {
    if !is_executable_file(path) {
        return None;
    }
    let target = venv_root_of(path).unwrap_or(path);
    let canonical = std::fs::canonicalize(target).ok()?;
    Some(normalize_key(canonical))
}

/// Whether `path` is a file that can be an interpreter. On Windows only
/// `.exe` files qualify: `.bat`/`.cmd` shims (e.g. depot_tools' `python3.bat`)
/// cannot be given arbitrary arguments safely.
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
        && (!cfg!(windows)
            || path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("exe")))
}

/// Strips the Windows verbatim prefix and folds case on case-insensitive
/// platforms so equal paths compare equal.
fn normalize_key(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let s = path.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        PathBuf::from(s.to_lowercase())
    } else {
        path
    }
}

/// Probes `path` and builds an [`Interpreter`] if it runs.
fn probe(path: &Path, kind: InterpreterKind) -> Option<Interpreter> {
    let version = query_version(path)?;
    let kind = match classify_path(path) {
        Some(k) if k.rank() < kind.rank() => k,
        _ => kind,
    };
    let display_name = display_name(path, version, kind);
    Some(Interpreter {
        path: path.to_path_buf(),
        version,
        kind,
        display_name,
    })
}

fn display_name(path: &Path, (major, minor, micro): (u8, u8, u8), kind: InterpreterKind) -> String {
    let base = format!("Python {major}.{minor}.{micro}");
    let file_name = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let label = match kind {
        InterpreterKind::Venv => match venv_root_of(path) {
            // `.venv` is anonymous; name it after the project folder.
            Some(root) if file_name(root).starts_with('.') => root
                .parent()
                .map(|p| format!("venv: {}", file_name(p)))
                .unwrap_or_else(|| "venv".into()),
            Some(root) => format!("venv: {}", file_name(root)),
            None => "venv".into(),
        },
        InterpreterKind::Conda => {
            let prefix = if cfg!(windows) {
                path.parent()
            } else {
                path.parent().and_then(Path::parent)
            };
            match prefix.map(file_name) {
                Some(name) if !name.is_empty() => format!("conda: {name}"),
                _ => "conda".into(),
            }
        }
        other => other.label().into(),
    };
    format!("{base} ({label})")
}

fn compare_interpreters(a: &Interpreter, b: &Interpreter) -> Ordering {
    a.kind
        .rank()
        .cmp(&b.kind.rank())
        .then_with(|| b.version.cmp(&a.version))
        .then_with(|| a.path.cmp(&b.path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn interp(version: (u8, u8, u8), kind: InterpreterKind) -> Interpreter {
        Interpreter {
            path: PathBuf::from(format!("/py/{}.{}", version.0, version.1)),
            version,
            kind,
            display_name: String::new(),
        }
    }

    #[test]
    fn parses_version_tuple() {
        assert_eq!(parse_version_output("(3, 11, 9)\n"), Some((3, 11, 9)));
        assert_eq!(parse_version_output("(3, 12, 0)\r\n"), Some((3, 12, 0)));
        assert_eq!(parse_version_output("(3, 13)"), Some((3, 13, 0)));
    }

    #[test]
    fn parses_dash_dash_version() {
        assert_eq!(parse_version_output("Python 3.11.9\n"), Some((3, 11, 9)));
        assert_eq!(parse_version_output("Python 3.13.0rc1"), Some((3, 13, 0)));
        assert_eq!(parse_version_output("Python 3.12.0+"), Some((3, 12, 0)));
        assert_eq!(parse_version_output("Python 2.7.18"), Some((2, 7, 18)));
    }

    #[test]
    fn rejects_garbage_version_output() {
        assert_eq!(parse_version_output(""), None);
        assert_eq!(
            parse_version_output("Python was not found; run without arguments to install"),
            None
        );
        assert_eq!(parse_version_output("(a, b, c)"), None);
    }

    #[test]
    fn parses_modern_py_launcher_output() {
        let out = " -V:3.13 *        C:\\Users\\me\\AppData\\Local\\Programs\\Python\\Python313\\python.exe\r\n \
                   -V:3.11          C:\\Users\\me\\AppData\\Local\\Programs\\Python\\Python311\\python.exe\r\n";
        assert_eq!(
            parse_py_launcher_list(out),
            vec![
                PathBuf::from(r"C:\Users\me\AppData\Local\Programs\Python\Python313\python.exe"),
                PathBuf::from(r"C:\Users\me\AppData\Local\Programs\Python\Python311\python.exe"),
            ]
        );
    }

    #[test]
    fn parses_legacy_py_launcher_output() {
        let out = "Installed Pythons found by C:\\WINDOWS\\py.exe Launcher for Windows\n \
                   -3.10-64       C:\\Python310\\python.exe *\n \
                   -2.7-32        C:\\Python27\\python.exe\n \
                   *               Active venv\n";
        assert_eq!(
            parse_py_launcher_list(out),
            vec![
                PathBuf::from(r"C:\Python310\python.exe"),
                PathBuf::from(r"C:\Python27\python.exe"),
            ]
        );
        assert!(parse_py_launcher_list("No Installed Pythons Found!").is_empty());
    }

    #[test]
    fn parses_uv_python_list_windows() {
        let out = "cpython-3.13.3-windows-x86_64-none     C:\\Python313\\python.exe\n\
                   cpython-3.12.13-windows-x86_64-none    C:\\Users\\me\\AppData\\Roaming\\uv\\python\\cpython-3.12-windows-x86_64-none\\python.exe\n";
        assert_eq!(
            parse_uv_python_list(out),
            vec![
                PathBuf::from(r"C:\Python313\python.exe"),
                PathBuf::from(
                    r"C:\Users\me\AppData\Roaming\uv\python\cpython-3.12-windows-x86_64-none\python.exe"
                ),
            ]
        );
    }

    #[test]
    fn parses_uv_python_list_unix_with_symlinks_and_downloads() {
        let out = "cpython-3.13.0+freethreaded-linux-x86_64-gnu    <download available>\n\
                   cpython-3.12.4-linux-x86_64-gnu     /home/me/.local/bin/python3.12 -> /home/me/.local/share/uv/python/cpython-3.12.4-linux-x86_64-gnu/bin/python3.12\n\
                   cpython-3.11.2-linux-x86_64-gnu     /usr/bin/python3.11\n\
                   \n";
        assert_eq!(
            parse_uv_python_list(out),
            vec![
                PathBuf::from("/home/me/.local/bin/python3.12"),
                PathBuf::from("/usr/bin/python3.11"),
            ]
        );
    }

    #[test]
    fn version_matching() {
        let i = interp((3, 12, 4), InterpreterKind::System);
        assert!(i.matches_version("3"));
        assert!(i.matches_version("3.12"));
        assert!(i.matches_version(" 3.12.4 "));
        assert!(!i.matches_version("3.1"));
        assert!(!i.matches_version("3.12.5"));
        assert!(!i.matches_version("3.12.4.1"));
        assert!(!i.matches_version("latest"));
        assert_eq!(i.version_string(), "3.12.4");
    }

    #[test]
    fn find_matching_prefers_base_interpreters() {
        let list = vec![
            interp((3, 12, 1), InterpreterKind::Venv),
            interp((3, 12, 4), InterpreterKind::System),
            interp((3, 11, 9), InterpreterKind::System),
        ];
        assert_eq!(
            find_matching(&list, Some("3.12")).map(|i| i.version),
            Some((3, 12, 4))
        );
        assert_eq!(
            find_matching(&list, Some("3.11")).map(|i| i.version),
            Some((3, 11, 9))
        );
        assert_eq!(
            find_matching(&list, None).map(|i| i.version),
            Some((3, 12, 4))
        );
        assert!(find_matching(&list, Some("3.9")).is_none());
        let only_venv = vec![interp((3, 10, 0), InterpreterKind::Venv)];
        assert!(find_matching(&only_venv, Some("3.10")).is_some());
    }

    #[test]
    fn sorting_groups_by_kind_then_newest() {
        let mut list = [
            interp((3, 11, 0), InterpreterKind::System),
            interp((3, 13, 0), InterpreterKind::System),
            interp((3, 10, 0), InterpreterKind::Venv),
        ];
        list.sort_by(compare_interpreters);
        let versions: Vec<_> = list.iter().map(|i| i.version.1).collect();
        assert_eq!(versions, vec![10, 13, 11]);
    }

    /// Creates a fake venv at `root` and returns the path of its python.
    fn fake_venv(root: &Path) -> PathBuf {
        let python = crate::venv::python_in_venv(root);
        fs::create_dir_all(python.parent().expect("has parent")).expect("mkdir");
        fs::write(&python, b"").expect("write python");
        fs::write(root.join("pyvenv.cfg"), b"home = /usr/bin\n").expect("write cfg");
        python
    }

    #[test]
    fn classifies_venv_conda_and_uv_layouts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let venv_python = fake_venv(&tmp.path().join("proj").join(".venv"));
        assert_eq!(classify_path(&venv_python), Some(InterpreterKind::Venv));

        let conda = tmp.path().join("miniconda");
        fs::create_dir_all(conda.join("conda-meta")).expect("mkdir");
        fs::create_dir_all(conda.join("bin")).expect("mkdir");
        assert_eq!(
            classify_path(&conda.join("python.exe")),
            Some(InterpreterKind::Conda)
        );
        assert_eq!(
            classify_path(&conda.join("bin").join("python")),
            Some(InterpreterKind::Conda)
        );

        let uv = tmp
            .path()
            .join("uv")
            .join("python")
            .join("cpython-3.12")
            .join("python.exe");
        assert_eq!(classify_path(&uv), Some(InterpreterKind::Uv));
        assert_eq!(classify_path(&tmp.path().join("python")), None);
    }

    #[test]
    fn venv_display_name_uses_project_folder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let python = fake_venv(&tmp.path().join("myproj").join(".venv"));
        assert_eq!(
            display_name(&python, (3, 12, 4), InterpreterKind::Venv),
            "Python 3.12.4 (venv: myproj)"
        );
        let named = fake_venv(&tmp.path().join("env311"));
        assert_eq!(
            display_name(&named, (3, 11, 0), InterpreterKind::Venv),
            "Python 3.11.0 (venv: env311)"
        );
        assert_eq!(
            display_name(
                Path::new("/usr/bin/python3"),
                (3, 10, 1),
                InterpreterKind::System
            ),
            "Python 3.10.1 (system)"
        );
    }

    #[test]
    fn dedupe_drops_missing_and_duplicates_and_keeps_best_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sys = tmp.path().join("python.exe");
        fs::write(&sys, b"").expect("write");
        let bat = tmp.path().join("python3.bat");
        fs::write(&bat, b"").expect("write");
        let venv_python = fake_venv(&tmp.path().join("p").join(".venv"));

        let mut candidates = vec![
            (sys.clone(), InterpreterKind::System),
            (tmp.path().join("missing.exe"), InterpreterKind::System),
            (sys.clone(), InterpreterKind::PyLauncher),
            (venv_python.clone(), InterpreterKind::System),
            (venv_python.clone(), InterpreterKind::Venv),
        ];
        if cfg!(windows) {
            candidates.insert(1, (bat, InterpreterKind::System));
        }
        let deduped = dedupe_candidates(candidates);
        assert_eq!(
            deduped,
            vec![
                (sys, InterpreterKind::PyLauncher),
                (venv_python, InterpreterKind::Venv),
            ]
        );
    }

    #[test]
    fn inspect_rejects_non_interpreters() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            inspect_interpreter(&tmp.path().join("nope")),
            Err(PythonError::NotAnInterpreter(_))
        ));
    }

    #[test]
    fn discovery_runs_and_returns_working_interpreters() {
        // Must never panic, whatever is installed; every result must be real.
        let found = discover_interpreters(None);
        for i in &found {
            assert!(i.path.is_file(), "{:?}", i.path);
            assert!(i.display_name.starts_with("Python "));
        }
        let mut keys: Vec<_> = found.iter().filter_map(|i| dedupe_key(&i.path)).collect();
        let before = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(before, keys.len(), "duplicates in {found:#?}");
    }
}

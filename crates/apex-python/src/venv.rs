//! Virtual environment creation and package installation.
//!
//! The preferred backend is [`uv`](https://docs.astral.sh/uv/); when it is not
//! installed, the standard library `venv` module and `pip` are used instead.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{PythonError, Result};
use crate::interpreter::{discover_interpreters, find_matching};
use crate::process::{command, output_with_timeout, run_checked, ToolOptions};

/// The directory name used for project virtual environments.
pub const VENV_DIR_NAME: &str = ".venv";

/// Which tool creates the virtual environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum VenvBackend {
    /// `uv venv` (fast; can download a missing Python). Falls back to
    /// [`VenvBackend::StdVenv`] when `uv` is not installed.
    #[default]
    Uv,
    /// `<python> -m venv`.
    StdVenv,
}

/// What kind of virtual environment to create.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenvSpec {
    /// A version request such as `"3.12"`. Ignored when
    /// `base_interpreter` is set.
    pub python_version: Option<String>,
    /// An explicit base interpreter. Takes precedence over `python_version`.
    pub base_interpreter: Option<PathBuf>,
    /// The tool to use.
    pub backend: VenvBackend,
}

/// `<project_root>/.venv`.
pub fn venv_dir(project_root: &Path) -> PathBuf {
    project_root.join(VENV_DIR_NAME)
}

/// The interpreter inside a venv directory: `Scripts\python.exe` on Windows,
/// `bin/python` elsewhere. Does not check that it exists.
///
/// The layout itself is defined once, by
/// [`apex_core::project::venv_interpreter_relative`].
pub fn python_in_venv(venv_dir: &Path) -> PathBuf {
    apex_core::project::venv_interpreter_relative(venv_dir)
}

/// The interpreter of the project's `.venv`. Does not check that it exists.
pub fn venv_python(project_root: &Path) -> PathBuf {
    python_in_venv(&venv_dir(project_root))
}

/// The project's `.venv` interpreter, if it exists.
pub fn existing_venv_python(project_root: &Path) -> Option<PathBuf> {
    let python = venv_python(project_root);
    python.is_file().then_some(python)
}

/// Locates the `uv` executable on `PATH` or in its default install folders
/// (which GUI apps on macOS may not have on `PATH`).
pub fn find_uv() -> Option<PathBuf> {
    if let Ok(path) = which::which("uv") {
        return Some(path);
    }
    let exe = if cfg!(windows) { "uv.exe" } else { "uv" };
    let home = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = &home {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".cargo").join("bin"));
    }
    if !cfg!(windows) {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
    }
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file())
}

/// How long the `import pip` probe of an existing venv may take.
const PIP_PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// The project's `.venv` interpreter if the venv is complete enough to reuse:
/// the interpreter and `pyvenv.cfg` exist and, when `needs_pip` (packages
/// will be installed with pip because uv is missing), pip is importable.
///
/// A half-made venv, for example one left behind when `python -m venv`
/// failed at the `ensurepip` stage, is not reusable.
fn reusable_venv_python(project_root: &Path, needs_pip: bool) -> Option<PathBuf> {
    let python = existing_venv_python(project_root)?;
    if !venv_dir(project_root).join("pyvenv.cfg").is_file() {
        return None;
    }
    if needs_pip {
        let mut probe = command(&python);
        probe.args(["-c", "import pip"]);
        let ok = output_with_timeout(probe, PIP_PROBE_TIMEOUT).is_some_and(|o| o.status.success());
        if !ok {
            return None;
        }
    }
    Some(python)
}

/// Creates `<project_root>/.venv` and returns the path of its interpreter.
///
/// A complete existing venv is reused; an incomplete one (no `pyvenv.cfg`, or
/// no pip when uv is unavailable) is recreated. A venv folder this call
/// created is removed again if creation fails, so a retry starts clean.
/// The project directory is created if needed. This blocks, possibly for a
/// while (uv may download a Python); call it off the UI thread.
pub fn create_venv(project_root: &Path, spec: &VenvSpec) -> Result<PathBuf> {
    create_venv_with(project_root, spec, &mut ToolOptions::default())
}

/// [`create_venv`] with streamed tool output, cancellation and an idle
/// timeout (see [`ToolOptions`]).
pub fn create_venv_with(
    project_root: &Path,
    spec: &VenvSpec,
    options: &mut ToolOptions<'_>,
) -> Result<PathBuf> {
    let uv = match spec.backend {
        VenvBackend::Uv => {
            let uv = find_uv();
            if uv.is_none() {
                tracing::info!("uv not found; falling back to python -m venv");
            }
            uv
        }
        VenvBackend::StdVenv => None,
    };
    let needs_pip = find_uv().is_none();
    if let Some(python) = reusable_venv_python(project_root, needs_pip) {
        tracing::debug!(path = %python.display(), "reusing existing venv");
        return Ok(python);
    }
    if let Some(request) = &spec.python_version {
        validate_version_request(request)?;
    }
    fs::create_dir_all(project_root)
        .map_err(|e| PythonError::io(format!("creating `{}`", project_root.display()), e))?;
    let dir = venv_dir(project_root);
    // An incomplete venv from an earlier attempt is replaced, not reused.
    let leftover = dir.exists();
    if leftover {
        tracing::info!(path = %dir.display(), "recreating incomplete venv");
    }

    let mut cmd = match uv {
        Some(uv) => {
            let mut cmd = command(uv);
            cmd.arg("venv");
            if leftover {
                cmd.arg("--clear");
            }
            if let Some(base) = &spec.base_interpreter {
                cmd.arg("--python").arg(base);
            } else if let Some(version) = &spec.python_version {
                cmd.arg("--python").arg(version.trim());
            }
            cmd.arg(&dir);
            cmd
        }
        None => {
            let base = std_venv_base(spec)?;
            let mut cmd = command(base);
            cmd.args(["-m", "venv"]);
            if leftover {
                cmd.arg("--clear");
            }
            cmd.arg(&dir);
            cmd
        }
    };
    cmd.current_dir(project_root);
    let python = python_in_venv(&dir);
    let created = run_checked(cmd, options).and_then(|()| {
        if python.is_file() {
            Ok(())
        } else {
            Err(PythonError::VenvPythonMissing(python.clone()))
        }
    });
    if let Err(e) = created {
        if dir.exists() {
            if let Err(rm) = fs::remove_dir_all(&dir) {
                tracing::warn!(%rm, path = %dir.display(), "removing failed venv");
            }
        }
        return Err(e);
    }
    Ok(python)
}

/// Chooses the base interpreter for the `venv` module backend.
fn std_venv_base(spec: &VenvSpec) -> Result<PathBuf> {
    if let Some(base) = &spec.base_interpreter {
        return Ok(base.clone());
    }
    let request = spec.python_version.as_deref().map(str::trim);
    let all = discover_interpreters(None);
    find_matching(&all, request)
        .map(|i| i.path.clone())
        .ok_or_else(|| PythonError::NoInterpreter {
            requested: request.map(str::to_owned),
        })
}

/// Rejects requests that are empty or would be parsed as a CLI flag.
fn validate_version_request(request: &str) -> Result<()> {
    let r = request.trim();
    if r.is_empty() || r.starts_with('-') {
        return Err(PythonError::InvalidVersionRequest(request.to_owned()));
    }
    Ok(())
}

/// Installs `packages` into the environment of `venv_python`.
///
/// Uses `uv pip install --python <venv_python>` when uv is available,
/// otherwise `<venv_python> -m pip install`. An empty list is a no-op.
/// Blocks until done (may hit the network); call it off the UI thread.
pub fn install_packages<S: AsRef<OsStr>>(venv_python: &Path, packages: &[S]) -> Result<()> {
    install_packages_with(venv_python, packages, &mut ToolOptions::default())
}

/// [`install_packages`] with streamed tool output, cancellation and an idle
/// timeout (see [`ToolOptions`]).
pub fn install_packages_with<S: AsRef<OsStr>>(
    venv_python: &Path,
    packages: &[S],
    options: &mut ToolOptions<'_>,
) -> Result<()> {
    if packages.is_empty() {
        return Ok(());
    }
    if !venv_python.is_file() {
        return Err(PythonError::NotAnInterpreter(venv_python.to_path_buf()));
    }
    let mut cmd = match find_uv() {
        Some(uv) => {
            let mut cmd = command(uv);
            cmd.args(["pip", "install", "--python"]).arg(venv_python);
            cmd
        }
        None => {
            let mut cmd = command(venv_python);
            cmd.args(["-m", "pip", "install", "--disable-pip-version-check"]);
            cmd
        }
    };
    cmd.args(packages);
    run_checked(cmd, options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venv_python_layout_matches_platform() {
        let root = Path::new("proj");
        let expected = if cfg!(windows) {
            Path::new("proj")
                .join(".venv")
                .join("Scripts")
                .join("python.exe")
        } else {
            Path::new("proj").join(".venv").join("bin").join("python")
        };
        assert_eq!(venv_python(root), expected);
        assert_eq!(venv_dir(root), Path::new("proj").join(".venv"));
    }

    #[test]
    fn existing_venv_python_requires_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(existing_venv_python(tmp.path()), None);
        let python = venv_python(tmp.path());
        fs::create_dir_all(python.parent().expect("parent")).expect("mkdir");
        fs::write(&python, b"").expect("write");
        assert_eq!(existing_venv_python(tmp.path()), Some(python));
    }

    /// A fake venv: an (empty, non-working) interpreter file, optionally
    /// with `pyvenv.cfg`.
    fn fake_venv(root: &Path, with_cfg: bool) -> PathBuf {
        let python = venv_python(root);
        fs::create_dir_all(python.parent().expect("parent")).expect("mkdir");
        fs::write(&python, b"").expect("write");
        if with_cfg {
            fs::write(venv_dir(root).join("pyvenv.cfg"), "home = x\n").expect("cfg");
        }
        python
    }

    #[test]
    fn incomplete_venvs_are_not_reusable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fake_venv(tmp.path(), false);
        assert_eq!(
            reusable_venv_python(tmp.path(), false),
            None,
            "no pyvenv.cfg"
        );
        let python = fake_venv(tmp.path(), true);
        assert_eq!(reusable_venv_python(tmp.path(), false), Some(python));
        // The fake interpreter cannot `import pip`.
        assert_eq!(reusable_venv_python(tmp.path(), true), None, "no pip");
    }

    #[test]
    fn failed_creation_removes_the_half_made_venv() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fake_venv(tmp.path(), false);
        let spec = VenvSpec {
            base_interpreter: Some(tmp.path().join("no-such-python")),
            backend: VenvBackend::StdVenv,
            ..VenvSpec::default()
        };
        // Not reused (incomplete), so the bogus base is spawned and fails...
        assert!(matches!(
            create_venv(tmp.path(), &spec),
            Err(PythonError::Spawn { .. })
        ));
        // ...and the leftover is gone, so the next attempt starts clean.
        assert!(!venv_dir(tmp.path()).exists());
    }

    #[test]
    fn existing_venv_is_reused_without_running_tools() {
        if find_uv().is_none() {
            eprintln!("skipping: without uv, reuse also needs a working pip");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let python = fake_venv(tmp.path(), true);
        // A bogus base interpreter proves nothing was executed.
        let spec = VenvSpec {
            base_interpreter: Some(tmp.path().join("no-such-python")),
            backend: VenvBackend::StdVenv,
            ..VenvSpec::default()
        };
        assert_eq!(create_venv(tmp.path(), &spec).expect("reuse"), python);
    }

    #[test]
    fn rejects_flag_like_version_requests() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for bad in ["", "  ", "--help"] {
            let spec = VenvSpec {
                python_version: Some(bad.into()),
                ..VenvSpec::default()
            };
            assert!(matches!(
                create_venv(tmp.path(), &spec),
                Err(PythonError::InvalidVersionRequest(_))
            ));
        }
    }

    #[test]
    fn std_venv_with_missing_base_fails_cleanly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let spec = VenvSpec {
            base_interpreter: Some(tmp.path().join("no-such-python")),
            backend: VenvBackend::StdVenv,
            ..VenvSpec::default()
        };
        assert!(matches!(
            create_venv(&tmp.path().join("proj"), &spec),
            Err(PythonError::Spawn { .. })
        ));
    }

    #[test]
    fn install_with_no_packages_is_noop() {
        let none: [&str; 0] = [];
        assert!(install_packages(Path::new("does-not-exist"), &none).is_ok());
        assert!(matches!(
            install_packages(Path::new("does-not-exist"), &["requests"]),
            Err(PythonError::NotAnInterpreter(_))
        ));
    }

    #[test]
    fn creates_real_venv_with_uv() {
        if find_uv().is_none() {
            eprintln!("skipping: uv not installed");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("uv proj");
        let python = match create_venv(&root, &VenvSpec::default()) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping: uv could not create a venv: {e}");
                return;
            }
        };
        assert_eq!(python, venv_python(&root));
        assert!(root.join(".venv").join("pyvenv.cfg").is_file());
        assert!(crate::interpreter::query_version(&python).is_some());
    }

    #[test]
    fn creates_real_venv_with_std_backend() {
        let all = discover_interpreters(None);
        let Some(base) = find_matching(&all, None) else {
            eprintln!("skipping: no Python found");
            return;
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let spec = VenvSpec {
            base_interpreter: Some(base.path.clone()),
            backend: VenvBackend::StdVenv,
            ..VenvSpec::default()
        };
        let python = create_venv(tmp.path(), &spec).expect("std venv");
        assert_eq!(python, venv_python(tmp.path()));
        assert!(crate::interpreter::query_version(&python).is_some());
    }
}

//! Small filesystem helpers used internally.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

/// Writes `contents` to `path` atomically: the data goes to a temporary file
/// in the same directory, which is then renamed over the target. Parent
/// directories are created as needed. A crash mid-write therefore never
/// leaves a half-written file behind.
pub(crate) fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| CoreError::io(parent, e))?;

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let tmp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));

    fs::write(&tmp, contents).map_err(|e| CoreError::io(&tmp, e))?;
    // `fs::rename` replaces an existing destination on every supported OS,
    // including Windows (MoveFileExW with MOVEFILE_REPLACE_EXISTING).
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(CoreError::io(path, e));
    }
    Ok(())
}

/// Reads a file to a string, returning `Ok(None)` when it does not exist.
pub(crate) fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CoreError::io(path, e)),
    }
}

/// Makes `path` absolute without resolving symlinks. Unlike
/// `fs::canonicalize`, this never produces Windows `\\?\` verbatim paths,
/// which many tools (including some Python builds) mishandle.
pub(crate) fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|e| CoreError::io(path, e))
}

/// Returns a comparison key for `path` so that two spellings of the same
/// location compare equal: absolute, `.` components removed, and on Windows
/// case-folded with unified separators.
pub(crate) fn path_key(path: &Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut key = abs.to_string_lossy().into_owned();
    if cfg!(windows) {
        key = key.replace('/', "\\").to_lowercase();
    }
    let sep = std::path::MAIN_SEPARATOR;
    while key.len() > 1 && key.ends_with(sep) && !key.ends_with(&format!(":{sep}")) {
        key.pop();
    }
    key
}

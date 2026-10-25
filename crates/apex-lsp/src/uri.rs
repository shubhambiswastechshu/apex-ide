//! Conversion between filesystem paths and LSP `file://` URIs.
//!
//! Windows paths are encoded the way language servers expect
//! (`C:\a b\x.py` becomes `file:///C:/a%20b/x.py`), and URIs written in the
//! VS Code style with an escaped drive colon (`file:///c%3A/x.py`) are decoded
//! correctly.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::Uri;

use crate::error::LspError;

/// Converts a filesystem path into a `file://` URI.
///
/// Relative paths are made absolute against the current directory first
/// (without touching the filesystem).
pub fn path_to_uri(path: &Path) -> Result<Uri, LspError> {
    let absolute =
        std::path::absolute(path).map_err(|_| LspError::InvalidPath(path.to_path_buf()))?;
    let url = url::Url::from_file_path(&absolute)
        .map_err(|()| LspError::InvalidPath(path.to_path_buf()))?;
    Uri::from_str(url.as_str()).map_err(|_| LspError::InvalidPath(path.to_path_buf()))
}

/// Converts a `file://` URI back into a filesystem path.
///
/// On Windows the drive letter is normalised to upper case, so URIs that differ
/// only in drive-letter case or colon escaping map to the same path.
pub fn uri_to_path(uri: &Uri) -> Result<PathBuf, LspError> {
    let text = normalize_drive_colon(uri.as_str());
    let url = url::Url::parse(&text).map_err(|_| LspError::InvalidUri(uri.as_str().to_owned()))?;
    if url.scheme() != "file" {
        return Err(LspError::InvalidUri(uri.as_str().to_owned()));
    }
    let path = url
        .to_file_path()
        .map_err(|()| LspError::InvalidUri(uri.as_str().to_owned()))?;
    Ok(uppercase_drive(path))
}

/// Rewrites `file:///c%3A/...` to `file:///c:/...`.
fn normalize_drive_colon(uri: &str) -> String {
    const PREFIX: &str = "file:///";
    let Some(rest) = uri.get(PREFIX.len()..) else {
        return uri.to_owned();
    };
    if !uri[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
        return uri.to_owned();
    }
    let bytes = rest.as_bytes();
    if bytes.len() >= 4 && bytes[0].is_ascii_alphabetic() && rest[1..4].eq_ignore_ascii_case("%3a")
    {
        format!("{PREFIX}{}:{}", &rest[..1], &rest[4..])
    } else {
        uri.to_owned()
    }
}

fn uppercase_drive(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    let Some(text) = path.to_str() else {
        return path;
    };
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_lowercase() && bytes[1] == b':' {
        let mut fixed = text.to_owned();
        fixed[..1].make_ascii_uppercase();
        PathBuf::from(fixed)
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        Uri::from_str(s).expect("valid uri in test")
    }

    #[test]
    fn round_trips_a_real_temp_file_with_spaces_and_unicode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("my project").join("caf\u{e9} #1.py");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "print(1)\n").unwrap();

        let u = path_to_uri(&file).unwrap();
        assert!(u.as_str().starts_with("file:///"), "{}", u.as_str());
        assert!(u.as_str().contains("my%20project"), "{}", u.as_str());
        assert!(u.as_str().contains("%231.py"), "{}", u.as_str());

        let back = uri_to_path(&u).unwrap();
        assert_eq!(
            std::fs::canonicalize(&back).unwrap(),
            std::fs::canonicalize(&file).unwrap()
        );
    }

    #[test]
    fn relative_paths_become_absolute() {
        let u = path_to_uri(Path::new("some_dir/x.py")).unwrap();
        let back = uri_to_path(&u).unwrap();
        assert!(back.is_absolute());
        assert!(back.ends_with(Path::new("some_dir").join("x.py")));
    }

    #[test]
    fn rejects_non_file_uris() {
        assert!(matches!(
            uri_to_path(&uri("https://example.com/x.py")),
            Err(LspError::InvalidUri(_))
        ));
        assert!(matches!(
            uri_to_path(&uri("untitled:Untitled-1")),
            Err(LspError::InvalidUri(_))
        ));
    }

    #[test]
    fn drive_colon_normalisation_is_textual() {
        assert_eq!(
            normalize_drive_colon("file:///c%3A/Users/x.py"),
            "file:///c:/Users/x.py"
        );
        assert_eq!(normalize_drive_colon("file:///d%3a/x"), "file:///d:/x");
        assert_eq!(normalize_drive_colon("file:///home/x"), "file:///home/x");
        assert_eq!(normalize_drive_colon("file:///"), "file:///");
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_letters() {
        let u = path_to_uri(Path::new(r"C:\Users\a b\main.py")).unwrap();
        assert_eq!(u.as_str(), "file:///C:/Users/a%20b/main.py");

        let expected = PathBuf::from(r"C:\Users\a b\main.py");
        assert_eq!(uri_to_path(&u).unwrap(), expected);
        // VS Code style: lower-case drive with an escaped colon.
        assert_eq!(
            uri_to_path(&uri("file:///c%3A/Users/a%20b/main.py")).unwrap(),
            expected
        );
        assert_eq!(
            uri_to_path(&uri("file:///c:/Users/a%20b/main.py")).unwrap(),
            expected
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_unc_paths() {
        let u = path_to_uri(Path::new(r"\\server\share\pkg\mod.py")).unwrap();
        assert_eq!(u.as_str(), "file://server/share/pkg/mod.py");
        assert_eq!(
            uri_to_path(&u).unwrap(),
            PathBuf::from(r"\\server\share\pkg\mod.py")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_paths() {
        let u = path_to_uri(Path::new("/home/me/a b/main.py")).unwrap();
        assert_eq!(u.as_str(), "file:///home/me/a%20b/main.py");
        assert_eq!(
            uri_to_path(&u).unwrap(),
            PathBuf::from("/home/me/a b/main.py")
        );
    }
}

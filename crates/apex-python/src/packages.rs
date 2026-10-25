//! Missing-package detection: which third-party modules a Python file
//! imports (or a project requires) that are not installed in its
//! environment, and the PyPI names to install for them.
//!
//! - [`parse_imports`] reads `import x` / `from x.y import z` statements
//!   (relative imports are skipped).
//! - [`is_stdlib`] knows the standard library of Python 3.10 – 3.13.
//! - [`pypi_name`] maps import names to distribution names
//!   (`sklearn` → `scikit-learn`, `cv2` → `opencv-python`, ...).
//! - [`Environment::scan`] finds what a virtual environment has installed
//!   by reading its `site-packages` (fast, no process), and
//!   [`list_installed`] asks `uv pip list` / `pip list` instead.
//! - [`declared_requirements`] reads `requirements.txt` and
//!   `pyproject.toml` dependencies.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::error::{PythonError, Result};
use crate::process::{command, output_with_timeout};

/// One imported top-level module and where its name is in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRef {
    /// Top-level module name (`numpy` for `import numpy.linalg`).
    pub module: String,
    /// 0-based line.
    pub line: usize,
    /// Char column where the dotted name starts.
    pub col_start: usize,
    /// Char column where the dotted name ends.
    pub col_end: usize,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Reads a dotted name at the start of `s` (after spaces); returns the
/// name and its char offset range within `s`.
fn dotted_name(s: &str, base: usize) -> Option<(String, usize, usize)> {
    let lead = s.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    let name: String = s
        .chars()
        .skip(lead)
        .take_while(|c| is_ident_char(*c) || *c == '.')
        .collect();
    if name.is_empty() || name.starts_with('.') {
        return None;
    }
    let start = base + lead;
    Some((name.clone(), start, start + name.chars().count()))
}

/// Finds the top-level modules imported by `source`. Imports inside
/// triple-quoted strings and comments are ignored; relative imports
/// (`from . import x`) are skipped.
pub fn parse_imports(source: &str) -> Vec<ImportRef> {
    let mut out = Vec::new();
    let mut in_triple: Option<&str> = None;
    for (line_no, raw) in source.lines().enumerate() {
        // Track triple-quoted strings roughly (enough for docstrings).
        let mut rest = raw;
        let mut skip_line = in_triple.is_some();
        loop {
            match in_triple {
                Some(q) => match rest.find(q) {
                    Some(i) => {
                        rest = &rest[i + 3..];
                        in_triple = None;
                    }
                    None => break,
                },
                None => {
                    let a = rest.find("\"\"\"");
                    let b = rest.find("'''");
                    let first = match (a, b) {
                        (Some(x), Some(y)) => Some(if x < y { (x, "\"\"\"") } else { (y, "'''") }),
                        (Some(x), None) => Some((x, "\"\"\"")),
                        (None, Some(y)) => Some((y, "'''")),
                        (None, None) => None,
                    };
                    match first {
                        Some((i, q)) => {
                            if raw.trim_start().starts_with(q) || i == 0 {
                                skip_line = true;
                            }
                            rest = &rest[i + 3..];
                            in_triple = Some(q);
                        }
                        None => break,
                    }
                }
            }
        }
        if skip_line {
            continue;
        }
        let code = raw.split('#').next().unwrap_or("");
        for stmt in code.split(';') {
            let stmt_offset = raw
                [..raw.len() - raw[stmt.as_ptr() as usize - raw.as_ptr() as usize..].len()]
                .chars()
                .count();
            let trimmed = stmt.trim_start();
            let indent = stmt.chars().count() - trimmed.chars().count();
            let base = stmt_offset + indent;
            if let Some(after) = trimmed.strip_prefix("import ") {
                let mut col = base + "import ".len();
                for part in after.split(',') {
                    if let Some((name, s, e)) = dotted_name(part, col) {
                        let module = name.split('.').next().unwrap_or("").to_owned();
                        out.push(ImportRef {
                            module,
                            line: line_no,
                            col_start: s,
                            col_end: e,
                        });
                    }
                    col += part.chars().count() + 1;
                }
            } else if let Some(after) = trimmed.strip_prefix("from ") {
                if after.trim_start().starts_with('.') {
                    continue;
                }
                if !after.contains(" import") {
                    continue;
                }
                if let Some((name, s, e)) = dotted_name(after, base + "from ".len()) {
                    let module = name.split('.').next().unwrap_or("").to_owned();
                    out.push(ImportRef {
                        module,
                        line: line_no,
                        col_start: s,
                        col_end: e,
                    });
                }
            }
        }
    }
    out
}

/// Top-level standard library modules of CPython 3.10 – 3.13 (union of
/// `sys.stdlib_module_names`, public names only, plus `__future__`).
pub const STDLIB_MODULES: &[&str] = &[
    "__future__",
    "_thread",
    "abc",
    "aifc",
    "antigravity",
    "argparse",
    "array",
    "ast",
    "asynchat",
    "asyncio",
    "asyncore",
    "atexit",
    "audioop",
    "base64",
    "bdb",
    "binascii",
    "bisect",
    "builtins",
    "bz2",
    "cProfile",
    "calendar",
    "cgi",
    "cgitb",
    "chunk",
    "cmath",
    "cmd",
    "code",
    "codecs",
    "codeop",
    "collections",
    "colorsys",
    "compileall",
    "concurrent",
    "configparser",
    "contextlib",
    "contextvars",
    "copy",
    "copyreg",
    "crypt",
    "csv",
    "ctypes",
    "curses",
    "dataclasses",
    "datetime",
    "dbm",
    "decimal",
    "difflib",
    "dis",
    "distutils",
    "doctest",
    "email",
    "encodings",
    "ensurepip",
    "enum",
    "errno",
    "faulthandler",
    "fcntl",
    "filecmp",
    "fileinput",
    "fnmatch",
    "fractions",
    "ftplib",
    "functools",
    "gc",
    "genericpath",
    "getopt",
    "getpass",
    "gettext",
    "glob",
    "graphlib",
    "grp",
    "gzip",
    "hashlib",
    "heapq",
    "hmac",
    "html",
    "http",
    "idlelib",
    "imaplib",
    "imghdr",
    "imp",
    "importlib",
    "inspect",
    "io",
    "ipaddress",
    "itertools",
    "json",
    "keyword",
    "lib2to3",
    "linecache",
    "locale",
    "logging",
    "lzma",
    "mailbox",
    "mailcap",
    "marshal",
    "math",
    "mimetypes",
    "mmap",
    "modulefinder",
    "msilib",
    "msvcrt",
    "multiprocessing",
    "netrc",
    "nis",
    "nntplib",
    "nt",
    "ntpath",
    "nturl2path",
    "numbers",
    "opcode",
    "operator",
    "optparse",
    "os",
    "ossaudiodev",
    "pathlib",
    "pdb",
    "pickle",
    "pickletools",
    "pipes",
    "pkgutil",
    "platform",
    "plistlib",
    "poplib",
    "posix",
    "posixpath",
    "pprint",
    "profile",
    "pstats",
    "pty",
    "pwd",
    "py_compile",
    "pyclbr",
    "pydoc",
    "pydoc_data",
    "pyexpat",
    "queue",
    "quopri",
    "random",
    "re",
    "readline",
    "reprlib",
    "resource",
    "rlcompleter",
    "runpy",
    "sched",
    "secrets",
    "select",
    "selectors",
    "shelve",
    "shlex",
    "shutil",
    "signal",
    "site",
    "smtpd",
    "smtplib",
    "sndhdr",
    "socket",
    "socketserver",
    "spwd",
    "sqlite3",
    "sre_compile",
    "sre_constants",
    "sre_parse",
    "ssl",
    "stat",
    "statistics",
    "string",
    "stringprep",
    "struct",
    "subprocess",
    "sunau",
    "symtable",
    "sys",
    "sysconfig",
    "syslog",
    "tabnanny",
    "tarfile",
    "telnetlib",
    "tempfile",
    "termios",
    "textwrap",
    "this",
    "threading",
    "time",
    "timeit",
    "tkinter",
    "token",
    "tokenize",
    "tomllib",
    "trace",
    "traceback",
    "tracemalloc",
    "tty",
    "turtle",
    "turtledemo",
    "types",
    "typing",
    "unicodedata",
    "unittest",
    "urllib",
    "uu",
    "uuid",
    "venv",
    "warnings",
    "wave",
    "weakref",
    "webbrowser",
    "winreg",
    "winsound",
    "wsgiref",
    "xdrlib",
    "xml",
    "xmlrpc",
    "zipapp",
    "zipfile",
    "zipimport",
    "zlib",
    "zoneinfo",
];

/// Whether `module` (a top-level name) belongs to the standard library.
pub fn is_stdlib(module: &str) -> bool {
    module.starts_with('_') || STDLIB_MODULES.contains(&module)
}

/// Import names whose PyPI distribution has a different name.
pub const IMPORT_TO_PYPI: &[(&str, &str)] = &[
    ("sklearn", "scikit-learn"),
    ("skimage", "scikit-image"),
    ("cv2", "opencv-python"),
    ("PIL", "Pillow"),
    ("yaml", "PyYAML"),
    ("bs4", "beautifulsoup4"),
    ("dotenv", "python-dotenv"),
    ("dateutil", "python-dateutil"),
    ("jwt", "PyJWT"),
    ("Crypto", "pycryptodome"),
    ("OpenSSL", "pyOpenSSL"),
    ("serial", "pyserial"),
    ("usb", "pyusb"),
    ("magic", "python-magic"),
    ("docx", "python-docx"),
    ("pptx", "python-pptx"),
    ("fitz", "PyMuPDF"),
    ("git", "GitPython"),
    ("google", "google-api-python-client"),
    ("googleapiclient", "google-api-python-client"),
    ("attr", "attrs"),
    ("win32api", "pywin32"),
    ("win32com", "pywin32"),
    ("pythoncom", "pywin32"),
    ("wx", "wxPython"),
    ("gi", "PyGObject"),
    ("MySQLdb", "mysqlclient"),
    ("psycopg2", "psycopg2-binary"),
    ("telegram", "python-telegram-bot"),
    ("discord", "discord.py"),
    ("socks", "PySocks"),
    ("Levenshtein", "python-Levenshtein"),
    ("slugify", "python-slugify"),
    ("multipart", "python-multipart"),
    ("jose", "python-jose"),
    ("ldap", "python-ldap"),
    ("lxml", "lxml"),
    ("sentencepiece", "sentencepiece"),
    ("tensorflow", "tensorflow"),
    ("torch", "torch"),
    ("faiss", "faiss-cpu"),
    ("umap", "umap-learn"),
    ("Bio", "biopython"),
    ("pkg_resources", "setuptools"),
    ("setuptools", "setuptools"),
    ("rest_framework", "djangorestframework"),
    ("corsheaders", "django-cors-headers"),
    ("environ", "django-environ"),
    ("debug_toolbar", "django-debug-toolbar"),
    ("crispy_forms", "django-crispy-forms"),
    ("langchain_openai", "langchain-openai"),
    ("langchain_core", "langchain-core"),
    ("langchain_community", "langchain-community"),
    ("langchain_openrouter", "langchain-openrouter"),
    ("deepagents", "deepagents"),
    ("speech_recognition", "SpeechRecognition"),
    ("pyaudio", "PyAudio"),
    ("zmq", "pyzmq"),
    ("Image", "Pillow"),
];

/// The PyPI distribution to install for import name `module`.
pub fn pypi_name(module: &str) -> String {
    IMPORT_TO_PYPI
        .iter()
        .find(|(m, _)| *m == module)
        .map(|(_, p)| (*p).to_owned())
        .unwrap_or_else(|| module.replace('_', "-"))
}

/// PEP 503 normalised distribution name (`Foo_Bar.baz` → `foo-bar-baz`).
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            dash = true;
        } else {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// What an environment has installed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    /// Importable top-level module names found in `site-packages`.
    pub modules: BTreeSet<String>,
    /// Normalised distribution names.
    pub distributions: BTreeSet<String>,
}

/// `site-packages` folders of a virtual environment.
pub fn site_packages_dirs(venv: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let win = venv.join("Lib").join("site-packages");
    if win.is_dir() {
        out.push(win);
    }
    if let Ok(entries) = std::fs::read_dir(venv.join("lib")) {
        for e in entries.flatten() {
            let p = e.path().join("site-packages");
            if p.is_dir() {
                out.push(p);
            }
        }
    }
    out
}

impl Environment {
    /// Reads the `site-packages` of `venv` (the environment folder).
    /// Returns `None` if it has none.
    pub fn scan(venv: &Path) -> Option<Environment> {
        let dirs = site_packages_dirs(venv);
        if dirs.is_empty() {
            return None;
        }
        let mut env = Environment::default();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let path = e.path();
                if let Some(dist) = name
                    .strip_suffix(".dist-info")
                    .or_else(|| name.strip_suffix(".egg-info"))
                {
                    let dist_name = dist.split('-').next().unwrap_or(dist);
                    env.distributions.insert(normalize(dist_name));
                    // top_level.txt lists the modules a distribution provides.
                    if let Ok(top) = std::fs::read_to_string(path.join("top_level.txt")) {
                        env.modules.extend(
                            top.lines()
                                .map(str::trim)
                                .filter(|l| !l.is_empty())
                                .map(str::to_owned),
                        );
                    }
                    continue;
                }
                if path.is_dir() {
                    env.modules.insert(name);
                } else if let Some(stem) = name.strip_suffix(".py").or_else(|| {
                    name.split_once('.')
                        .filter(|_| name.ends_with(".pyd") || name.ends_with(".so"))
                        .map(|(s, _)| s)
                }) {
                    env.modules.insert(stem.to_owned());
                } else if name.ends_with(".pth") {
                    // Editable installs: ignore, we cannot resolve them cheaply.
                }
            }
        }
        Some(env)
    }

    /// Whether `module` can be imported.
    pub fn has_module(&self, module: &str) -> bool {
        self.modules.contains(module)
            || self.distributions.contains(&normalize(module))
            || self.distributions.contains(&normalize(&pypi_name(module)))
    }

    /// Whether distribution `name` is installed.
    pub fn has_distribution(&self, name: &str) -> bool {
        self.distributions.contains(&normalize(name))
            || self.modules.contains(name)
            || self.modules.contains(&name.replace('-', "_"))
    }
}

/// Parses `pip list --format json` / `uv pip list --format json` output into
/// normalised names.
pub fn parse_pip_list_json(json: &str) -> Result<BTreeSet<String>> {
    let value: serde_json::Value =
        serde_json::from_str(json.trim()).map_err(|e| PythonError::Parse(e.to_string()))?;
    let list = value
        .as_array()
        .ok_or_else(|| PythonError::Parse("expected a JSON array".into()))?;
    Ok(list
        .iter()
        .filter_map(|p| p.get("name")?.as_str().map(normalize))
        .collect())
}

/// Installed distributions of the environment of `python`, via
/// `uv pip list --format json --python <python>` (or
/// `<python> -m pip list --format json`). Blocks; call off the UI thread.
///
/// # Errors
///
/// [`PythonError::Parse`] if neither tool produced a usable list.
pub fn list_installed(python: &Path) -> Result<BTreeSet<String>> {
    let timeout = std::time::Duration::from_secs(60);
    if let Some(uv) = crate::venv::find_uv() {
        let mut cmd = command(uv);
        cmd.args(["pip", "list", "--format", "json", "--python"])
            .arg(python);
        if let Some(out) = output_with_timeout(cmd, timeout) {
            if out.status.success() {
                if let Ok(list) = parse_pip_list_json(&String::from_utf8_lossy(&out.stdout)) {
                    return Ok(list);
                }
            }
        }
    }
    let mut cmd = command(python);
    cmd.args([
        "-m",
        "pip",
        "list",
        "--format",
        "json",
        "--disable-pip-version-check",
    ]);
    match output_with_timeout(cmd, timeout) {
        Some(out) if out.status.success() => {
            parse_pip_list_json(&String::from_utf8_lossy(&out.stdout))
        }
        Some(out) => Err(PythonError::Parse(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )),
        None => Err(PythonError::Parse("pip list timed out".into())),
    }
}

/// Strips version specifiers, extras and markers from a requirement line
/// (`requests[socks]>=2; python_version>"3"` → `requests`). `None` for
/// comments, options (`-r`, `--index-url`) and URLs.
pub fn requirement_name(line: &str) -> Option<String> {
    let line = line.split('#').next()?.trim();
    if line.is_empty() || line.starts_with('-') || line.contains("://") {
        return None;
    }
    let name: String = line
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Requirements declared by the project: `requirements.txt` lines and
/// `pyproject.toml` `[project] dependencies`.
pub fn declared_requirements(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("requirements.txt")) {
        out.extend(text.lines().filter_map(requirement_name));
    }
    if let Ok(text) = std::fs::read_to_string(root.join("pyproject.toml")) {
        if let Ok(value) = text.parse::<toml::Table>() {
            if let Some(deps) = value
                .get("project")
                .and_then(|p| p.get("dependencies"))
                .and_then(|d| d.as_array())
            {
                out.extend(
                    deps.iter()
                        .filter_map(|d| d.as_str())
                        .filter_map(requirement_name),
                );
            }
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|n| seen.insert(normalize(n)));
    out
}

/// Whether `module` is a module of the project itself (a `module.py` or
/// `module/` package at the root or in `src/`).
pub fn is_local_module(root: &Path, module: &str) -> bool {
    [root.to_path_buf(), root.join("src")]
        .iter()
        .any(|dir| dir.join(format!("{module}.py")).is_file() || dir.join(module).is_dir())
}

/// A package the user should install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingPackage {
    /// Name to pass to pip/uv.
    pub pypi: String,
    /// Import name, when it comes from an import.
    pub module: Option<String>,
}

/// Imports of `source` that are neither stdlib, local nor installed in
/// `env`, plus declared requirements of `root` that are not installed.
/// Returns the import references (for underlines) and the packages to
/// install (deduplicated, in file order).
pub fn find_missing(
    source: &str,
    root: &Path,
    env: &Environment,
) -> (Vec<ImportRef>, Vec<MissingPackage>) {
    find_missing_with(source, env, &declared_requirements(root), |m| {
        is_local_module(root, m)
    })
}

/// [`find_missing`] with the project's requirements already read and a
/// (possibly cached) check for project-local modules. Each distinct module
/// is checked once.
pub fn find_missing_with(
    source: &str,
    env: &Environment,
    requirements: &[String],
    mut is_local: impl FnMut(&str) -> bool,
) -> (Vec<ImportRef>, Vec<MissingPackage>) {
    let mut refs = Vec::new();
    let mut packages: Vec<MissingPackage> = Vec::new();
    let mut verdict: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for r in parse_imports(source) {
        let missing = match verdict.get(&r.module) {
            Some(m) => *m,
            None => {
                let m = !(is_stdlib(&r.module) || env.has_module(&r.module) || is_local(&r.module));
                verdict.insert(r.module.clone(), m);
                m
            }
        };
        if !missing {
            continue;
        }
        let pypi = pypi_name(&r.module);
        if !packages
            .iter()
            .any(|p| normalize(&p.pypi) == normalize(&pypi))
        {
            packages.push(MissingPackage {
                pypi,
                module: Some(r.module.clone()),
            });
        }
        refs.push(r);
    }
    for req in requirements {
        if !env.has_distribution(req)
            && !packages
                .iter()
                .any(|p| normalize(&p.pypi) == normalize(req))
        {
            packages.push(MissingPackage {
                pypi: req.clone(),
                module: None,
            });
        }
    }
    (refs, packages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_are_parsed_with_columns() {
        let src = "import os, numpy.linalg as la\nfrom sklearn.model_selection import train_test_split\nfrom . import local\nfrom .x import y\n    import cv2  # cam\nx = 1; import yaml\n# import fake\n\"\"\"\nimport indoc\n\"\"\"\nfrom PIL import Image\n";
        let refs = parse_imports(src);
        let mods: Vec<&str> = refs.iter().map(|r| r.module.as_str()).collect();
        assert_eq!(mods, vec!["os", "numpy", "sklearn", "cv2", "yaml", "PIL"]);
        let np = &refs[1];
        assert_eq!((np.line, np.col_start, np.col_end), (0, 11, 23));
        let sk = &refs[2];
        assert_eq!((sk.col_start, sk.col_end), (5, 28));
        let cv = &refs[3];
        assert_eq!((cv.line, cv.col_start), (4, 11));
        let yaml = &refs[4];
        assert_eq!((yaml.line, yaml.col_start), (5, 14));
    }

    #[test]
    fn stdlib_and_pypi_names() {
        assert!(is_stdlib("os"));
        assert!(is_stdlib("tomllib"));
        assert!(is_stdlib("_ast"));
        assert!(!is_stdlib("numpy"));
        assert_eq!(pypi_name("sklearn"), "scikit-learn");
        assert_eq!(pypi_name("cv2"), "opencv-python");
        assert_eq!(pypi_name("PIL"), "Pillow");
        assert_eq!(pypi_name("yaml"), "PyYAML");
        assert_eq!(pypi_name("bs4"), "beautifulsoup4");
        assert_eq!(pypi_name("dotenv"), "python-dotenv");
        assert_eq!(pypi_name("some_pkg"), "some-pkg");
        assert_eq!(normalize("Foo_Bar.baz"), "foo-bar-baz");
    }

    #[test]
    fn requirement_lines() {
        assert_eq!(
            requirement_name("requests[socks]>=2.0 ; python_version > '3'"),
            Some("requests".into())
        );
        assert_eq!(requirement_name("# comment"), None);
        assert_eq!(requirement_name("-r other.txt"), None);
        assert_eq!(requirement_name("git+https://x/y.git"), None);
        assert_eq!(requirement_name("Django==5.0"), Some("Django".into()));
    }

    #[test]
    fn pip_list_json() {
        let set = parse_pip_list_json(
            r#"[{"name":"Scikit_Learn","version":"1"},{"name":"numpy","version":"2"}]"#,
        )
        .unwrap();
        assert!(set.contains("scikit-learn"));
        assert!(parse_pip_list_json("nope").is_err());
    }

    #[test]
    fn environment_scan_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let venv = root.join(".venv");
        let sp = venv.join("Lib").join("site-packages");
        std::fs::create_dir_all(sp.join("numpy")).unwrap();
        std::fs::create_dir_all(sp.join("scikit_learn-1.5.dist-info")).unwrap();
        std::fs::write(
            sp.join("scikit_learn-1.5.dist-info").join("top_level.txt"),
            "sklearn\n",
        )
        .unwrap();
        std::fs::write(sp.join("six.py"), "").unwrap();
        std::fs::write(root.join("helpers.py"), "").unwrap();
        std::fs::write(root.join("requirements.txt"), "numpy\nrequests>=2\n").unwrap();
        let env = Environment::scan(&venv).unwrap();
        assert!(env.has_module("numpy"));
        assert!(env.has_module("sklearn"));
        assert!(env.has_module("six"));
        assert!(env.has_distribution("Scikit-Learn"));
        let src = "import os\nimport numpy\nimport sklearn\nimport helpers\nimport cv2\nfrom yaml import safe_load\nimport cv2.data\n";
        let (refs, pkgs) = find_missing(src, root, &env);
        assert_eq!(
            refs.iter().map(|r| r.module.as_str()).collect::<Vec<_>>(),
            vec!["cv2", "yaml", "cv2"]
        );
        let names: Vec<&str> = pkgs.iter().map(|p| p.pypi.as_str()).collect();
        assert_eq!(names, vec!["opencv-python", "PyYAML", "requests"]);
        assert!(Environment::scan(&root.join("missing")).is_none());
    }

    #[test]
    fn pyproject_dependencies_are_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\ndependencies = [\"pandas>=2\", \"matplotlib\"]\n",
        )
        .unwrap();
        assert_eq!(
            declared_requirements(dir.path()),
            vec!["pandas", "matplotlib"]
        );
    }
}

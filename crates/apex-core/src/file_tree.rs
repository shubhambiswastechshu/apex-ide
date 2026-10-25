//! The project file tree.
//!
//! [`FileTree::build`] walks a folder with the `ignore` crate, so
//! `.gitignore` (including those in parent folders), `.ignore` and
//! `.git/info/exclude` rules are honoured, even before `git init`; the
//! user's global gitignore is not. Folders named in [`ALWAYS_SKIPPED`] are never
//! listed, and neither is any virtual environment (a folder holding
//! `pyvenv.cfg`, whatever its name) or a Cargo `target` folder (one next to a
//! `Cargo.toml`); a `target` folder anywhere else is ordinary project content.
//! Hidden files such as `.env` or `.gitignore` *are* listed, as in
//! other IDEs. Children are sorted directories first, then by name
//! case-insensitively.
//!
//! For list-based tree widgets, [`FileTree::visible_rows`] flattens the tree
//! into the rows currently visible given a set of expanded folders.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};
use crate::fs_util;

/// Names that are never included in the tree, whatever the ignore files say.
pub const ALWAYS_SKIPPED: [&str; 4] = [".git", ".venv", "__pycache__", "node_modules"];

/// Folders hidden because of what they contain rather than their name:
/// virtual environments (`pyvenv.cfg`) and Cargo build output.
fn is_generated_dir(path: &Path, name: &str) -> bool {
    path.join("pyvenv.cfg").is_file()
        || (name == "target"
            && path
                .parent()
                .is_some_and(|p| p.join("Cargo.toml").is_file()))
}

/// Options for [`FileTree::build_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTreeOptions {
    /// Honour `.gitignore`, `.ignore` and git exclude files. Default `true`.
    pub respect_gitignore: bool,
    /// Include hidden (dot) files and folders. Default `true`.
    pub show_hidden: bool,
    /// Maximum depth below the root to list; `None` lists everything.
    /// Depth 1 means only the root's direct children.
    pub max_depth: Option<usize>,
    /// Extra entry names to skip, in addition to [`ALWAYS_SKIPPED`].
    pub extra_skipped: Vec<String>,
}

impl Default for FileTreeOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            show_hidden: true,
            max_depth: None,
            extra_skipped: Vec::new(),
        }
    }
}

/// One file or folder in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNode {
    /// Full path of the entry.
    pub path: PathBuf,
    /// File name (last path component), lossily converted to UTF-8.
    pub name: String,
    /// Whether the entry is a directory. Symlinks are not followed and are
    /// reported as files.
    pub is_dir: bool,
    /// Children, sorted directories first then case-insensitively by name.
    /// Always empty for files.
    pub children: Vec<FileNode>,
    /// Whether `children` has been read. Folders below the depth limit of a
    /// lazy build (see [`FileTree::build_lazy`]) are not read until
    /// [`FileTree::load_dir`] is called; files are always "loaded".
    pub loaded: bool,
}

impl FileNode {
    /// Finds the node at `path` in this subtree.
    pub fn find(&self, path: &Path) -> Option<&FileNode> {
        if self.path == path {
            return Some(self);
        }
        if !path.starts_with(&self.path) {
            return None;
        }
        self.children.iter().find_map(|c| c.find(path))
    }

    /// Number of files (not folders) in this subtree.
    pub fn file_count(&self) -> usize {
        if self.is_dir {
            self.children.iter().map(FileNode::file_count).sum()
        } else {
            1
        }
    }
}

/// One visible row of a flattened tree, ready for a list UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// Full path of the entry.
    pub path: PathBuf,
    /// Display name.
    pub name: String,
    /// Indentation level; the root's direct children are depth 0.
    pub depth: usize,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// Whether the directory is expanded (always `false` for files).
    pub expanded: bool,
    /// Whether the directory has any children (always `false` for files);
    /// lets the UI hide the expand chevron on empty folders.
    pub has_children: bool,
}

/// A snapshot of a folder's contents. Rebuild it to pick up changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTree {
    /// The root folder node.
    pub root: FileNode,
    /// Options used to read folders, kept for lazy loading.
    options: FileTreeOptions,
}

impl FileTree {
    /// Builds the tree for `root` with [`FileTreeOptions::default`].
    pub fn build(root: impl AsRef<Path>) -> Result<Self> {
        Self::build_with(root, &FileTreeOptions::default())
    }

    /// Builds the tree for `root`. Fails only if `root` is not a readable
    /// directory; entries that cannot be read are skipped and logged.
    pub fn build_with(root: impl AsRef<Path>, options: &FileTreeOptions) -> Result<Self> {
        let root = fs_util::absolute(root.as_ref())?;
        if !root.is_dir() {
            return Err(CoreError::NotADirectory(root));
        }

        let skipped: HashSet<String> = ALWAYS_SKIPPED
            .iter()
            .map(|s| (*s).to_owned())
            .chain(options.extra_skipped.iter().cloned())
            .collect();

        let mut walker = ignore::WalkBuilder::new(&root);
        walker
            .standard_filters(false)
            .hidden(!options.show_hidden)
            .parents(options.respect_gitignore)
            .ignore(options.respect_gitignore)
            .git_ignore(options.respect_gitignore)
            // The user's global gitignore is deliberately not read, so every
            // machine shows the same tree for the same project.
            .git_global(false)
            .git_exclude(options.respect_gitignore)
            .require_git(false)
            .follow_links(false)
            .max_depth(options.max_depth)
            .filter_entry(move |entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name().to_string_lossy();
                if skipped.contains(name.as_ref()) {
                    return false;
                }
                let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                !(is_dir && is_generated_dir(entry.path(), &name))
            });

        // Group entries by parent directory, then assemble recursively.
        let mut children_of: HashMap<PathBuf, Vec<(PathBuf, bool)>> = HashMap::new();
        for result in walker.build() {
            let entry = match result {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::warn!("skipping unreadable entry: {err}");
                    continue;
                }
            };
            if entry.depth() == 0 {
                continue;
            }
            // Links are not followed, so a symlinked folder reports as a file.
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let path = entry.into_path();
            let Some(parent) = path.parent().map(Path::to_path_buf) else {
                continue;
            };
            children_of.entry(parent).or_default().push((path, is_dir));
        }

        let root_node = assemble(root, true, 0, options.max_depth, &mut children_of);
        Ok(Self {
            root: root_node,
            options: options.clone(),
        })
    }

    /// Builds only the root's direct children; deeper folders are read on
    /// demand with [`FileTree::load_dir`] / [`FileTree::ensure_loaded`].
    /// This keeps memory flat for large projects where most folders stay
    /// collapsed.
    pub fn build_lazy(root: impl AsRef<Path>, options: &FileTreeOptions) -> Result<Self> {
        let mut lazy = options.clone();
        lazy.max_depth = Some(1);
        let mut tree = Self::build_with(root, &lazy)?;
        tree.options = FileTreeOptions {
            max_depth: Some(1),
            ..options.clone()
        };
        Ok(tree)
    }

    /// Reads the children of the folder at `path` if they are not loaded
    /// yet. Returns `true` if the tree changed.
    pub fn load_dir(&mut self, path: &Path) -> Result<bool> {
        let Some(node) = find_mut(&mut self.root, path) else {
            return Ok(false);
        };
        if node.loaded || !node.is_dir {
            return Ok(false);
        }
        let opts = FileTreeOptions {
            max_depth: Some(1),
            ..self.options.clone()
        };
        let sub = Self::build_with(path, &opts)?;
        node.children = sub.root.children;
        node.loaded = true;
        Ok(true)
    }

    /// Loads every folder in `dirs` (and the folders on the way to them)
    /// that is not loaded yet; shallow folders first. Returns `true` if the
    /// tree changed.
    pub fn ensure_loaded<'a>(&mut self, dirs: impl IntoIterator<Item = &'a PathBuf>) -> bool {
        let mut dirs: Vec<&PathBuf> = dirs.into_iter().collect();
        dirs.sort_by_key(|p| p.components().count());
        let mut changed = false;
        for d in dirs {
            match self.load_dir(d) {
                Ok(c) => changed |= c,
                Err(e) => tracing::warn!("cannot read {}: {e}", d.display()),
            }
        }
        changed
    }

    /// Reads every folder that is not loaded yet (needed before searching
    /// the whole project). Returns `true` if the tree changed.
    pub fn load_all(&mut self) -> Result<bool> {
        if is_fully_loaded(&self.root) {
            return Ok(false);
        }
        let opts = FileTreeOptions {
            max_depth: None,
            ..self.options.clone()
        };
        let full = Self::build_with(&self.root.path, &opts)?;
        self.root = full.root;
        Ok(true)
    }

    /// Folders in `expanded` (that exist in the tree) whose children have
    /// not been read yet, shallow first. Reading them can then happen on a
    /// worker thread; see [`FileTree::graft`].
    pub fn unloaded_dirs(&self, expanded: &HashSet<PathBuf>) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = expanded
            .iter()
            .filter(|p| self.find(p).is_some_and(|n| n.is_dir && !n.loaded))
            .cloned()
            .collect();
        dirs.sort_by_key(|p| p.components().count());
        dirs
    }

    /// Reads one folder level the same way a lazy build does (blocking;
    /// call it on a worker). The result is passed to [`FileTree::graft`].
    pub fn read_dir_level(&self, dir: &Path) -> Result<FileTree> {
        let opts = FileTreeOptions {
            max_depth: Some(1),
            ..self.options.clone()
        };
        Self::build_with(dir, &opts)
    }

    /// Inserts a subtree read by [`FileTree::read_dir_level`] (or any build
    /// rooted at a folder of this tree). Returns `false` if the folder is no
    /// longer in the tree or was already loaded.
    pub fn graft(&mut self, sub: FileTree) -> bool {
        let Some(node) = find_mut(&mut self.root, &sub.root.path) else {
            return false;
        };
        if node.loaded || !node.is_dir {
            return false;
        }
        node.children = sub.root.children;
        node.loaded = true;
        true
    }

    /// The options this tree was built with.
    pub fn options(&self) -> &FileTreeOptions {
        &self.options
    }

    /// Whether every folder's children have been read.
    pub fn is_fully_loaded(&self) -> bool {
        is_fully_loaded(&self.root)
    }

    /// Path of the root folder.
    pub fn root_path(&self) -> &Path {
        &self.root.path
    }

    /// Finds the node at `path`.
    pub fn find(&self, path: &Path) -> Option<&FileNode> {
        self.root.find(path)
    }

    /// Flattens the tree into the rows a list UI should display: every
    /// top-level entry, plus the children of each directory in `expanded`
    /// whose ancestors are all expanded too. The root itself is not a row.
    pub fn visible_rows(&self, expanded: &HashSet<PathBuf>) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        push_rows(&self.root.children, 0, expanded, &mut rows);
        rows
    }
}

fn find_mut<'a>(node: &'a mut FileNode, path: &Path) -> Option<&'a mut FileNode> {
    if node.path == path {
        return Some(node);
    }
    if !path.starts_with(&node.path) {
        return None;
    }
    node.children.iter_mut().find_map(|c| find_mut(c, path))
}

fn is_fully_loaded(node: &FileNode) -> bool {
    node.loaded && node.children.iter().all(is_fully_loaded)
}

fn assemble(
    path: PathBuf,
    is_dir: bool,
    depth: usize,
    max_depth: Option<usize>,
    children_of: &mut HashMap<PathBuf, Vec<(PathBuf, bool)>>,
) -> FileNode {
    // The walker lists entries down to `max_depth`; folders at that depth
    // were not read.
    let loaded = !is_dir || max_depth.is_none_or(|m| depth < m);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let mut children: Vec<FileNode> = if is_dir {
        children_of
            .remove(&path)
            .unwrap_or_default()
            .into_iter()
            .map(|(child, child_is_dir)| {
                assemble(child, child_is_dir, depth + 1, max_depth, children_of)
            })
            .collect()
    } else {
        Vec::new()
    };
    children.sort_by(compare_nodes);
    FileNode {
        path,
        name,
        is_dir,
        children,
        loaded,
    }
}

/// Directories first, then case-insensitive name, then exact name so the
/// order is total and deterministic.
fn compare_nodes(a: &FileNode, b: &FileNode) -> Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

fn push_rows(
    nodes: &[FileNode],
    depth: usize,
    expanded: &HashSet<PathBuf>,
    rows: &mut Vec<TreeRow>,
) {
    for node in nodes {
        let is_expanded = node.is_dir && expanded.contains(&node.path);
        rows.push(TreeRow {
            path: node.path.clone(),
            name: node.name.clone(),
            depth,
            is_dir: node.is_dir,
            expanded: is_expanded,
            // An unread folder may have children; it shows a chevron until
            // it is expanded and read.
            has_children: !node.children.is_empty() || !node.loaded,
        });
        if is_expanded {
            push_rows(&node.children, depth + 1, expanded, rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "").unwrap();
    }

    fn names(nodes: &[FileNode]) -> Vec<&str> {
        nodes.iter().map(|n| n.name.as_str()).collect()
    }

    fn sample() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        touch(r, "main.py");
        touch(r, "README.md");
        touch(r, "b.txt");
        touch(r, "Apple.txt");
        touch(r, "src/zeta.py");
        touch(r, "src/Alpha.py");
        touch(r, "src/pkg/__init__.py");
        touch(r, "src/__pycache__/main.cpython-311.pyc");
        touch(r, "Docs/index.md");
        touch(r, ".env");
        touch(r, ".git/HEAD");
        touch(r, ".venv/pyvenv.cfg");
        touch(r, "node_modules/x/index.js");
        touch(r, "build/out.bin");
        touch(r, "logs/app.log");
        fs::write(r.join(".gitignore"), "build/\n*.log\n").unwrap();
        fs::create_dir(r.join("empty")).unwrap();
        dir
    }

    #[test]
    fn sorts_dirs_first_case_insensitively() {
        let dir = sample();
        let tree = FileTree::build(dir.path()).unwrap();
        assert_eq!(
            names(&tree.root.children),
            vec![
                "Docs",
                "empty",
                "logs",
                "src",
                ".env",
                ".gitignore",
                "Apple.txt",
                "b.txt",
                "main.py",
                "README.md"
            ]
        );
        let src = tree.find(&tree.root_path().join("src")).unwrap();
        assert_eq!(names(&src.children), vec!["pkg", "Alpha.py", "zeta.py"]);
    }

    #[test]
    fn skips_always_skipped_and_gitignored_entries() {
        let dir = sample();
        let tree = FileTree::build(dir.path()).unwrap();
        let root = tree.root_path().to_path_buf();
        for gone in [
            ".git",
            ".venv",
            "node_modules",
            "target",
            "build",
            "src/__pycache__",
            "logs/app.log",
        ] {
            assert!(
                tree.find(&root.join(gone)).is_none(),
                "{gone} should be hidden"
            );
        }
        // The ignored file's folder itself is still shown (now empty).
        let logs = tree.find(&root.join("logs")).unwrap();
        assert!(logs.is_dir && logs.children.is_empty());
        assert_eq!(tree.root.file_count(), 10);
    }

    #[test]
    fn venvs_and_cargo_targets_are_hidden_but_other_targets_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        touch(r, "venv/pyvenv.cfg");
        touch(r, "venv/Lib/site-packages/big/__init__.py");
        touch(r, "env-3.12/pyvenv.cfg");
        touch(r, "src/pipeline/target/encoder.py");
        touch(r, "target/report.txt");
        touch(r, "rust/Cargo.toml");
        touch(r, "rust/target/debug/app");
        let tree = FileTree::build(r).unwrap();
        let root = tree.root_path().to_path_buf();
        for gone in ["venv", "env-3.12", "rust/target"] {
            assert!(
                tree.find(&root.join(gone)).is_none(),
                "{gone} should be hidden"
            );
        }
        for shown in [
            "src/pipeline/target/encoder.py",
            "target/report.txt",
            "rust/Cargo.toml",
        ] {
            assert!(
                tree.find(&root.join(shown)).is_some(),
                "{shown} should be shown"
            );
        }
    }

    #[test]
    fn options_can_disable_gitignore_hidden_and_limit_depth() {
        let dir = sample();
        let opts = FileTreeOptions {
            respect_gitignore: false,
            show_hidden: false,
            max_depth: Some(1),
            extra_skipped: vec!["Docs".into()],
        };
        let tree = FileTree::build_with(dir.path(), &opts).unwrap();
        let top = names(&tree.root.children);
        assert!(top.contains(&"build"));
        assert!(!top.contains(&".env"));
        assert!(!top.contains(&"Docs"));
        // Always-skipped names stay skipped even without gitignore.
        assert!(!top.contains(&".git") && !top.contains(&"target"));
        // Depth 1: folders are listed but not descended into.
        let src = tree.find(&tree.root_path().join("src")).unwrap();
        assert!(src.children.is_empty());
    }

    #[test]
    fn visible_rows_follow_expanded_set() {
        let dir = sample();
        let tree = FileTree::build(dir.path()).unwrap();
        let root = tree.root_path().to_path_buf();

        let collapsed = tree.visible_rows(&HashSet::new());
        assert_eq!(collapsed.len(), tree.root.children.len());
        assert!(collapsed.iter().all(|r| r.depth == 0 && !r.expanded));

        let src = root.join("src");
        let pkg = src.join("pkg");
        // `pkg` expanded but `src` not: pkg's children stay hidden.
        let only_pkg: HashSet<_> = [pkg.clone()].into_iter().collect();
        assert_eq!(tree.visible_rows(&only_pkg).len(), collapsed.len());

        let both: HashSet<_> = [src.clone(), pkg.clone()].into_iter().collect();
        let rows = tree.visible_rows(&both);
        let summary: Vec<(&str, usize)> = rows
            .iter()
            .skip_while(|r| r.path != src)
            .take(5)
            .map(|r| (r.name.as_str(), r.depth))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("src", 0),
                ("pkg", 1),
                ("__init__.py", 2),
                ("Alpha.py", 1),
                ("zeta.py", 1)
            ]
        );
        let src_row = rows.iter().find(|r| r.path == src).unwrap();
        assert!(src_row.expanded && src_row.has_children);
        let empty_row = rows.iter().find(|r| r.name == "empty").unwrap();
        assert!(empty_row.is_dir && !empty_row.has_children && !empty_row.expanded);
        let file_row = rows.iter().find(|r| r.name == "main.py").unwrap();
        assert!(!file_row.is_dir && !file_row.expanded);
    }

    #[test]
    fn build_fails_for_missing_or_file_root() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            FileTree::build(dir.path().join("missing")),
            Err(CoreError::NotADirectory(_))
        ));
        touch(dir.path(), "f.txt");
        assert!(matches!(
            FileTree::build(dir.path().join("f.txt")),
            Err(CoreError::NotADirectory(_))
        ));
    }

    #[test]
    fn root_node_is_named_after_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("myproj");
        fs::create_dir(&root).unwrap();
        let tree = FileTree::build(&root).unwrap();
        assert_eq!(tree.root.name, "myproj");
        assert!(tree.root.is_dir);
        assert!(tree.root.children.is_empty());
        assert!(tree.visible_rows(&HashSet::new()).is_empty());
    }

    #[test]
    fn lazy_trees_load_folders_on_demand() {
        let dir = sample();
        let mut tree = FileTree::build_lazy(dir.path(), &FileTreeOptions::default()).unwrap();
        let src = tree.find(&dir.path().join("src")).unwrap();
        assert!(!src.loaded && src.children.is_empty());
        assert!(!tree.is_fully_loaded());
        let rows = tree.visible_rows(&HashSet::new());
        assert!(rows.iter().find(|r| r.name == "src").unwrap().has_children);

        let expanded: HashSet<PathBuf> =
            [dir.path().join("src"), dir.path().join("src/pkg")].into();
        assert!(tree.ensure_loaded(&expanded));
        assert!(!tree.ensure_loaded(&expanded), "second load is a no-op");
        let pkg = tree.find(&dir.path().join("src/pkg")).unwrap();
        assert!(pkg.loaded);
        assert_eq!(names(&pkg.children), ["__init__.py"]);
        // Skipped folders stay skipped in lazily loaded subtrees.
        let src = tree.find(&dir.path().join("src")).unwrap();
        assert!(!names(&src.children).contains(&"__pycache__"));

        // Worker-style loading: read a level, then graft it.
        let mut lazy = FileTree::build_lazy(dir.path(), &FileTreeOptions::default()).unwrap();
        let src: HashSet<PathBuf> = [dir.path().join("src")].into();
        let todo = lazy.unloaded_dirs(&src);
        assert_eq!(todo, [dir.path().join("src")]);
        let sub = lazy.read_dir_level(&todo[0]).unwrap();
        assert!(lazy.graft(sub.clone()));
        assert!(!lazy.graft(sub), "grafting twice is a no-op");
        assert!(lazy.unloaded_dirs(&src).is_empty());

        assert!(tree.load_all().unwrap());
        assert!(tree.is_fully_loaded());
        let full = FileTree::build(dir.path()).unwrap();
        assert_eq!(tree.root, full.root);
    }
}

//! Project explorer helpers: file-type icons, name filtering, collapse-all,
//! new file / folder, and the explorer layout (visible, width, dock side)
//! that is kept in [`apex_core::Settings`].

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use apex_core::{FileNode, FileTree, FileTreeOptions, TreeRow};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::app::App;
use crate::events::NoticeKind;
use crate::{AppWindow, TreeItem, WorkspaceApi};

thread_local! {
    /// The one tree model; rows are replaced in place on every update so
    /// the list view keeps its scroll position and nothing is reallocated
    /// beyond the rows themselves.
    static TREE_MODEL: Rc<VecModel<TreeItem>> = Rc::new(VecModel::default());
    /// Folders being read on a worker, so each is requested once.
    static LOADING: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
    static APP: RefCell<Weak<App>> = const { RefCell::new(Weak::new()) };
}

/// Shows `items` in the explorer, reusing the existing model.
pub fn set_tree_rows(api: &WorkspaceApi<'_>, items: Vec<TreeItem>) {
    TREE_MODEL.with(|m| {
        let same_model = api
            .get_tree()
            .as_any()
            .downcast_ref::<VecModel<TreeItem>>()
            .is_some()
            && api.get_tree().row_count() == m.row_count();
        if same_model && m.row_count() == items.len() {
            // Same shape: update changed rows only.
            for (i, item) in items.into_iter().enumerate() {
                if m.row_data(i).as_ref() != Some(&item) {
                    m.set_row_data(i, item);
                }
            }
        } else {
            m.set_vec(items);
        }
        api.set_tree(ModelRc::from(m.clone()));
    });
}

/// The `FileIcon` kind for a tree entry, picked from its name.
pub fn file_icon_kind(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "folder";
    }
    let lower = name.to_ascii_lowercase();
    if lower == ".env" || lower.starts_with(".env.") {
        return "env";
    }
    if lower.starts_with(".git") {
        return "git";
    }
    let ext = Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "py" | "pyi" | "pyw" => "python",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yaml" | "yml" | "ini" | "cfg" | "conf" => "config",
        "md" | "markdown" | "rst" => "md",
        "ipynb" => "ipynb",
        "txt" | "log" | "csv" => "txt",
        "sql" | "db" | "sqlite" | "sqlite3" => "sql",
        "html" | "htm" | "jinja" | "j2" => "html",
        "css" | "scss" => "css",
        "js" | "mjs" | "ts" => "js",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp" => "image",
        "lock" => "lock",
        _ => "file",
    }
}

/// Rows for a filtered explorer: every entry whose name contains `query`
/// (case-insensitive), with its ancestor folders shown expanded.
pub fn filter_rows(tree: &FileTree, query: &str) -> Vec<TreeRow> {
    let needle = query.trim().to_lowercase();
    let mut rows = Vec::new();
    for child in &tree.root.children {
        push_matches(child, 0, &needle, &mut rows);
    }
    rows
}

/// Appends `node` (and its matching descendants) when it or any descendant
/// matches; returns whether anything was appended.
fn push_matches(node: &FileNode, depth: usize, needle: &str, rows: &mut Vec<TreeRow>) -> bool {
    let self_match = node.name.to_lowercase().contains(needle);
    let at = rows.len();
    let mut any_child = false;
    if node.is_dir {
        // Reserve the folder's slot, then add matching children after it.
        rows.push(row_of(node, depth, true));
        for child in &node.children {
            any_child |= push_matches(child, depth + 1, needle, rows);
        }
        if !any_child && !self_match {
            rows.truncate(at);
            return false;
        }
        rows[at].expanded = any_child;
        true
    } else if self_match {
        rows.push(row_of(node, depth, false));
        true
    } else {
        false
    }
}

fn row_of(node: &FileNode, depth: usize, expanded: bool) -> TreeRow {
    TreeRow {
        path: node.path.clone(),
        name: node.name.clone(),
        depth,
        is_dir: node.is_dir,
        expanded: node.is_dir && expanded,
        has_children: node.is_dir && !node.children.is_empty(),
    }
}

/// Why a new file or folder name is unusable, or `None` if it is fine.
pub fn invalid_entry_name(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Some("Enter a name");
    }
    if name == "." || name == ".." {
        return Some("That name is reserved");
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control())
    {
        return Some("The name contains characters that are not allowed");
    }
    if name.split(['/', '\\']).any(|part| part == "..") {
        return Some("The name must stay inside the project");
    }
    None
}

impl App {
    pub(crate) fn wire_tree_view(self: &Rc<Self>, ui: &AppWindow) {
        APP.with(|a| *a.borrow_mut() = Rc::downgrade(self));
        let api = ui.global::<WorkspaceApi>();
        {
            let st = self.state.borrow();
            api.set_tree_visible(st.settings.project_panel_visible);
            api.set_tree_width(st.settings.project_panel_width);
            api.set_tree_right(st.settings.project_panel_right);
        }

        let app = self.clone();
        api.on_tree_layout_changed(move || app.save_tree_layout());
        let app = self.clone();
        api.on_collapse_all(move || {
            if let Some(ws) = app.state.borrow_mut().workspace.as_mut() {
                ws.expanded.clear();
            }
            app.push_tree();
        });
        let app = self.clone();
        api.on_tree_filter_changed(move |text| {
            if let Some(ws) = app.state.borrow_mut().workspace.as_mut() {
                ws.tree_filter = text.to_string();
            }
            app.push_tree();
        });
        let app = self.clone();
        api.on_create_entry(move |name, is_dir| app.create_entry(&name, is_dir));
    }

    /// Starts worker reads for expanded folders that are not loaded yet,
    /// and for the whole tree while the explorer filter is active.
    pub(crate) fn schedule_tree_loads(&self) {
        let (generation, root, opts, dirs, full) = {
            let st = self.state.borrow();
            let Some(ws) = st.workspace.as_ref() else {
                return;
            };
            let Some(tree) = ws.tree.as_ref() else { return };
            let full = !ws.tree_filter.trim().is_empty() && !tree.is_fully_loaded();
            let dirs = if full {
                vec![tree.root.path.clone()]
            } else {
                tree.unloaded_dirs(&ws.expanded)
            };
            (
                st.generation,
                tree.root.path.clone(),
                tree.options().clone(),
                dirs,
                full,
            )
        };
        let dirs: Vec<PathBuf> = LOADING.with(|l| {
            let mut l = l.borrow_mut();
            dirs.into_iter().filter(|d| l.insert(d.clone())).collect()
        });
        if dirs.is_empty() {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("tree-load".into())
            .spawn(move || {
                let mut results = Vec::new();
                for d in &dirs {
                    let o = FileTreeOptions {
                        max_depth: if full { None } else { Some(1) },
                        ..opts.clone()
                    };
                    match FileTree::build_with(d, &o) {
                        Ok(t) => results.push(t),
                        Err(e) => tracing::warn!("cannot read {}: {e}", d.display()),
                    }
                }
                let _ = slint::invoke_from_event_loop(move || {
                    LOADING.with(|l| {
                        let mut l = l.borrow_mut();
                        for d in &dirs {
                            l.remove(d);
                        }
                    });
                    let Some(app) = APP.with(|a| a.borrow().upgrade()) else {
                        return;
                    };
                    let changed = {
                        let mut st = app.state.borrow_mut();
                        if st.generation != generation {
                            return;
                        }
                        let Some(tree) = st.workspace.as_mut().and_then(|w| w.tree.as_mut()) else {
                            return;
                        };
                        if tree.root.path != root {
                            return;
                        }
                        let mut changed = false;
                        for t in results {
                            if full && t.root.path == tree.root.path {
                                *tree = t;
                                changed = true;
                            } else {
                                changed |= tree.graft(t);
                            }
                        }
                        changed
                    };
                    if changed {
                        app.push_tree();
                    }
                });
            });
        if spawned.is_err() {
            LOADING.with(|l| l.borrow_mut().clear());
        }
    }

    fn save_tree_layout(&self) {
        let (visible, width, right) = {
            let Some(ui) = self.ui.upgrade() else { return };
            let api = ui.global::<WorkspaceApi>();
            (
                api.get_tree_visible(),
                api.get_tree_width(),
                api.get_tree_right(),
            )
        };
        let changed = {
            let mut st = self.state.borrow_mut();
            let s = &mut st.settings;
            let before = (
                s.project_panel_visible,
                s.project_panel_width,
                s.project_panel_right,
            );
            s.project_panel_visible = visible;
            s.project_panel_width = width;
            s.project_panel_right = right;
            s.sanitize();
            before
                != (
                    s.project_panel_visible,
                    s.project_panel_width,
                    s.project_panel_right,
                )
        };
        if changed {
            self.save_settings();
        }
    }

    /// The folder new entries go into: the selected folder, the folder of
    /// the selected file, or the project root.
    fn target_folder(&self) -> Option<PathBuf> {
        let st = self.state.borrow();
        let ws = st.workspace.as_ref()?;
        let root = ws.project.root.clone();
        let Some(sel) = ws.selected.as_ref() else {
            return Some(root);
        };
        let is_dir = ws
            .tree
            .as_ref()
            .and_then(|t| t.find(sel))
            .is_some_and(|n| n.is_dir);
        Some(if is_dir {
            sel.clone()
        } else {
            sel.parent().map_or(root, Path::to_path_buf)
        })
    }

    fn create_entry(&self, name: &str, is_dir: bool) {
        if let Some(why) = invalid_entry_name(name) {
            self.notify(NoticeKind::Warning, why);
            return;
        }
        let Some(folder) = self.target_folder() else {
            return;
        };
        let path = folder.join(name.trim());
        if path.exists() {
            self.notify(
                NoticeKind::Warning,
                format!("{} already exists", path.display()),
            );
            return;
        }
        let result = if is_dir {
            std::fs::create_dir_all(&path)
        } else {
            path.parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .map(drop)
                })
        };
        match result {
            Ok(()) => {
                if let Some(ws) = self.state.borrow_mut().workspace.as_mut() {
                    let mut p = path.parent();
                    while let Some(dir) = p {
                        if dir == ws.project.root {
                            break;
                        }
                        ws.expanded.insert(dir.to_path_buf());
                        p = dir.parent();
                    }
                    if is_dir {
                        ws.expanded.insert(path.clone());
                    }
                    ws.selected = Some(path.clone());
                }
                self.build_tree();
                if !is_dir {
                    self.open_file(&path);
                }
            }
            Err(e) => self.notify(
                NoticeKind::Error,
                format!("Cannot create {}: {e}", path.display()),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_follow_names() {
        assert_eq!(file_icon_kind("src", true), "folder");
        assert_eq!(file_icon_kind("main.py", false), "python");
        assert_eq!(file_icon_kind("pyproject.toml", false), "toml");
        assert_eq!(file_icon_kind("README.md", false), "md");
        assert_eq!(file_icon_kind("data.JSON", false), "json");
        assert_eq!(file_icon_kind("nb.ipynb", false), "ipynb");
        assert_eq!(file_icon_kind(".env", false), "env");
        assert_eq!(file_icon_kind(".env.local", false), "env");
        assert_eq!(file_icon_kind(".gitignore", false), "git");
        assert_eq!(file_icon_kind("uv.lock", false), "lock");
        assert_eq!(file_icon_kind("logo.png", false), "image");
        assert_eq!(file_icon_kind("q.sql", false), "sql");
        assert_eq!(file_icon_kind("index.html", false), "html");
        assert_eq!(file_icon_kind("notes.txt", false), "txt");
        assert_eq!(file_icon_kind("Makefile", false), "file");
    }

    #[test]
    fn filter_keeps_matches_and_their_folders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg/sub")).unwrap();
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        std::fs::write(dir.path().join("pkg/sub/models.py"), "").unwrap();
        std::fs::write(dir.path().join("pkg/views.py"), "").unwrap();
        std::fs::write(dir.path().join("other/readme.md"), "").unwrap();
        let tree = FileTree::build(dir.path()).unwrap();

        let rows = filter_rows(&tree, "MODEL");
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["pkg", "sub", "models.py"]);
        assert!(rows[0].expanded && rows[1].expanded);
        assert_eq!(rows[2].depth, 2);

        // A matching folder with no matching children is shown collapsed.
        let rows = filter_rows(&tree, "other");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].expanded);
        assert!(filter_rows(&tree, "zzz").is_empty());
    }

    #[test]
    fn entry_names_are_checked() {
        assert!(invalid_entry_name("  ").is_some());
        assert!(invalid_entry_name("..").is_some());
        assert!(invalid_entry_name("a:b").is_some());
        assert!(invalid_entry_name("../x").is_some());
        assert!(invalid_entry_name("pkg/__init__.py").is_none());
        assert!(invalid_entry_name("main.py").is_none());
    }
}

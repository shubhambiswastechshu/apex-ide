//! `apex-core`: the platform-independent foundation of Apex IDE.
//!
//! This crate owns the pieces every other crate and the UI build on:
//!
//! - [`project`]: the on-disk project model (`<root>/.apex/project.toml`),
//!   including run configurations.
//! - [`file_tree`]: a sorted, `.gitignore`-aware project file tree plus a flat
//!   row view for list-based tree widgets.
//! - [`settings`]: application-wide user settings (theme, font, recent
//!   projects) that never fail to load.
//! - [`keymap`]: the command registry, default shortcuts and user
//!   keymap overrides; the single source for menus and key handling.
//! - [`search`]: fuzzy matching, the "Go to File" index and the
//!   "Find in Files" content search.
//! - [`paths`]: the per-user directories Apex IDE stores its data in.
//!
//! All fallible operations return [`Result`], whose error type is
//! [`CoreError`].

#![warn(missing_docs)]

pub mod error;
pub mod file_tree;
mod fs_util;
pub mod keymap;
pub mod paths;
pub mod project;
pub mod search;
pub mod settings;

pub use error::{CoreError, Result};
pub use file_tree::{FileNode, FileTree, FileTreeOptions, TreeRow, ALWAYS_SKIPPED};
pub use keymap::{Chord, Command, KeySeq, Keymap, Lookup, COMMANDS};
pub use project::{
    Project, ProjectConfig, RunConfiguration, RunTarget, TemplateKind, CONFIG_DIR_NAME,
    CONFIG_FILE_NAME, CONFIG_VERSION,
};
pub use settings::{
    zoom_step, Density, LoadedSettings, Settings, DEFAULT_ZOOM, MAX_FONT_SIZE, MAX_RECENT_PROJECTS,
    MAX_ZOOM, MIN_FONT_SIZE, MIN_ZOOM, ZOOM_STEPS,
};

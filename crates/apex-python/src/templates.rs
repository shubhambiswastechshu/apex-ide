//! New-project templates.
//!
//! A template knows which packages to install and how to lay out starter
//! files. [`ProjectTemplate::scaffold`] never overwrites existing files, so
//! it is safe to run on a non-empty folder.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{PythonError, Result};

/// The kinds of project the "New Project" dialog can create.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectTemplate {
    /// A plain Python project with `main.py`.
    #[default]
    Pure,
    /// A Django web project.
    Django,
    /// Machine learning / data science with notebooks.
    DataScience,
    /// Data analytics with DataFrames and SQL.
    DataAnalytics,
}

/// Files written (or left alone) by [`ProjectTemplate::scaffold`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScaffoldReport {
    /// Files that were created.
    pub created: Vec<PathBuf>,
    /// Files that already existed and were not touched.
    pub skipped: Vec<PathBuf>,
}

/// A command to run after packages are installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostInstallCommand {
    /// What the command does, for progress UI.
    pub description: String,
    /// Arguments for the **venv interpreter**; run
    /// `<venv python> <python_args...>` with the project root as working
    /// directory.
    pub python_args: Vec<String>,
}

/// The Django project package created by [`ProjectTemplate::post_install_commands`].
pub const DJANGO_CONFIG_PACKAGE: &str = "config";

impl ProjectTemplate {
    /// Every template, in display order.
    pub const ALL: [ProjectTemplate; 4] = [
        Self::Pure,
        Self::Django,
        Self::DataScience,
        Self::DataAnalytics,
    ];

    /// Stable identifier, e.g. `"data-science"` (matches the serde form).
    pub fn id(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::Django => "django",
            Self::DataScience => "data-science",
            Self::DataAnalytics => "data-analytics",
        }
    }

    /// Looks a template up by [`id`](Self::id).
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.id() == id)
    }

    /// Name shown in the UI.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Pure => "Pure Python",
            Self::Django => "Django",
            Self::DataScience => "Data Science",
            Self::DataAnalytics => "Data Analytics",
        }
    }

    /// One-line description shown in the UI.
    pub fn description(self) -> &'static str {
        match self {
            Self::Pure => "A simple Python project with a main.py entry point.",
            Self::Django => "A Django web application with manage.py and a config package.",
            Self::DataScience => {
                "NumPy, pandas, scikit-learn and Jupyter notebooks for modelling and experiments."
            }
            Self::DataAnalytics => {
                "pandas, Polars and DuckDB with notebooks for exploring and reporting on data."
            }
        }
    }

    /// Packages to install into the new venv.
    pub fn packages(self) -> &'static [&'static str] {
        match self {
            Self::Pure => &[],
            Self::Django => &["django"],
            Self::DataScience => &[
                "numpy",
                "pandas",
                "scikit-learn",
                "matplotlib",
                "jupyter",
                "ipykernel",
            ],
            Self::DataAnalytics => &[
                "pandas",
                "polars",
                "duckdb",
                "matplotlib",
                "seaborn",
                "jupyter",
            ],
        }
    }

    /// The file to open in the editor once the project is ready, relative to
    /// the project root.
    pub fn entry_file(self) -> &'static str {
        match self {
            Self::Pure => "main.py",
            Self::Django => "manage.py",
            Self::DataScience | Self::DataAnalytics => "src/analysis.py",
        }
    }

    /// Writes the template's starter files into `root` (created if needed).
    ///
    /// `name` is the human project name; it is normalised for
    /// `pyproject.toml` (see [`package_name`]). Existing files are never
    /// overwritten; they are listed in [`ScaffoldReport::skipped`].
    pub fn scaffold(self, root: &Path, name: &str) -> Result<ScaffoldReport> {
        let package = package_name(name)?;
        let mut writer = Writer {
            root,
            report: ScaffoldReport::default(),
        };
        fs::create_dir_all(root)
            .map_err(|e| PythonError::io(format!("creating `{}`", root.display()), e))?;

        writer.file("pyproject.toml", &pyproject(&package, self))?;
        writer.file(".gitignore", GITIGNORE)?;
        match self {
            Self::Pure => writer.file("main.py", &pure_main(name))?,
            Self::Django => {}
            Self::DataScience => {
                writer.file("data/.gitkeep", "")?;
                writer.file("src/analysis.py", DATA_SCIENCE_ANALYSIS)?;
                writer.file("notebooks/exploration.ipynb", &data_science_notebook())?;
            }
            Self::DataAnalytics => {
                writer.file("data/.gitkeep", "")?;
                writer.file("src/analysis.py", DATA_ANALYTICS_ANALYSIS)?;
                writer.file("notebooks/analysis.ipynb", &data_analytics_notebook())?;
            }
        }
        Ok(writer.report)
    }

    /// Commands to run with the venv interpreter after
    /// [`packages`](Self::packages) are installed.
    ///
    /// Django runs `python -m django startproject config .` (creating
    /// `manage.py` and the `config` package) unless `manage.py` already
    /// exists in `root`. Other templates need nothing.
    pub fn post_install_commands(self, root: &Path) -> Vec<PostInstallCommand> {
        match self {
            Self::Django if !root.join("manage.py").exists() => vec![PostInstallCommand {
                description: "Creating Django project".into(),
                python_args: ["-m", "django", "startproject", DJANGO_CONFIG_PACKAGE, "."]
                    .map(String::from)
                    .to_vec(),
            }],
            _ => Vec::new(),
        }
    }
}

/// Normalises a project name into a PEP 503 package name
/// (`"My Cool_App"` becomes `"my-cool-app"`).
pub fn package_name(name: &str) -> Result<String> {
    let mut out = String::with_capacity(name.len());
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_owned();
    if out.is_empty() {
        Err(PythonError::InvalidProjectName(name.to_owned()))
    } else {
        Ok(out)
    }
}

/// Writes files under a root, recording what happened.
struct Writer<'a> {
    root: &'a Path,
    report: ScaffoldReport,
}

impl Writer<'_> {
    /// Creates `rel` (a `/`-separated relative path) unless it exists.
    fn file(&mut self, rel: &str, contents: &str) -> Result<()> {
        let path = rel
            .split('/')
            .fold(self.root.to_path_buf(), |p, c| p.join(c));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| PythonError::io(format!("creating `{}`", parent.display()), e))?;
        }
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(contents.as_bytes())
                    .map_err(|e| PythonError::io(format!("writing `{}`", path.display()), e))?;
                self.report.created.push(path);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => self.report.skipped.push(path),
            Err(e) => return Err(PythonError::io(format!("creating `{}`", path.display()), e)),
        }
        Ok(())
    }
}

/// A quoted, escaped TOML string.
fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

fn pyproject(package: &str, template: ProjectTemplate) -> String {
    let deps: String = template
        .packages()
        .iter()
        .map(|p| format!("    {},\n", toml_str(p)))
        .collect();
    let deps = if deps.is_empty() {
        "[]".to_owned()
    } else {
        format!("[\n{deps}]")
    };
    format!(
        "[project]\nname = {name}\nversion = \"0.1.0\"\ndescription = {desc}\nrequires-python = \">=3.9\"\ndependencies = {deps}\n",
        name = toml_str(package),
        desc = toml_str(template.description()),
    )
}

fn pure_main(name: &str) -> String {
    let greeting = format!("Hello from {}!", name.trim());
    // A JSON string literal is also a valid Python string literal.
    let literal = serde_json::Value::String(greeting).to_string();
    format!(
        "def main() -> None:\n    print({literal})\n\n\nif __name__ == \"__main__\":\n    main()\n"
    )
}

/// Builds a minimal nbformat 4.5 notebook from `(is_code, source)` cells.
fn notebook(cells: &[(bool, &str)]) -> String {
    let cells: Vec<_> = cells
        .iter()
        .enumerate()
        .map(|(i, (is_code, source))| {
            let lines: Vec<&str> = source.split_inclusive('\n').collect();
            let id = format!("cell-{i}");
            if *is_code {
                json!({
                    "cell_type": "code",
                    "execution_count": null,
                    "id": id,
                    "metadata": {},
                    "outputs": [],
                    "source": lines,
                })
            } else {
                json!({
                    "cell_type": "markdown",
                    "id": id,
                    "metadata": {},
                    "source": lines,
                })
            }
        })
        .collect();
    let nb = json!({
        "cells": cells,
        "metadata": {
            "kernelspec": {
                "display_name": "Python 3",
                "language": "python",
                "name": "python3"
            },
            "language_info": { "name": "python" }
        },
        "nbformat": 4,
        "nbformat_minor": 5
    });
    // Serialising a `Value` built from literals cannot fail; fall back to
    // the compact form just in case.
    serde_json::to_string_pretty(&nb).unwrap_or_else(|_| nb.to_string()) + "\n"
}

fn data_science_notebook() -> String {
    notebook(&[
        (false, "# Exploration\n\nLoad a dataset and take a first look."),
        (
            true,
            "import pandas as pd\nfrom sklearn.datasets import load_iris\n\niris = load_iris(as_frame=True)\ndf = iris.frame\ndf.head()",
        ),
        (true, "df.describe()"),
    ])
}

fn data_analytics_notebook() -> String {
    notebook(&[
        (false, "# Analysis\n\nQuery DataFrames with DuckDB SQL."),
        (
            true,
            "import duckdb\nimport polars as pl\n\nsales = pl.DataFrame({\"region\": [\"north\", \"south\", \"north\"], \"amount\": [120, 80, 45]})\nduckdb.sql(\"SELECT region, SUM(amount) AS total FROM sales GROUP BY region\").pl()",
        ),
    ])
}

const GITIGNORE: &str = "\
.venv/
__pycache__/
*.py[cod]
.ipynb_checkpoints/
.idea/
.apex/
*.sqlite3
dist/
build/
";

const DATA_SCIENCE_ANALYSIS: &str = r#""""Train a small model on the iris dataset."""

from sklearn.datasets import load_iris
from sklearn.ensemble import RandomForestClassifier
from sklearn.model_selection import train_test_split


def main() -> None:
    features, labels = load_iris(return_X_y=True, as_frame=True)
    x_train, x_test, y_train, y_test = train_test_split(
        features, labels, test_size=0.25, random_state=42
    )
    model = RandomForestClassifier(random_state=42).fit(x_train, y_train)
    print(f"Test accuracy: {model.score(x_test, y_test):.3f}")


if __name__ == "__main__":
    main()
"#;

const DATA_ANALYTICS_ANALYSIS: &str = r#""""Summarise a small dataset with Polars and DuckDB."""

import duckdb
import polars as pl


def main() -> None:
    sales = pl.DataFrame(
        {"region": ["north", "south", "north", "east"], "amount": [120, 80, 45, 60]}
    )
    totals = duckdb.sql(
        "SELECT region, SUM(amount) AS total FROM sales GROUP BY region ORDER BY total DESC"
    ).pl()
    print(totals)


if __name__ == "__main__":
    main()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("read")
    }

    #[test]
    fn ids_round_trip_and_metadata_is_present() {
        for t in ProjectTemplate::ALL {
            assert_eq!(ProjectTemplate::from_id(t.id()), Some(t));
            assert!(!t.display_name().is_empty());
            assert!(!t.description().is_empty());
            let json = serde_json::to_string(&t).expect("serialize");
            assert_eq!(json, format!("\"{}\"", t.id()));
        }
        assert_eq!(ProjectTemplate::from_id("fastapi"), None);
    }

    #[test]
    fn package_lists() {
        assert!(ProjectTemplate::Pure.packages().is_empty());
        assert_eq!(ProjectTemplate::Django.packages(), ["django"]);
        assert!(ProjectTemplate::DataScience
            .packages()
            .contains(&"scikit-learn"));
        assert!(ProjectTemplate::DataAnalytics
            .packages()
            .contains(&"duckdb"));
    }

    #[test]
    fn package_name_normalises() {
        assert_eq!(package_name("My Cool_App").expect("ok"), "my-cool-app");
        assert_eq!(package_name("  data--lab.v2 ").expect("ok"), "data-lab-v2");
        assert!(package_name("").is_err());
        assert!(package_name("!!!").is_err());
    }

    #[test]
    fn pure_scaffold_writes_main_and_pyproject() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("hello");
        let report = ProjectTemplate::Pure
            .scaffold(&root, "Hello \"World\"")
            .expect("scaffold");
        assert!(report.skipped.is_empty());
        assert!(report.created.contains(&root.join("main.py")));
        assert!(read(&root.join("main.py")).contains(r#"print("Hello from Hello \"World\"!")"#));

        let toml: toml::Table = read(&root.join("pyproject.toml")).parse().expect("toml");
        assert_eq!(toml["project"]["name"].as_str(), Some("hello-world"));
        assert_eq!(
            toml["project"]["dependencies"].as_array().map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn data_templates_write_valid_notebooks_and_folders() {
        for (template, notebook) in [
            (ProjectTemplate::DataScience, "exploration.ipynb"),
            (ProjectTemplate::DataAnalytics, "analysis.ipynb"),
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            template.scaffold(tmp.path(), "lab").expect("scaffold");
            assert!(tmp.path().join("data").is_dir());
            assert!(tmp.path().join("src").join("analysis.py").is_file());
            assert!(tmp.path().join(template.entry_file()).is_file());

            let nb: serde_json::Value =
                serde_json::from_str(&read(&tmp.path().join("notebooks").join(notebook)))
                    .expect("notebook json");
            assert_eq!(nb["nbformat"], 4);
            let cells = nb["cells"].as_array().expect("cells");
            assert!(!cells.is_empty());
            for cell in cells {
                assert!(cell["id"].is_string());
                assert!(cell["source"].is_array());
            }

            let toml: toml::Table = read(&tmp.path().join("pyproject.toml"))
                .parse()
                .expect("toml");
            let deps: Vec<&str> = toml["project"]["dependencies"]
                .as_array()
                .expect("deps")
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert_eq!(deps, template.packages());
        }
    }

    #[test]
    fn django_scaffold_and_post_install() {
        let tmp = tempfile::tempdir().expect("tempdir");
        ProjectTemplate::Django
            .scaffold(tmp.path(), "shop")
            .expect("scaffold");
        assert!(tmp.path().join("pyproject.toml").is_file());
        let cmds = ProjectTemplate::Django.post_install_commands(tmp.path());
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0].python_args,
            ["-m", "django", "startproject", "config", "."]
        );
        fs::write(tmp.path().join("manage.py"), "").expect("write");
        assert!(ProjectTemplate::Django
            .post_install_commands(tmp.path())
            .is_empty());
        assert!(ProjectTemplate::Pure
            .post_install_commands(tmp.path())
            .is_empty());
    }

    #[test]
    fn scaffold_never_overwrites() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("main.py");
        fs::write(&main, "print('mine')\n").expect("write");
        let report = ProjectTemplate::Pure
            .scaffold(tmp.path(), "p")
            .expect("scaffold");
        assert_eq!(report.skipped, vec![main.clone()]);
        assert_eq!(read(&main), "print('mine')\n");

        // Running twice skips everything the first run created.
        let again = ProjectTemplate::Pure
            .scaffold(tmp.path(), "p")
            .expect("scaffold");
        assert!(again.created.is_empty());
        assert_eq!(again.skipped.len(), report.created.len() + 1);
    }

    #[test]
    fn scaffold_rejects_bad_names_before_writing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("x");
        assert!(matches!(
            ProjectTemplate::Pure.scaffold(&root, "???"),
            Err(PythonError::InvalidProjectName(_))
        ));
        assert!(!root.exists());
    }
}

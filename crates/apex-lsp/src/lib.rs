//! # apex-lsp
//!
//! A small, synchronous Language Server Protocol client for Apex IDE.
//!
//! No async runtime is involved: the client talks to the server over stdio
//! using background threads and [`crossbeam_channel`]s.
//!
//! - [`transport`]: `Content-Length` framing and JSON-RPC message parsing.
//! - [`LspClient`]: spawns a server, correlates requests with responses and
//!   forwards server notifications as [`LspEvent`]s.
//! - [`find_python_servers`]: discovers basedpyright, pyright, pylsp or ruff.
//! - [`path_to_uri`] / [`uri_to_path`]: path ↔ `file://` URI conversion that
//!   is correct for Windows drive letters and UNC paths.
//!
//! ```no_run
//! use apex_lsp::{find_python_servers, path_to_uri, LspClient, LspEvent};
//! use lsp_types::Position;
//!
//! # fn main() -> Result<(), apex_lsp::LspError> {
//! let root = path_to_uri(std::path::Path::new("my_project"))?;
//! let Some(spec) = find_python_servers().into_iter().next() else { return Ok(()) };
//! let client = LspClient::start(&spec.with_root_uri(root))?;
//! client.initialize()?;
//!
//! let file = path_to_uri(std::path::Path::new("my_project/main.py"))?;
//! client.did_open(&file, "python", 1, "import os\nos.")?;
//! let items = client.completion(&file, Position::new(1, 3))?;
//! println!("{} completions", items.len());
//!
//! while let Ok(event) = client.events().try_recv() {
//!     if let LspEvent::Diagnostics(d) = event {
//!         println!("{} diagnostics", d.diagnostics.len());
//!     }
//! }
//! client.shutdown()?;
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

mod client;
mod error;
mod server;
pub mod transport;
mod types;
mod uri;

pub use client::{client_capabilities, ClientOptions, LspClient};
pub use error::{LspError, TransportError};
pub use server::{
    find_python_servers, find_python_servers_in, venv_bin_dir, ServerSpec, PYTHON_SERVERS,
};
pub use types::{Completion, HoverInfo, LspEvent};
pub use uri::{path_to_uri, uri_to_path};

/// Re-export of the `lsp-types` crate used in this API.
pub use lsp_types;

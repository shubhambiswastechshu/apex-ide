//! Smoke test against a real Python language server, if one is installed.
//!
//! Ignored by default because it depends on the machine; run it with
//! `cargo test -p apex-lsp -- --ignored`.

use std::time::{Duration, Instant};

use apex_lsp::lsp_types::Position;
use apex_lsp::{find_python_servers, path_to_uri, uri_to_path, LspClient, LspEvent};

#[test]
#[ignore = "needs pyright/basedpyright installed"]
fn pyright_round_trip() {
    let Some(spec) = find_python_servers()
        .into_iter()
        .find(|s| s.display_name().contains("pyright"))
    else {
        eprintln!("no pyright-family server found; skipping");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.py");
    let text = "import os\nos.pa\nprint(undefined_name)\n";
    std::fs::write(&file, text).unwrap();

    let root = path_to_uri(dir.path()).unwrap();
    let uri = path_to_uri(&file).unwrap();
    let client = LspClient::start(&spec.with_root_uri(root)).unwrap();
    client.initialize().unwrap();
    client.did_open(&uri, "python", 1, text).unwrap();

    let events = client.events();
    let deadline = Instant::now() + Duration::from_secs(60);
    let diagnostics = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events
            .recv_timeout(left)
            .expect("no diagnostics from server")
        {
            LspEvent::Diagnostics(d)
                if uri_to_path(&d.uri).ok() == uri_to_path(&uri).ok()
                    && !d.diagnostics.is_empty() =>
            {
                break d
            }
            _ => {}
        }
    };
    assert!(diagnostics
        .diagnostics
        .iter()
        .any(|d| d.message.contains("undefined_name")));

    let items = client.completion(&uri, Position::new(1, 5)).unwrap();
    assert!(items.iter().any(|c| c.label == "path"), "{items:?}");

    let hover = client.hover(&uri, Position::new(0, 8)).unwrap();
    assert!(hover.is_some_and(|h| !h.contents.is_empty()));

    client.shutdown().unwrap();
}

//! Simplified, UI-friendly views of LSP data and the events a server can push.

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, GotoDefinitionResponse,
    Hover, HoverContents, Location, LogMessageParams, MarkedString, ProgressParams,
    PublishDiagnosticsParams, Range, ShowMessageParams,
};
use serde_json::Value;

/// Something the server sent without being asked.
#[derive(Debug, Clone)]
pub enum LspEvent {
    /// `textDocument/publishDiagnostics`: the full set of diagnostics for one file.
    Diagnostics(PublishDiagnosticsParams),
    /// `window/logMessage`.
    Log(LogMessageParams),
    /// `window/showMessage`.
    ShowMessage(ShowMessageParams),
    /// `$/progress` (work-done progress for indexing and the like).
    Progress(ProgressParams),
    /// Any other notification, passed through unparsed.
    Notification {
        /// Method name.
        method: String,
        /// Raw parameters.
        params: Value,
    },
    /// One line the server wrote to stderr.
    Stderr(String),
    /// The server's output ended; no more events or responses will arrive.
    Disconnected,
}

/// A completion item reduced to what an editor popup needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    /// Text shown in the list.
    pub label: String,
    /// Item kind (function, class, keyword, ...), for the icon.
    pub kind: Option<CompletionItemKind>,
    /// Extra detail such as a signature or type.
    pub detail: Option<String>,
    /// Documentation as plain text or Markdown.
    pub documentation: Option<String>,
    /// Text to insert when accepted.
    pub insert_text: String,
    /// Range to replace with `insert_text`; `None` means "the word at the cursor".
    pub replace_range: Option<Range>,
    /// Key used for ordering (already applied to the returned list).
    pub sort_text: String,
    /// Key to filter on while the user keeps typing.
    pub filter_text: String,
}

impl From<CompletionItem> for Completion {
    fn from(item: CompletionItem) -> Self {
        let (insert_text, replace_range) = match item.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => (edit.new_text, Some(edit.range)),
            Some(CompletionTextEdit::InsertAndReplace(edit)) => (edit.new_text, Some(edit.insert)),
            None => (item.insert_text.unwrap_or_else(|| item.label.clone()), None),
        };
        Completion {
            sort_text: item.sort_text.unwrap_or_else(|| item.label.clone()),
            filter_text: item.filter_text.unwrap_or_else(|| item.label.clone()),
            documentation: item.documentation.map(|d| match d {
                Documentation::String(s) => s,
                Documentation::MarkupContent(m) => m.value,
            }),
            label: item.label,
            kind: item.kind,
            detail: item.detail,
            insert_text,
            replace_range,
        }
    }
}

/// Converts and orders completion items by their sort key (stable for ties).
pub(crate) fn completions_from(items: Vec<CompletionItem>) -> Vec<Completion> {
    let mut out: Vec<Completion> = items.into_iter().map(Completion::from).collect();
    out.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    out
}

/// Hover information rendered to a single Markdown string.
#[derive(Debug, Clone, PartialEq)]
pub struct HoverInfo {
    /// Markdown (code blocks are fenced with their language).
    pub contents: String,
    /// The range the hover applies to, if the server gave one.
    pub range: Option<Range>,
}

impl From<Hover> for HoverInfo {
    fn from(hover: Hover) -> Self {
        let contents = match hover.contents {
            HoverContents::Scalar(s) => marked_to_markdown(s),
            HoverContents::Array(parts) => parts
                .into_iter()
                .map(marked_to_markdown)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
            HoverContents::Markup(m) => m.value,
        };
        HoverInfo {
            contents,
            range: hover.range,
        }
    }
}

fn marked_to_markdown(s: MarkedString) -> String {
    match s {
        MarkedString::String(s) => s,
        MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
    }
}

/// Flattens any definition response shape into plain locations.
pub(crate) fn locations_from(resp: Option<GotoDefinitionResponse>) -> Vec<Location> {
    match resp {
        None => Vec::new(),
        Some(GotoDefinitionResponse::Scalar(loc)) => vec![loc],
        Some(GotoDefinitionResponse::Array(locs)) => locs,
        Some(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|l| Location::new(l.target_uri, l.target_selection_range))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        InsertReplaceEdit, LanguageString, LocationLink, MarkupContent, MarkupKind, Position,
        TextEdit, Uri,
    };

    fn range(l: u32, a: u32, b: u32) -> Range {
        Range::new(Position::new(l, a), Position::new(l, b))
    }

    #[test]
    fn completion_prefers_text_edit_then_insert_text_then_label() {
        let edit = CompletionItem {
            label: "append".into(),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
                range(0, 2, 4),
                "append".into(),
            ))),
            insert_text: Some("ignored".into()),
            ..Default::default()
        };
        let c = Completion::from(edit);
        assert_eq!(c.insert_text, "append");
        assert_eq!(c.replace_range, Some(range(0, 2, 4)));

        let ir = CompletionItem {
            label: "x".into(),
            text_edit: Some(CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
                new_text: "xyz".into(),
                insert: range(1, 0, 1),
                replace: range(1, 0, 3),
            })),
            ..Default::default()
        };
        assert_eq!(Completion::from(ir).replace_range, Some(range(1, 0, 1)));

        let plain = CompletionItem {
            label: "print".into(),
            insert_text: Some("print()".into()),
            documentation: Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: "**docs**".into(),
            })),
            ..Default::default()
        };
        let c = Completion::from(plain);
        assert_eq!(c.insert_text, "print()");
        assert_eq!(c.documentation.as_deref(), Some("**docs**"));
        assert_eq!(c.filter_text, "print");

        let bare = Completion::from(CompletionItem::new_simple("len".into(), "fn".into()));
        assert_eq!(bare.insert_text, "len");
        assert_eq!(bare.detail.as_deref(), Some("fn"));
    }

    #[test]
    fn completions_are_sorted_by_sort_text() {
        let item = |label: &str, sort: Option<&str>| CompletionItem {
            label: label.into(),
            sort_text: sort.map(Into::into),
            ..Default::default()
        };
        let out = completions_from(vec![
            item("zeta", Some("0")),
            item("beta", None),
            item("alpha", None),
        ]);
        let labels: Vec<_> = out.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["zeta", "alpha", "beta"]);
    }

    #[test]
    fn hover_rendering() {
        let h = Hover {
            contents: HoverContents::Array(vec![
                MarkedString::LanguageString(LanguageString {
                    language: "python".into(),
                    value: "def f() -> int".into(),
                }),
                MarkedString::String("Returns one.".into()),
            ]),
            range: None,
        };
        assert_eq!(
            HoverInfo::from(h).contents,
            "```python\ndef f() -> int\n```\n\nReturns one."
        );
    }

    #[test]
    fn definition_shapes_flatten() {
        let uri: Uri = "file:///a.py".parse().unwrap();
        assert!(locations_from(None).is_empty());
        let link = LocationLink {
            origin_selection_range: None,
            target_uri: uri.clone(),
            target_range: range(0, 0, 10),
            target_selection_range: range(0, 4, 5),
        };
        assert_eq!(
            locations_from(Some(GotoDefinitionResponse::Link(vec![link]))),
            vec![Location::new(uri, range(0, 4, 5))]
        );
    }
}

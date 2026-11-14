//! `isma lsp`: a Language Server Protocol server over stdio.
//!
//! It reuses the real compiler front end, so editor squiggles are exactly
//! the errors `isma build` would report. Features: diagnostics (errors +
//! lints), hover types, go-to-definition, completion (including `.` member
//! completion), document outline and inlay type hints.

use crate::check::Analysis;
use crate::diagnostics::{Diagnostic as IsmaDiagnostic, Severity};
use crate::ide::*;
use crate::span::Span;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types as lt;
use std::collections::HashMap;
use std::error::Error;

type BoxError = Box<dyn Error + Send + Sync>;

pub fn run() -> Result<(), BoxError> {
    let (conn, io_threads) = Connection::stdio();
    let caps = lt::ServerCapabilities {
        text_document_sync: Some(lt::TextDocumentSyncCapability::Kind(lt::TextDocumentSyncKind::FULL)),
        hover_provider: Some(lt::HoverProviderCapability::Simple(true)),
        definition_provider: Some(lt::OneOf::Left(true)),
        completion_provider: Some(lt::CompletionOptions {
            trigger_characters: Some(vec![".".into()]),
            ..Default::default()
        }),
        document_symbol_provider: Some(lt::OneOf::Left(true)),
        inlay_hint_provider: Some(lt::OneOf::Left(true)),
        ..Default::default()
    };
    conn.initialize(serde_json::to_value(caps)?)?;
    let mut server = Server { docs: HashMap::new() };
    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    break;
                }
                let resp = server.handle_request(req);
                conn.sender.send(Message::Response(resp))?;
            }
            Message::Notification(n) => {
                if let Some(out) = server.handle_notification(n) {
                    conn.sender.send(Message::Notification(out))?;
                }
            }
            Message::Response(_) => {}
        }
    }
    // The writer thread only stops once every sender is gone.
    drop(conn);
    io_threads.join()?;
    Ok(())
}

struct Document {
    text: String,
    lines: LineMap,
    ide: IdeInfo,
}

struct Server {
    docs: HashMap<lt::Uri, Document>,
}

/// Run the compiler front end, surviving any internal panic.
fn analyze_safely(text: &str) -> Option<Analysis> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::analyze(text))).ok()
}

fn params<P: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<P, String> {
    serde_json::from_value(v).map_err(|e| e.to_string())
}

impl Server {
    fn handle_notification(&mut self, n: Notification) -> Option<Notification> {
        use lt::notification::*;
        match n.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let p: lt::DidOpenTextDocumentParams = params(n.params).ok()?;
                Some(self.update(p.text_document.uri, p.text_document.text, Some(p.text_document.version)))
            }
            DidChangeTextDocument::METHOD => {
                let p: lt::DidChangeTextDocumentParams = params(n.params).ok()?;
                let text = p.content_changes.into_iter().last()?.text;
                Some(self.update(p.text_document.uri, text, Some(p.text_document.version)))
            }
            DidCloseTextDocument::METHOD => {
                let p: lt::DidCloseTextDocumentParams = params(n.params).ok()?;
                self.docs.remove(&p.text_document.uri);
                let clear = lt::PublishDiagnosticsParams { uri: p.text_document.uri, diagnostics: Vec::new(), version: None };
                Some(lsp_server::Notification::new(PublishDiagnostics::METHOD.into(), clear))
            }
            _ => None,
        }
    }

    /// Re-analyze a document and build its `publishDiagnostics` notification.
    fn update(&mut self, uri: lt::Uri, text: String, version: Option<i32>) -> Notification {
        let lines = LineMap::new(&text);
        let (diagnostics, ide) = match analyze_safely(&text) {
            Some(a) => {
                let ds = a.diagnostics.iter().map(|d| to_lsp_diagnostic(d, &lines, &uri)).collect();
                (ds, a.ide)
            }
            None => {
                let d = lt::Diagnostic {
                    range: lines.range(Span::default()),
                    severity: Some(lt::DiagnosticSeverity::WARNING),
                    source: Some("isma".into()),
                    message: "internal compiler error while analyzing this file (please report it)".into(),
                    ..Default::default()
                };
                (vec![d], IdeInfo::default())
            }
        };
        self.docs.insert(uri.clone(), Document { text, lines, ide });
        let p = lt::PublishDiagnosticsParams { uri, diagnostics, version };
        use lt::notification::Notification as _;
        Notification::new(lt::notification::PublishDiagnostics::METHOD.into(), p)
    }

    fn handle_request(&mut self, req: Request) -> Response {
        use lt::request::*;
        let id = req.id.clone();
        let result: Result<serde_json::Value, String> = match req.method.as_str() {
            HoverRequest::METHOD => params(req.params).map(|p| json(self.hover(p))),
            GotoDefinition::METHOD => params(req.params).map(|p| json(self.definition(p))),
            Completion::METHOD => params(req.params).map(|p| json(self.completion(p))),
            DocumentSymbolRequest::METHOD => params(req.params).map(|p| json(self.symbols(p))),
            InlayHintRequest::METHOD => params(req.params).map(|p| json(self.inlay_hints(p))),
            _ => return Response::new_err(id, ErrorCode::MethodNotFound as i32, format!("unsupported: {}", req.method)),
        };
        match result {
            Ok(v) => Response::new_ok(id, v),
            Err(e) => Response::new_err(id, ErrorCode::InvalidParams as i32, e),
        }
    }

    // ------------------------------------------------------------ hover

    fn hover(&self, p: lt::HoverParams) -> Option<lt::Hover> {
        let pos = p.text_document_position_params;
        let doc = self.docs.get(&pos.text_document.uri)?;
        let off = doc.lines.offset(pos.position);
        let h = doc.ide.hovers.iter().filter(|h| contains(h.span, off)).min_by_key(|h| h.span.end - h.span.start)?;
        let mut value = format!("```isma\n{}\n```", h.code);
        if let Some(d) = &h.doc {
            value.push_str("\n\n");
            value.push_str(d);
        }
        Some(lt::Hover {
            contents: lt::HoverContents::Markup(lt::MarkupContent { kind: lt::MarkupKind::Markdown, value }),
            range: Some(doc.lines.range(h.span)),
        })
    }

    // ------------------------------------------------------------ definition

    fn definition(&self, p: lt::GotoDefinitionParams) -> Option<lt::GotoDefinitionResponse> {
        let pos = p.text_document_position_params;
        let doc = self.docs.get(&pos.text_document.uri)?;
        let off = doc.lines.offset(pos.position);
        let (_, def) = doc.ide.definitions.iter().filter(|(u, _)| contains(*u, off)).min_by_key(|(u, _)| u.end - u.start)?;
        Some(lt::GotoDefinitionResponse::Scalar(lt::Location { uri: pos.text_document.uri, range: doc.lines.range(*def) }))
    }

    // ------------------------------------------------------------ completion

    fn completion(&self, p: lt::CompletionParams) -> Option<lt::CompletionResponse> {
        let pos = p.text_document_position;
        let doc = self.docs.get(&pos.text_document.uri)?;
        let off = doc.lines.offset(pos.position);
        let bytes = doc.text.as_bytes();
        let mut word_start = off;
        while word_start > 0 && is_ident(bytes[word_start - 1]) {
            word_start -= 1;
        }
        if in_comment_or_string(&doc.text, off) {
            return None;
        }
        let items = if word_start > 0 && bytes[word_start - 1] == b'.' {
            self.member_completion(doc, word_start - 1, off)?
        } else {
            self.scope_completion(doc, off)
        };
        Some(lt::CompletionResponse::Array(items))
    }

    /// Completion after `receiver.`: figure out the receiver's type by
    /// analyzing a patched copy of the file in which the dangling `.` is gone.
    fn member_completion(&self, doc: &Document, dot: usize, off: usize) -> Option<Vec<lt::CompletionItem>> {
        let text = &doc.text;
        let mut word_end = off;
        while word_end < text.len() && is_ident(text.as_bytes()[word_end]) {
            word_end += 1;
        }
        // Attempt 1: just drop `.partial` and analyze the file.
        let patched = format!("{}{}", &text[..dot], &text[word_end..]);
        let mut found = analyze_safely(&patched).and_then(|a| {
            a.ide
                .expr_types
                .iter()
                .filter(|(s, t)| s.end as usize == dot && !t.is_error())
                .min_by_key(|(s, _)| s.end - s.start)
                .map(|(_, t)| (t.clone(), a.ide.clone()))
        });
        // Attempt 2: the line may not parse without its ending (e.g. `if p.`),
        // so replace the whole line with `val __probe = <receiver>`.
        if found.is_none() {
            let recv_start = receiver_start(text, dot);
            let line_start = text[..dot].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let line_end = text[dot..].find('\n').map(|i| dot + i).unwrap_or(text.len());
            let indent: String = text[line_start..].chars().take_while(|c| *c == ' ').collect();
            let receiver = &text[recv_start..dot];
            if !receiver.trim().is_empty() {
                let probe_prefix = format!("{indent}val __probe = ");
                let start = line_start + probe_prefix.len();
                let patched = format!("{}{probe_prefix}{receiver}{}", &text[..line_start], &text[line_end..]);
                found = analyze_safely(&patched).and_then(|a| {
                    a.ide
                        .expr_types
                        .iter()
                        .find(|(s, t)| s.start as usize == start && s.end as usize == start + receiver.len() && !t.is_error())
                        .map(|(_, t)| (t.clone(), a.ide.clone()))
                });
            }
        }
        let (ty, ide) = found?;
        let items = ide
            .members(&ty)
            .into_iter()
            .map(|m| {
                let (insert, format) = match (m.is_method, m.takes_args) {
                    (false, _) => (m.name.clone(), lt::InsertTextFormat::PLAIN_TEXT),
                    (true, false) => (format!("{}()", m.name), lt::InsertTextFormat::PLAIN_TEXT),
                    (true, true) => (format!("{}($1)", m.name), lt::InsertTextFormat::SNIPPET),
                };
                lt::CompletionItem {
                    label: m.name.clone(),
                    kind: Some(if m.is_method { lt::CompletionItemKind::METHOD } else { lt::CompletionItemKind::FIELD }),
                    detail: Some(m.signature.clone()),
                    documentation: Some(markdown(&m.doc)),
                    insert_text: Some(insert),
                    insert_text_format: Some(format),
                    ..Default::default()
                }
            })
            .collect();
        Some(items)
    }

    fn scope_completion(&self, doc: &Document, off: usize) -> Vec<lt::CompletionItem> {
        let mut items = Vec::new();
        let mut add = |label: &str, kind, detail: Option<String>, documentation: Option<String>, sort: &str| {
            items.push(lt::CompletionItem {
                label: label.to_string(),
                kind: Some(kind),
                detail,
                documentation: documentation.map(|d| markdown(&d)),
                sort_text: Some(format!("{sort}{label}")),
                ..Default::default()
            });
        };
        for l in visible_locals(doc, off) {
            let kind = match l.kind {
                LocalKind::Param => "parameter",
                LocalKind::LoopVar => "loop variable",
                LocalKind::Val => "val",
                LocalKind::Var => "var",
            };
            add(&l.name, lt::CompletionItemKind::VARIABLE, Some(format!("{}: {}", l.name, l.ty)), Some(kind.into()), "0");
        }
        for f in doc.ide.functions.iter().filter(|f| f.owner.is_none()) {
            add(&f.name, lt::CompletionItemKind::FUNCTION, Some(f.signature.clone()), None, "1");
        }
        for s in &doc.ide.structs {
            let fields: Vec<String> = s.fields.iter().map(|f| format!("{}: {}", f.name, f.ty)).collect();
            add(&s.name, lt::CompletionItemKind::STRUCT, Some(format!("struct {}({})", s.name, fields.join(", "))), None, "1");
        }
        for name in BUILTIN_FUNCTIONS {
            let (sig, d) = builtin_function(name).unwrap();
            add(name, lt::CompletionItemKind::FUNCTION, Some(sig.into()), Some(d.into()), "2");
        }
        for (name, d) in BUILTIN_TYPES {
            add(name, lt::CompletionItemKind::CLASS, Some(format!("type {name}")), Some((*d).into()), "3");
        }
        for k in KEYWORDS {
            add(k, lt::CompletionItemKind::KEYWORD, None, None, "4");
        }
        items
    }

    // ------------------------------------------------------------ outline

    #[allow(deprecated)] // `DocumentSymbol::deprecated` must still be filled in
    fn symbols(&self, p: lt::DocumentSymbolParams) -> Option<lt::DocumentSymbolResponse> {
        let doc = self.docs.get(&p.text_document.uri)?;
        let ide = &doc.ide;
        let sym = |name: &str, detail: Option<String>, kind, full: Span, sel: Span, children| lt::DocumentSymbol {
            name: name.to_string(),
            detail,
            kind,
            tags: None,
            deprecated: None,
            range: doc.lines.range(full),
            selection_range: doc.lines.range(sel),
            children,
        };
        let mut out: Vec<(u32, lt::DocumentSymbol)> = Vec::new();
        for s in &ide.structs {
            let mut children: Vec<lt::DocumentSymbol> = s
                .fields
                .iter()
                .map(|f| sym(&f.name, Some(f.ty.clone()), lt::SymbolKind::FIELD, f.span, f.span, None))
                .collect();
            for &m in &s.methods {
                let f = &ide.functions[m];
                children.push(sym(&f.name, Some(f.signature.clone()), lt::SymbolKind::METHOD, f.full, f.name_span, None));
            }
            out.push((s.name_span.start, sym(&s.name, None, lt::SymbolKind::STRUCT, s.full, s.name_span, Some(children))));
        }
        for f in ide.functions.iter().filter(|f| f.owner.is_none()) {
            out.push((f.full.start, sym(&f.name, Some(f.signature.clone()), lt::SymbolKind::FUNCTION, f.full, f.name_span, None)));
        }
        out.sort_by_key(|(start, _)| *start);
        Some(lt::DocumentSymbolResponse::Nested(out.into_iter().map(|(_, s)| s).collect()))
    }

    // ------------------------------------------------------------ inlay hints

    fn inlay_hints(&self, p: lt::InlayHintParams) -> Option<Vec<lt::InlayHint>> {
        let doc = self.docs.get(&p.text_document.uri)?;
        let (start, end) = (doc.lines.offset(p.range.start), doc.lines.offset(p.range.end));
        let hints = doc
            .ide
            .inlay_hints
            .iter()
            .filter(|(o, _)| (start..=end).contains(&(*o as usize)))
            .map(|(o, label)| lt::InlayHint {
                position: doc.lines.position(*o as usize),
                label: lt::InlayHintLabel::String(label.clone()),
                kind: Some(lt::InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: None,
                data: None,
            })
            .collect();
        Some(hints)
    }
}

// ---------------------------------------------------------------- helpers

fn json<T: serde::Serialize>(v: T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

fn markdown(s: &str) -> lt::Documentation {
    lt::Documentation::MarkupContent(lt::MarkupContent { kind: lt::MarkupKind::Markdown, value: s.to_string() })
}

fn contains(span: Span, off: usize) -> bool {
    let (s, e) = (span.start as usize, span.end as usize);
    s <= off && (off < e || (s == e && off == s))
}

fn is_ident(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Rough check whether the cursor sits inside a `#` comment or a string literal.
fn in_comment_or_string(text: &str, off: usize) -> bool {
    let line_start = text[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let mut in_str = false;
    let mut escaped = false;
    for &b in &text.as_bytes()[line_start..off] {
        match b {
            _ if escaped => escaped = false,
            b'\\' if in_str => escaped = true,
            b'"' => in_str = !in_str,
            b'#' if !in_str => return true,
            _ => {}
        }
    }
    in_str
}

/// Walk back from a `.` over a postfix chain like `self.items[i].pos`.
fn receiver_start(text: &str, dot: usize) -> usize {
    let b = text.as_bytes();
    let mut i = dot;
    loop {
        // identifier (or number) segment
        let seg_end = i;
        while i > 0 && is_ident(b[i - 1]) {
            i -= 1;
        }
        // a balanced (...) or [...] group directly before
        if i == seg_end && i > 0 && matches!(b[i - 1], b')' | b']') {
            let mut depth = 0i32;
            while i > 0 {
                i -= 1;
                match b[i] {
                    b')' | b']' => depth += 1,
                    b'(' | b'[' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    b'\n' => return seg_end,
                    _ => {}
                }
            }
            continue;
        }
        if i > 0 && b[i - 1] == b'.' {
            i -= 1;
            continue;
        }
        return i;
    }
}

/// Locals visible at `off`: declared earlier in the same function, and not
/// inside a block that was already closed (judged by indentation).
fn visible_locals(doc: &Document, off: usize) -> Vec<&LocalSymbol> {
    let ide = &doc.ide;
    let mut starts: Vec<u32> = ide.functions.iter().map(|f| f.full.start).collect();
    starts.sort();
    let Some(&fn_start) = starts.iter().rfind(|s| (**s as usize) <= off) else { return Vec::new() };
    let fn_end = starts.iter().find(|s| **s > fn_start).map(|s| *s as usize).unwrap_or(doc.text.len());
    if off > fn_end {
        return Vec::new();
    }
    let indent_at = |line: usize| -> Option<usize> {
        let (s, e) = doc.lines.line_bounds(line, &doc.text);
        let l = &doc.text[s..e];
        let t = l.trim_start_matches(' ');
        if t.trim().is_empty() || t.starts_with('#') { None } else { Some(l.len() - t.len()) }
    };
    let cursor_line = doc.lines.position(off).line as usize;
    let mut out: Vec<&LocalSymbol> = Vec::new();
    for l in &ide.locals {
        let d = l.decl.start as usize;
        if d < fn_start as usize || d >= off {
            continue;
        }
        let decl_line = doc.lines.position(d).line as usize;
        let Some(decl_indent) = indent_at(decl_line) else { continue };
        let closed = (decl_line + 1..cursor_line).any(|ln| indent_at(ln).is_some_and(|i| i < decl_indent));
        let cursor_indent = {
            let (s, _) = doc.lines.line_bounds(cursor_line, &doc.text);
            doc.text[s..off].len() - doc.text[s..off].trim_start_matches(' ').len()
        };
        let closed_here = cursor_line > decl_line && cursor_indent < decl_indent;
        if closed || closed_here {
            continue;
        }
        out.retain(|o| o.name != l.name); // later declarations shadow earlier ones
        out.push(l);
    }
    out
}

fn to_lsp_diagnostic(d: &IsmaDiagnostic, lines: &LineMap, uri: &lt::Uri) -> lt::Diagnostic {
    let mut message = d.message.clone();
    for n in &d.notes {
        message.push_str(&format!("\nnote: {n}"));
    }
    if let Some(h) = &d.help {
        message.push_str(&format!("\nhelp: {h}"));
    }
    let related: Vec<lt::DiagnosticRelatedInformation> = d
        .labels
        .iter()
        .map(|(span, msg)| lt::DiagnosticRelatedInformation {
            location: lt::Location { uri: uri.clone(), range: lines.range(*span) },
            message: msg.clone(),
        })
        .collect();
    let fades = d.severity == Severity::Warning && (d.message.starts_with("unused ") || d.message == "unreachable code");
    lt::Diagnostic {
        range: lines.range(d.span),
        severity: Some(match d.severity {
            Severity::Error => lt::DiagnosticSeverity::ERROR,
            Severity::Warning => lt::DiagnosticSeverity::WARNING,
        }),
        source: Some("isma".into()),
        message,
        related_information: if related.is_empty() { None } else { Some(related) },
        tags: if fades { Some(vec![lt::DiagnosticTag::UNNECESSARY]) } else { None },
        ..Default::default()
    }
}

/// Byte offsets <-> LSP positions (lines + UTF-16 columns).
pub struct LineMap {
    starts: Vec<usize>,
    text: String,
}

impl LineMap {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        LineMap { starts, text: text.to_string() }
    }

    fn line_bounds(&self, line: usize, text: &str) -> (usize, usize) {
        let s = self.starts.get(line).copied().unwrap_or(text.len());
        let e = self.starts.get(line + 1).map(|n| n - 1).unwrap_or(text.len());
        (s, e.max(s))
    }

    pub fn position(&self, off: usize) -> lt::Position {
        let off = off.min(self.text.len());
        let line = match self.starts.binary_search(&off) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let start = self.starts[line];
        let col: usize = self.text[start..floor_char(&self.text, off)].chars().map(char::len_utf16).sum();
        lt::Position { line: line as u32, character: col as u32 }
    }

    pub fn offset(&self, pos: lt::Position) -> usize {
        let Some(&start) = self.starts.get(pos.line as usize) else { return self.text.len() };
        let mut units = 0;
        for (i, c) in self.text[start..].char_indices() {
            if units >= pos.character as usize || c == '\n' {
                return start + i;
            }
            units += c.len_utf16();
        }
        self.text.len()
    }

    pub fn range(&self, span: Span) -> lt::Range {
        let (s, mut e) = (span.start as usize, span.end as usize);
        if e <= s {
            // zero-width span: underline one character so it's visible
            e = self.text[s.min(self.text.len())..].chars().next().filter(|c| *c != '\n').map(|c| s + c.len_utf8()).unwrap_or(s);
        }
        lt::Range { start: self.position(s), end: self.position(e) }
    }
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        let a = crate::analyze(text);
        Document { text: text.to_string(), lines: LineMap::new(text), ide: a.ide }
    }

    fn server_with(text: &str) -> (Server, lt::Uri) {
        let uri: lt::Uri = "file:///test.isma".parse().unwrap();
        let mut s = Server { docs: HashMap::new() };
        s.docs.insert(uri.clone(), doc(text));
        (s, uri)
    }

    fn at(text: &str, needle: &str, delta: usize) -> lt::Position {
        let off = text.find(needle).unwrap() + delta;
        LineMap::new(text).position(off)
    }

    fn tdp(uri: &lt::Uri, pos: lt::Position) -> lt::TextDocumentPositionParams {
        lt::TextDocumentPositionParams { text_document: lt::TextDocumentIdentifier { uri: uri.clone() }, position: pos }
    }

    const SRC: &str = "struct Vec2:\n    var x: Float\n    y: Float\n\n    fn len(self) -> Float:\n        return self.x\n\nfn main():\n    val v = Vec2(1.0, 2.0)\n    val n = v.len()\n    print(n)\n    v.\n";

    #[test]
    fn hover_shows_inferred_type() {
        let (s, uri) = server_with(SRC);
        let h = s
            .hover(lt::HoverParams { text_document_position_params: tdp(&uri, at(SRC, "print(n)", 6)), work_done_progress_params: Default::default() })
            .unwrap();
        let lt::HoverContents::Markup(m) = h.contents else { panic!() };
        assert!(m.value.contains("val n: Float"), "{}", m.value);
    }

    #[test]
    fn definition_of_method() {
        let (s, uri) = server_with(SRC);
        let r = s
            .definition(lt::GotoDefinitionParams {
                text_document_position_params: tdp(&uri, at(SRC, "v.len()", 2)),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap();
        let lt::GotoDefinitionResponse::Scalar(loc) = r else { panic!() };
        assert_eq!(loc.range.start, at(SRC, "len(self)", 0));
    }

    #[test]
    fn member_completion_after_dot() {
        let (s, uri) = server_with(SRC);
        let pos = at(SRC, "    v.\n", 6);
        let r = s
            .completion(lt::CompletionParams {
                text_document_position: tdp(&uri, pos),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            })
            .unwrap();
        let lt::CompletionResponse::Array(items) = r else { panic!() };
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["x", "y", "len"]);
    }

    #[test]
    fn member_completion_in_unfinished_if() {
        let src = "fn main():\n    val xs = [1, 2]\n    if xs.\n";
        let (s, uri) = server_with(src);
        let r = s
            .completion(lt::CompletionParams {
                text_document_position: tdp(&uri, at(src, "xs.\n", 3)),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            })
            .unwrap();
        let lt::CompletionResponse::Array(items) = r else { panic!() };
        assert!(items.iter().any(|i| i.label == "push"));
    }

    #[test]
    fn scope_completion_respects_blocks() {
        let src = "fn main():\n    val a = 1\n    if a > 0:\n        val inner = 2\n        print(inner)\n    pri\n";
        let (s, uri) = server_with(src);
        let items = s.scope_completion(&s.docs[&uri], src.find("pri\n").unwrap());
        let names: Vec<_> = items.iter().filter(|i| i.kind == Some(lt::CompletionItemKind::VARIABLE)).map(|i| i.label.as_str()).collect();
        assert_eq!(names, ["a"]);
    }

    #[test]
    fn positions_roundtrip_utf16() {
        let text = "val s = \"ü😀\"\nx";
        let m = LineMap::new(text);
        let off = text.find('x').unwrap();
        assert_eq!(m.position(off), lt::Position { line: 1, character: 0 });
        let end_of_str = text.find("\"\n").unwrap();
        let p = m.position(end_of_str);
        assert_eq!(p.character, 8 + 1 + 1 + 2); // `val s = "` + ü + 😀 (2 UTF-16 units)
        assert_eq!(m.offset(p), end_of_str);
    }
}

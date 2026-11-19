//! `tarn lsp`: a Language Server Protocol server over stdio.
//!
//! It reuses the real compiler front end, so editor squiggles are exactly
//! the errors `tarn build` would report. Features: diagnostics (errors +
//! lints), hover types, go-to-definition, completion (including `.` member
//! completion), document outline and inlay type hints.
//!
//! Each document is analyzed as the entry file of a program: its imports are
//! read from the open documents first (so unsaved edits count), then from
//! disk. Changing a document re-checks the open documents that import it.

use crate::check::Analysis;
use crate::diagnostics::{Diagnostic as TarnDiagnostic, Severity};
use crate::ide::*;
use crate::project::FileKind;
use crate::span::Span;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types as lt;
use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};

type BoxError = Box<dyn Error + Send + Sync>;

pub fn run() -> Result<(), BoxError> {
    let (conn, io_threads) = Connection::stdio();
    let caps = lt::ServerCapabilities {
        text_document_sync: Some(lt::TextDocumentSyncCapability::Options(lt::TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(lt::TextDocumentSyncKind::FULL),
            // Saving a file can fix (or break) the files that import it.
            save: Some(lt::TextDocumentSyncSaveOptions::Supported(true)),
            ..Default::default()
        })),
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
                for out in server.handle_notification(n) {
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
    /// The file on disk (imports are found relative to it).
    path: Option<PathBuf>,
    /// The user's other files in the program, for go-to-definition into them.
    others: Vec<OtherFile>,
}

/// An imported file of the user's, as seen from a document that imports it.
struct OtherFile {
    path: PathBuf,
    uri: lt::Uri,
    base: u32,
    len: u32,
    lines: LineMap,
}

struct Server {
    docs: HashMap<lt::Uri, Document>,
}

/// `file:///a/b%20c.tr` -> `/a/b c.tr`
fn uri_to_path(uri: &lt::Uri) -> Option<PathBuf> {
    let rest = uri.as_str().strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = rest.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    let path = PathBuf::from(String::from_utf8(out).ok()?);
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

fn path_to_uri(path: &Path) -> Option<lt::Uri> {
    let mut s = String::from("file://");
    for b in path.to_str()?.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s.parse().ok()
}

fn params<P: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<P, String> {
    serde_json::from_value(v).map_err(|e| e.to_string())
}

impl Server {
    fn handle_notification(&mut self, n: Notification) -> Vec<Notification> {
        use lt::notification::*;
        match n.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let Ok(p) = params::<lt::DidOpenTextDocumentParams>(n.params) else { return Vec::new() };
                self.update(p.text_document.uri, p.text_document.text, Some(p.text_document.version))
            }
            DidChangeTextDocument::METHOD => {
                let Ok(p) = params::<lt::DidChangeTextDocumentParams>(n.params) else { return Vec::new() };
                let Some(change) = p.content_changes.into_iter().last() else { return Vec::new() };
                self.update(p.text_document.uri, change.text, Some(p.text_document.version))
            }
            DidSaveTextDocument::METHOD => {
                // A file that is now on disk may be what another document was missing.
                let Ok(p) = params::<lt::DidSaveTextDocumentParams>(n.params) else { return Vec::new() };
                let others: Vec<lt::Uri> = self.docs.keys().filter(|u| **u != p.text_document.uri).cloned().collect();
                others.into_iter().filter_map(|u| self.reanalyze(u)).collect()
            }
            DidChangeWatchedFiles::METHOD => {
                // `.tr` files created, deleted or changed outside the editor.
                let Ok(p) = params::<lt::DidChangeWatchedFilesParams>(n.params) else { return Vec::new() };
                let open: Vec<&lt::Uri> = p.changes.iter().map(|c| &c.uri).filter(|u| self.docs.contains_key(*u)).collect();
                if p.changes.len() == open.len() {
                    return Vec::new(); // only open documents, which the editor keeps us up to date on
                }
                let all: Vec<lt::Uri> = self.docs.keys().cloned().collect();
                all.into_iter().filter_map(|u| self.reanalyze(u)).collect()
            }
            DidCloseTextDocument::METHOD => {
                let Ok(p) = params::<lt::DidCloseTextDocumentParams>(n.params) else { return Vec::new() };
                let closed = self.docs.remove(&p.text_document.uri);
                let clear = lt::PublishDiagnosticsParams { uri: p.text_document.uri, diagnostics: Vec::new(), version: None };
                let mut out = vec![lsp_server::Notification::new(PublishDiagnostics::METHOD.into(), clear)];
                // Files that import it now read it from disk again (unsaved edits are gone).
                if let Some(path) = closed.and_then(|d| d.path) {
                    out.extend(self.dependents(&path).into_iter().filter_map(|u| self.reanalyze(u)));
                }
                out
            }
            _ => Vec::new(),
        }
    }

    /// Open documents that import the file at `path`.
    fn dependents(&self, path: &Path) -> Vec<lt::Uri> {
        self.docs.iter().filter(|(_, d)| d.others.iter().any(|o| o.path == path)).map(|(u, _)| u.clone()).collect()
    }

    /// Re-analyze a changed document, and the open documents that import it.
    fn update(&mut self, uri: lt::Uri, text: String, version: Option<i32>) -> Vec<Notification> {
        let mut out = vec![self.publish(uri.clone(), text, version)];
        if let Some(path) = self.docs.get(&uri).and_then(|d| d.path.clone()) {
            out.extend(self.dependents(&path).into_iter().filter(|u| *u != uri).filter_map(|u| self.reanalyze(u)));
        }
        out
    }

    fn reanalyze(&mut self, uri: lt::Uri) -> Option<Notification> {
        let text = self.docs.get(&uri)?.text.clone();
        Some(self.publish(uri, text, None))
    }

    /// Run the compiler front end on `text`, the contents of the file at
    /// `path`. Imported files are read from the open documents first (so
    /// unsaved edits count), then from disk. Survives any internal panic.
    fn analyze(&self, path: Option<&Path>, text: &str) -> Option<(Analysis, Vec<OtherFile>)> {
        let overlay: HashMap<&Path, &str> = self.docs.values().filter_map(|d| Some((d.path.as_deref()?, d.text.as_str()))).collect();
        let read = |p: &Path| match overlay.get(p) {
            Some(t) => Ok(t.to_string()),
            None => std::fs::read_to_string(p),
        };
        let run = || {
            let project = match path {
                Some(p) => {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    crate::project::load(Some(p), &name, text, &read)
                }
                None => crate::load_source(text),
            };
            let a = crate::analyze_project(&project);
            let others = project.files[1..]
                .iter()
                .filter(|f| f.kind == FileKind::User)
                .filter_map(|f| {
                    let path = f.path.clone()?;
                    Some(OtherFile { uri: path_to_uri(&path)?, path, base: f.base, len: f.text.len() as u32, lines: LineMap::new(&f.text) })
                })
                .collect();
            (a, others)
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).ok()
    }

    /// Re-analyze a document and build its `publishDiagnostics` notification.
    fn publish(&mut self, uri: lt::Uri, text: String, version: Option<i32>) -> Notification {
        let lines = LineMap::new(&text);
        let path = uri_to_path(&uri);
        let (diagnostics, ide, others) = match self.analyze(path.as_deref(), &text) {
            Some((a, others)) => {
                let ds = a.diagnostics.iter().map(|d| to_lsp_diagnostic(d, &lines, &uri)).collect();
                (ds, a.ide, others)
            }
            None => {
                let d = lt::Diagnostic {
                    range: lines.range(Span::default()),
                    severity: Some(lt::DiagnosticSeverity::WARNING),
                    source: Some("tarn".into()),
                    message: "internal compiler error while analyzing this file (please report it)".into(),
                    ..Default::default()
                };
                (vec![d], IdeInfo::default(), Vec::new())
            }
        };
        self.docs.insert(uri.clone(), Document { text, lines, ide, path, others });
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
        let mut value = format!("```tarn\n{}\n```", h.code);
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
        if def.start as usize <= doc.text.len() {
            return Some(lt::GotoDefinitionResponse::Scalar(lt::Location { uri: pos.text_document.uri, range: doc.lines.range(*def) }));
        }
        // In another of the user's files.
        let f = doc.others.iter().find(|f| f.base <= def.start && def.start <= f.base + f.len)?;
        let local = Span { start: def.start - f.base, end: def.end - f.base };
        Some(lt::GotoDefinitionResponse::Scalar(lt::Location { uri: f.uri.clone(), range: f.lines.range(local) }))
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
        // `import ge|`, `from shapes.ci|`, `from geometry import Po|`
        let line_start = doc.text[..word_start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if let Some(items) = self.import_completion(doc, &doc.text[line_start..word_start]) {
            return Some(lt::CompletionResponse::Array(items));
        }
        let items = if word_start > 0 && bytes[word_start - 1] == b'.' {
            self.member_completion(doc, word_start - 1, off)?
        } else {
            self.scope_completion(doc, off)
        };
        Some(lt::CompletionResponse::Array(items))
    }

    /// Completion after `receiver.` (or `receiver?.`): figure out the receiver's
    /// type by analyzing a patched copy of the file in which the dangling `.` is gone.
    fn member_completion(&self, doc: &Document, dot: usize, off: usize) -> Option<Vec<lt::CompletionItem>> {
        let text = &doc.text;
        let safe = dot > 0 && text.as_bytes()[dot - 1] == b'?';
        // From here on `dot` is where the receiver ends (before `?.` or `.`).
        let op_end = dot + 1;
        let dot = if safe { dot - 1 } else { dot };
        // `time.` -> the module's functions and types
        let recv_start = receiver_start(text, dot);
        let receiver = &text[recv_start..dot];
        if let Some(ns) = doc.ide.imported_module(receiver) {
            return Some(doc.ide.module_members(ns).into_iter().map(member_item).collect());
        }
        // `Shape.`, `geo.Shape.` -> the enum's variants
        let named = match receiver.split_once('.') {
            None => doc.ide.structs.iter().position(|s| s.ns == 0 && !s.instance && s.name == receiver).or_else(|| {
                doc.ide.imported.iter().find_map(|(b, k)| match k {
                    ImportedSymbol::Struct(sid) if b == receiver => Some(*sid),
                    _ => None,
                })
            }),
            Some((module, name)) => doc.ide.imported_module(module).and_then(|ns| {
                doc.ide.structs.iter().position(|s| s.ns == ns && !s.instance && s.name.rsplit('.').next() == Some(name))
            }),
        };
        // `Point.` -> its static methods (and an enum's variants)
        if let Some(sid) = named {
            let s = &doc.ide.structs[sid];
            let mut items: Vec<lt::CompletionItem> = s.variants.iter().flatten().map(variant_item).collect();
            items.extend(doc.ide.static_members(sid).into_iter().map(member_item));
            return Some(items);
        }
        let mut word_end = off;
        while word_end < text.len() && is_ident(text.as_bytes()[word_end]) {
            word_end += 1;
        }
        // Attempt 1: just drop `.partial` and analyze the file.
        let patched = format!("{}{}", &text[..dot], &text[word_end..]);
        let mut found = self.analyze(doc.path.as_deref(), &patched).map(|(a, _)| a).and_then(|a| {
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
                found = self.analyze(doc.path.as_deref(), &patched).map(|(a, _)| a).and_then(|a| {
                    a.ide
                        .expr_types
                        .iter()
                        .find(|(s, t)| s.start as usize == start && s.end as usize == start + receiver.len() && !t.is_error())
                        .map(|(_, t)| (t.clone(), a.ide.clone()))
                });
            }
        }
        let (ty, ide) = found?;
        let mut items: Vec<lt::CompletionItem> = ide.members(&ty).into_iter().map(member_item).collect();
        if ty.is_nullable() && !safe {
            // `name.` on a value that might be null: completing turns the `.` into `?.`.
            let range = doc.lines.range(Span { start: dot as u32, end: op_end as u32 });
            for item in &mut items {
                item.additional_text_edits = Some(vec![lt::TextEdit { range, new_text: "?.".into() }]);
                item.detail = item.detail.take().map(|d| format!("{d}  (via ?.)"));
            }
        }
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
        for (m, _, ns) in &doc.ide.imports {
            let module = doc.ide.modules.iter().find(|x| x.ns == *ns);
            let d = module.and_then(|x| x.doc.clone());
            let detail = match module {
                Some(x) if x.name != *m => format!("import {} as {m}", x.name),
                _ => format!("import {m}"),
            };
            add(m, lt::CompletionItemKind::MODULE, Some(detail), d, "1");
        }
        for (name, what) in &doc.ide.imported {
            match *what {
                ImportedSymbol::Func(fid) => {
                    let f = &doc.ide.functions[fid];
                    add(name, lt::CompletionItemKind::FUNCTION, Some(f.signature.clone()), f.doc.clone(), "1");
                }
                ImportedSymbol::Struct(sid) => {
                    let st = &doc.ide.structs[sid];
                    let (kind, kw) = if st.iface {
                        (lt::CompletionItemKind::INTERFACE, "interface")
                    } else if st.variants.is_some() {
                        (lt::CompletionItemKind::ENUM, "enum")
                    } else {
                        (lt::CompletionItemKind::STRUCT, "struct")
                    };
                    add(name, kind, Some(format!("{kw} {}", st.name)), st.doc.clone(), "1");
                }
                ImportedSymbol::Const(cid) => {
                    let c = &doc.ide.consts[cid];
                    add(name, lt::CompletionItemKind::CONSTANT, Some(c.signature.clone()), c.doc.clone(), "1");
                }
            }
        }
        for c in doc.ide.consts.iter().filter(|c| c.ns == 0) {
            add(&c.name, lt::CompletionItemKind::CONSTANT, Some(c.signature.clone()), c.doc.clone(), "1");
        }
        for f in doc.ide.functions.iter().filter(|f| f.owner.is_none() && f.ns == 0 && !f.instance) {
            add(&f.name, lt::CompletionItemKind::FUNCTION, Some(f.signature.clone()), f.doc.clone(), "1");
        }
        for s in doc.ide.structs.iter().filter(|s| s.ns == 0 && !s.instance) {
            if s.iface {
                let methods: Vec<&str> = s.methods.iter().map(|&m| doc.ide.functions[m].name.as_str()).collect();
                add(&s.name, lt::CompletionItemKind::INTERFACE, Some(format!("interface {}: {}", s.name, methods.join(", "))), s.doc.clone(), "1");
                continue;
            }
            if let Some(variants) = &s.variants {
                let names: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
                add(&s.name, lt::CompletionItemKind::ENUM, Some(format!("enum {}: {}", s.name, names.join(" | "))), s.doc.clone(), "1");
                continue;
            }
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
        // The prelude, in every file: `Result` (with `Ok` and `Err`) and `Unit`.
        if let Some(p) = doc.ide.modules.iter().find(|m| m.name == crate::stdlib::PRELUDE).map(|m| m.ns) {
            for s in doc.ide.structs.iter().filter(|s| s.ns == p && !s.instance) {
                let shown = if s.type_params.is_empty() { s.name.clone() } else { format!("{}[{}]", s.name, s.type_params.join(", ")) };
                add(&s.name, lt::CompletionItemKind::ENUM, Some(format!("enum {shown}")), s.doc.clone(), "3");
                if s.name == "Result" {
                    for v in s.variants.iter().flatten() {
                        add(&v.name, lt::CompletionItemKind::ENUM_MEMBER, Some(v.signature.clone()), v.doc.clone(), "3");
                    }
                }
            }
        }
        for k in KEYWORDS {
            add(k, lt::CompletionItemKind::KEYWORD, None, None, "4");
        }
        items
    }

    /// Completion inside an `import` or `from` line (`before`: the line up to the word being typed).
    fn import_completion(&self, doc: &Document, before: &str) -> Option<Vec<lt::CompletionItem>> {
        let line = before.trim_start();
        let dir = doc.path.as_deref().and_then(Path::parent);
        // `from geometry import Po|`, `from geometry import Point, ar|`
        if let Some(rest) = line.strip_prefix("from ")
            && let Some((module, _)) = rest.split_once(" import ")
        {
            return Some(self.importable_names(doc, dir, module.trim()));
        }
        // `import shapes.ci|`, `from ..ge|`
        let (path, from) = match (line.strip_prefix("import "), line.strip_prefix("from ")) {
            (Some(p), _) => (p, false),
            (None, Some(p)) => (p, true),
            _ => return None,
        };
        if path.contains(' ') || (!from && path.starts_with('.')) {
            return None;
        }
        let level = path.chars().take_while(|c| *c == '.').count();
        let segs: Vec<&str> = path[level..].split('.').collect();
        let folders = &segs[..segs.len() - 1];
        let mut items = Vec::new();
        if level == 0 && folders.is_empty() {
            for m in crate::stdlib::importable() {
                let doc = crate::analyze(&format!("import {}\nfn main():\n    pass\n", m.name)).ide.modules.iter().find(|x| x.name == m.name).and_then(|m| m.doc.clone());
                items.push(module_item(m.name, format!("import {}", m.name), doc, lt::CompletionItemKind::MODULE));
            }
        }
        let Some(mut dir) = dir.map(Path::to_path_buf) else { return Some(items) };
        for _ in 1..level.max(1) {
            dir.pop();
        }
        for f in folders {
            dir.push(f);
        }
        let own = doc.path.as_deref();
        for name in crate::project::module_names(&dir) {
            let file = dir.join(format!("{name}.tr"));
            if Some(file.as_path()) == own || items.iter().any(|i| i.label == name) {
                continue;
            }
            let docs = std::fs::read_to_string(&file).ok().and_then(|t| leading_doc(&t));
            items.push(module_item(&name, format!("{name}.tr"), docs, lt::CompletionItemKind::MODULE));
        }
        for sub in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = sub.file_name().to_string_lossy().into_owned();
            let is_package = sub.path().is_dir() && !name.starts_with('.') && !crate::project::module_names(&sub.path()).is_empty();
            if is_package && !items.iter().any(|i| i.label == name) {
                items.push(module_item(&name, format!("{name}/"), None, lt::CompletionItemKind::FOLDER));
            }
        }
        Some(items)
    }

    /// The public names of the module at `module` (as written after `from`),
    /// plus its folder's modules.
    fn importable_names(&self, doc: &Document, dir: Option<&Path>, module: &str) -> Vec<lt::CompletionItem> {
        // Analyze a probe that imports the module the same way, from the same folder.
        let probe_path = dir.map(|d| d.join("__tarn_probe__.tr"));
        let level = module.chars().take_while(|c| *c == '.').count();
        let (head, last) = match module[level..].rsplit_once('.') {
            Some((h, l)) => (format!("{}{h}", &module[..level]), l.to_string()),
            None if level > 0 => (module[..level].to_string(), module[level..].to_string()),
            None => (String::new(), module.to_string()),
        };
        let probe = if head.is_empty() {
            format!("import {last} as __m\nfn main():\n    pass\n")
        } else {
            format!("from {head} import {last} as __m\nfn main():\n    pass\n")
        };
        let mut items = Vec::new();
        if let Some((a, _)) = self.analyze(probe_path.as_deref(), &probe)
            && let Some(ns) = a.ide.imported_module("__m")
        {
            for m in a.ide.module_members(ns) {
                let kind = match m.kind {
                    MemberKind::Function => lt::CompletionItemKind::FUNCTION,
                    MemberKind::Constant => lt::CompletionItemKind::CONSTANT,
                    _ if m.signature.starts_with("interface ") => lt::CompletionItemKind::INTERFACE,
                    _ => lt::CompletionItemKind::STRUCT,
                };
                items.push(lt::CompletionItem {
                    label: m.name.clone(),
                    kind: Some(kind),
                    detail: Some(m.signature.clone()),
                    documentation: if m.doc.is_empty() { None } else { Some(markdown(&m.doc)) },
                    ..Default::default()
                });
            }
        }
        // `from shapes import ci|`: the files in the folder `shapes/`.
        if let Some(mut folder) = dir.map(Path::to_path_buf) {
            for _ in 1..level.max(1) {
                folder.pop();
            }
            for seg in module[level..].split('.').filter(|s| !s.is_empty()) {
                folder.push(seg);
            }
            let own = doc.path.as_deref();
            for name in crate::project::module_names(&folder) {
                let file = folder.join(format!("{name}.tr"));
                if Some(file.as_path()) != own && !items.iter().any(|i| i.label == name) {
                    let docs = std::fs::read_to_string(&file).ok().and_then(|t| leading_doc(&t));
                    items.push(module_item(&name, format!("{name}.tr"), docs, lt::CompletionItemKind::MODULE));
                }
            }
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
        for s in ide.structs.iter().filter(|s| s.ns == 0 && !s.instance) {
            let mut children: Vec<lt::DocumentSymbol> = s
                .fields
                .iter()
                .map(|f| sym(&f.name, Some(f.ty.clone()), lt::SymbolKind::FIELD, f.span, f.span, None))
                .collect();
            for v in s.variants.iter().flatten() {
                children.push(sym(&v.name, Some(v.signature.clone()), lt::SymbolKind::ENUM_MEMBER, v.span, v.span, None));
            }
            for &m in &s.methods {
                let f = &ide.functions[m];
                children.push(sym(&f.name, Some(f.signature.clone()), lt::SymbolKind::METHOD, f.full, f.name_span, None));
            }
            let kind = if s.iface {
                lt::SymbolKind::INTERFACE
            } else if s.variants.is_some() {
                lt::SymbolKind::ENUM
            } else {
                lt::SymbolKind::STRUCT
            };
            out.push((s.name_span.start, sym(&s.name, None, kind, s.full, s.name_span, Some(children))));
        }
        for f in ide.functions.iter().filter(|f| f.owner.is_none() && f.ns == 0 && !f.instance) {
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

fn member_item(m: Member) -> lt::CompletionItem {
    let (insert, format) = match (m.kind, m.takes_args) {
        (MemberKind::Field | MemberKind::Struct | MemberKind::Constant, _) => (m.name.clone(), lt::InsertTextFormat::PLAIN_TEXT),
        (_, false) => (format!("{}()", m.name), lt::InsertTextFormat::PLAIN_TEXT),
        (_, true) => (format!("{}($1)", m.name), lt::InsertTextFormat::SNIPPET),
    };
    let kind = match m.kind {
        MemberKind::Field => lt::CompletionItemKind::FIELD,
        MemberKind::Method => lt::CompletionItemKind::METHOD,
        MemberKind::Function => lt::CompletionItemKind::FUNCTION,
        MemberKind::Struct => lt::CompletionItemKind::STRUCT,
        MemberKind::Constant => lt::CompletionItemKind::CONSTANT,
    };
    lt::CompletionItem {
        label: m.name.clone(),
        kind: Some(kind),
        detail: Some(m.signature.clone()),
        documentation: if m.doc.is_empty() { None } else { Some(markdown(&m.doc)) },
        insert_text: Some(insert),
        insert_text_format: Some(format),
        ..Default::default()
    }
}

fn module_item(name: &str, detail: String, doc: Option<String>, kind: lt::CompletionItemKind) -> lt::CompletionItem {
    lt::CompletionItem {
        label: name.to_string(),
        kind: Some(kind),
        detail: Some(detail),
        documentation: doc.map(|d| markdown(&d)),
        ..Default::default()
    }
}

/// A module's documentation: the comment block at the top of its file.
fn leading_doc(text: &str) -> Option<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .take_while(|l| l.starts_with('#'))
        .filter(|l| !l.starts_with("#!"))
        .map(|l| l.trim_start_matches('#').trim())
        .collect();
    let doc = lines.join("\n").trim().to_string();
    (!doc.is_empty()).then_some(doc)
}

fn variant_item(v: &VariantSymbol) -> lt::CompletionItem {
    let has_fields = v.signature.ends_with(')');
    lt::CompletionItem {
        label: v.name.clone(),
        kind: Some(lt::CompletionItemKind::ENUM_MEMBER),
        detail: Some(v.signature.clone()),
        documentation: v.doc.as_ref().map(|d| markdown(d)),
        insert_text: Some(if has_fields { format!("{}($1)", v.name) } else { v.name.clone() }),
        insert_text_format: Some(if has_fields { lt::InsertTextFormat::SNIPPET } else { lt::InsertTextFormat::PLAIN_TEXT }),
        ..Default::default()
    }
}

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
            // `a?.b`, `a!!.b`
            if i > 0 && b[i - 1] == b'?' {
                i -= 1;
            } else {
                while i > 0 && b[i - 1] == b'!' {
                    i -= 1;
                }
            }
            continue;
        }
        return i;
    }
}

/// Locals visible at `off`: declared earlier in the same function, and not
/// inside a block that was already closed (judged by indentation).
fn visible_locals(doc: &Document, off: usize) -> Vec<&LocalSymbol> {
    let ide = &doc.ide;
    let mut starts: Vec<u32> = ide.functions.iter().filter(|f| f.ns == 0 && !f.instance).map(|f| f.full.start).collect();
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

fn to_lsp_diagnostic(d: &TarnDiagnostic, lines: &LineMap, uri: &lt::Uri) -> lt::Diagnostic {
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
        source: Some("tarn".into()),
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
        Document { text: text.to_string(), lines: LineMap::new(text), ide: a.ide, path: None, others: Vec::new() }
    }

    fn server_with(text: &str) -> (Server, lt::Uri) {
        let uri: lt::Uri = "file:///test.tr".parse().unwrap();
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
    fn lambdas_hover_hints_and_captures() {
        let src = "fn main():\n    val base = 10\n    val xs = [1, 2, 3]\n    val ys = xs.map(fn(x) -> x + base)\n    val f = fn(s: String) -> s.len() + base\n    print(ys.len() + f(\"ab\"))\n";
        let (s, uri) = server_with(src);
        let hover = |needle: &str, delta: usize| {
            let h = s.hover(lt::HoverParams { text_document_position_params: tdp(&uri, at(src, needle, delta)), work_done_progress_params: Default::default() }).unwrap();
            let lt::HoverContents::Markup(m) = h.contents else { panic!() };
            m.value
        };
        // An inlined lambda's parameter, typed from the list; a closure's type.
        assert!(hover("x + base", 0).contains("x: Int"), "{}", hover("x + base", 0));
        assert!(hover("fn(s: String)", 0).contains("fn(String) -> Int"), "{}", hover("fn(s: String)", 0));
        // A captured name leads to its declaration outside the lambda.
        let r = s
            .definition(lt::GotoDefinitionParams {
                text_document_position_params: tdp(&uri, at(src, "base\n    print", 0)),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap();
        let lt::GotoDefinitionResponse::Scalar(loc) = r else { panic!() };
        assert_eq!(loc.range.start, at(src, "base = 10", 0));
        // The inferred parameter type is shown inline.
        let full = lt::Range { start: lt::Position { line: 0, character: 0 }, end: lt::Position { line: 9, character: 0 } };
        let hints = s
            .inlay_hints(lt::InlayHintParams { text_document: lt::TextDocumentIdentifier { uri: uri.clone() }, range: full, work_done_progress_params: Default::default() })
            .unwrap();
        assert!(hints.iter().any(|h| h.position == at(src, "x) ->", 1) && matches!(&h.label, lt::InlayHintLabel::String(l) if l == ": Int")));
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

    const TIME_SRC: &str = "import time\n\nfn main():\n    val now = time.now()\n    print(now.to_iso())\n    time.\n";

    fn complete(s: &Server, uri: &lt::Uri, pos: lt::Position) -> Vec<lt::CompletionItem> {
        let r = s
            .completion(lt::CompletionParams {
                text_document_position: tdp(uri, pos),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            })
            .unwrap();
        let lt::CompletionResponse::Array(items) = r else { panic!() };
        items
    }

    const OPS_SRC: &str = "import time\n\nstruct V:\n    x: Float\n\n    fn +(self, other: V) -> V:\n        return V(self.x + other.x)\n\nfn main():\n    val a = V(1.0) + V(2.0)\n    val t = time.now()\n    print(time.now() - t)\n    a.\n";

    #[test]
    fn operators_hover_and_jump_to_their_function() {
        let (s, uri) = server_with(OPS_SRC);
        let hover = |needle: &str, delta: usize| {
            let h = s.hover(lt::HoverParams { text_document_position_params: tdp(&uri, at(OPS_SRC, needle, delta)), work_done_progress_params: Default::default() });
            let lt::HoverContents::Markup(m) = h.unwrap().contents else { panic!() };
            m.value
        };
        let shown = hover("+ V(2.0)", 0);
        assert!(shown.contains("fn +(self, other: V) -> V") && shown.contains("operator function of `V`"), "{shown}");
        let shown = hover("- t)", 0);
        assert!(shown.contains("fn -(self, other: time.DateTime) -> time.Duration"), "{shown}");
        let r = s
            .definition(lt::GotoDefinitionParams {
                text_document_position_params: tdp(&uri, at(OPS_SRC, "+ V(2.0)", 0)),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap();
        let lt::GotoDefinitionResponse::Scalar(loc) = r else { panic!() };
        assert_eq!(loc.range.start, at(OPS_SRC, "+(self", 0));
        // Operator functions are used through their operator, not offered after `.`.
        let items = complete(&s, &uri, at(OPS_SRC, "    a.\n", 6));
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["x"]);
    }

    const GEN_SRC: &str = "fn max[T](a: T, b: T) -> T:\n    if a > b:\n        return a\n    return b\n\nstruct Box[T]:\n    value: T\n\n    fn get(self) -> T:\n        return self.value\n\nfn main():\n    val m = max(1, 2)\n    val b = Box(m)\n    b.\n";

    #[test]
    fn generics_hover_complete_and_outline() {
        let (s, uri) = server_with(GEN_SRC);
        let hover = |needle: &str, delta: usize| {
            let h = s.hover(lt::HoverParams { text_document_position_params: tdp(&uri, at(GEN_SRC, needle, delta)), work_done_progress_params: Default::default() });
            let lt::HoverContents::Markup(m) = h.unwrap().contents else { panic!() };
            m.value
        };
        let shown = hover("max[T]", 0);
        assert!(shown.contains("fn max[T](a: T, b: T) -> T") && shown.contains("`T` can be any type that supports `<` and `>`"), "{shown}");
        let shown = hover("max(1, 2)", 0);
        assert!(shown.contains("fn max[T](a: T, b: T) -> T") && shown.contains("here `T` is `Int`"), "{shown}");
        let shown = hover("T, b: T", 0);
        assert!(shown.contains("type parameter of `max`"), "{shown}");
        // The members of a `Box[Int]` have its types.
        let items = complete(&s, &uri, at(GEN_SRC, "    b.\n", 6));
        let got: Vec<_> = items.iter().map(|i| (i.label.as_str(), i.detail.clone().unwrap_or_default())).collect();
        assert!(got.iter().any(|(l, d)| *l == "get" && d.contains("-> Int")), "{got:?}");
        assert!(got.iter().any(|(l, d)| *l == "value" && d.contains("Int")), "{got:?}");
        // The outline lists what's declared, not the copies made for `Int`.
        let a = crate::analyze(GEN_SRC);
        assert_eq!(a.ide.structs.iter().filter(|s| !s.instance && s.ns == 0).count(), 1);
        assert!(a.ide.structs.iter().any(|s| s.instance && s.name == "Box[Int]"));
    }

    const IFACE_SRC: &str = "# Things with an area.\ninterface Shape:\n    # How much room it takes.\n    fn area(self) -> Float\n\ninterface Solid(Shape):\n    fn volume(self) -> Float\n\nstruct Cube(Solid):\n    side: Float\n\n    fn area(self) -> Float:\n        return 6.0 * self.side * self.side\n\n    fn volume(self) -> Float:\n        return self.side * self.side * self.side\n\nfn total[T: Shape](xs: List[T]) -> Float:\n    var sum = 0.0\n    for x in xs:\n        sum += x.area()\n    return sum\n\nfn main():\n    val s: Solid = Cube(1.0)\n    print(s.volume() + total([Cube(2.0)]))\n    s.\n";

    #[test]
    fn results_bytes_hover_and_complete() {
        const SRC: &str = "fn parse(s: String) -> Result[Int, String]:\n    val n = s.to_int_or_null() ?: return Err(\"bad\")\n    return Ok(n)\n\nfn main():\n    val r = parse(\"1\")\n    val b = \"hi\".encode()\n    print(b.hex())\n    r.\n    b.\n    \n";
        let (s, uri) = server_with(SRC);
        let hover = |needle: &str, delta: usize| {
            let h = s.hover(lt::HoverParams { text_document_position_params: tdp(&uri, at(SRC, needle, delta)), work_done_progress_params: Default::default() });
            let lt::HoverContents::Markup(m) = h.unwrap().contents else { panic!() };
            m.value
        };
        // `Ok`/`Err` are the prelude's `Result` variants; `Bytes` methods have docs.
        let shown = hover("Ok(n)", 0);
        assert!(shown.contains("Ok(value: T)"), "{shown}");
        let shown = hover("hex()", 0);
        assert!(shown.contains("fn hex() -> String") && shown.contains("hex digits"), "{shown}");
        let shown = hover("encode()", 0);
        assert!(shown.contains("fn encode() -> Bytes"), "{shown}");
        // `r.` offers `Result`'s methods, `b.` those of `Bytes`.
        let labels = |pos| complete(&s, &uri, pos).iter().map(|i| i.label.clone()).collect::<Vec<_>>();
        let got = labels(at(SRC, "    r.\n", 6));
        assert!(got.iter().any(|l| l == "is_ok") && got.iter().any(|l| l == "map_err"), "{got:?}");
        let got = labels(at(SRC, "    b.\n", 6));
        assert!(got.iter().any(|l| l == "decode") && got.iter().any(|l| l == "slice"), "{got:?}");
        // Anywhere, `Result`, `Ok`, `Err`, `Unit` and `Bytes` are offered.
        let got = labels(at(SRC, "    \n", 4));
        for name in ["Result", "Ok", "Err", "Unit", "Bytes"] {
            assert!(got.iter().any(|l| l == name), "{name} missing from {got:?}");
        }
    }

    #[test]
    fn interfaces_hover_complete_and_outline() {
        let (s, uri) = server_with(IFACE_SRC);
        let hover = |needle: &str, delta: usize| {
            let h = s.hover(lt::HoverParams { text_document_position_params: tdp(&uri, at(IFACE_SRC, needle, delta)), work_done_progress_params: Default::default() });
            let lt::HoverContents::Markup(m) = h.unwrap().contents else { panic!() };
            m.value
        };
        // An interface type shows its methods, its own and included ones.
        let shown = hover("Solid = Cube", 0);
        assert!(shown.contains("interface Solid:") && shown.contains("fn volume(self) -> Float") && shown.contains("fn area(self) -> Float"), "{shown}");
        // A call through an interface value, and a method of a bound.
        let shown = hover("volume() +", 0);
        assert!(shown.contains("fn volume(self) -> Float") && shown.contains("method of interface `Solid`"), "{shown}");
        let shown = hover("area()\n    return sum", 0);
        assert!(shown.contains("fn area(self) -> Float") && shown.contains("a bound of `T`"), "{shown}");
        let shown = hover("T: Shape", 0);
        assert!(shown.contains("T: Shape"), "{shown}");
        let shown = hover("total([", 0);
        assert!(shown.contains("`T` can be any type that is a `Shape`"), "{shown}");
        let shown = hover("area(self) -> Float\n", 0);
        assert!(shown.contains("required by interface `Shape`") && shown.contains("How much room it takes."), "{shown}");
        // Go to the method the interface asks for.
        let r = s
            .definition(lt::GotoDefinitionParams {
                text_document_position_params: tdp(&uri, at(IFACE_SRC, "volume() +", 0)),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap();
        let lt::GotoDefinitionResponse::Scalar(loc) = r else { panic!() };
        assert_eq!(loc.range.start, at(IFACE_SRC, "volume(self) -> Float\n\nstruct", 0));
        // `s.` offers the interface's methods.
        let items = complete(&s, &uri, at(IFACE_SRC, "    s.\n", 6));
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["volume", "area"]);
        // The outline shows interfaces as interfaces, and no boxes.
        let r = s.symbols(lt::DocumentSymbolParams { text_document: lt::TextDocumentIdentifier { uri: uri.clone() }, work_done_progress_params: Default::default(), partial_result_params: Default::default() });
        let Some(lt::DocumentSymbolResponse::Nested(syms)) = r else { panic!() };
        let kinds: Vec<_> = syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert_eq!(
            kinds,
            [("Shape", lt::SymbolKind::INTERFACE), ("Solid", lt::SymbolKind::INTERFACE), ("Cube", lt::SymbolKind::STRUCT), ("total", lt::SymbolKind::FUNCTION), ("main", lt::SymbolKind::FUNCTION)]
        );
    }

    const TYPES_SRC: &str = "interface Shape:\n    fn area(self) -> Float\n\n    fn describe(self) -> String:\n        return \"shape\"\n\nstruct Circle(Shape):\n    r: Float = 1.0\n\n    fn new(r: Float) -> Circle:\n        return Circle(r)\n\n    fn area(self) -> Float:\n        return self.r\n\nfn main():\n    val c = Circle.new(2.0)\n    val t = (1, \"a\")\n    print(t.0)\n    print(c.describe())\n    c.\n";

    #[test]
    fn static_methods_defaults_and_tuples() {
        let (s, uri) = server_with(TYPES_SRC);
        let hover = |needle: &str, delta: usize| {
            let h = s.hover(lt::HoverParams { text_document_position_params: tdp(&uri, at(TYPES_SRC, needle, delta)), work_done_progress_params: Default::default() });
            let lt::HoverContents::Markup(m) = h.unwrap_or_else(|| panic!("no hover at {needle}")).contents else { panic!() };
            m.value
        };
        let shown = hover("new(2.0)", 0);
        assert!(shown.contains("fn new(r: Float) -> Circle") && shown.contains("static method of `Circle`"), "{shown}");
        let shown = hover("r: Float = 1.0", 0);
        assert!(shown.contains("val r: Float = 1.0"), "{shown}");
        let shown = hover("0)\n    print(c", 0);
        assert!(shown.contains("(Int, String).0: Int"), "{shown}");
        let shown = hover("describe())", 0);
        assert!(shown.contains("fn describe(self) -> String") && shown.contains("default method of interface `Shape`"), "{shown}");
        // `c.` offers the default method too, but not the static one.
        let items = complete(&s, &uri, at(TYPES_SRC, "    c.\n", 6));
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"describe") && labels.contains(&"area") && !labels.contains(&"new"), "{labels:?}");
        // `Circle.` offers the static methods.
        let src = "struct Circle:\n    r: Float\n\n    fn new(r: Float) -> Circle:\n        return Circle(r)\n\n    fn area(self) -> Float:\n        return self.r\n\nfn main():\n    Circle.\n";
        let (s2, uri2) = server_with(src);
        let items = complete(&s2, &uri2, at(src, "    Circle.\n", 11));
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["new"]);
    }

    #[test]
    fn module_files_need_no_main() {
        let a = crate::analyze("fn square(x: Int) -> Int:\n    return x * x\n");
        assert!(a.diagnostics.is_empty(), "{:#?}", a.diagnostics);
    }

    #[test]
    fn stdlib_is_warning_free() {
        let a = crate::analyze("import time\nimport math\nimport fs\nfn main():\n    print(time.now().to_iso())\n    print(math.pi)\n    print(fs.cwd())\n");
        assert!(a.diagnostics.is_empty(), "{:#?}", a.diagnostics);
        assert!(a.program.is_some());
    }

    const MAP_SRC: &str = "import math\nval limit = 10\nfn main():\n    val m = {\"a\": 1}\n    print(m.get(\"a\", limit))\n    print(math.pi)\n    m.\n";

    #[test]
    fn maps_and_constants_in_the_editor() {
        let (s, uri) = server_with(MAP_SRC);
        let items = complete(&s, &uri, at(MAP_SRC, "    m.\n", 6));
        let got: Vec<_> = items.iter().map(|i| (i.label.as_str(), i.detail.clone().unwrap_or_default())).collect();
        assert!(got.iter().any(|(l, d)| *l == "get" && d.contains("fn get(key: String) -> Int?")), "{got:?}");
        assert!(got.iter().any(|(l, _)| *l == "keys"), "{got:?}");
        let hover = |needle: &str, delta: usize| {
            let h = s.hover(lt::HoverParams { text_document_position_params: tdp(&uri, at(MAP_SRC, needle, delta)), work_done_progress_params: Default::default() });
            let lt::HoverContents::Markup(m) = h.unwrap().contents else { panic!() };
            m.value
        };
        let shown = hover("limit))", 0);
        assert!(shown.contains("val limit: Int = 10"), "{shown}");
        let shown = hover("pi)", 0);
        assert!(shown.contains("val pi: Float = 3.141592653589793"), "{shown}");
        let shown = hover("get(", 0);
        assert!(shown.contains("fn get(key: String) -> Int?"), "{shown}");
        // `math.` offers its constants as well as its functions.
        let src = "import math\nfn main():\n    math.\n";
        let (s, uri) = server_with(src);
        let items = complete(&s, &uri, at(src, "    math.\n", 9));
        let kinds: Vec<_> = items.iter().filter(|i| i.label == "pi" || i.label == "sqrt").map(|i| (i.label.as_str(), i.kind)).collect();
        assert!(kinds.contains(&("pi", Some(lt::CompletionItemKind::CONSTANT))), "{kinds:?}");
        assert!(kinds.contains(&("sqrt", Some(lt::CompletionItemKind::FUNCTION))), "{kinds:?}");
    }

    #[test]
    fn module_member_completion() {
        let (s, uri) = server_with(TIME_SRC);
        let items = complete(&s, &uri, at(TIME_SRC, "    time.\n", 9));
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        for want in ["now", "utc", "parse_iso", "from_unix", "DateTime"] {
            assert!(labels.contains(&want), "{labels:?}");
        }
        assert!(!labels.iter().any(|l| l.starts_with('_')), "private names leaked: {labels:?}");
        let now = items.iter().find(|i| i.label == "now").unwrap();
        assert_eq!(now.insert_text.as_deref(), Some("now()"));
    }

    #[test]
    fn datetime_member_completion_hides_private() {
        let src = "import time\n\nfn main():\n    val d = time.now()\n    d.\n";
        let (s, uri) = server_with(src);
        let items = complete(&s, &uri, at(src, "    d.\n", 6));
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"year") && labels.contains(&"format") && labels.contains(&"add_days"), "{labels:?}");
        assert!(!labels.contains(&"_format_code"), "{labels:?}");
    }

    #[test]
    fn import_completion_lists_modules() {
        let src = "import \nfn main():\n    pass\n";
        let (s, uri) = server_with(src);
        let items = complete(&s, &uri, at(src, "import ", 7));
        assert!(items.iter().any(|i| i.label == "time"));
    }

    #[test]
    fn hover_on_module_function_shows_docs() {
        let (s, uri) = server_with(TIME_SRC);
        let h = s
            .hover(lt::HoverParams { text_document_position_params: tdp(&uri, at(TIME_SRC, "now()", 1)), work_done_progress_params: Default::default() })
            .unwrap();
        let lt::HoverContents::Markup(m) = h.contents else { panic!() };
        assert!(m.value.contains("fn now() -> time.DateTime"), "{}", m.value);
        assert!(m.value.contains("current date and time in UTC"), "{}", m.value);
    }

    #[test]
    fn outline_excludes_stdlib() {
        let (s, uri) = server_with(TIME_SRC);
        let Some(lt::DocumentSymbolResponse::Nested(syms)) = s.symbols(lt::DocumentSymbolParams {
            text_document: lt::TextDocumentIdentifier { uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }) else {
            panic!()
        };
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["main"]);
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

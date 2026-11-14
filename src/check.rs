//! Semantic analysis: name resolution, strict type checking, mutability,
//! definite-return analysis and lints. Produces the typed AST, plus the
//! editor information (`IdeInfo`) the language server needs.

use crate::ast::*;
use crate::diagnostics::Diagnostic;
use crate::ide::*;
use crate::patterns::{self, Ctor, Pat, Space};
use crate::project::{FileKind, Resolved, SourceFile, Target};
use crate::span::Span;
use crate::tast::*;
use crate::types::*;
use std::collections::HashMap;

const BUILTINS: &[&str] = &["print", "input", "args", "panic", "assert", "sqrt", "exit"];
const RESERVED_TYPES: &[&str] = &["Int", "Float", "Bool", "String", "List", "Unit"];

struct FuncSig {
    name: String,
    symbol: String,
    params: Vec<(String, Type)>,
    ret: Type,
    span: Span,
}

/// `import geometry`: a module bound to a name in one file.
struct ImportInfo {
    module_ns: usize,
    span: Span,
    used: bool,
    /// The whole line goes when it's unused (`import x`, not `from m import x`).
    whole_line: bool,
}

/// What `from geometry import x` brought in.
#[derive(Clone, Copy)]
enum Imported {
    Struct(StructId),
    Func(FuncId),
}

/// `from geometry import Point`: a name of another module usable unqualified in one file.
struct NameImport {
    target: Imported,
    span: Span,
    used: bool,
}

/// A `from` import whose name is looked up once the modules' declarations are known.
struct PendingName {
    ns: usize,
    module_ns: usize,
    /// `geometry`, as written in the import.
    module: String,
    name: Ident,
    binding: Ident,
}

fn is_private(name: &str) -> bool {
    name.starts_with('_')
}

struct LocalBinding {
    id: LocalId,
    mutable: bool,
    span: Span,
}

/// A value that smart casts can track: a local, or a chain of fields of one
/// (`node.next.value`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct NullPath {
    root: LocalId,
    fields: Vec<(StructId, usize)>,
}

impl NullPath {
    fn local(root: LocalId) -> Self {
        NullPath { root, fields: Vec::new() }
    }
}

/// How a possibly-null value was used where a non-null one is needed (for the error's help text).
enum NullUse<'a> {
    /// A field (`None`) or a method call (`Some(takes arguments)`).
    Member(&'a str, Option<bool>),
    Index,
    Value,
    Condition,
}

/// Bindings while checking one `case` pattern.
#[derive(Default)]
struct PatCx {
    /// Every name the pattern binds: (name, type, local).
    binds: Vec<(String, Type, LocalId)>,
    /// Names bound so far on the current alternative of a `|`.
    seen: Vec<String>,
    errors: bool,
}

/// Can this pattern match `null`?
fn pat_matches_null(p: &TPat) -> bool {
    match &p.kind {
        TPatKind::Wild | TPatKind::Null => true,
        TPatKind::Bind(_, inner) => pat_matches_null(inner),
        TPatKind::Or(alts) => alts.iter().any(pat_matches_null),
        _ => false,
    }
}

/// "a `Shape`" / "an `Int`"
fn a_type(name: &str) -> String {
    let an = name.starts_with(['A', 'E', 'I', 'O', 'U', 'a', 'e', 'i', 'o', 'u']);
    format!("{} `{name}`", if an { "an" } else { "a" })
}

/// "`a`", "`a` and `b`", "`a`, `b` and `c`"
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// Per-local facts for lints (indexed by `LocalId`).
struct LocalMeta {
    name: String,
    span: Span,
    kind: LocalKind,
    used: bool,
    reassigned: bool,
}

pub struct Analysis {
    /// `Some` when there are no errors (warnings are fine).
    pub program: Option<TProgram>,
    /// Errors and warnings.
    pub diagnostics: Vec<Diagnostic>,
    pub ide: IdeInfo,
}

pub struct Checker {
    /// Structs and enums, in one table.
    structs: Vec<TStruct>,
    /// The type each entry of `structs` defines (`Type::Struct` or `Type::Enum`).
    def_types: Vec<Type>,
    struct_spans: Vec<Span>,
    struct_ns: Vec<usize>,
    /// (namespace, name) -> struct. Namespace 0 is the user's file, k > 0 the k-th module.
    struct_index: HashMap<(usize, String), StructId>,
    funcs: Vec<FuncSig>,
    func_index: HashMap<(usize, String), FuncId>,
    /// The namespace currently being checked.
    ns: usize,
    /// Per namespace: the module's name (empty for the entry file).
    ns_names: Vec<String>,
    /// Per namespace: a unique name for generated symbols.
    ns_symbols: Vec<String>,
    ns_kinds: Vec<FileKind>,
    ns_paths: Vec<Option<std::path::PathBuf>>,
    /// (span base, source text) per namespace, for doc comments.
    ns_sources: Vec<(u32, String)>,
    /// (namespace, name) -> module, for `import` (and `from` imports of a module file).
    imports: HashMap<(usize, String), ImportInfo>,
    /// (namespace, name) -> a struct or function brought in by `from ... import`.
    names: HashMap<(usize, String), NameImport>,
    /// Names whose import failed (already reported): their uses stay quiet.
    failed_imports: std::collections::HashSet<(usize, String)>,
    methods: HashMap<(StructId, String), FuncId>,
    diags: Vec<Diagnostic>,
    // per-function state
    locals: Vec<TLocal>,
    scopes: Vec<HashMap<String, LocalBinding>>,
    loop_depth: usize,
    ret_ty: Type,
    /// Smart casts: values known not to be `null` at this point of the function.
    nonnull: Vec<NullPath>,
    // editor support + lints
    ide: IdeInfo,
    local_meta: Vec<LocalMeta>,
    field_spans: Vec<Vec<Span>>,
    pending_struct_hovers: Vec<(Span, StructId)>,
}

/// Type-check: `Ok((program, warnings))` or `Err(errors and warnings)`.
pub fn check(files: &[SourceFile]) -> Result<(TProgram, Vec<Diagnostic>), Vec<Diagnostic>> {
    let a = analyze(files);
    match a.program {
        Some(p) => Ok((p, a.diagnostics)),
        None => Err(a.diagnostics),
    }
}

/// Type-check and collect everything an editor needs, even when there are
/// errors. File `k` is namespace `k`; file 0 is the entry file.
pub fn analyze(files: &[SourceFile]) -> Analysis {
    let mut c = Checker {
        structs: Vec::new(),
        def_types: Vec::new(),
        struct_spans: Vec::new(),
        struct_ns: Vec::new(),
        struct_index: HashMap::new(),
        funcs: Vec::new(),
        func_index: HashMap::new(),
        ns: 0,
        ns_names: files.iter().map(|f| f.name.clone()).collect(),
        ns_symbols: files.iter().map(|f| f.symbol.clone()).collect(),
        ns_kinds: files.iter().map(|f| f.kind).collect(),
        ns_paths: files.iter().map(|f| f.path.clone()).collect(),
        ns_sources: files.iter().map(|f| (f.base, f.text.clone())).collect(),
        imports: HashMap::new(),
        names: HashMap::new(),
        failed_imports: Default::default(),
        methods: HashMap::new(),
        diags: Vec::new(),
        locals: Vec::new(),
        scopes: Vec::new(),
        loop_depth: 0,
        ret_ty: Type::Unit,
        nonnull: Vec::new(),
        ide: IdeInfo::default(),
        local_meta: Vec::new(),
        field_spans: Vec::new(),
        pending_struct_hovers: Vec::new(),
    };
    for (ns, f) in files.iter().enumerate().skip(1) {
        let doc = c.leading_comment(ns);
        c.ide.modules.push(ModuleSymbol { name: f.name.clone(), ns, doc, path: f.path.clone() });
    }
    // Modules first: the entry file's functions come last in the program.
    let mut units: Vec<(usize, &Program)> = files.iter().enumerate().skip(1).map(|(ns, f)| (ns, &f.program)).collect();
    units.push((0, &files[0].program));
    let funcs = c.program(&units, files);
    c.finish_ide();
    let has_errors = c.diags.iter().any(|d| d.is_error());
    let program = if has_errors { None } else { Some(TProgram { structs: c.structs, funcs }) };
    Analysis { program, diagnostics: c.diags, ide: c.ide }
}

fn fn_full_span(f: &FnDecl) -> Span {
    let end = f.body.last().map(|s| s.span.end).unwrap_or(f.sig_span.end).max(f.sig_span.end);
    Span { start: f.sig_span.start, end }
}

fn local_code(name: &str, ty: &str, kind: LocalKind) -> String {
    match kind {
        LocalKind::Param => format!("{name}: {ty}"),
        LocalKind::Val | LocalKind::LoopVar => format!("val {name}: {ty}"),
        LocalKind::Var => format!("var {name}: {ty}"),
    }
}

fn local_doc(kind: LocalKind) -> Option<String> {
    match kind {
        LocalKind::Param => Some("parameter".into()),
        LocalKind::LoopVar => Some("loop variable".into()),
        _ => None,
    }
}

/// Turn `#` comment lines into Markdown: prose stays prose, indented lines
/// (examples, tables) become code blocks.
fn format_doc(lines: &[&str]) -> Option<String> {
    let bodies: Vec<&str> = lines
        .iter()
        .map(|l| {
            let b = l.trim_start_matches('#');
            b.strip_prefix(' ').unwrap_or(b)
        })
        .collect();
    if bodies.iter().all(|b| b.trim().is_empty()) {
        return None;
    }
    let mut out = String::new();
    let mut i = 0;
    while i < bodies.len() {
        if bodies[i].starts_with("  ") {
            let start = i;
            while i < bodies.len() && (bodies[i].starts_with("  ") || bodies[i].trim().is_empty() && i + 1 < bodies.len() && bodies[i + 1].starts_with("  ")) {
                i += 1;
            }
            let block = &bodies[start..i];
            let indent = block.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
            let first = block[0].trim_start();
            let lang = if ["import ", "val ", "var ", "fn ", "print("].iter().any(|k| first.starts_with(k)) { "isma" } else { "text" };
            out.push_str(&format!("\n```{lang}\n"));
            for l in block {
                out.push_str(l.get(indent..).unwrap_or("").trim_end());
                out.push('\n');
            }
            out.push_str("```\n");
        } else {
            out.push_str(bodies[i]);
            out.push('\n');
            i += 1;
        }
    }
    Some(out.trim().to_string())
}

/// Edit distance for "did you mean" suggestions.
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == *cb { 0 } else { 1 };
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    let max = name.len().div_ceil(3).max(1);
    candidates
        .filter(|c| *c != name)
        .map(|c| (levenshtein(name, c), c))
        .filter(|(d, _)| *d <= max)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c.to_string())
}

impl Checker {
    fn err(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    fn struct_names(&self) -> Vec<String> {
        self.structs.iter().map(|s| s.name.clone()).collect()
    }

    fn tn(&self, t: &Type) -> String {
        t.display(&self.struct_names())
    }

    /// The `#` comment block directly above the line containing `offset`.
    fn doc_comment(&self, offset: u32) -> Option<String> {
        let (base, src) = self.ns_sources.iter().rev().find(|(b, _)| *b <= offset)?;
        let off = ((offset - base) as usize).min(src.len());
        let line_start = src[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let mut lines: Vec<&str> = Vec::new();
        let mut end = line_start;
        while end > 0 {
            let prev_start = src[..end - 1].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let line = src[prev_start..end - 1].trim();
            if !line.starts_with('#') || line.starts_with("#!") {
                break;
            }
            lines.push(line);
            end = prev_start;
        }
        lines.reverse();
        format_doc(&lines)
    }

    /// The comment block at the very top of a namespace's file (module docs).
    fn leading_comment(&self, ns: usize) -> Option<String> {
        let src = &self.ns_sources[ns].1;
        let lines: Vec<&str> = src.lines().map(str::trim).take_while(|l| l.starts_with('#')).filter(|l| !l.starts_with("#!")).collect();
        format_doc(&lines)
    }

    fn display_struct(&self, ns: usize, name: &str) -> String {
        if ns == 0 { name.to_string() } else { format!("{}.{name}", self.ns_names[ns]) }
    }

    /// Resolve `module` as imported in the current namespace (marking it used).
    fn imported_module(&mut self, module: &str) -> Option<usize> {
        let info = self.imports.get_mut(&(self.ns, module.to_string()))?;
        info.used = true;
        Some(info.module_ns)
    }

    /// Hover on a module name, and go-to-definition to its file.
    fn module_hover(&mut self, module_ns: usize, span: Span) {
        let m = self.ide.modules.iter().find(|m| m.ns == module_ns);
        let mut doc = m.and_then(|m| m.doc.clone());
        if self.ns_kinds[module_ns] != FileKind::Std
            && let Some(file) = m.and_then(|m| m.path.as_ref()).and_then(|p| p.file_name())
        {
            let file = format!("`{}`", file.to_string_lossy());
            doc = Some(match doc {
                Some(d) => format!("{file}\n\n{d}"),
                None => file,
            });
        }
        self.hover(span, format!("import {}", self.ns_names[module_ns]), doc);
        let start = self.ns_sources[module_ns].0;
        self.ide.definitions.push((span, Span { start, end: start }));
    }

    /// Hint for a name that exists in a module but was used unqualified.
    fn module_hint(&self, name: &str) -> Option<String> {
        if is_private(name) {
            return None;
        }
        let mut here: Vec<(&String, usize)> =
            self.imports.iter().filter(|((ns, _), _)| *ns == self.ns).map(|((_, b), i)| (b, i.module_ns)).collect();
        here.sort();
        for (binding, mns) in &here {
            let key = (*mns, name.to_string());
            if self.func_index.contains_key(&key) || self.struct_index.contains_key(&key) {
                return Some(format!("did you mean `{binding}.{name}`?"));
            }
        }
        for m in crate::stdlib::MODULES {
            if here.iter().any(|(_, mns)| self.ns_kinds[*mns] == FileKind::Std && self.ns_names[*mns] == m.name) {
                continue;
            }
            let needle = format!("\nfn {name}(");
            if m.source.contains(&needle) {
                return Some(format!("it lives in the `{}` module: add `import {}` and write `{}.{name}`", m.name, m.name, m.name));
            }
        }
        None
    }

    /// Did importing `name` into the current file fail? (The error was reported at the import.)
    fn import_failed(&self, name: &str) -> bool {
        self.failed_imports.contains(&(self.ns, name.to_string()))
    }

    /// Remember the names an `import` line would have bound, after reporting why it failed.
    fn fail_import(&mut self, ns: usize, im: &Import) {
        let bindings: Vec<String> = match &im.names {
            Some(names) => names.iter().map(|n| n.binding().name.clone()).collect(),
            None => im.binding().map(|b| b.name.clone()).into_iter().collect(),
        };
        for b in bindings {
            self.failed_imports.insert((ns, b));
        }
    }

    /// Is there a module file called `name` next to the file being checked?
    fn module_file_nearby(&self, name: &str) -> bool {
        let dir = self.ns_paths[self.ns].as_ref().and_then(|p| p.parent());
        dir.is_some_and(|d| d.join(format!("{name}.isma")).is_file())
    }

    /// A struct or enum known by its bare name in the current file: defined
    /// here, or brought in with `from ... import`.
    fn visible_struct(&mut self, name: &str) -> Option<StructId> {
        if let Some(&sid) = self.struct_index.get(&(self.ns, name.to_string())) {
            return Some(sid);
        }
        let n = self.names.get_mut(&(self.ns, name.to_string()))?;
        let Imported::Struct(sid) = n.target else { return None };
        n.used = true;
        Some(sid)
    }

    /// A top-level function known by its bare name in the current file.
    fn visible_func(&mut self, name: &str) -> Option<FuncId> {
        if let Some(&fid) = self.func_index.get(&(self.ns, name.to_string())) {
            return Some(fid);
        }
        let n = self.names.get_mut(&(self.ns, name.to_string()))?;
        let Imported::Func(fid) = n.target else { return None };
        n.used = true;
        Some(fid)
    }

    fn hover(&mut self, span: Span, code: String, doc: Option<String>) {
        self.ide.hovers.push(HoverInfo { span, code, doc });
    }

    fn signature(&self, name: &str, params: &[(String, Type)], ret: &Type, is_method: bool) -> String {
        let ps: Vec<String> = params
            .iter()
            .enumerate()
            .map(|(i, (n, t))| if is_method && i == 0 { "self".to_string() } else { format!("{n}: {}", self.tn(t)) })
            .collect();
        let mut s = format!("fn {name}({})", ps.join(", "));
        if *ret != Type::Unit {
            s.push_str(&format!(" -> {}", self.tn(ret)));
        }
        s
    }

    fn struct_summary(&self, sid: StructId) -> String {
        let s = &self.ide.structs[sid];
        if let Some(variants) = &s.variants {
            let mut out = format!("enum {}:", s.name);
            for v in variants {
                let short = v.signature.split_once('.').map(|(_, r)| r).unwrap_or(&v.signature);
                out.push_str(&format!("\n    {short}"));
            }
            for &m in &s.methods {
                out.push_str(&format!("\n    {}", self.ide.functions[m].signature));
            }
            return out;
        }
        let mut out = format!("struct {}:", s.name);
        for f in &s.fields {
            out.push_str(&format!("\n    {} {}: {}", if f.mutable { "var" } else { "val" }, f.name, f.ty));
        }
        for &m in &s.methods {
            out.push_str(&format!("\n    {}", self.ide.functions[m].signature));
        }
        if s.fields.is_empty() && s.methods.is_empty() {
            out.push_str("\n    pass");
        }
        out
    }

    fn finish_ide(&mut self) {
        for (span, sid) in std::mem::take(&mut self.pending_struct_hovers) {
            let code = self.struct_summary(sid);
            self.hover(span, code, None);
        }
    }

    /// Hover + go-to-definition for a use of local `id` at `span` (`smart`:
    /// it is smart-cast to its non-null type here).
    fn local_use(&mut self, id: LocalId, decl: Span, span: Span, smart: bool) {
        let m = &self.local_meta[id];
        let declared = &self.locals[id].ty;
        let shown = if smart { declared.non_null() } else { declared };
        let code = local_code(&m.name, &self.tn(shown), m.kind);
        let mut doc = local_doc(m.kind);
        if smart {
            let note = format!("smart cast from `{}`: it was checked, so it can't be `null` here", self.tn(declared));
            doc = Some(match doc {
                Some(d) => format!("{d}\n\n{note}"),
                None => note,
            });
        }
        self.hover(span, code, doc);
        self.ide.definitions.push((span, decl));
    }

    fn field_use(&mut self, sid: StructId, idx: usize, span: Span) {
        let f = &self.ide.structs[sid].fields[idx];
        let code = format!("{} {}: {}", if f.mutable { "var" } else { "val" }, f.name, f.ty);
        let doc = Some(format!("field of `{}`", self.ide.structs[sid].name));
        let def = self.field_spans[sid][idx];
        self.hover(span, code, doc);
        self.ide.definitions.push((span, def));
        self.check_field_privacy(sid, idx, span);
    }

    /// Fields named `_x` belong to the module that defines the struct.
    fn check_field_privacy(&mut self, sid: StructId, idx: usize, span: Span) {
        let name = &self.structs[sid].fields[idx].name;
        if is_private(name) && self.struct_ns[sid] != self.ns {
            let module = &self.ns_names[self.struct_ns[sid]];
            self.err(
                Diagnostic::error(format!("field `{name}` of `{}` is private", self.structs[sid].name), span)
                    .with_primary("private field")
                    .with_note(format!("names starting with `_` are internal to the `{module}` module")),
            );
        }
    }

    // ---------- declarations ----------

    fn program(&mut self, units: &[(usize, &Program)], files: &[SourceFile]) -> Vec<TFunc> {
        // Pass 0: imports.
        let mut pending: Vec<PendingName> = Vec::new();
        for &(ns, program) in units {
            self.ns = ns;
            let resolved = &files[ns].imports;
            let imports = program.items.iter().filter_map(|i| if let Item::Import(im) = i { Some(im) } else { None });
            for (im, r) in imports.zip(resolved) {
                self.import(ns, im, r, &mut pending);
            }
        }

        // Pass 1: struct and enum names (one namespace).
        for &(ns, program) in units {
            self.ns = ns;
            for item in &program.items {
                let (name_id, end, enum_boxed) = match item {
                    Item::Struct(s) => {
                        let end = s.fields.iter().map(|f| f.ty.span.end).chain(s.methods.iter().map(|m| fn_full_span(m).end)).max();
                        (&s.name, end, None)
                    }
                    Item::Enum(e) => {
                        let end = e.variants.iter().map(|v| v.span.end).chain(e.methods.iter().map(|m| fn_full_span(m).end)).max();
                        (&e.name, end, Some(e.variants.iter().any(|v| !v.fields.is_empty())))
                    }
                    _ => continue,
                };
                let name = &name_id.name;
                let what = if enum_boxed.is_some() { "enum" } else { "struct" };
                if RESERVED_TYPES.contains(&name.as_str()) {
                    self.err(Diagnostic::error(format!("`{name}` is a built-in type name"), name_id.span));
                    continue;
                }
                if let Some(&prev) = self.struct_index.get(&(ns, name.clone())) {
                    let prev_span = self.struct_spans[prev];
                    self.err(
                        Diagnostic::error(format!("the type `{name}` is defined more than once"), name_id.span)
                            .with_label(prev_span, "first defined here")
                            .with_note(format!("this one is {} {what}", if what == "enum" { "an" } else { "a" })),
                    );
                    continue;
                }
                let id = self.structs.len();
                let display = self.display_struct(ns, name);
                self.structs.push(TStruct { name: display.clone(), fields: Vec::new(), variants: enum_boxed.map(|_| Vec::new()) });
                self.def_types.push(match enum_boxed {
                    Some(boxed) => Type::Enum(id, boxed),
                    None => Type::Struct(id),
                });
                self.struct_spans.push(name_id.span);
                self.struct_ns.push(ns);
                self.struct_index.insert((ns, name.clone()), id);
                let doc = self.doc_comment(name_id.span.start);
                self.ide.structs.push(StructSymbol {
                    name: display,
                    name_span: name_id.span,
                    full: Span { start: name_id.span.start, end: end.unwrap_or(name_id.span.end) },
                    fields: Vec::new(),
                    variants: enum_boxed.map(|_| Vec::new()),
                    methods: Vec::new(),
                    ns,
                    doc,
                });
                self.field_spans.push(Vec::new());
                self.pending_struct_hovers.push((name_id.span, id));
            }
        }

        // `from m import Type`: types first, since signatures need them.
        self.resolve_name_imports(&mut pending, false);

        // Pass 2: struct fields, function + method signatures.
        let mut bodies: Vec<(&FnDecl, Option<StructId>, usize)> = Vec::new();
        for &(ns, program) in units {
            self.ns = ns;
            for item in &program.items {
                match item {
                    Item::Import(_) => {}
                    Item::Struct(s) => {
                        let Some(&sid) = self.struct_index.get(&(ns, s.name.name.clone())) else { continue };
                        if self.struct_spans[sid] != s.name.span {
                            continue; // duplicate definition already reported
                        }
                        let mut seen: HashMap<&str, Span> = HashMap::new();
                        let mut fields = Vec::new();
                        for f in &s.fields {
                            if let Some(prev) = seen.get(f.name.name.as_str()) {
                                self.err(
                                    Diagnostic::error(format!("field `{}` is declared more than once", f.name.name), f.name.span)
                                        .with_label(*prev, "first declared here"),
                                );
                                continue;
                            }
                            seen.insert(&f.name.name, f.name.span);
                            let ty = self.resolve_type(&f.ty);
                            let tn = self.tn(&ty);
                            self.ide.structs[sid].fields.push(FieldSymbol {
                                name: f.name.name.clone(),
                                ty: tn.clone(),
                                mutable: f.mutable,
                                span: f.name.span,
                            });
                            self.field_spans[sid].push(f.name.span);
                            let kw = if f.mutable { "var" } else { "val" };
                            let sname = self.structs[sid].name.clone();
                            self.hover(f.name.span, format!("{kw} {}: {tn}", f.name.name), Some(format!("field of `{sname}`")));
                            fields.push(TField { name: f.name.name.clone(), ty, mutable: f.mutable });
                        }
                        self.structs[sid].fields = fields;
                        let fields: Vec<&str> = seen.keys().copied().collect();
                        self.declare_methods(sid, &s.methods, &fields, ns, &mut bodies);
                    }
                    Item::Enum(e) => {
                        let Some(&sid) = self.struct_index.get(&(ns, e.name.name.clone())) else { continue };
                        if self.struct_spans[sid] != e.name.span {
                            continue; // duplicate definition already reported
                        }
                        if e.variants.is_empty() {
                            self.err(
                                Diagnostic::error(format!("enum `{}` has no variants", e.name.name), e.name.span)
                                    .with_help("list the possible values, one per line:\n\nenum Color:\n    Red\n    Green"),
                            );
                        }
                        let ename = self.structs[sid].name.clone();
                        let mut seen: HashMap<&str, Span> = HashMap::new();
                        let mut variants = Vec::new();
                        for v in &e.variants {
                            if let Some(prev) = seen.get(v.name.name.as_str()) {
                                self.err(
                                    Diagnostic::error(format!("variant `{}` is declared more than once", v.name.name), v.name.span)
                                        .with_label(*prev, "first declared here"),
                                );
                                continue;
                            }
                            seen.insert(&v.name.name, v.name.span);
                            let mut fseen: HashMap<&str, Span> = HashMap::new();
                            let mut fields = Vec::new();
                            for f in &v.fields {
                                if let Some(prev) = fseen.get(f.name.name.as_str()) {
                                    self.err(
                                        Diagnostic::error(format!("field `{}` is declared more than once", f.name.name), f.name.span)
                                            .with_label(*prev, "first declared here"),
                                    );
                                    continue;
                                }
                                fseen.insert(&f.name.name, f.name.span);
                                let ty = self.resolve_type(&f.ty);
                                let tn = self.tn(&ty);
                                self.hover(f.name.span, format!("{}: {tn}", f.name.name), Some(format!("field of variant `{ename}.{}`", v.name.name)));
                                fields.push(TField { name: f.name.name.clone(), ty, mutable: false });
                            }
                            let tv = TVariant { name: v.name.name.clone(), fields };
                            let signature = self.variant_signature(&ename, &tv);
                            let doc = self.doc_comment(v.name.span.start);
                            self.hover(v.name.span, signature.clone(), Some(format!("variant of `{ename}`")));
                            if let Some(vs) = &mut self.ide.structs[sid].variants {
                                vs.push(VariantSymbol { name: tv.name.clone(), signature, span: v.name.span, doc });
                            }
                            variants.push(tv);
                        }
                        self.structs[sid].variants = Some(variants);
                        self.declare_methods(sid, &e.methods, &[], ns, &mut bodies);
                    }
                    Item::Fn(f) => {
                        let name = &f.name.name;
                        if let Some(self_span) = f.self_param {
                            self.err(Diagnostic::error("`self` is only allowed in methods inside a `struct`", self_span));
                        }
                        if BUILTINS.contains(&name.as_str()) {
                            self.err(Diagnostic::error(format!("`{name}` is a built-in function and cannot be redefined"), f.name.span));
                            continue;
                        }
                        if self.struct_index.contains_key(&(ns, name.clone())) {
                            self.err(Diagnostic::error(format!("`{name}` is already the name of a struct"), f.name.span));
                            continue;
                        }
                        if let Some(&prev) = self.func_index.get(&(ns, name.clone())) {
                            let prev_span = self.funcs[prev].span;
                            self.err(
                                Diagnostic::error(format!("function `{name}` is defined more than once"), f.name.span)
                                    .with_label(prev_span, "first defined here")
                                    .with_note("Isma has no function overloading"),
                            );
                            continue;
                        }
                        let fid = self.declare_fn(f, None);
                        self.func_index.insert((ns, name.clone()), fid);
                        bodies.push((f, None, ns));
                    }
                }
            }
        }

        // `from m import function`, and names that don't exist.
        self.resolve_name_imports(&mut pending, true);
        self.import_conflicts();

        // `main` checks. A file without `main` is fine here: it's a module.
        // (Building or running one is refused by the driver, see `crate::missing_main`.)
        if let Some(&id) = self.func_index.get(&(0, "main".to_string())) {
            let f = &self.funcs[id];
            if !f.params.is_empty() || f.ret != Type::Unit {
                let span = f.span;
                self.err(Diagnostic::error("`main` must take no parameters and return nothing", span));
            }
        }

        // Pass 3: bodies.
        let mut out = Vec::new();
        for (decl, owner, ns) in bodies {
            self.ns = ns;
            out.push(self.fn_body(decl, owner, out.len()));
        }
        self.ns = 0;

        // Lint: unused imports (in the user's own files).
        let mut unused: Vec<(String, Span, bool)> = self
            .imports
            .iter()
            .filter(|((ns, _), i)| self.ns_kinds[*ns] != FileKind::Std && !i.used)
            .map(|((_, n), i)| (n.clone(), i.span, i.whole_line))
            .chain(
                self.names
                    .iter()
                    .filter(|((ns, _), i)| self.ns_kinds[*ns] != FileKind::Std && !i.used)
                    .map(|((_, n), i)| (n.clone(), i.span, false)),
            )
            .collect();
        unused.sort_by_key(|(_, s, _)| s.start);
        for (name, span, whole_line) in unused {
            self.diags.push(
                Diagnostic::warning(format!("unused import `{name}`"), span)
                    .with_primary("never used")
                    .with_help(if whole_line { "remove this line" } else { "remove it from the import" }),
            );
        }
        out
    }

    /// Bind the names one `import` line brings into namespace `ns`.
    fn import(&mut self, ns: usize, im: &Import, r: &Resolved, pending: &mut Vec<PendingName>) {
        let path = im.path_text();
        let path_span = im.path_span();
        match &r.target {
            Target::Missing { tried, nearby } => {
                let mut d = Diagnostic::error(format!("no module named `{path}`"), path_span).with_primary("unknown module");
                d = match tried.as_slice() {
                    [] => d.with_note(format!("available modules: {}", nearby.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", "))),
                    files => d.with_note(format!("there is no file `{}`", files.join("`, `"))),
                };
                let last = im.path.last().map(|i| i.name.as_str()).unwrap_or_default();
                if let Some(sugg) = suggest(last, nearby.iter().map(|s| s.as_str())) {
                    let fixed = path.rsplit_once('.').map(|(head, _)| format!("{head}.{sugg}")).unwrap_or(sugg);
                    d = d.with_help(format!("did you mean `{fixed}`?"));
                }
                self.err(d);
                self.fail_import(ns, im);
                return;
            }
            Target::Dir(dir) if im.names.is_none() => {
                let mut d = Diagnostic::error(format!("`{path}` is a folder, not a module"), path_span).with_primary("a folder");
                d = match crate::project::module_names(dir).first().cloned() {
                    Some(first) => d.with_help(format!("import a file inside it, e.g. `from {path} import {first}`")),
                    None => d.with_note("it contains no `.isma` files"),
                };
                self.err(d);
                self.fail_import(ns, im);
                return;
            }
            &Target::File(t) if t == ns => {
                self.err(Diagnostic::error("a module cannot import itself", path_span).with_primary("this file"));
                self.fail_import(ns, im);
                return;
            }
            _ => {}
        }
        let Some(names) = &im.names else {
            let (Target::File(t), Some(binding)) = (&r.target, im.binding()) else { return };
            let t = *t;
            self.module_hover(t, path_span);
            self.bind_module(ns, binding, t, im.span, true);
            return;
        };
        for (i, n) in names.iter().enumerate() {
            let binding = n.binding().clone();
            if let Some(&Some(sub)) = r.submodules.get(i) {
                if sub == ns {
                    self.err(Diagnostic::error("a module cannot import itself", n.name.span).with_primary("this file"));
                    self.failed_imports.insert((ns, binding.name.clone()));
                    continue;
                }
                self.module_hover(sub, n.name.span);
                self.bind_module(ns, &binding, sub, binding.span, false);
                continue;
            }
            match &r.target {
                Target::File(t) => pending.push(PendingName { ns, module_ns: *t, module: path.clone(), name: n.name.clone(), binding }),
                Target::Dir(dir) => {
                    let mut d = Diagnostic::error(format!("there is no module `{}` in `{path}`", n.name.name), n.name.span).with_primary("not found");
                    let inside = crate::project::module_names(dir);
                    if let Some(sugg) = suggest(&n.name.name, inside.iter().map(|s| s.as_str())) {
                        d = d.with_help(format!("did you mean `{sugg}`?"));
                    } else if !inside.is_empty() {
                        d = d.with_note(format!("it has: {}", inside.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")));
                    }
                    self.err(d);
                    self.failed_imports.insert((ns, binding.name.clone()));
                }
                Target::Missing { .. } => {}
            }
        }
    }

    /// The span of an earlier import of the same name into `ns`.
    fn imported_before(&self, ns: usize, name: &str) -> Option<Span> {
        let key = (ns, name.to_string());
        self.imports.get(&key).map(|i| i.span).or_else(|| self.names.get(&key).map(|n| n.span))
    }

    /// `import geometry` / `from shapes import circle`: `binding` names module `module_ns` in `ns`.
    fn bind_module(&mut self, ns: usize, binding: &Ident, module_ns: usize, span: Span, whole_line: bool) {
        if let Some(prev) = self.imported_before(ns, &binding.name) {
            self.err(
                Diagnostic::error(format!("`{}` is imported more than once", binding.name), binding.span)
                    .with_primary("imported again")
                    .with_label(prev, "first imported here"),
            );
            return;
        }
        self.module_hover(module_ns, binding.span);
        self.imports.insert((ns, binding.name.clone()), ImportInfo { module_ns, span, used: false, whole_line });
        if ns == 0 {
            self.ide.imports.push((binding.name.clone(), binding.span, module_ns));
        }
    }

    /// Look up the names of `from m import a, b` once the declarations they
    /// may refer to are known: types (`functions` false) or the rest.
    fn resolve_name_imports(&mut self, pending: &mut Vec<PendingName>, functions: bool) {
        for p in std::mem::take(pending) {
            let key = (p.module_ns, p.name.name.clone());
            let target = match (self.struct_index.get(&key), self.func_index.get(&key)) {
                (Some(&sid), _) => Imported::Struct(sid),
                (None, Some(&fid)) if functions => Imported::Func(fid),
                (None, _) if !functions => {
                    pending.push(p);
                    continue;
                }
                (None, _) => {
                    self.no_such_name(&p);
                    continue;
                }
            };
            self.bind_imported(&p, target);
        }
    }

    /// `name` is not defined in `module_ns` but imported into it: say where it's from.
    fn reexport_hint(&self, module_ns: usize, module: &str, name: &str) -> Option<String> {
        let n = self.names.get(&(module_ns, name.to_string()))?;
        let home = match n.target {
            Imported::Struct(sid) => self.struct_ns[sid],
            Imported::Func(fid) => self.ide.functions[fid].ns,
        };
        let from = &self.ns_names[home];
        Some(format!("`{module}` imports `{name}` from the `{from}` module, but doesn't define it: import it from `{from}`"))
    }

    fn no_such_name(&mut self, p: &PendingName) {
        self.failed_imports.insert((p.ns, p.binding.name.clone()));
        let name = &p.name.name;
        let mut d = Diagnostic::error(format!("module `{}` has no `{name}`", p.module), p.name.span).with_primary("not found");
        if let Some(h) = self.reexport_hint(p.module_ns, &p.module, name) {
            self.err(d.with_help(h));
            return;
        }
        let candidates: Vec<String> = self
            .func_index
            .keys()
            .chain(self.struct_index.keys())
            .filter(|(ns, n)| *ns == p.module_ns && !is_private(n))
            .map(|(_, n)| n.clone())
            .collect();
        if let Some(sugg) = suggest(name, candidates.iter().map(|s| s.as_str())) {
            d = d.with_help(format!("did you mean `{sugg}`?"));
        } else if let Some(sid) = self.struct_index.iter().find(|((ns, _), sid)| {
            *ns == p.module_ns && self.structs[**sid].variants.as_ref().is_some_and(|vs| vs.iter().any(|v| &v.name == name))
        }) {
            let ename = sid.0.1.clone();
            d = d.with_help(format!("`{name}` is a variant of the enum `{ename}`: import `{ename}` and write `{ename}.{name}`"));
        }
        self.err(d);
    }

    /// `from geometry import Point`: `Point` now names that struct in `p.ns`.
    fn bind_imported(&mut self, p: &PendingName, target: Imported) {
        let name = &p.name.name;
        // A private name is still bound (marked used), so its uses don't cause more errors.
        let private = is_private(name) && p.module_ns != p.ns;
        if private {
            self.err(
                Diagnostic::error(format!("`{}.{name}` is private to the `{}` module", p.module, p.module), p.name.span)
                    .with_primary("private")
                    .with_note("names starting with `_` are internal to their module"),
            );
        }
        if let Some(prev) = self.imported_before(p.ns, &p.binding.name) {
            self.err(
                Diagnostic::error(format!("`{}` is imported more than once", p.binding.name), p.binding.span)
                    .with_primary("imported again")
                    .with_label(prev, "first imported here"),
            );
            return;
        }
        let spans = if p.binding.span == p.name.span { vec![p.name.span] } else { vec![p.name.span, p.binding.span] };
        for span in spans {
            match target {
                Imported::Struct(sid) => {
                    self.pending_struct_hovers.push((span, sid));
                    self.ide.definitions.push((span, self.struct_spans[sid]));
                }
                Imported::Func(fid) => {
                    let f = &self.ide.functions[fid];
                    let doc = Some(format!("from module `{}`{}", p.module, f.doc.as_ref().map(|d| format!("\n\n{d}")).unwrap_or_default()));
                    let code = f.signature.clone();
                    self.hover(span, code, doc);
                    self.ide.definitions.push((span, self.funcs[fid].span));
                }
            }
        }
        self.names.insert((p.ns, p.binding.name.clone()), NameImport { target, span: p.binding.span, used: private });
        if p.ns == 0 {
            let kind = match target {
                Imported::Struct(sid) => ImportedSymbol::Struct(sid),
                Imported::Func(fid) => ImportedSymbol::Func(fid),
            };
            self.ide.imported.push((p.binding.name.clone(), kind));
        }
    }

    /// An imported name that the file also defines itself.
    fn import_conflicts(&mut self) {
        let mut found: Vec<(Span, String, Span)> = Vec::new();
        let defined = |c: &Checker, key: &(usize, String)| match (c.struct_index.get(key), c.func_index.get(key)) {
            (Some(&sid), _) => Some(c.struct_spans[sid]),
            (None, Some(&fid)) => Some(c.funcs[fid].span),
            _ => None,
        };
        let keys: Vec<(usize, String)> = self.imports.keys().chain(self.names.keys()).cloned().collect();
        for key in keys {
            let Some(def) = defined(self, &key) else { continue };
            // The local definition wins; don't also call the import unused.
            if let Some(i) = self.imports.get_mut(&key) {
                i.used = true;
                found.push((i.span, key.1.clone(), def));
            } else if let Some(n) = self.names.get_mut(&key) {
                n.used = true;
                found.push((n.span, key.1.clone(), def));
            }
        }
        found.sort_by_key(|(s, _, _)| s.start);
        for (span, name, def) in found {
            self.err(
                Diagnostic::error(format!("`{name}` is imported, but this file also defines `{name}`"), span)
                    .with_primary("imported here")
                    .with_label(def, "defined here")
                    .with_help("rename one of them (an import can be renamed with `as`)"),
            );
        }
    }

    /// The methods of struct or enum `sid` (`fields`: names they may not reuse).
    fn declare_methods<'p>(&mut self, sid: StructId, methods: &'p [FnDecl], fields: &[&str], ns: usize, bodies: &mut Vec<(&'p FnDecl, Option<StructId>, usize)>) {
        let tname = self.structs[sid].name.clone();
        for m in methods {
            if fields.contains(&m.name.name.as_str()) {
                self.err(Diagnostic::error(format!("`{}` is both a field and a method of `{tname}`", m.name.name), m.name.span));
                continue;
            }
            if m.self_param.is_none() {
                self.err(
                    Diagnostic::error(format!("method `{}` must take `self` as its first parameter", m.name.name), m.sig_span)
                        .with_note("static methods are not supported yet; use a top-level function"),
                );
                continue;
            }
            if let Some(&prev) = self.methods.get(&(sid, m.name.name.clone())) {
                let prev_span = self.funcs[prev].span;
                self.err(
                    Diagnostic::error(format!("method `{}` is defined more than once", m.name.name), m.name.span)
                        .with_label(prev_span, "first defined here"),
                );
                continue;
            }
            let fid = self.declare_fn(m, Some(sid));
            self.methods.insert((sid, m.name.name.clone()), fid);
            self.ide.structs[sid].methods.push(fid);
            bodies.push((m, Some(sid), ns));
        }
    }

    /// `Shape.Circle(radius: Float)`
    fn variant_signature(&self, ename: &str, v: &TVariant) -> String {
        let short = ename.rsplit('.').next().unwrap_or(ename);
        if v.fields.is_empty() {
            return format!("{short}.{}", v.name);
        }
        let fs: Vec<String> = v.fields.iter().map(|f| format!("{}: {}", f.name, self.tn(&f.ty))).collect();
        format!("{short}.{}({})", v.name, fs.join(", "))
    }

    fn declare_fn(&mut self, f: &FnDecl, owner: Option<StructId>) -> FuncId {
        let mut params = Vec::new();
        if let Some(sid) = owner {
            params.push(("self".to_string(), self.def_types[sid].clone()));
        }
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for p in &f.params {
            if p.name.name == "self" {
                self.err(Diagnostic::error("`self` must be the first parameter", p.name.span));
            }
            if let Some(prev) = seen.get(p.name.name.as_str()) {
                self.err(
                    Diagnostic::error(format!("parameter `{}` is declared more than once", p.name.name), p.name.span)
                        .with_label(*prev, "first declared here"),
                );
            }
            seen.insert(&p.name.name, p.name.span);
            let ty = self.resolve_type(&p.ty);
            params.push((p.name.name.clone(), ty));
        }
        let ret = match &f.ret {
            Some(t) => self.resolve_type(t),
            None => Type::Unit,
        };
        let module = &self.ns_names[self.ns];
        let msym = &self.ns_symbols[self.ns];
        let (name, symbol) = match owner {
            Some(sid) => {
                let sname = &self.structs[sid].name;
                let short = sname.rsplit('.').next().unwrap_or(sname);
                let symbol = if self.ns == 0 { format!("isma_m_{short}_{}", f.name.name) } else { format!("isma_m_{msym}__{short}_{}", f.name.name) };
                (format!("{sname}.{}", f.name.name), symbol)
            }
            None if self.ns == 0 && f.name.name == "main" => ("main".into(), "isma_main".into()),
            None if self.ns == 0 => (f.name.name.clone(), format!("isma_fn_{}", f.name.name)),
            None => (format!("{module}.{}", f.name.name), format!("isma_{msym}__{}", f.name.name)),
        };
        let sig = self.signature(&f.name.name, &params, &ret, owner.is_some());
        let comment = self.doc_comment(f.sig_span.start);
        let hover_doc = match (owner, &comment) {
            (Some(sid), Some(c)) => Some(format!("method of `{}`\n\n{c}", self.structs[sid].name)),
            (Some(sid), None) => Some(format!("method of `{}`", self.structs[sid].name)),
            (None, c) if self.ns != 0 => Some(format!("in module `{module}`{}", c.as_ref().map(|c| format!("\n\n{c}")).unwrap_or_default())),
            (None, c) => c.clone(),
        };
        self.hover(f.name.span, sig.clone(), hover_doc);
        self.ide.functions.push(FnSymbol {
            name: f.name.name.clone(),
            signature: sig,
            name_span: f.name.span,
            full: fn_full_span(f),
            owner,
            ns: self.ns,
            doc: comment,
        });
        let id = self.funcs.len();
        self.funcs.push(FuncSig { name, symbol, params, ret, span: f.name.span });
        id
    }

    fn resolve_type(&mut self, t: &TypeExpr) -> Type {
        let ty = self.resolve_type_inner(t);
        if t.nullable { ty.nullable() } else { ty }
    }

    fn resolve_type_inner(&mut self, t: &TypeExpr) -> Type {
        let name = t.name.name.as_str();
        if let Some(m) = &t.module {
            return self.resolve_module_type(m, t);
        }
        let expect_args = |c: &mut Checker, n: usize| -> bool {
            if t.args.len() != n {
                let msg = if n == 0 {
                    format!("type `{name}` does not take type arguments")
                } else {
                    format!("type `{name}` takes {n} type argument(s), found {}", t.args.len())
                };
                c.err(Diagnostic::error(msg, t.span));
                false
            } else {
                true
            }
        };
        if let Some((_, doc)) = BUILTIN_TYPES.iter().find(|(n, _)| *n == name) {
            self.hover(t.name.span, name.to_string(), Some(doc.to_string()));
        }
        match name {
            "Int" | "Float" | "Bool" | "String" => {
                if !expect_args(self, 0) {
                    return Type::Error;
                }
                match name {
                    "Int" => Type::Int,
                    "Float" => Type::Float,
                    "Bool" => Type::Bool,
                    _ => Type::Str,
                }
            }
            "List" => {
                if !expect_args(self, 1) {
                    return Type::Error;
                }
                let inner = self.resolve_type(&t.args[0]);
                Type::List(Box::new(inner))
            }
            _ => {
                if let Some(sid) = self.visible_struct(name) {
                    self.ide.definitions.push((t.name.span, self.struct_spans[sid]));
                    self.pending_struct_hovers.push((t.name.span, sid));
                    if !expect_args(self, 0) {
                        return Type::Error;
                    }
                    return self.def_types[sid].clone();
                }
                if self.import_failed(name) {
                    return Type::Error;
                }
                let mut d = Diagnostic::error(format!("cannot find type `{name}`"), t.name.span).with_primary("not found");
                let hint = match name {
                    "int" | "i64" | "i32" | "Long" => Some("Int"),
                    "float" | "f64" | "Double" => Some("Float"),
                    "bool" | "Boolean" => Some("Bool"),
                    "str" | "string" | "Str" => Some("String"),
                    "list" | "Vec" | "Array" => Some("List"),
                    _ => None,
                };
                let ns = self.ns;
                let names: Vec<String> = RESERVED_TYPES
                    .iter()
                    .map(|s| s.to_string())
                    .chain(self.struct_index.keys().filter(|(n, _)| *n == ns).map(|(_, s)| s.clone()))
                    .collect();
                if let Some(h) = self.module_hint(name) {
                    d = d.with_help(h);
                } else if let Some(h) = hint.map(String::from).or_else(|| suggest(name, names.iter().map(|s| s.as_str()))) {
                    d = d.with_help(format!("did you mean `{h}`?"));
                }
                self.err(d);
                Type::Error
            }
        }
    }

    /// `time.DateTime` in a type annotation.
    fn resolve_module_type(&mut self, m: &Ident, t: &TypeExpr) -> Type {
        let Some(module_ns) = self.imported_module(&m.name) else {
            if self.import_failed(&m.name) {
                return Type::Error;
            }
            let mut d = Diagnostic::error(format!("`{}` is not an imported module", m.name), m.span);
            if self.module_file_nearby(&m.name) || crate::stdlib::find(&m.name).is_some() {
                d = d.with_help(format!("add `import {}` at the top of the file", m.name));
            }
            self.err(d);
            return Type::Error;
        };
        self.module_hover(module_ns, m.span);
        let name = &t.name.name;
        match self.struct_index.get(&(module_ns, name.clone())) {
            Some(&sid) if !is_private(name) || module_ns == self.ns => {
                self.pending_struct_hovers.push((t.name.span, sid));
                self.ide.definitions.push((t.name.span, self.struct_spans[sid]));
                if !t.args.is_empty() {
                    self.err(Diagnostic::error(format!("type `{}.{name}` does not take type arguments", m.name), t.span));
                    return Type::Error;
                }
                self.def_types[sid].clone()
            }
            Some(_) => {
                self.err(Diagnostic::error(format!("`{}.{name}` is private to the `{}` module", m.name, m.name), t.name.span));
                Type::Error
            }
            None => {
                let candidates: Vec<String> = self
                    .struct_index
                    .keys()
                    .filter(|(ns, n)| *ns == module_ns && !is_private(n))
                    .map(|(_, n)| n.clone())
                    .collect();
                let mut d = Diagnostic::error(format!("module `{}` has no type `{name}`", m.name), t.name.span).with_primary("not found");
                if let Some(sugg) = suggest(name, candidates.iter().map(|s| s.as_str())) {
                    d = d.with_help(format!("did you mean `{}.{sugg}`?", m.name));
                }
                self.err(d);
                Type::Error
            }
        }
    }

    // ---------- function bodies ----------

    fn fn_body(&mut self, f: &FnDecl, owner: Option<StructId>, fid: FuncId) -> TFunc {
        self.locals.clear();
        self.local_meta.clear();
        self.nonnull.clear();
        self.scopes = vec![HashMap::new()];
        self.loop_depth = 0;
        let sig_params: Vec<(String, Type)> = self.funcs[fid].params.clone();
        self.ret_ty = self.funcs[fid].ret.clone();
        let mut params = Vec::new();
        for (i, (name, ty)) in sig_params.iter().enumerate() {
            let span = if owner.is_some() && i == 0 {
                f.self_param.unwrap_or(f.name.span)
            } else {
                f.params[i - owner.is_some() as usize].name.span
            };
            let id = self.new_local(name, ty.clone(), span, LocalKind::Param);
            self.scopes[0].insert(name.clone(), LocalBinding { id, mutable: false, span });
            params.push(id);
        }
        let body = self.block(&f.body, false);
        if self.ret_ty != Type::Unit && !self.ret_ty.is_error() && !block_diverges(&body) {
            let rt = self.tn(&self.ret_ty);
            let span = f.ret.as_ref().map(|r| r.span).unwrap_or(f.sig_span);
            self.err(
                Diagnostic::error(format!("function `{}` might not return a value", f.name.name), span)
                    .with_primary(format!("`{}` must return `{rt}` on every path", f.name.name))
                    .with_help("add a `return` at the end of the function"),
            );
        }
        self.lint_locals();
        let sig = &self.funcs[fid];
        TFunc {
            name: sig.name.clone(),
            symbol: sig.symbol.clone(),
            params,
            ret: sig.ret.clone(),
            locals: std::mem::take(&mut self.locals),
            body,
            exported: sig.symbol == "isma_main",
            track_caller: self.ns_kinds[self.ns] == FileKind::Std,
        }
    }

    fn new_local(&mut self, name: &str, ty: Type, span: Span, kind: LocalKind) -> LocalId {
        if name != "_" {
            let tn = self.tn(&ty);
            self.ide.locals.push(LocalSymbol { name: name.to_string(), ty: tn.clone(), decl: span, kind });
            self.hover(span, local_code(name, &tn, kind), local_doc(kind));
        }
        self.local_meta.push(LocalMeta { name: name.to_string(), span, kind, used: false, reassigned: false });
        self.locals.push(TLocal { name: name.to_string(), ty });
        self.locals.len() - 1
    }

    /// Warnings: unused locals, and `var`s that are never reassigned.
    fn lint_locals(&mut self) {
        let metas = std::mem::take(&mut self.local_meta);
        for (id, m) in metas.iter().enumerate() {
            if m.name.starts_with('_') || m.name == "self" {
                continue;
            }
            if !m.used {
                let (what, help) = match m.kind {
                    LocalKind::Param => ("parameter", format!("if this is intentional, prefix it with an underscore: `_{}`", m.name)),
                    LocalKind::LoopVar => ("loop variable", "if you only need the repetition, write `for _ in ...`".to_string()),
                    _ => ("variable", format!("remove it, or prefix it with an underscore: `_{}`", m.name)),
                };
                self.diags.push(
                    Diagnostic::warning(format!("unused {what} `{}`", m.name), m.span)
                        .with_primary(format!("`{}` is never used", m.name))
                        .with_help(help),
                );
            } else if m.kind == LocalKind::Var && !m.reassigned {
                let mut d = Diagnostic::warning(format!("`{}` is declared `var` but never reassigned", m.name), m.span)
                    .with_primary("never reassigned")
                    .with_help(format!("use `val {}` instead", m.name));
                if matches!(self.locals[id].ty, Type::List(_) | Type::Struct(_)) {
                    d = d.with_note("`val` only stops rebinding the name; you can still change the contents (`push`, field assignment)");
                }
                self.diags.push(d);
            }
        }
        self.local_meta = metas;
    }

    fn declare_local(&mut self, name: &Ident, ty: Type, kind: LocalKind) -> LocalId {
        let mutable = kind == LocalKind::Var;
        if name.name == "_" {
            // `_` is a throwaway name: never visible, may be declared repeatedly.
            return self.new_local("_", ty, name.span, kind);
        }
        if let Some(prev) = self.scopes.last().unwrap().get(&name.name) {
            let prev_span = prev.span;
            self.err(
                Diagnostic::error(format!("`{}` is already defined in this scope", name.name), name.span)
                    .with_label(prev_span, "previously defined here")
                    .with_help(if mutable {
                        "use plain assignment to change it, or pick a new name"
                    } else {
                        "pick a different name (or declare the original with `var` and reassign it)"
                    }),
            );
        }
        let id = self.new_local(&name.name, ty, name.span, kind);
        self.scopes.last_mut().unwrap().insert(name.name.clone(), LocalBinding { id, mutable, span: name.span });
        id
    }

    fn lookup(&self, name: &str) -> Option<&LocalBinding> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    /// A compiler-introduced local (the receiver inside `x?.member`).
    fn hidden_local(&mut self, ty: Type) -> LocalId {
        self.local_meta.push(LocalMeta { name: "_it".into(), span: Span::default(), kind: LocalKind::Val, used: true, reassigned: false });
        self.locals.push(TLocal { name: "_it".into(), ty });
        self.locals.len() - 1
    }

    // ---------- null safety ----------

    /// The source text of a short, single-line span (for messages like "`user.name` might be null").
    fn snippet(&self, span: Span) -> Option<String> {
        let (base, src) = self.ns_sources.iter().rev().find(|(b, _)| *b <= span.start)?;
        let s = src.get((span.start - base) as usize..(span.end - base) as usize)?;
        (!s.is_empty() && s.len() <= 40 && !s.contains('\n')).then(|| s.to_string())
    }

    /// The trackable value an expression denotes, if any.
    fn path_of(e: &TExpr) -> Option<NullPath> {
        match &e.kind {
            TExprKind::Local(id) => Some(NullPath::local(*id)),
            TExprKind::Unwrap(inner) => Self::path_of(inner),
            TExprKind::Field(obj, sid, idx) => {
                let mut p = Self::path_of(obj)?;
                p.fields.push((*sid, *idx));
                Some(p)
            }
            _ => None,
        }
    }

    /// A path can only be smart-cast if nothing can change it behind our
    /// back: every field on it must be a `val` (locals are tracked exactly).
    fn first_var_field(&self, p: &NullPath) -> Option<String> {
        p.fields.iter().find(|(sid, idx)| self.structs[*sid].fields[*idx].mutable).map(|(sid, idx)| self.structs[*sid].fields[*idx].name.clone())
    }

    fn is_smart_cast(&self, p: &NullPath) -> bool {
        self.nonnull.contains(p) && self.first_var_field(p).is_none()
    }

    fn add_facts(&mut self, facts: Vec<NullPath>) {
        for p in facts {
            if !self.nonnull.contains(&p) {
                self.nonnull.push(p);
            }
        }
    }

    /// Paths that are non-null when `e` (a nullable value) turns out non-null:
    /// `e` itself, and for `a?.b` also `a`.
    fn nonnull_when_nonnull(e: &TExpr) -> Vec<NullPath> {
        let mut out: Vec<NullPath> = Self::path_of(e).into_iter().collect();
        if let TExprKind::SafeCall { recv, .. } = &e.kind {
            out.extend(Self::nonnull_when_nonnull(recv));
        }
        out
    }

    /// What a condition tells us about nullness: (paths non-null when it's
    /// true, paths non-null when it's false).
    fn cond_facts(e: &TExpr) -> (Vec<NullPath>, Vec<NullPath>) {
        let union = |mut a: Vec<NullPath>, b: Vec<NullPath>| {
            for p in b {
                if !a.contains(&p) {
                    a.push(p);
                }
            }
            a
        };
        let inter = |a: Vec<NullPath>, b: &[NullPath]| a.into_iter().filter(|p| b.contains(p)).collect::<Vec<_>>();
        match &e.kind {
            TExprKind::IsNull(inner) => (Vec::new(), Self::nonnull_when_nonnull(inner)),
            TExprKind::Not(inner) => {
                let (t, f) = Self::cond_facts(inner);
                (f, t)
            }
            // `x == 5` (x: Int?) is only true when x isn't null.
            TExprKind::Binary(op @ (BinOp::Eq | BinOp::Ne), a, b) => {
                let mut facts = Vec::new();
                for (x, y) in [(a, b), (b, a)] {
                    if x.ty.is_nullable() && matches!(y.kind, TExprKind::Wrap(_)) {
                        facts.extend(Self::nonnull_when_nonnull(x));
                    }
                }
                if *op == BinOp::Eq { (facts, Vec::new()) } else { (Vec::new(), facts) }
            }
            TExprKind::And(a, b) => {
                let ((at, af), (bt, bf)) = (Self::cond_facts(a), Self::cond_facts(b));
                let false_b = union(at.clone(), bf);
                (union(at, bt), inter(af, &false_b))
            }
            TExprKind::Or(a, b) => {
                let ((at, af), (bt, bf)) = (Self::cond_facts(a), Self::cond_facts(b));
                let true_b = union(af.clone(), bt);
                (inter(at, &true_b), union(af, bf))
            }
            _ => (Vec::new(), Vec::new()),
        }
    }

    /// Smart cast: a nullable expression already known to be non-null here.
    fn narrow(&self, e: TExpr) -> TExpr {
        match (&e.ty, Self::path_of(&e)) {
            (Type::Nullable(inner), Some(p)) if self.is_smart_cast(&p) => {
                let (ty, span) = ((**inner).clone(), e.span);
                Self::mk(TExprKind::Unwrap(Box::new(e)), ty, span)
            }
            _ => e,
        }
    }

    /// Forget smart casts of locals that a loop body assigns: they may not hold on the next iteration.
    fn forget_assigned_in(&mut self, body: &[Stmt]) {
        fn names(stmts: &[Stmt], out: &mut Vec<String>) {
            for s in stmts {
                match &s.kind {
                    StmtKind::Assign { target: Expr { kind: ExprKind::Name(n), .. }, .. } => out.push(n.clone()),
                    StmtKind::If { branches, else_block } => {
                        for (_, b) in branches {
                            names(b, out);
                        }
                        if let Some(b) = else_block {
                            names(b, out);
                        }
                    }
                    StmtKind::While { body, .. } | StmtKind::For { body, .. } => names(body, out),
                    _ => {}
                }
            }
        }
        let mut assigned = Vec::new();
        names(body, &mut assigned);
        let ids: Vec<LocalId> = assigned.iter().filter_map(|n| self.lookup(n).map(|b| b.id)).collect();
        self.nonnull.retain(|p| !ids.contains(&p.root));
    }

    /// Why a checked value still can't be smart-cast, if that's the case.
    fn unstable_note(&self, e: &TExpr) -> Option<String> {
        let p = Self::path_of(e)?;
        if !self.nonnull.contains(&p) {
            return None;
        }
        let field = self.first_var_field(&p)?;
        let copy = self.snippet(e.span).map(|s| format!(": `val {} = {s}`", field)).unwrap_or_default();
        Some(format!(
            "it was checked for `null`, but `{field}` is a `var` field, which could have changed since then; copy it into a `val` first{copy}"
        ))
    }

    /// "`x` might be null", with advice that fits how it was used.
    fn null_use_error(&mut self, e: &TExpr, how: NullUse) {
        let tn = self.tn(&e.ty);
        let snip = self.snippet(e.span);
        let name = snip.as_ref().map(|s| format!("`{s}`")).unwrap_or_else(|| "this value".into());
        let s = snip.unwrap_or_else(|| "value".into());
        let default = match e.ty.non_null() {
            Type::Int => "0",
            Type::Float => "0.0",
            Type::Bool => "false",
            Type::Str => "\"\"",
            Type::List(_) => "[]",
            _ => "<default>",
        };
        let (help, assert) = match how {
            NullUse::Member(m, Some(has_args)) => {
                let args = if has_args { "(...)" } else { "()" };
                (format!("call it only when it isn't null with `{s}?.{m}{args}`, or check `if {s} != null:` first"), format!("{s}!!.{m}{args}"))
            }
            NullUse::Member(f, None) => (format!("use `{s}?.{f}` to get `null` instead, or check `if {s} != null:` first"), format!("{s}!!.{f}")),
            NullUse::Index => (format!("check `if {s} != null:` first"), format!("{s}!![i]")),
            NullUse::Value => (format!("give a default with `{s} ?: {default}`, or check `if {s} != null:` first"), format!("{s}!!")),
            NullUse::Condition => (format!("compare explicitly: `{s} == true`"), format!("{s}!!")),
        };
        let mut d = Diagnostic::error(format!("{name} might be null"), e.span)
            .with_primary(format!("this is `{tn}`"))
            .with_help(help)
            .with_note(format!("if it can never be null here, assert that with `{assert}` (it panics if it is)"));
        if let Some(n) = self.unstable_note(e) {
            d = d.with_note(n);
        }
        self.err(d);
    }

    /// `a?.member`, where `access` builds the member access on the (non-null) receiver.
    fn safe_access(&mut self, recv: TExpr, op_span: Span, span: Span, access: impl FnOnce(&mut Checker, TExpr) -> TExpr) -> TExpr {
        match recv.ty.clone() {
            Type::Nullable(inner) => {
                let binder = self.hidden_local((*inner).clone());
                let it = Self::mk(TExprKind::Local(binder), *inner, recv.span);
                // Inside the access (e.g. in arguments), the receiver isn't null.
                let saved = self.nonnull.clone();
                self.add_facts(Self::nonnull_when_nonnull(&recv));
                let body = access(self, it);
                self.nonnull = saved;
                let ty = if body.ty == Type::Unit { Type::Unit } else { body.ty.clone().nullable() };
                Self::mk(TExprKind::SafeCall { recv: Box::new(recv), binder, body: Box::new(body) }, ty, span)
            }
            Type::Error => {
                let _ = access(self, recv);
                Self::error_expr(span)
            }
            Type::Null => {
                self.err(Diagnostic::error("`null` has no fields or methods", recv.span).with_primary("this is always `null`"));
                Self::error_expr(span)
            }
            ty => {
                let tn = self.tn(&ty);
                self.diags.push(
                    Diagnostic::warning("unnecessary `?.`: this value is never `null`", op_span)
                        .with_label(recv.span, format!("this is `{tn}`"))
                        .with_help("use `.` instead"),
                );
                access(self, recv)
            }
        }
    }

    fn block(&mut self, stmts: &[Stmt], new_scope: bool) -> Vec<TStmt> {
        if new_scope {
            self.scopes.push(HashMap::new());
        }
        let mut out = Vec::new();
        let mut ended_at: Option<Span> = None;
        let mut warned = false;
        for s in stmts {
            if let (Some(end), false) = (ended_at, warned) {
                warned = true;
                self.diags.push(
                    Diagnostic::warning("unreachable code", s.span)
                        .with_primary("this code will never run")
                        .with_label(end, "any code after this is unreachable"),
                );
            }
            if let Some(t) = self.stmt(s) {
                if ended_at.is_none() && ends_flow(&t) {
                    ended_at = Some(s.span);
                }
                out.push(t);
            }
        }
        if new_scope {
            self.scopes.pop();
        }
        out
    }

    fn expect_bool(&mut self, e: &Expr, what: &str) -> TExpr {
        let t = self.expr(e, Some(&Type::Bool));
        if t.ty == Type::Bool.nullable() {
            self.null_use_error(&t, NullUse::Condition);
        } else if !t.ty.compatible(&Type::Bool) {
            let found = self.tn(&t.ty);
            let mut d = Diagnostic::error(format!("{what} must be a `Bool`, found `{found}`"), e.span)
                .with_primary(format!("this is `{found}`"))
                .with_note("Isma has no \"truthy\" values: conditions must be `Bool`");
            d = match &t.ty {
                Type::Int => d.with_help("compare explicitly, e.g. `x != 0`"),
                Type::Str => d.with_help("compare explicitly, e.g. `s.len() > 0`"),
                Type::List(_) => d.with_help("compare explicitly, e.g. `xs.len() > 0`"),
                _ => d,
            };
            self.err(d);
        }
        t
    }

    fn stmt(&mut self, s: &Stmt) -> Option<TStmt> {
        Some(match &s.kind {
            StmtKind::Let { mutable, name, ty, value } => {
                let declared = ty.as_ref().map(|t| self.resolve_type(t));
                let mut v = self.expr(value, declared.as_ref());
                if v.ty == Type::Unit {
                    self.err(
                        Diagnostic::error("cannot bind a value of type `Unit`", value.span)
                            .with_primary("this expression produces no value"),
                    );
                }
                let known_nonnull = !v.ty.is_nullable();
                let ty = match declared.clone() {
                    Some(d) => {
                        v = self.coerce(v, &d, ty.as_ref().map(|t| t.span));
                        d
                    }
                    None if v.ty == Type::Null => {
                        self.err(
                            Diagnostic::error("cannot infer a type for `null`", value.span)
                                .with_primary("`null` of what?")
                                .with_help(format!("add a nullable type annotation, e.g. `{} {}: Int? = null`", if *mutable { "var" } else { "val" }, name.name)),
                        );
                        Type::Error
                    }
                    None => v.ty.clone(),
                };
                if declared.is_none() && name.name != "_" && !ty.is_error() {
                    let hint = format!(": {}", self.tn(&ty));
                    self.ide.inlay_hints.push((name.span.end, hint));
                }
                let kind = if *mutable { LocalKind::Var } else { LocalKind::Val };
                let nullable = ty.is_nullable();
                let id = self.declare_local(name, ty, kind);
                if nullable && known_nonnull {
                    self.add_facts(vec![NullPath::local(id)]);
                }
                TStmt::Let(id, v)
            }
            StmtKind::Assign { target, op, value } => self.assign(target, *op, value, s.span)?,
            StmtKind::Expr(e) => {
                let t = self.expr(e, None);
                // `x!!` and `x ?: return` are fine on their own: they check, and smart-cast `x`.
                let checks = matches!(&t.kind, TExprKind::NotNull(_)) || matches!(&t.kind, TExprKind::Elvis(_, r) if r.diverges());
                if !matches!(e.kind, ExprKind::Call { .. } | ExprKind::Error) && !t.ty.is_error() && !checks {
                    let mut d = Diagnostic::error("expression result is unused", e.span)
                        .with_primary("this value is computed and thrown away");
                    if matches!(&e.kind, ExprKind::Binary(BinOp::Eq, _, _)) {
                        d = d.with_help("did you mean `=` (assignment) instead of `==`?");
                    } else {
                        d = d.with_help("bind it with `val name = ...` or remove it");
                    }
                    self.err(d);
                }
                TStmt::Expr(t)
            }
            StmtKind::If { branches, else_block } => {
                // Smart casts after the `if` are the ones that hold at the end of
                // every branch that doesn't jump away (`return`, `break`, ...).
                let mut tb = Vec::new();
                let mut ends: Vec<Vec<NullPath>> = Vec::new();
                for (cond, body) in branches {
                    let c = self.expect_bool(cond, "`if` condition");
                    let after_cond = self.nonnull.clone();
                    let (when_true, when_false) = Self::cond_facts(&c);
                    self.add_facts(when_true);
                    let b = self.block(body, true);
                    if !b.iter().any(ends_flow) {
                        ends.push(std::mem::take(&mut self.nonnull));
                    }
                    self.nonnull = after_cond;
                    self.add_facts(when_false);
                    tb.push((c, b));
                }
                let eb = else_block.as_ref().map(|b| self.block(b, true));
                if eb.as_ref().is_none_or(|b| !b.iter().any(ends_flow)) {
                    ends.push(self.nonnull.clone());
                }
                if let Some((first, rest)) = ends.split_first() {
                    let mut join = first.clone();
                    join.retain(|p| rest.iter().all(|e| e.contains(p)));
                    self.nonnull = join;
                }
                TStmt::If { branches: tb, else_block: eb }
            }
            StmtKind::While { cond, body } => {
                self.forget_assigned_in(body);
                let before = self.nonnull.clone();
                let c = self.expect_bool(cond, "`while` condition");
                let after_cond = self.nonnull.clone();
                let (when_true, when_false) = Self::cond_facts(&c);
                self.add_facts(when_true);
                self.loop_depth += 1;
                let b = self.block(body, true);
                self.loop_depth -= 1;
                // Without a `break`, the loop only ends when the condition is false.
                if contains_break(&b) {
                    self.nonnull = before;
                } else {
                    self.nonnull = after_cond;
                    self.add_facts(when_false);
                }
                TStmt::While { cond: c, body: b }
            }
            StmtKind::For { var, iter, body } => match iter {
                ForIter::Range { start, end, inclusive } => {
                    let ts = self.expr(start, Some(&Type::Int));
                    let te = self.expr(end, Some(&Type::Int));
                    for t in [&ts, &te] {
                        if t.ty == Type::Int.nullable() {
                            self.null_use_error(t, NullUse::Value);
                        } else if !t.ty.compatible(&Type::Int) {
                            let found = self.tn(&t.ty);
                            self.err(
                                Diagnostic::error(format!("range bounds must be `Int`, found `{found}`"), t.span)
                                    .with_primary(format!("this is `{found}`")),
                            );
                        }
                    }
                    self.forget_assigned_in(body);
                    let before = self.nonnull.clone();
                    self.scopes.push(HashMap::new());
                    let v = self.declare_local(var, Type::Int, LocalKind::LoopVar);
                    self.loop_depth += 1;
                    let b = self.block(body, true);
                    self.loop_depth -= 1;
                    self.scopes.pop();
                    self.nonnull = before;
                    TStmt::ForRange { var: v, start: ts, end: te, inclusive: *inclusive, body: b }
                }
                ForIter::Expr(e) => {
                    let tl = self.expr(e, None);
                    let elem = match &tl.ty {
                        Type::List(t) => (**t).clone(),
                        Type::Error => Type::Error,
                        Type::Nullable(inner) if matches!(**inner, Type::List(_)) => {
                            self.null_use_error(&tl, NullUse::Value);
                            Type::Error
                        }
                        other => {
                            let found = self.tn(other);
                            let mut d = Diagnostic::error(format!("cannot iterate over a value of type `{found}`"), e.span)
                                .with_primary("not iterable");
                            d = match other {
                                Type::Int => d.with_help("to loop over numbers use a range, e.g. `for i in 0..n:`"),
                                Type::Str => d.with_note("iterating over strings is not supported yet"),
                                _ => d.with_note("only `List[T]` and ranges (`a..b`) can be iterated"),
                            };
                            self.err(d);
                            Type::Error
                        }
                    };
                    self.scopes.push(HashMap::new());
                    if var.name != "_" && !elem.is_error() {
                        let hint = format!(": {}", self.tn(&elem));
                        self.ide.inlay_hints.push((var.span.end, hint));
                    }
                    self.forget_assigned_in(body);
                    let before = self.nonnull.clone();
                    let v = self.declare_local(var, elem, LocalKind::LoopVar);
                    self.loop_depth += 1;
                    let b = self.block(body, true);
                    self.loop_depth -= 1;
                    self.scopes.pop();
                    self.nonnull = before;
                    TStmt::ForList { var: v, list: tl, body: b }
                }
            },
            StmtKind::Match { scrutinee, arms } => self.match_stmt(scrutinee, arms),
            StmtKind::Break | StmtKind::Continue => self.loop_jump(matches!(s.kind, StmtKind::Break), s.span),
            StmtKind::Return(value) => self.return_stmt(value.as_ref(), s.span),
            StmtKind::Pass => return None,
        })
    }

    fn error_count(&self) -> usize {
        self.diags.iter().filter(|d| d.is_error()).count()
    }

    fn match_stmt(&mut self, scrutinee: &Expr, arms: &[MatchArm]) -> TStmt {
        let ts = self.expr(scrutinee, None);
        let sty = ts.ty.clone();
        if sty == Type::Unit {
            self.err(Diagnostic::error("`match` needs a value", scrutinee.span).with_primary("this expression produces no value"));
        }
        let check_cover = !sty.is_error() && sty != Type::Unit;
        let path = Self::path_of(&ts);
        let before = self.nonnull.clone();
        // Earlier unguarded cases, for exhaustiveness and reachability.
        let mut rows: Vec<Vec<Pat>> = Vec::new();
        let mut tarms = Vec::new();
        let mut ends = Vec::new();
        let mut pattern_errors = false;
        let mut guarded = false;
        for arm in arms {
            self.nonnull = before.clone();
            self.scopes.push(HashMap::new());
            let null_left = sty.is_nullable() && Space { structs: &self.structs }.useful(&rows, &[Pat::Ctor(Ctor::Null, vec![])], std::slice::from_ref(&sty)).is_some();
            let mut cx = PatCx::default();
            let errors = self.error_count();
            // After `case null:`, a name in a later case binds the non-null value.
            let pat = if sty.is_nullable() && !null_left && matches!(arm.pattern.kind, PatternKind::Bind(_) | PatternKind::As(..)) {
                let inner = self.check_pattern(&arm.pattern, sty.non_null(), &mut cx);
                TPat { kind: TPatKind::Some(Box::new(inner)), ty: sty.clone() }
            } else {
                self.check_pattern(&arm.pattern, &sty, &mut cx)
            };
            let bad = cx.errors || self.error_count() > errors;
            pattern_errors |= bad;
            let ap = patterns::of_tpat(&pat);
            if check_cover && !bad {
                self.warn_unreachable(&rows, &ap, &sty, arm);
            }
            // Smart cast: in a case that can't see `null`, the matched value isn't null.
            if let Some(p) = &path
                && sty.is_nullable()
                && !(null_left && pat_matches_null(&pat))
            {
                self.add_facts(vec![p.clone()]);
            }
            let guard = arm.guard.as_ref().map(|g| {
                let tg = self.expect_bool(g, "`case` guard");
                let (when_true, _) = Self::cond_facts(&tg);
                self.add_facts(when_true);
                tg
            });
            let body = self.block(&arm.body, true);
            if !body.iter().any(ends_flow) {
                ends.push(std::mem::take(&mut self.nonnull));
            }
            self.scopes.pop();
            match guard {
                _ if bad => {} // (a case with errors covers nothing)
                None => rows.push(vec![ap]),
                Some(_) => guarded = true,
            }
            tarms.push(TArm { pat, guard, body });
        }
        if check_cover && !pattern_errors {
            let space = Space { structs: &self.structs };
            let missing = space.missing(rows, &sty, 3);
            if !missing.is_empty() {
                let mut shown: Vec<String> = missing.iter().take(3).map(|p| format!("`{}`", space.show(p, &sty))).collect();
                if missing.len() > 3 {
                    shown.push("more".into());
                }
                let what = if shown.len() == 1 { format!("{} is", shown[0]) } else { format!("{} are", and_list(&shown)) };
                let first = space.show(&missing[0], &sty);
                let tn = self.tn(&sty);
                let mut d = Diagnostic::error(format!("this `match` doesn't handle every value: {what} not covered"), scrutinee.span)
                    .with_primary(format!("this is `{tn}`"))
                    .with_help(format!("add a case for it:\n\ncase {first}:\n    ...\n\nor end with `case _:` to handle everything else"));
                if guarded {
                    d = d.with_note("cases with an `if` guard don't count here, because their guard might be false");
                }
                self.err(d);
            }
        }
        if arms.is_empty() {
            self.err(Diagnostic::error("`match` has no cases", scrutinee.span));
        }
        self.nonnull = match ends.split_first() {
            Some((first, rest)) => first.iter().filter(|p| rest.iter().all(|e| e.contains(p))).cloned().collect(),
            None => before,
        };
        TStmt::Match { scrutinee: ts, arms: tarms }
    }

    /// Warn about a case (or an alternative of a `|`) that earlier cases already cover.
    fn warn_unreachable(&mut self, rows: &[Vec<Pat>], p: &Pat, ty: &Type, arm: &MatchArm) {
        let space = Space { structs: &self.structs };
        let tys = std::slice::from_ref(ty);
        if space.useful(rows, std::slice::from_ref(p), tys).is_none() {
            self.diags.push(
                Diagnostic::warning("unreachable `case`", arm.pattern.span)
                    .with_primary("this case never runs")
                    .with_note("the cases above it already match every value it would"),
            );
            return;
        }
        let ast_alts = match &arm.pattern.kind {
            PatternKind::Or(a) => a,
            PatternKind::As(inner, _) => match &inner.kind {
                PatternKind::Or(a) => a,
                _ => return,
            },
            _ => return,
        };
        let Pat::Or(alts) = p else { return };
        for (j, alt) in alts.iter().enumerate() {
            let mut r = rows.to_vec();
            r.extend(alts[..j].iter().map(|a| vec![a.clone()]));
            if space.useful(&r, std::slice::from_ref(alt), tys).is_none() {
                self.diags.push(
                    Diagnostic::warning("unreachable pattern", ast_alts[j].span)
                        .with_primary("this alternative never matches")
                        .with_note("what it matches is already handled before it"),
                );
            }
        }
    }

    /// Check `p` against a value of type `ty`, declaring the names it binds.
    fn check_pattern(&mut self, p: &Pattern, ty: &Type, cx: &mut PatCx) -> TPat {
        let pat = |kind| TPat { kind, ty: ty.clone() };
        let wild = || TPat { kind: TPatKind::Wild, ty: ty.clone() };
        match &p.kind {
            PatternKind::Wild => pat(TPatKind::Wild),
            PatternKind::Bind(name) => {
                let id = self.bind_name(name, ty, cx);
                pat(TPatKind::Bind(id, Box::new(wild())))
            }
            PatternKind::As(inner, name) => {
                let t = self.check_pattern(inner, ty, cx);
                let id = self.bind_name(name, ty, cx);
                pat(TPatKind::Bind(id, Box::new(t)))
            }
            PatternKind::Or(alts) => {
                let before = cx.seen.clone();
                let mut out = Vec::new();
                let mut first: Option<(Vec<String>, Span)> = None;
                for a in alts {
                    cx.seen = before.clone();
                    out.push(self.check_pattern(a, ty, cx));
                    let mut names: Vec<String> = cx.seen[before.len()..].to_vec();
                    names.sort();
                    match &first {
                        None => first = Some((names, a.span)),
                        Some((want, span)) if *want != names => {
                            let odd: Vec<String> = want.iter().filter(|n| !names.contains(n)).chain(names.iter().filter(|n| !want.contains(n))).map(|n| format!("`{n}`")).collect();
                            self.err(
                                Diagnostic::error("every alternative of `|` must bind the same names", a.span)
                                    .with_label(*span, "compared to this alternative")
                                    .with_note(format!("{} {} bound by only one of them", and_list(&odd), if odd.len() == 1 { "is" } else { "are" }))
                                    .with_help("bind the name in every alternative, or use `_` instead"),
                            );
                            cx.errors = true;
                        }
                        _ => {}
                    }
                }
                cx.seen = before;
                cx.seen.extend(first.map(|f| f.0).unwrap_or_default());
                pat(TPatKind::Or(out))
            }
            PatternKind::Null => {
                if !ty.is_nullable() && !ty.is_error() {
                    let tn = self.tn(ty);
                    self.err(
                        Diagnostic::error("this case can never match: the value is never `null`", p.span)
                            .with_primary(format!("the value is a `{tn}`"))
                            .with_help("remove this case"),
                    );
                    cx.errors = true;
                }
                pat(TPatKind::Null)
            }
            // Any other pattern describes a non-null value.
            _ if ty.is_nullable() => {
                let inner = self.check_pattern(p, ty.non_null(), cx);
                pat(TPatKind::Some(Box::new(inner)))
            }
            _ if ty.is_error() => {
                if let PatternKind::Ctor { args: Some(args), .. } = &p.kind {
                    for a in args {
                        let _ = self.check_pattern(&a.pat, &Type::Error, cx);
                    }
                }
                wild()
            }
            PatternKind::Int(v) => self.literal_pattern(p, ty, &Type::Int, TPatKind::Int(*v), cx),
            PatternKind::Str(v) => self.literal_pattern(p, ty, &Type::Str, TPatKind::Str(v.clone()), cx),
            PatternKind::Bool(v) => self.literal_pattern(p, ty, &Type::Bool, TPatKind::Bool(*v), cx),
            PatternKind::Float(_) => {
                self.err(
                    Diagnostic::error("floats can't be used as patterns", p.span)
                        .with_primary("a `Float`")
                        .with_note("an exact float comparison is rarely what you want")
                        .with_help("bind the value and test it in a guard: `case x if x > 0.5:`"),
                );
                cx.errors = true;
                wild()
            }
            PatternKind::Ctor { path, args } => self.ctor_pattern(p, path, args.as_deref(), ty, cx),
        }
    }

    fn literal_pattern(&mut self, p: &Pattern, ty: &Type, lit_ty: &Type, kind: TPatKind, cx: &mut PatCx) -> TPat {
        if ty != lit_ty {
            let (want, found) = (self.tn(ty), self.tn(lit_ty));
            self.err(
                Diagnostic::error(format!("mismatched types: this case is {}, but the value is {}", a_type(&found), a_type(&want)), p.span)
                    .with_primary(format!("expected {} pattern", a_type(&want))),
            );
            cx.errors = true;
            return TPat { kind: TPatKind::Wild, ty: ty.clone() };
        }
        TPat { kind, ty: ty.clone() }
    }

    /// A name in a pattern: declares it, or (in another alternative of a `|`) reuses it.
    fn bind_name(&mut self, name: &Ident, ty: &Type, cx: &mut PatCx) -> LocalId {
        if cx.seen.contains(&name.name) {
            self.err(Diagnostic::error(format!("`{}` is bound more than once in this pattern", name.name), name.span).with_help("use a different name, or `_`"));
            cx.errors = true;
        }
        cx.seen.push(name.name.clone());
        if let Some((_, bty, id)) = cx.binds.iter().find(|b| b.0 == name.name).cloned() {
            if !bty.compatible(ty) {
                let (a, b) = (self.tn(&bty), self.tn(ty));
                self.err(Diagnostic::error(format!("`{}` is a `{a}` in one alternative and a `{b}` in another", name.name), name.span));
                cx.errors = true;
            }
            let decl = self.local_meta[id].span;
            self.local_use(id, decl, name.span, false);
            return id;
        }
        let id = self.declare_local(name, ty.clone(), LocalKind::Val);
        if !name.name.starts_with('_') && !ty.is_error() {
            let hint = format!(": {}", self.tn(ty));
            self.ide.inlay_hints.push((name.span.end, hint));
        }
        cx.binds.push((name.name.clone(), ty.clone(), id));
        id
    }

    /// `Circle(r)`, `Shape.Rect(w, h=_)`, `Empty`, `Point(x, y)`.
    fn ctor_pattern(&mut self, p: &Pattern, path: &[Ident], args: Option<&[PatArg]>, ty: &Type, cx: &mut PatCx) -> TPat {
        let last = path.last().unwrap();
        let shown: Vec<&str> = path.iter().map(|i| i.name.as_str()).collect();
        let shown = shown.join(".");
        // The qualifier (`Shape` in `Shape.Circle`) must name the value's type.
        let (sid, qual_ok) = match ty {
            Type::Enum(id, _) => (*id, path.len() == 1 || self.names_type(&path[..path.len() - 1], *id)),
            Type::Struct(id) => (*id, self.names_type(path, *id)),
            _ => {
                let tn = self.tn(ty);
                self.err(
                    Diagnostic::error(format!("mismatched types: `{shown}` can't match {}", a_type(&tn)), p.span)
                        .with_primary(format!("expected {} pattern", a_type(&tn))),
                );
                cx.errors = true;
                return TPat { kind: TPatKind::Wild, ty: ty.clone() };
            }
        };
        let tname = self.structs[sid].name.clone();
        let short = tname.rsplit('.').next().unwrap_or(&tname).to_string();
        if !qual_ok {
            self.err(
                Diagnostic::error(format!("mismatched types: `{shown}` can't match {}", a_type(&tname)), p.span)
                    .with_primary(format!("expected {} pattern", a_type(&tname))),
            );
            cx.errors = true;
            return TPat { kind: TPatKind::Wild, ty: ty.clone() };
        }
        if let Type::Struct(_) = ty {
            let fields = self.structs[sid].fields.clone();
            let subs = self.field_patterns(&fields, args, &short, p.span, cx);
            for (idx, sub) in subs.iter().enumerate() {
                if !matches!(sub.kind, TPatKind::Wild) && is_private(&fields[idx].name) && self.struct_ns[sid] != self.ns {
                    self.check_field_privacy(sid, idx, p.span);
                    cx.errors = true;
                }
            }
            return TPat { kind: TPatKind::Struct(sid, subs), ty: ty.clone() };
        }
        let variants = self.structs[sid].variants().to_vec();
        let Some(k) = variants.iter().position(|v| v.name == last.name) else {
            let mut d = Diagnostic::error(format!("enum `{tname}` has no variant `{}`", last.name), last.span).with_primary("unknown variant");
            if let Some(s) = suggest(&last.name, variants.iter().map(|v| v.name.as_str())) {
                d = d.with_help(format!("did you mean `{s}`?"));
            } else {
                let names: Vec<String> = variants.iter().map(|v| format!("`{}`", v.name)).collect();
                d = d.with_note(format!("its variants are {}", and_list(&names)));
            }
            self.err(d);
            cx.errors = true;
            return TPat { kind: TPatKind::Wild, ty: ty.clone() };
        };
        self.variant_use(sid, k, last.span);
        let fields = variants[k].fields.clone();
        if fields.is_empty() && args.is_some() {
            self.err(
                Diagnostic::error(format!("`{}` has no fields", last.name), p.span)
                    .with_primary("no fields to match")
                    .with_help(format!("write `{}` without parentheses", last.name)),
            );
            cx.errors = true;
        }
        let subs = self.field_patterns(&fields, args, &last.name, p.span, cx);
        TPat { kind: TPatKind::Variant(sid, k, subs), ty: ty.clone() }
    }

    /// Does `path` (`Shape`, `time.Month`) name the struct or enum `sid`? Adds hovers when it does.
    fn names_type(&mut self, path: &[Ident], sid: StructId) -> bool {
        let found = match path {
            [name] => self.visible_struct(&name.name),
            [module, name] => self.imported_module(&module.name).and_then(|m| self.struct_index.get(&(m, name.name.clone())).copied()),
            _ => None,
        };
        if found == Some(sid) {
            let name = path.last().unwrap();
            self.pending_struct_hovers.push((name.span, sid));
            self.ide.definitions.push((name.span, self.struct_spans[sid]));
        }
        found == Some(sid)
    }

    /// The patterns for each field of a variant or struct: positional, then by
    /// name; fields not mentioned (or all, without parentheses) match anything.
    fn field_patterns(&mut self, fields: &[TField], args: Option<&[PatArg]>, what: &str, span: Span, cx: &mut PatCx) -> Vec<TPat> {
        let mut slots: Vec<Option<TPat>> = vec![None; fields.len()];
        let args = args.unwrap_or(&[]);
        let positional = args.iter().take_while(|a| a.name.is_none()).count();
        let named = args.len() > positional;
        let n = fields.len();
        if positional > n || (positional > 0 && positional < n && !named) {
            let s = if n == 1 { "" } else { "s" };
            let example: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
            self.err(
                Diagnostic::error(format!("`{what}` has {n} field{s}, but the pattern gives {positional}"), span)
                    .with_primary(format!("expected {n} field pattern{s}"))
                    .with_help(format!("match every field, using `_` for the ones you don't need: `{what}({})`, or name them: `{what}({}=...)`", example.join(", "), example.first().cloned().unwrap_or_default())),
            );
            cx.errors = true;
        }
        for (i, a) in args.iter().enumerate() {
            let idx = match &a.name {
                None if i < positional => i,
                None => {
                    self.err(Diagnostic::error("positional patterns must come before named ones", a.pat.span));
                    cx.errors = true;
                    continue;
                }
                Some(fname) => match fields.iter().position(|f| f.name == fname.name) {
                    Some(idx) if slots[idx].is_some() => {
                        self.err(Diagnostic::error(format!("field `{}` is matched more than once", fname.name), fname.span));
                        cx.errors = true;
                        continue;
                    }
                    Some(idx) => idx,
                    None => {
                        let mut d = Diagnostic::error(format!("`{what}` has no field `{}`", fname.name), fname.span);
                        if let Some(s) = suggest(&fname.name, fields.iter().map(|f| f.name.as_str())) {
                            d = d.with_help(format!("did you mean `{s}`?"));
                        }
                        self.err(d);
                        cx.errors = true;
                        continue;
                    }
                },
            };
            if idx >= n {
                let _ = self.check_pattern(&a.pat, &Type::Error, cx);
                continue;
            }
            slots[idx] = Some(self.check_pattern(&a.pat, &fields[idx].ty, cx));
        }
        slots.into_iter().zip(fields).map(|(s, f)| s.unwrap_or(TPat { kind: TPatKind::Wild, ty: f.ty.clone() })).collect()
    }

    /// Hover and go-to-definition for a use of variant `k` of enum `sid`.
    fn variant_use(&mut self, sid: StructId, k: usize, span: Span) {
        let Some(v) = self.ide.structs[sid].variants.as_ref().and_then(|vs| vs.get(k)) else { return };
        let (code, def, doc) = (v.signature.clone(), v.span, v.doc.clone());
        let ename = self.structs[sid].name.clone();
        let doc = Some(match doc {
            Some(d) => format!("variant of `{ename}`\n\n{d}"),
            None => format!("variant of `{ename}`"),
        });
        self.hover(span, code, doc);
        self.ide.definitions.push((span, def));
    }

    /// `Shape` or `module.Shape`, when an expression names an enum (to build a variant).
    fn enum_ref(&mut self, e: &Expr) -> Option<StructId> {
        let sid = match &e.kind {
            ExprKind::Name(n) if self.lookup(n).is_none() => self.visible_struct(n)?,
            ExprKind::Field(m, n) => {
                let ExprKind::Name(mn) = &m.kind else { return None };
                if self.lookup(mn).is_some() || !self.imports.contains_key(&(self.ns, mn.clone())) {
                    return None;
                }
                let module_ns = self.imported_module(mn)?;
                let sid = self.struct_index.get(&(module_ns, n.name.clone())).copied()?;
                if !matches!(self.def_types[sid], Type::Enum(..)) {
                    return None;
                }
                self.module_hover(module_ns, m.span);
                sid
            }
            _ => return None,
        };
        if !matches!(self.def_types[sid], Type::Enum(..)) {
            return None;
        }
        let name_span = match &e.kind {
            ExprKind::Field(_, n) => n.span,
            _ => e.span,
        };
        self.pending_struct_hovers.push((name_span, sid));
        self.ide.definitions.push((name_span, self.struct_spans[sid]));
        Some(sid)
    }

    /// `Shape.Circle(args)` (or `Shape.Empty` when `args` is `None`).
    fn variant_value(&mut self, sid: StructId, name: &Ident, args: Option<&[Arg]>, span: Span) -> TExpr {
        let ename = self.structs[sid].name.clone();
        let variants = self.structs[sid].variants().to_vec();
        let ty = self.def_types[sid].clone();
        let Some(k) = variants.iter().position(|v| v.name == name.name) else {
            let mut d = Diagnostic::error(format!("enum `{ename}` has no variant `{}`", name.name), name.span).with_primary("unknown variant");
            if self.methods.contains_key(&(sid, name.name.clone())) {
                d = d.with_note(format!("`{}` is a method: call it on a `{ename}` value", name.name));
            } else if let Some(s) = suggest(&name.name, variants.iter().map(|v| v.name.as_str())) {
                d = d.with_help(format!("did you mean `{s}`?"));
            } else {
                let names: Vec<String> = variants.iter().map(|v| format!("`{}`", v.name)).collect();
                d = d.with_note(format!("its variants are {}", and_list(&names)));
            }
            self.err(d);
            for a in args.unwrap_or(&[]) {
                let _ = self.expr(&a.value, None);
            }
            return Self::error_expr(span);
        };
        self.variant_use(sid, k, name.span);
        let v = &variants[k];
        let short = ename.rsplit('.').next().unwrap_or(&ename).to_string();
        match args {
            None if v.fields.is_empty() => Self::mk(TExprKind::EnumNew(sid, k, Vec::new()), ty, span),
            None => {
                let sig = self.variant_signature(&ename, v);
                self.err(
                    Diagnostic::error(format!("`{short}.{}` needs its fields", v.name), span)
                        .with_primary("missing fields")
                        .with_help(format!("build it with `{sig}`")),
                );
                Self::error_expr(span)
            }
            Some(args) if v.fields.is_empty() => {
                for a in args {
                    let _ = self.expr(&a.value, None);
                }
                self.err(
                    Diagnostic::error(format!("`{short}.{}` has no fields", v.name), span)
                        .with_primary("no fields to pass")
                        .with_help(format!("write `{short}.{}` without parentheses", v.name)),
                );
                Self::error_expr(span)
            }
            Some(args) => {
                let params: Vec<(String, Type)> = v.fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect();
                let what = format!("variant `{short}.{}`", v.name);
                match self.match_args(&what, &params, args, span) {
                    Some(targs) => Self::mk(TExprKind::EnumNew(sid, k, targs), ty, span),
                    None => Self::mk(TExprKind::Int(0), ty, span),
                }
            }
        }
    }

    fn loop_jump(&mut self, is_break: bool, span: Span) -> TStmt {
        if self.loop_depth == 0 {
            let kw = if is_break { "break" } else { "continue" };
            self.err(Diagnostic::error(format!("`{kw}` outside of a loop"), span).with_primary("not inside `while` or `for`"));
        }
        if is_break { TStmt::Break } else { TStmt::Continue }
    }

    fn return_stmt(&mut self, value: Option<&Expr>, span: Span) -> TStmt {
        let ret = self.ret_ty.clone();
        match value {
            None => {
                if ret != Type::Unit && !ret.is_error() {
                    let rt = self.tn(&ret);
                    self.err(Diagnostic::error(format!("`return` without a value in a function returning `{rt}`"), span));
                }
                TStmt::Return(None)
            }
            Some(v) => {
                if ret == Type::Unit {
                    let t = self.expr(v, None);
                    self.err(
                        Diagnostic::error("this function does not return a value", v.span)
                            .with_help("declare a return type, e.g. `fn name() -> Int:`"),
                    );
                    TStmt::Return(Some(t))
                } else {
                    let t = self.expr(v, Some(&ret));
                    let t = self.coerce(t, &ret, None);
                    TStmt::Return(Some(t))
                }
            }
        }
    }

    fn assign(&mut self, target: &Expr, op: Option<BinOp>, value: &Expr, span: Span) -> Option<TStmt> {
        let (place, place_ty) = match &target.kind {
            ExprKind::Name(name) => {
                let Some(b) = self.lookup(name) else {
                    let _ = self.expr(target, None); // reports unknown name
                    let _ = self.expr(value, None);
                    return None;
                };
                let (id, mutable, decl_span) = (b.id, b.mutable, b.span);
                self.local_use(id, decl_span, target.span, false);
                self.local_meta[id].reassigned = true;
                if op.is_some() {
                    self.local_meta[id].used = true;
                }
                if !mutable {
                    self.err(
                        Diagnostic::error(format!("cannot assign twice to immutable variable `{name}`"), target.span)
                            .with_primary("cannot assign twice to immutable variable")
                            .with_label(decl_span, format!("`{name}` is declared with `val` here"))
                            .with_help(format!("make it mutable with `var {name}`")),
                    );
                }
                (Place::Local(id), self.locals[id].ty.clone())
            }
            ExprKind::Field(obj, field) => {
                let tobj = self.expr(obj, None);
                match &tobj.ty {
                    Type::Struct(sid) => {
                        let sid = *sid;
                        let Some(idx) = self.structs[sid].fields.iter().position(|f| f.name == field.name) else {
                            self.no_field(&tobj.ty, field);
                            let _ = self.expr(value, None);
                            return None;
                        };
                        let f = self.structs[sid].fields[idx].clone();
                        self.field_use(sid, idx, field.span);
                        if !f.mutable {
                            let sname = self.structs[sid].name.clone();
                            self.err(
                                Diagnostic::error(format!("cannot assign to immutable field `{}` of `{sname}`", f.name), target.span)
                                    .with_primary("field is immutable")
                                    .with_help(format!("declare it as `var {}: {}` in `struct {sname}`", f.name, self.tn(&f.ty))),
                            );
                        }
                        (Place::Field { obj: tobj, sid, idx }, f.ty)
                    }
                    Type::Error => {
                        let _ = self.expr(value, None);
                        return None;
                    }
                    other => {
                        self.no_field(&other.clone(), field);
                        let _ = self.expr(value, None);
                        return None;
                    }
                }
            }
            ExprKind::Index(list, index) => {
                let tl = self.expr(list, None);
                let ti = self.expr(index, Some(&Type::Int));
                let elem = self.index_elem_type(&tl, &ti, target.span);
                (Place::Index { list: tl, index: ti, span: target.span }, elem)
            }
            _ => {
                self.err(
                    Diagnostic::error("invalid assignment target", target.span)
                        .with_primary("cannot assign to this")
                        .with_note("you can assign to variables, `var` fields and list elements"),
                );
                let _ = self.expr(value, None);
                return None;
            }
        };
        let mut v = self.expr(value, Some(&place_ty));
        let local = match &place {
            Place::Local(id) => Some(*id),
            _ => None,
        };
        // `x += 1` on a smart-cast `x: Int?` is `x = x + 1` on its non-null value.
        if let (Some(op), Some(id), Type::Nullable(inner)) = (op, local, &place_ty)
            && self.nonnull.contains(&NullPath::local(id))
            && v.ty.compatible(inner)
        {
            let inner = (**inner).clone();
            let cur = Self::mk(TExprKind::Unwrap(Box::new(Self::mk(TExprKind::Local(id), place_ty.clone(), target.span))), inner.clone(), target.span);
            let ok = match op {
                BinOp::Add => matches!(inner, Type::Int | Type::Float | Type::Str),
                _ => inner.is_numeric(),
            };
            if ok {
                let sum = Self::mk(TExprKind::Binary(op, Box::new(cur), Box::new(v)), inner, span);
                let value = self.coerce(sum, &place_ty, None);
                return Some(TStmt::Assign { place, op: None, value, span });
            }
            let a = self.tn(&inner);
            self.err(Diagnostic::error(format!("cannot apply `{}=` to `{a}`", op.symbol()), span).with_label(target.span, format!("this is `{a}`")));
            return None;
        }
        let known_nonnull = !v.ty.is_nullable();
        if let Some(id) = local {
            // Reassigning a local ends what we knew about it, unless the new value is non-null.
            self.nonnull.retain(|p| p.root != id);
            if place_ty.is_nullable() && known_nonnull {
                self.add_facts(vec![NullPath::local(id)]);
            }
        }
        match op {
            None => v = self.coerce(v, &place_ty, None),
            Some(op) if place_ty.is_nullable() && !place_ty.is_error() && v.ty.compatible(place_ty.non_null()) => {
                let tn = self.tn(&place_ty);
                let t = self.snippet(target.span).unwrap_or_else(|| "x".into());
                let rhs = self.snippet(value.span).unwrap_or_else(|| "...".into());
                let default = match place_ty.non_null() {
                    Type::Float => "0.0",
                    Type::Str => "\"\"",
                    _ => "0",
                };
                let mut d = Diagnostic::error(format!("`{t}` might be null"), target.span).with_primary(format!("this is `{tn}`"));
                d = if local.is_some() {
                    d.with_help(format!("check `if {t} != null:` first, or write `{t} = ({t} ?: {default}) {} {rhs}`", op.symbol()))
                } else {
                    d.with_help(format!("write `{t} = ({t} ?: {default}) {} {rhs}`", op.symbol()))
                };
                self.err(d);
            }
            Some(op) => {
                if !place_ty.is_error() && !v.ty.is_error() {
                    let ok = place_ty == v.ty
                        && match op {
                            BinOp::Add => matches!(place_ty, Type::Int | Type::Float | Type::Str),
                            _ => place_ty.is_numeric(),
                        };
                    if !ok {
                        let (a, b) = (self.tn(&place_ty), self.tn(&v.ty));
                        self.err(
                            Diagnostic::error(format!("cannot apply `{}=` to `{a}` and `{b}`", op.symbol()), span)
                                .with_label(target.span, format!("this is `{a}`"))
                                .with_label(value.span, format!("this is `{b}`")),
                        );
                    }
                }
            }
        }
        Some(TStmt::Assign { place, op, value: v, span })
    }

    /// Check that `e` fits where a value of type `expected` is needed, turning
    /// a `T` into a `T?` (and typing `null`) when the expected type is nullable.
    fn coerce(&mut self, e: TExpr, expected: &Type, annotation: Option<Span>) -> TExpr {
        if e.ty.compatible(expected) || e.ty == Type::Unit && *expected == Type::Unit {
            return e;
        }
        if let Type::Nullable(inner) = expected {
            if e.ty == Type::Null {
                return Self::mk(TExprKind::Null, expected.clone(), e.span);
            }
            if e.ty.compatible(inner) && e.ty != Type::Unit {
                let span = e.span;
                return Self::mk(TExprKind::Wrap(Box::new(e)), expected.clone(), span);
            }
        }
        self.expect_type(&e, expected, annotation);
        e
    }

    fn expect_type(&mut self, e: &TExpr, expected: &Type, annotation: Option<Span>) {
        if e.ty.compatible(expected) || e.ty == Type::Unit && *expected == Type::Unit {
            return;
        }
        let (exp, found) = (self.tn(expected), self.tn(&e.ty));
        let mut d = Diagnostic::error(format!("mismatched types: expected `{exp}`, found `{found}`"), e.span)
            .with_primary(format!("expected `{exp}`, found `{found}`"));
        if let Some(a) = annotation {
            d = d.with_label(a, "expected because of this annotation");
        }
        let snip = self.snippet(e.span).unwrap_or_else(|| "value".into());
        d = match (expected, &e.ty) {
            (_, Type::Null) => d
                .with_primary(format!("`{exp}` can't be `null`"))
                .with_help(format!("if it may be absent, make the type nullable: `{exp}?`")),
            (exp_ty, Type::Nullable(inner)) if exp_ty.compatible(inner) => {
                d = d.with_primary("this might be `null`").with_help(format!(
                    "give a default with `{snip} ?: ...`, check `if {snip} != null:` first, or assert it isn't with `{snip}!!`"
                ));
                match self.unstable_note(e) {
                    Some(n) => d.with_note(n),
                    None => d,
                }
            }
            (Type::Float, Type::Int) => d.with_help("Isma never converts implicitly; use `.to_float()`, or write a float literal like `1.0`"),
            (Type::Int, Type::Float) => d.with_help("Isma never converts implicitly; use `.to_int()` to truncate"),
            (Type::Str, Type::Int | Type::Float | Type::Bool) => d.with_help("use `.to_string()` or an f-string like f\"{x}\""),
            _ => d,
        };
        self.err(d);
    }

    fn no_field(&mut self, ty: &Type, field: &Ident) {
        let tn = self.tn(ty);
        let mut d = Diagnostic::error(format!("no field `{}` on type `{tn}`", field.name), field.span).with_primary("unknown field");
        if let Type::Enum(sid, _) = ty {
            let sid = *sid;
            if self.methods.contains_key(&(sid, field.name.clone())) {
                d = d.with_help(format!("`{}` is a method; call it with `.{}()`", field.name, field.name));
            } else {
                let example = self.structs[sid]
                    .variants()
                    .iter()
                    .find(|v| v.fields.iter().any(|f| f.name == field.name))
                    .or(self.structs[sid].variants().first())
                    .map(|v| if v.fields.is_empty() { v.name.clone() } else { format!("{}({})", v.name, v.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")) })
                    .unwrap_or_default();
                d = d.with_note("the data inside an enum value is taken apart with `match`").with_help(format!("match on it: `case {example}:`"));
            }
            self.err(d);
            return;
        }
        if let Type::Struct(sid) = ty {
            let sid = *sid;
            if self.methods.contains_key(&(sid, field.name.clone())) {
                d = d.with_help(format!("`{}` is a method; call it with `.{}()`", field.name, field.name));
            } else if let Some(s) = suggest(&field.name, self.structs[sid].fields.iter().map(|f| f.name.as_str())) {
                d = d.with_help(format!("did you mean `{s}`?"));
            } else {
                let names: Vec<_> = self.structs[sid].fields.iter().map(|f| format!("`{}`", f.name)).collect();
                if !names.is_empty() {
                    d = d.with_note(format!("available fields: {}", names.join(", ")));
                }
            }
        }
        self.err(d);
    }

    fn index_elem_type(&mut self, list: &TExpr, index: &TExpr, span: Span) -> Type {
        if index.ty == Type::Int.nullable() {
            self.null_use_error(index, NullUse::Value);
        } else if !index.ty.compatible(&Type::Int) {
            let found = self.tn(&index.ty);
            self.err(Diagnostic::error(format!("list index must be `Int`, found `{found}`"), index.span));
        }
        match &list.ty {
            Type::List(t) => (**t).clone(),
            Type::Error => Type::Error,
            Type::Nullable(inner) if matches!(**inner, Type::List(_)) => {
                self.null_use_error(list, NullUse::Index);
                Type::Error
            }
            Type::Str => {
                self.err(Diagnostic::error("strings cannot be indexed yet", span));
                Type::Error
            }
            other => {
                let found = self.tn(other);
                self.err(Diagnostic::error(format!("cannot index into a value of type `{found}`"), list.span).with_primary("not a list"));
                Type::Error
            }
        }
    }

    // ---------- expressions ----------

    fn mk(kind: TExprKind, ty: Type, span: Span) -> TExpr {
        TExpr { kind, ty, span }
    }

    fn error_expr(span: Span) -> TExpr {
        Self::mk(TExprKind::Int(0), Type::Error, span)
    }

    fn expr(&mut self, e: &Expr, expected: Option<&Type>) -> TExpr {
        let t = self.expr_inner(e, expected);
        self.ide.expr_types.push((e.span, t.ty.clone()));
        t
    }

    fn expr_inner(&mut self, e: &Expr, expected: Option<&Type>) -> TExpr {
        let span = e.span;
        match &e.kind {
            ExprKind::Int(v) => Self::mk(TExprKind::Int(*v), Type::Int, span),
            ExprKind::Float(v) => Self::mk(TExprKind::Float(*v), Type::Float, span),
            ExprKind::Bool(v) => Self::mk(TExprKind::Bool(*v), Type::Bool, span),
            ExprKind::Str(s) => Self::mk(TExprKind::Str(s.clone()), Type::Str, span),
            ExprKind::Error => Self::error_expr(span),
            ExprKind::FStr(parts) => {
                let mut out = Vec::new();
                for p in parts {
                    match p {
                        FStrPart::Lit(s) => out.push(Self::mk(TExprKind::Str(s.clone()), Type::Str, span)),
                        FStrPart::Expr(pe) => {
                            let t = self.expr(pe, None);
                            out.push(self.stringify(t, pe.span));
                        }
                    }
                }
                Self::mk(TExprKind::FString(out), Type::Str, span)
            }
            ExprKind::Name(name) => {
                if let Some(b) = self.lookup(name) {
                    let (id, decl) = (b.id, b.span);
                    let ty = self.locals[id].ty.clone();
                    let smart = ty.is_nullable() && self.is_smart_cast(&NullPath::local(id));
                    self.local_use(id, decl, span, smart);
                    self.local_meta[id].used = true;
                    return self.narrow(Self::mk(TExprKind::Local(id), ty, span));
                }
                if let Some(module_ns) = self.imported_module(name) {
                    self.module_hover(module_ns, span);
                    self.err(
                        Diagnostic::error(format!("`{name}` is a module, not a value"), span)
                            .with_help(format!("use one of its functions, e.g. `{name}.{}(...)`", self.first_module_fn(module_ns))),
                    );
                    return Self::error_expr(span);
                }
                if self.import_failed(name) {
                    return Self::error_expr(span);
                }
                let d = if self.visible_func(name).is_some() || BUILTINS.contains(&name.as_str()) {
                    Diagnostic::error(format!("`{name}` is a function, not a value"), span)
                        .with_help(format!("call it: `{name}(...)`"))
                        .with_note("functions are not first-class values yet")
                } else if let Some(sid) = self.visible_struct(name) {
                    match self.structs[sid].variants() {
                        [] if self.structs[sid].variants.is_none() => {
                            Diagnostic::error(format!("`{name}` is a struct, not a value"), span).with_help(format!("construct one with `{name}(...)`"))
                        }
                        vs => Diagnostic::error(format!("`{name}` is an enum, not a value"), span)
                            .with_help(format!("pick a variant, e.g. `{name}.{}`", vs.first().map(|v| v.name.as_str()).unwrap_or("..."))),
                    }
                } else {
                    let mut d = Diagnostic::error(format!("cannot find value `{name}` in this scope"), span).with_primary("not found in this scope");
                    let candidates: Vec<String> = self.scopes.iter().flat_map(|s| s.keys().cloned()).collect();
                    let hint = match name.as_str() {
                        "True" => Some("true".to_string()),
                        "False" => Some("false".to_string()),
                        _ if self.module_file_nearby(name) => {
                            d = d.with_help(format!("`{name}.isma` is a module next to this file: add `import {name}` at the top"));
                            None
                        }
                        _ if crate::stdlib::find(name).is_some() => {
                            d = d.with_help(format!("`{name}` is a standard-library module: add `import {name}` at the top of the file"));
                            None
                        }
                        "None" | "nil" | "NULL" | "nullptr" => Some("null".to_string()),
                        _ => suggest(name, candidates.iter().map(|s| s.as_str())),
                    };
                    if let Some(h) = hint {
                        d = d.with_help(format!("did you mean `{h}`?"));
                    }
                    d
                };
                self.err(d);
                Self::error_expr(span)
            }
            ExprKind::Binary(op, l, r) => self.binary(*op, l, r, span),
            ExprKind::Unary(op, inner) => {
                let t = self.expr(inner, None);
                match op {
                    UnOp::Neg => {
                        if matches!(&t.ty, Type::Nullable(i) if i.is_numeric()) {
                            self.null_use_error(&t, NullUse::Value);
                            return Self::error_expr(span);
                        }
                        if !t.ty.is_numeric() && !t.ty.is_error() {
                            let found = self.tn(&t.ty);
                            self.err(Diagnostic::error(format!("cannot negate a value of type `{found}`"), span));
                            return Self::error_expr(span);
                        }
                        let ty = t.ty.clone();
                        Self::mk(TExprKind::Neg(Box::new(t)), ty, span)
                    }
                    UnOp::Not => {
                        if t.ty == Type::Bool.nullable() {
                            self.null_use_error(&t, NullUse::Value);
                        } else if !t.ty.compatible(&Type::Bool) {
                            let found = self.tn(&t.ty);
                            self.err(Diagnostic::error(format!("`not` requires a `Bool`, found `{found}`"), inner.span));
                        }
                        Self::mk(TExprKind::Not(Box::new(t)), Type::Bool, span)
                    }
                }
            }
            ExprKind::Call { callee, args } => self.call(callee, args, span, expected),
            ExprKind::Field(obj, field) => {
                if let Some(sid) = self.enum_ref(obj) {
                    return self.variant_value(sid, field, None, span);
                }
                let t = self.expr(obj, None);
                self.field_access(t, field, span)
            }
            ExprKind::SafeField(obj, field) => {
                let t = self.expr(obj, None);
                let op = Span { start: obj.span.end, end: field.span.start };
                self.safe_access(t, op, span, |c, it| c.field_access(it, field, span))
            }
            ExprKind::Null => {
                // Typed by what it's used as (see `coerce`).
                match expected {
                    Some(t @ Type::Nullable(_)) => Self::mk(TExprKind::Null, t.clone(), span),
                    _ => Self::mk(TExprKind::Null, Type::Null, span),
                }
            }
            ExprKind::NotNull(inner) => {
                let t = self.expr(inner, expected.map(|t| t.clone().nullable()).as_ref());
                match t.ty.clone() {
                    Type::Nullable(ty) => {
                        // Past this point it isn't null (or the program stopped).
                        self.add_facts(Self::nonnull_when_nonnull(&t));
                        Self::mk(TExprKind::NotNull(Box::new(t)), *ty, span)
                    }
                    Type::Error => t,
                    Type::Null => {
                        self.err(Diagnostic::error("`!!` on `null` always panics", span));
                        Self::error_expr(span)
                    }
                    ty => {
                        let tn = self.tn(&ty);
                        self.diags.push(
                            Diagnostic::warning("unnecessary `!!`: this value is never `null`", Span { start: inner.span.end, end: span.end })
                                .with_label(inner.span, format!("this is `{tn}`"))
                                .with_help("remove the `!!`"),
                        );
                        t
                    }
                }
            }
            ExprKind::Elvis(l, r) => self.elvis(l, r, span, expected),
            ExprKind::Jump(j) => {
                let stmt = match j {
                    Jump::Break => self.loop_jump(true, span),
                    Jump::Continue => self.loop_jump(false, span),
                    Jump::Return(v) => self.return_stmt(v.as_deref(), span),
                };
                Self::mk(TExprKind::Jump(Box::new(stmt)), Type::Unit, span)
            }
            ExprKind::Index(list, index) => {
                let tl = self.expr(list, None);
                let ti = self.expr(index, Some(&Type::Int));
                let elem = self.index_elem_type(&tl, &ti, span);
                Self::mk(TExprKind::Index(Box::new(tl), Box::new(ti)), elem, span)
            }
            ExprKind::List(elems) => {
                let expected_elem = match expected.map(Type::non_null) {
                    Some(Type::List(t)) => Some((**t).clone()),
                    _ => None,
                };
                if elems.is_empty() {
                    return match expected_elem {
                        Some(t) => Self::mk(TExprKind::ListLit(Vec::new()), Type::List(Box::new(t)), span),
                        None => {
                            self.err(
                                Diagnostic::error("cannot infer the element type of an empty list", span)
                                    .with_help("add a type annotation, e.g. `var xs: List[Int] = []`"),
                            );
                            Self::error_expr(span)
                        }
                    };
                }
                let mut out = Vec::new();
                let mut elem_ty = expected_elem.clone();
                for el in elems {
                    let t = self.expr(el, elem_ty.as_ref());
                    match &elem_ty {
                        None => {
                            if t.ty == Type::Unit {
                                self.err(Diagnostic::error("list elements must have a value", el.span));
                            }
                            elem_ty = Some(t.ty.clone());
                        }
                        // `[1, null]` is a `List[Int?]`.
                        Some(et) if expected_elem.is_none() && !t.ty.assignable(et) && join_types(et, &t.ty).is_some() => {
                            elem_ty = join_types(et, &t.ty);
                        }
                        Some(et) => {
                            if !t.ty.assignable(et) {
                                let (a, b) = (self.tn(et), self.tn(&t.ty));
                                let mut d = Diagnostic::error(format!("list elements must all have the same type: expected `{a}`, found `{b}`"), el.span)
                                    .with_primary(format!("this is `{b}`"));
                                if expected_elem.is_none() {
                                    d = d.with_label(elems[0].span, format!("the element type was inferred as `{a}` from this"));
                                }
                                self.err(d);
                            }
                        }
                    }
                    out.push(t);
                }
                let et = elem_ty.unwrap();
                if et == Type::Null {
                    self.err(
                        Diagnostic::error("cannot infer the element type of a list of `null`s", span)
                            .with_help("add a type annotation, e.g. `val xs: List[Int?] = [null]`"),
                    );
                    return Self::error_expr(span);
                }
                let out = out.into_iter().map(|t| if t.ty.assignable(&et) { self.coerce(t, &et, None) } else { t }).collect();
                Self::mk(TExprKind::ListLit(out), Type::List(Box::new(et)), span)
            }
        }
    }

    /// `t.field` (`t` already checked).
    fn field_access(&mut self, t: TExpr, field: &Ident, span: Span) -> TExpr {
        match &t.ty {
            Type::Struct(sid) => {
                let sid = *sid;
                match self.structs[sid].fields.iter().position(|f| f.name == field.name) {
                    Some(idx) => {
                        let ty = self.structs[sid].fields[idx].ty.clone();
                        self.field_use(sid, idx, field.span);
                        self.narrow(Self::mk(TExprKind::Field(Box::new(t), sid, idx), ty, span))
                    }
                    None => {
                        self.no_field(&Type::Struct(sid), field);
                        Self::error_expr(span)
                    }
                }
            }
            Type::Error => Self::error_expr(span),
            Type::Nullable(inner) if matches!(**inner, Type::Struct(_)) => {
                if let Type::Struct(sid) = **inner
                    && let Some(idx) = self.structs[sid].fields.iter().position(|f| f.name == field.name)
                {
                    self.field_use(sid, idx, field.span);
                }
                self.null_use_error(&t, NullUse::Member(&field.name, None));
                Self::error_expr(span)
            }
            other => {
                let other = other.clone();
                let mut d_ty = other.clone();
                if matches!(other, Type::List(_) | Type::Str) && field.name == "len" {
                    d_ty = Type::Error;
                    self.err(Diagnostic::error("`len` is a method", field.span).with_help("call it with `.len()`"));
                }
                if !d_ty.is_error() {
                    self.no_field(&other, field);
                }
                Self::error_expr(span)
            }
        }
    }

    /// `l ?: r`: `l` if it isn't null, otherwise `r` (which may also leave with
    /// `return`/`break`/`continue`/`panic`, and then the result is never null).
    fn elvis(&mut self, l: &Expr, r: &Expr, span: Span, expected: Option<&Type>) -> TExpr {
        let tl = self.expr(l, expected.map(|t| t.clone().nullable()).as_ref());
        let inner = match tl.ty.clone() {
            Type::Nullable(inner) => *inner,
            Type::Error => {
                let _ = self.expr(r, None);
                return Self::error_expr(span);
            }
            Type::Null => {
                self.err(Diagnostic::error("the left side of `?:` is always `null`", l.span).with_help("use the right side directly"));
                let _ = self.expr(r, None);
                return Self::error_expr(span);
            }
            ty => {
                let tn = self.tn(&ty);
                self.diags.push(
                    Diagnostic::warning("unnecessary `?:`: the left side is never `null`", r.span)
                        .with_primary("this is never used")
                        .with_label(l.span, format!("this is `{tn}`"))
                        .with_help("remove the `?:` and its right side"),
                );
                let saved = self.nonnull.clone();
                let _ = self.expr(r, Some(&ty));
                self.nonnull = saved;
                return tl;
            }
        };
        // `r` only runs when `l` is null: smart casts it causes don't last.
        let saved = self.nonnull.clone();
        let tr = self.expr(r, Some(&inner));
        self.nonnull = saved;
        if tr.diverges() {
            // After `x ?: return`, `x` isn't null.
            self.add_facts(Self::nonnull_when_nonnull(&tl));
            return Self::mk(TExprKind::Elvis(Box::new(tl), Box::new(tr)), inner, span);
        }
        let ty = if tr.ty.assignable(&inner) {
            inner
        } else if tr.ty.assignable(&inner.clone().nullable()) {
            inner.nullable()
        } else {
            let (a, b) = (self.tn(&tl.ty), self.tn(&tr.ty));
            self.err(
                Diagnostic::error(format!("the two sides of `?:` have different types: `{a}` and `{b}`"), span)
                    .with_label(l.span, format!("this is `{a}`"))
                    .with_label(r.span, format!("this is `{b}`"))
                    .with_note("the right side is the value to use when the left side is `null`, so it must have the same type"),
            );
            Type::Error
        };
        let tr = if ty.is_error() { tr } else { self.coerce(tr, &ty, None) };
        Self::mk(TExprKind::Elvis(Box::new(tl), Box::new(tr)), ty, span)
    }

    /// Convert a value to String for f-strings.
    fn stringify(&mut self, t: TExpr, span: Span) -> TExpr {
        match &t.ty {
            Type::Str => t,
            Type::Int | Type::Float | Type::Bool | Type::Enum(_, false) => Self::mk(TExprKind::Builtin(Builtin::ToStr, vec![t]), Type::Str, span),
            Type::Error => t,
            Type::Null => Self::mk(TExprKind::Str("null".into()), Type::Str, span),
            // `x?.to_string() ?: "null"`
            Type::Nullable(inner) if inner.is_primitive() => {
                let inner = (**inner).clone();
                let binder = self.hidden_local(inner.clone());
                let it = Self::mk(TExprKind::Local(binder), inner, span);
                let body = self.stringify(it, span);
                let safe = Self::mk(TExprKind::SafeCall { recv: Box::new(t), binder, body: Box::new(body) }, Type::Str.nullable(), span);
                let null = Self::mk(TExprKind::Str("null".into()), Type::Str, span);
                Self::mk(TExprKind::Elvis(Box::new(safe), Box::new(null)), Type::Str, span)
            }
            other => {
                let found = self.tn(other);
                self.err(
                    Diagnostic::error(format!("cannot format a value of type `{found}` in an f-string"), span)
                        .with_note("only `Int`, `Float`, `Bool` and `String` (and their nullable versions) can be formatted"),
                );
                Self::error_expr(span)
            }
        }
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr, span: Span) -> TExpr {
        if matches!(op, BinOp::And | BinOp::Or) {
            let tl = self.expect_bool(l, &format!("left operand of `{}`", op.symbol()));
            // `x != null and x.ok()`: the right side only runs when the left allows it.
            let (when_true, when_false) = Self::cond_facts(&tl);
            let saved = self.nonnull.clone();
            self.add_facts(if op == BinOp::And { when_true } else { when_false });
            let tr = self.expect_bool(r, &format!("right operand of `{}`", op.symbol()));
            self.nonnull = saved;
            let kind = if op == BinOp::And {
                TExprKind::And(Box::new(tl), Box::new(tr))
            } else {
                TExprKind::Or(Box::new(tl), Box::new(tr))
            };
            return Self::mk(kind, Type::Bool, span);
        }
        if matches!(op, BinOp::Eq | BinOp::Ne) && (matches!(l.kind, ExprKind::Null) || matches!(r.kind, ExprKind::Null)) {
            return self.null_comparison(op, l, r, span);
        }
        let tl = self.expr(l, None);
        let tr = self.expr(r, Some(&tl.ty));
        if tl.ty.is_error() || tr.ty.is_error() {
            let ty = if op.is_comparison() { Type::Bool } else { Type::Error };
            return Self::mk(TExprKind::Binary(op, Box::new(tl), Box::new(tr)), ty, span);
        }
        if tl.ty.is_nullable() || tr.ty.is_nullable() {
            let base = tl.ty.non_null().clone();
            if base == *tr.ty.non_null() {
                // `x == 5` and `a == b` work on nullable primitives (null equals only null).
                if matches!(op, BinOp::Eq | BinOp::Ne) && base.is_primitive() {
                    let nt = base.nullable();
                    let (a, b) = (self.coerce(tl, &nt, None), self.coerce(tr, &nt, None));
                    return Self::mk(TExprKind::Binary(op, Box::new(a), Box::new(b)), Type::Bool, span);
                }
                let works = match op {
                    BinOp::Add => matches!(base, Type::Int | Type::Float | Type::Str),
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => matches!(base, Type::Int | Type::Float | Type::Str),
                    BinOp::Eq | BinOp::Ne => false,
                    _ => base.is_numeric(),
                };
                if works {
                    let culprit = if tl.ty.is_nullable() { &tl } else { &tr };
                    self.null_use_error(culprit, NullUse::Value);
                    let ty = if op.is_comparison() { Type::Bool } else { Type::Error };
                    return Self::mk(TExprKind::Binary(op, Box::new(tl), Box::new(tr)), ty, span);
                }
            }
        }
        let same = tl.ty == tr.ty;
        let ok = same
            && match op {
                BinOp::Add => matches!(tl.ty, Type::Int | Type::Float | Type::Str),
                BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => tl.ty.is_numeric(),
                BinOp::Eq | BinOp::Ne => tl.ty.is_primitive(),
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => matches!(tl.ty, Type::Int | Type::Float | Type::Str),
                BinOp::And | BinOp::Or => unreachable!(),
            };
        if !ok {
            let (a, b) = (self.tn(&tl.ty), self.tn(&tr.ty));
            let sym = op.symbol();
            let msg = if same {
                format!("operator `{sym}` cannot be applied to `{a}`")
            } else {
                format!("operator `{sym}` cannot be applied to `{a}` and `{b}`")
            };
            let mut d = Diagnostic::error(msg, span)
                .with_label(l.span, format!("this is `{a}`"))
                .with_label(r.span, format!("this is `{b}`"));
            d = match (&tl.ty, &tr.ty) {
                (Type::Int, Type::Float) => d.with_help("Isma never converts implicitly; use `.to_float()` on the `Int` side"),
                (Type::Float, Type::Int) => d.with_help("Isma never converts implicitly; use `.to_float()` on the `Int` side"),
                (Type::Str, Type::Int | Type::Float | Type::Bool) | (Type::Int | Type::Float | Type::Bool, Type::Str) if op == BinOp::Add => {
                    d.with_help("use an f-string like f\"{a}{b}\" or call `.to_string()`")
                }
                (Type::Struct(sid), _) | (_, Type::Struct(sid)) if self.structs[*sid].name == "time.DateTime" => {
                    let help = match op {
                        BinOp::Sub => "to get the time between two moments use `a.millis_since(b)` or `a.seconds_since(b)`",
                        BinOp::Add => "to move a moment use `.add_seconds(n)`, `.add_minutes(n)`, `.add_days(n)`, `.add_months(n)`...",
                        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => "to compare moments use `a.is_before(b)`, `a.is_after(b)` or `a.compare(b)`",
                        BinOp::Eq | BinOp::Ne => "to check for the same moment use `a.equals(b)`",
                        _ => "use the methods of `time.DateTime` (e.g. `millis_since`, `add_seconds`)",
                    };
                    d.with_help(help).with_note("Isma has no operator overloading: operators work on numbers, `Bool` and `String`")
                }
                (Type::Enum(..), _) | (_, Type::Enum(..)) if op.is_comparison() => d
                    .with_note("`==` works on enums whose variants carry no data; this one's do")
                    .with_help("use `match` to look at the variant and its data"),
                (Type::Struct(_) | Type::List(_), _) if op.is_comparison() => {
                    d.with_note("`==` is only defined for `Int`, `Float`, `Bool`, `String` and enums without data")
                }
                _ => d,
            };
            self.err(d);
            let ty = if op.is_comparison() { Type::Bool } else { Type::Error };
            return Self::mk(TExprKind::Binary(op, Box::new(tl), Box::new(tr)), ty, span);
        }
        let ty = if op.is_comparison() { Type::Bool } else { tl.ty.clone() };
        Self::mk(TExprKind::Binary(op, Box::new(tl), Box::new(tr)), ty, span)
    }

    /// `x == null` / `x != null`.
    fn null_comparison(&mut self, op: BinOp, l: &Expr, r: &Expr, span: Span) -> TExpr {
        let (other, null) = if matches!(l.kind, ExprKind::Null) { (r, l) } else { (l, r) };
        if matches!(other.kind, ExprKind::Null) {
            self.diags.push(Diagnostic::warning("comparing `null` with `null`", span).with_primary(format!("this is always `{}`", op == BinOp::Eq)));
            return Self::mk(TExprKind::Bool(op == BinOp::Eq), Type::Bool, span);
        }
        let t = self.expr(other, None);
        self.ide.expr_types.push((null.span, t.ty.clone().nullable()));
        if t.ty == Type::Unit {
            self.err(Diagnostic::error("this expression has no value to compare with `null`", other.span));
        } else if !t.ty.is_nullable() && !t.ty.is_error() {
            let tn = self.tn(&t.ty);
            let what = self.snippet(other.span).map(|s| format!("`{s}`")).unwrap_or_else(|| "this value".into());
            let mut d = Diagnostic::warning(format!("{what} is never `null`, so this is always `{}`", op == BinOp::Ne), span)
                .with_label(other.span, format!("this is `{tn}`"));
            if matches!(t.kind, TExprKind::Unwrap(_)) {
                d = d.with_note("it was already checked for `null` above");
            }
            self.diags.push(d);
        }
        let is_null = Self::mk(TExprKind::IsNull(Box::new(t)), Type::Bool, span);
        if op == BinOp::Eq { is_null } else { Self::mk(TExprKind::Not(Box::new(is_null)), Type::Bool, span) }
    }

    /// Match positional/named arguments against parameters; returns args in parameter order.
    fn match_args(&mut self, what: &str, params: &[(String, Type)], args: &[Arg], span: Span) -> Option<Vec<TExpr>> {
        let mut slots: Vec<Option<TExpr>> = (0..params.len()).map(|_| None).collect();
        let mut ok = true;
        let mut seen_named = false;
        for (i, a) in args.iter().enumerate() {
            let idx = match &a.name {
                None => {
                    if seen_named {
                        self.err(Diagnostic::error("positional arguments must come before named arguments", a.value.span));
                        ok = false;
                        continue;
                    }
                    if i >= params.len() {
                        let _ = self.expr(&a.value, None);
                        ok = false;
                        continue;
                    }
                    i
                }
                Some(n) => {
                    seen_named = true;
                    match params.iter().position(|(pn, _)| *pn == n.name) {
                        Some(idx) => {
                            if slots[idx].is_some() {
                                self.err(Diagnostic::error(format!("argument `{}` is given more than once", n.name), n.span));
                                ok = false;
                                continue;
                            }
                            idx
                        }
                        None => {
                            let mut d = Diagnostic::error(format!("{what} has no parameter named `{}`", n.name), n.span);
                            if let Some(s) = suggest(&n.name, params.iter().map(|(p, _)| p.as_str())) {
                                d = d.with_help(format!("did you mean `{s}`?"));
                            }
                            self.err(d);
                            let _ = self.expr(&a.value, None);
                            ok = false;
                            continue;
                        }
                    }
                }
            };
            let pty = params[idx].1.clone();
            let t = self.expr(&a.value, Some(&pty));
            let t = self.coerce(t, &pty, None);
            slots[idx] = Some(t);
        }
        if args.len() > params.len() {
            self.err(
                Diagnostic::error(format!("{what} takes {} argument(s) but {} were given", params.len(), args.len()), span)
                    .with_primary(format!("expected {} argument(s)", params.len())),
            );
            return None;
        }
        let missing: Vec<String> = params
            .iter()
            .zip(&slots)
            .filter(|(_, s)| s.is_none())
            .map(|((n, t), _)| format!("`{n}: {}`", self.tn(t)))
            .collect();
        if !missing.is_empty() && ok {
            self.err(
                Diagnostic::error(format!("{what} is missing argument(s): {}", missing.join(", ")), span)
                    .with_primary(format!("expected {} argument(s), found {}", params.len(), args.len())),
            );
            return None;
        }
        if !ok || !missing.is_empty() {
            return None;
        }
        Some(slots.into_iter().map(|s| s.unwrap()).collect())
    }

    fn call(&mut self, callee: &Expr, args: &[Arg], span: Span, _expected: Option<&Type>) -> TExpr {
        match &callee.kind {
            ExprKind::Name(name) => {
                if self.lookup(name).is_some() {
                    self.err(Diagnostic::error(format!("`{name}` is a variable, not a function"), callee.span));
                    for a in args {
                        let _ = self.expr(&a.value, None);
                    }
                    return Self::error_expr(span);
                }
                if let Some(fid) = self.visible_func(name) {
                    let code = self.ide.functions[fid].signature.clone();
                    let fns = self.ide.functions[fid].ns;
                    let doc = (fns != self.ns).then(|| {
                        let d = self.ide.functions[fid].doc.as_ref().map(|d| format!("\n\n{d}")).unwrap_or_default();
                        format!("from module `{}`{d}", self.ns_names[fns])
                    });
                    self.hover(callee.span, code, doc);
                    self.ide.definitions.push((callee.span, self.funcs[fid].span));
                    let params = self.funcs[fid].params.clone();
                    let ret = self.funcs[fid].ret.clone();
                    let what = format!("function `{name}`");
                    return match self.match_args(&what, &params, args, span) {
                        Some(targs) => Self::mk(TExprKind::Call(fid, targs), ret, span),
                        None => Self::mk(TExprKind::Int(0), if ret == Type::Unit { Type::Unit } else { Type::Error }, span),
                    };
                }
                if let Some(sid) = self.visible_struct(name) {
                    return self.construct(sid, name, callee.span, args, span);
                }
                if self.ns_kinds[self.ns] == FileKind::Std && crate::stdlib::INTRINSICS.iter().any(|(n, _, _)| n == name) {
                    return self.intrinsic_call(name, args, span);
                }
                if BUILTINS.contains(&name.as_str()) {
                    if let Some((code, doc)) = builtin_function(name) {
                        self.hover(callee.span, code.to_string(), Some(doc.to_string()));
                    }
                    return self.builtin_call(name, args, span);
                }
                if self.import_failed(name) {
                    for a in args {
                        let _ = self.expr(&a.value, None);
                    }
                    return Self::error_expr(span);
                }
                let mut d = Diagnostic::error(format!("cannot find function `{name}` in this scope"), callee.span).with_primary("not found");
                let ns = self.ns;
                let names: Vec<String> = self
                    .func_index
                    .keys()
                    .filter(|(n, _)| *n == ns)
                    .map(|(_, s)| s.clone())
                    .chain(BUILTINS.iter().map(|s| s.to_string()))
                    .chain(self.struct_index.keys().filter(|(n, _)| *n == ns).map(|(_, s)| s.clone()))
                    .collect();
                if let Some(h) = self.module_hint(name) {
                    self.err(d.with_help(h));
                    for a in args {
                        let _ = self.expr(&a.value, None);
                    }
                    return Self::error_expr(span);
                }
                let hint = match name.as_str() {
                    "println" | "printf" | "echo" => Some("print".to_string()),
                    "len" => {
                        d = d.with_note("length is a method in Isma");
                        Some("x.len()".to_string())
                    }
                    "str" | "int" | "float" => {
                        d = d.with_note("conversions are methods: `.to_string()`, `.to_int()`, `.to_float()`");
                        None
                    }
                    _ => suggest(name, names.iter().map(|s| s.as_str())),
                };
                if let Some(h) = hint {
                    d = d.with_help(format!("did you mean `{h}`?"));
                }
                self.err(d);
                for a in args {
                    let _ = self.expr(&a.value, None);
                }
                Self::error_expr(span)
            }
            ExprKind::Field(obj, method) => {
                if let Some(sid) = self.enum_ref(obj) {
                    return self.variant_value(sid, method, Some(args), span);
                }
                if let ExprKind::Name(m) = &obj.kind
                    && self.lookup(m).is_none()
                    && let Some(module_ns) = self.imported_module(m)
                {
                    self.module_hover(module_ns, obj.span);
                    return self.module_call(module_ns, m, method, args, span);
                }
                let recv = self.expr(obj, None);
                self.method_call(recv, method, args, span)
            }
            ExprKind::SafeField(obj, method) => {
                let recv = self.expr(obj, None);
                let op = Span { start: obj.span.end, end: method.span.start };
                self.safe_access(recv, op, span, |c, it| c.method_call(it, method, args, span))
            }
            _ => {
                let _ = self.expr(callee, None);
                self.err(Diagnostic::error("this expression is not callable", callee.span));
                Self::error_expr(span)
            }
        }
    }

    /// `Point(1, 2)` / `geometry.Point(1, 2)`: build struct `sid` (`name` as written).
    fn construct(&mut self, sid: StructId, name: &str, name_span: Span, args: &[Arg], span: Span) -> TExpr {
        self.pending_struct_hovers.push((name_span, sid));
        self.ide.definitions.push((name_span, self.struct_spans[sid]));
        let sns = self.struct_ns[sid];
        let refuse = |c: &mut Checker, d: Diagnostic| {
            c.err(d);
            for a in args {
                let _ = c.expr(&a.value, None);
            }
            Self::error_expr(span)
        };
        if let Some(first) = self.structs[sid].variants.as_ref().map(|vs| vs.first().map(|v| v.name.clone()).unwrap_or_default()) {
            return refuse(
                self,
                Diagnostic::error(format!("`{name}` is an enum: a value is one of its variants"), name_span)
                    .with_primary("not a constructor")
                    .with_help(format!("build a variant, e.g. `{name}.{first}(...)`")),
            );
        }
        let private: Vec<String> = self.structs[sid].fields.iter().filter(|f| is_private(&f.name)).map(|f| format!("`{}`", f.name)).collect();
        if sns != self.ns && (self.ns_kinds[sns] == FileKind::Std || !private.is_empty()) {
            let module = self.ns_names[sns].clone();
            let full = self.structs[sid].name.clone();
            let ctor_fns: Vec<String> = self
                .ide
                .functions
                .iter()
                .filter(|f| f.ns == sns && f.owner.is_none() && !is_private(&f.name) && f.signature.ends_with(&format!("-> {full}")))
                .map(|f| if f.signature.contains("()") { format!("`{module}.{}()`", f.name) } else { format!("`{module}.{}(...)`", f.name) })
                .collect();
            let note = if self.ns_kinds[sns] == FileKind::Std {
                format!("structs from the `{module}` module are created by its functions, which validate the values")
            } else {
                format!("it has private fields ({}), so only the `{module}` module can create one", and_list(&private))
            };
            let mut d = Diagnostic::error(format!("`{full}` cannot be constructed directly"), name_span).with_primary("not constructible here").with_note(note);
            if !ctor_fns.is_empty() {
                d = d.with_help(format!("use {}", ctor_fns.join(", ")));
            }
            return refuse(self, d);
        }
        let params: Vec<(String, Type)> = self.structs[sid].fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect();
        let what = format!("struct `{name}`");
        match self.match_args(&what, &params, args, span) {
            Some(targs) => Self::mk(TExprKind::StructNew(sid, targs), Type::Struct(sid), span),
            None => Self::mk(TExprKind::Int(0), Type::Struct(sid), span),
        }
    }

    fn first_module_fn(&self, module_ns: usize) -> String {
        let mut names: Vec<&String> = self
            .func_index
            .iter()
            .filter(|((ns, n), fid)| *ns == module_ns && !is_private(n) && self.ide.functions[**fid].owner.is_none())
            .map(|((_, n), _)| n)
            .collect();
        names.sort();
        names.first().map(|s| s.to_string()).unwrap_or_else(|| "...".into())
    }

    /// `time.now()`: a call to a function in an imported module.
    fn module_call(&mut self, module_ns: usize, module: &str, func: &Ident, args: &[Arg], span: Span) -> TExpr {
        let key = (module_ns, func.name.clone());
        if let Some(&fid) = self.func_index.get(&key) {
            if is_private(&func.name) && module_ns != self.ns {
                self.err(
                    Diagnostic::error(format!("`{module}.{}` is private to the `{module}` module", func.name), func.span)
                        .with_note("names starting with `_` are internal to their module"),
                );
                for a in args {
                    let _ = self.expr(&a.value, None);
                }
                return Self::error_expr(span);
            }
            let f = &self.ide.functions[fid];
            let code = f.signature.clone();
            let doc = Some(format!("in module `{module}`{}", f.doc.as_ref().map(|d| format!("\n\n{d}")).unwrap_or_default()));
            self.hover(func.span, code, doc);
            self.ide.definitions.push((func.span, self.funcs[fid].span));
            let params = self.funcs[fid].params.clone();
            let ret = self.funcs[fid].ret.clone();
            let what = format!("function `{module}.{}`", func.name);
            return match self.match_args(&what, &params, args, span) {
                Some(targs) => Self::mk(TExprKind::Call(fid, targs), ret, span),
                None => Self::mk(TExprKind::Int(0), if ret == Type::Unit { Type::Unit } else { Type::Error }, span),
            };
        }
        if let Some(&sid) = self.struct_index.get(&key) {
            if is_private(&func.name) && module_ns != self.ns {
                self.err(Diagnostic::error(format!("`{module}.{}` is private to the `{module}` module", func.name), func.span).with_primary("private"));
                for a in args {
                    let _ = self.expr(&a.value, None);
                }
                return Self::error_expr(span);
            }
            return self.construct(sid, &format!("{module}.{}", func.name), func.span, args, span);
        }
        let candidates: Vec<String> = self
            .func_index
            .iter()
            .filter(|((ns, n), fid)| *ns == module_ns && !is_private(n) && self.ide.functions[**fid].owner.is_none())
            .map(|((_, n), _)| n.clone())
            .collect();
        let mut d = Diagnostic::error(format!("module `{module}` has no function `{}`", func.name), func.span).with_primary("not found");
        if let Some(h) = self.reexport_hint(module_ns, module, &func.name) {
            d = d.with_help(h);
        } else if let Some(sugg) = suggest(&func.name, candidates.iter().map(|s| s.as_str())) {
            d = d.with_help(format!("did you mean `{module}.{sugg}`?"));
        }
        self.err(d);
        for a in args {
            let _ = self.expr(&a.value, None);
        }
        Self::error_expr(span)
    }

    /// Hooks into the runtime that only standard-library modules may call.
    fn intrinsic_call(&mut self, name: &str, args: &[Arg], span: Span) -> TExpr {
        let (_, params, ret) = crate::stdlib::INTRINSICS.iter().find(|(n, _, _)| *n == name).unwrap();
        let ty_of = |t: &str| match t {
            "Int" => Type::Int,
            _ => Type::Unit,
        };
        if !self.arity(args, params.len(), &format!("`{name}`"), span) {
            return Self::error_expr(span);
        }
        let mut targs = Vec::new();
        for (a, p) in args.iter().zip(params.iter()) {
            let pt = ty_of(p);
            let t = self.expr(&a.value, Some(&pt));
            let t = self.coerce(t, &pt, None);
            targs.push(t);
        }
        let b = match name {
            "__realtime_ns" => Builtin::RealtimeNs,
            "__monotonic_ns" => Builtin::MonotonicNs,
            _ => Builtin::SleepNs,
        };
        Self::mk(TExprKind::Builtin(b, targs), ty_of(ret), span)
    }

    /// A built-in method that takes arguments of fixed types.
    #[allow(clippy::too_many_arguments)]
    fn builtin_method(&mut self, recv: TExpr, b: Builtin, params: &[Type], ret: Type, method: &Ident, args: &[Arg], span: Span) -> TExpr {
        let what = format!("method `{}`", method.name);
        if !self.arity(args, params.len(), &what, span) {
            return Self::error_expr(span);
        }
        let mut targs = vec![recv];
        for (a, p) in args.iter().zip(params) {
            let t = self.expr(&a.value, Some(p));
            let t = self.coerce(t, p, None);
            targs.push(t);
        }
        Self::mk(TExprKind::Builtin(b, targs), ret, span)
    }

    fn reject_named(&mut self, args: &[Arg], what: &str) -> bool {
        for a in args {
            if let Some(n) = &a.name {
                self.err(Diagnostic::error(format!("{what} does not take named arguments"), n.span));
                return true;
            }
        }
        false
    }

    fn arity(&mut self, args: &[Arg], n: usize, what: &str, span: Span) -> bool {
        if args.len() != n {
            self.err(
                Diagnostic::error(format!("{what} takes {n} argument(s) but {} were given", args.len()), span)
                    .with_primary(format!("expected {n} argument(s)")),
            );
            for a in args {
                let _ = self.expr(&a.value, None);
            }
            return false;
        }
        !self.reject_named(args, what)
    }

    fn builtin_call(&mut self, name: &str, args: &[Arg], span: Span) -> TExpr {
        let what = format!("`{name}`");
        let (n, param, ret, b) = match name {
            "print" => (1, None, Type::Unit, Builtin::Print),
            "input" => (0, None, Type::Str, Builtin::Input),
            "args" => (0, None, Type::List(Box::new(Type::Str)), Builtin::Args),
            "panic" => (1, Some(Type::Str), Type::Unit, Builtin::Panic),
            "assert" => (1, Some(Type::Bool), Type::Unit, Builtin::Assert),
            "sqrt" => (1, Some(Type::Float), Type::Float, Builtin::Sqrt),
            "exit" => (1, Some(Type::Int), Type::Unit, Builtin::Exit),
            _ => unreachable!(),
        };
        if !self.arity(args, n, &what, span) {
            return Self::mk(TExprKind::Int(0), if ret == Type::Unit { Type::Unit } else { Type::Error }, span);
        }
        let mut targs = Vec::new();
        for a in args {
            let mut t = self.expr(&a.value, param.as_ref());
            match &param {
                Some(p) => t = self.coerce(t, p, None),
                // print accepts any primitive, and prints `null` for a null one
                None if t.ty == Type::Null || t.ty.is_nullable() && t.ty.non_null().is_primitive() || matches!(t.ty, Type::Enum(_, false)) => {
                    t = self.stringify(t, a.value.span)
                }
                None => {
                    if !t.ty.is_primitive() && !t.ty.is_error() {
                        let found = self.tn(&t.ty);
                        self.err(
                            Diagnostic::error(format!("`print` cannot print a value of type `{found}`"), a.value.span)
                                .with_note("`print` accepts `Int`, `Float`, `Bool` and `String` (and their nullable versions)")
                                .with_help("print individual fields, or use an f-string"),
                        );
                    }
                }
            }
            if b == Builtin::Assert {
                // After `assert(x != null)`, `x` isn't null.
                let (when_true, _) = Self::cond_facts(&t);
                self.add_facts(when_true);
            }
            targs.push(t);
        }
        Self::mk(TExprKind::Builtin(b, targs), ret, span)
    }

    fn method_call(&mut self, recv: TExpr, method: &Ident, args: &[Arg], span: Span) -> TExpr {
        let m = method.name.as_str();
        let rty = recv.ty.clone();
        if !matches!(rty, Type::Struct(_) | Type::Error) {
            let tn = self.tn(&rty);
            if let Some(b) = builtin_methods(&rty, &tn, &|t| self.tn(t)).into_iter().find(|b| b.name == m) {
                self.hover(method.span, b.signature, Some(format!("{} (on `{tn}`)", b.doc)));
            }
        }
        let what = format!("method `{}`", m);
        let simple = |c: &mut Checker, recv: TExpr, b: Builtin, ret: Type, args: &[Arg]| -> TExpr {
            if !c.arity(args, 0, &what, span) {
                return Self::error_expr(span);
            }
            Self::mk(TExprKind::Builtin(b, vec![recv]), ret, span)
        };
        match (&rty, m) {
            (Type::Error, _) => {
                for a in args {
                    let _ = self.expr(&a.value, None);
                }
                Self::error_expr(span)
            }
            (Type::Nullable(_), _) => {
                self.null_use_error(&recv, NullUse::Member(m, Some(!args.is_empty())));
                for a in args {
                    let _ = self.expr(&a.value, None);
                }
                Self::error_expr(span)
            }
            (Type::Int | Type::Float | Type::Bool, "to_string") | (Type::Str, "to_string") => {
                if !self.arity(args, 0, &what, span) {
                    return Self::error_expr(span);
                }
                self.stringify(recv, span)
            }
            (Type::Int, "to_float") => simple(self, recv, Builtin::IntToFloat, Type::Float, args),
            (Type::Float, "to_int") => simple(self, recv, Builtin::FloatToInt, Type::Int, args),
            (Type::Int, "to_int") | (Type::Float, "to_float") => {
                if !self.arity(args, 0, &what, span) {
                    return Self::error_expr(span);
                }
                recv
            }
            (Type::Str, "len") => simple(self, recv, Builtin::StrLen, Type::Int, args),
            (Type::Str, "to_int") => simple(self, recv, Builtin::StrToInt, Type::Int, args),
            (Type::Str, "chars") => simple(self, recv, Builtin::StrChars, Type::List(Box::new(Type::Str)), args),
            (Type::Str, "substring") => self.builtin_method(recv, Builtin::StrSubstring, &[Type::Int, Type::Int], Type::Str, method, args, span),
            (Type::Str, "contains") => self.builtin_method(recv, Builtin::StrContains, &[Type::Str], Type::Bool, method, args, span),
            (Type::Str, "starts_with") => self.builtin_method(recv, Builtin::StrStartsWith, &[Type::Str], Type::Bool, method, args, span),
            (Type::Str, "ends_with") => self.builtin_method(recv, Builtin::StrEndsWith, &[Type::Str], Type::Bool, method, args, span),
            (Type::Str, "to_float") => simple(self, recv, Builtin::StrToFloat, Type::Float, args),
            (Type::Str, "to_int_or_null") => simple(self, recv, Builtin::StrToIntOrNull, Type::Int.nullable(), args),
            (Type::Str, "to_float_or_null") => simple(self, recv, Builtin::StrToFloatOrNull, Type::Float.nullable(), args),
            (Type::List(_), "len") => simple(self, recv, Builtin::ListLen, Type::Int, args),
            (Type::List(elem), "pop") => {
                let elem = (**elem).clone();
                simple(self, recv, Builtin::ListPop, elem, args)
            }
            (Type::List(elem), "push") => {
                let elem = (**elem).clone();
                if !self.arity(args, 1, &what, span) {
                    return Self::mk(TExprKind::Int(0), Type::Unit, span);
                }
                let v = self.expr(&args[0].value, Some(&elem));
                let v = self.coerce(v, &elem, None);
                Self::mk(TExprKind::Builtin(Builtin::ListPush, vec![recv, v]), Type::Unit, span)
            }
            (Type::Enum(sid, false), "to_string") if !self.methods.contains_key(&(*sid, m.to_string())) => {
                if !self.arity(args, 0, &what, span) {
                    return Self::error_expr(span);
                }
                self.stringify(recv, span)
            }
            (Type::Struct(sid) | Type::Enum(sid, _), _) => {
                let sid = *sid;
                match self.methods.get(&(sid, m.to_string())) {
                    Some(&fid) if is_private(m) && self.struct_ns[sid] != self.ns => {
                        let _ = fid;
                        let sname = self.structs[sid].name.clone();
                        self.err(Diagnostic::error(format!("method `{m}` of `{sname}` is private"), method.span));
                        for a in args {
                            let _ = self.expr(&a.value, None);
                        }
                        Self::error_expr(span)
                    }
                    Some(&fid) => {
                        let code = self.ide.functions[fid].signature.clone();
                        let comment = self.ide.functions[fid].doc.clone();
                        let doc = Some(format!("method of `{}`{}", self.structs[sid].name, comment.map(|c| format!("\n\n{c}")).unwrap_or_default()));
                        self.hover(method.span, code, doc);
                        self.ide.definitions.push((method.span, self.funcs[fid].span));
                        let params = self.funcs[fid].params[1..].to_vec();
                        let ret = self.funcs[fid].ret.clone();
                        let what = format!("method `{}`", self.funcs[fid].name);
                        match self.match_args(&what, &params, args, span) {
                            Some(mut targs) => {
                                targs.insert(0, recv);
                                Self::mk(TExprKind::Call(fid, targs), ret, span)
                            }
                            None => Self::mk(TExprKind::Int(0), if ret == Type::Unit { Type::Unit } else { Type::Error }, span),
                        }
                    }
                    None => {
                        let sname = self.structs[sid].name.clone();
                        let kind = if self.structs[sid].variants.is_some() { "enum" } else { "struct" };
                        let mut d = Diagnostic::error(format!("no method `{m}` on {kind} `{sname}`"), method.span).with_primary("method not found");
                        if self.structs[sid].fields.iter().any(|f| f.name == m) {
                            d = d.with_note(format!("`{m}` is a field, not a method; access it without parentheses: `.{m}`"));
                        } else {
                            let names: Vec<String> = self.methods.keys().filter(|(s, _)| *s == sid).map(|(_, n)| n.clone()).collect();
                            if let Some(s) = suggest(m, names.iter().map(|s| s.as_str())) {
                                d = d.with_help(format!("did you mean `{s}`?"));
                            }
                        }
                        self.err(d);
                        for a in args {
                            let _ = self.expr(&a.value, None);
                        }
                        Self::error_expr(span)
                    }
                }
            }
            _ => {
                let tn = self.tn(&rty);
                let available = match &rty {
                    Type::Int => "`to_string()`, `to_float()`",
                    Type::Float => "`to_string()`, `to_int()`",
                    Type::Bool => "`to_string()`",
                    Type::Str => "`len()`, `chars()`, `substring(start, end)`, `contains(s)`, `starts_with(s)`, `ends_with(s)`, `to_int()`, `to_float()`, `to_int_or_null()`, `to_float_or_null()`, `to_string()`",
                    Type::List(_) => "`len()`, `push(x)`, `pop()`",
                    _ => "none",
                };
                self.err(
                    Diagnostic::error(format!("no method `{m}` on type `{tn}`"), method.span)
                        .with_primary("method not found")
                        .with_note(format!("methods available on `{tn}`: {available}")),
                );
                for a in args {
                    let _ = self.expr(&a.value, None);
                }
                Self::error_expr(span)
            }
        }
    }
}

/// The type of a list literal holding both an `a` and a `b`, e.g. `Int` and `null` make `Int?`.
fn join_types(a: &Type, b: &Type) -> Option<Type> {
    if a.assignable(b) {
        Some(b.clone())
    } else if b.assignable(a) {
        Some(a.clone())
    } else {
        match (a, b) {
            (Type::Null, t) | (t, Type::Null) if *t != Type::Unit => Some(t.clone().nullable()),
            _ => None,
        }
    }
}

// ---------- definite-return analysis ----------

fn block_diverges(stmts: &[TStmt]) -> bool {
    stmts.iter().any(stmt_diverges)
}

fn stmt_diverges(s: &TStmt) -> bool {
    match s {
        TStmt::Return(_) => true,
        TStmt::Expr(e) => matches!(e.kind, TExprKind::Builtin(Builtin::Panic | Builtin::Exit, _)),
        TStmt::If { branches, else_block } => {
            else_block.as_ref().is_some_and(|b| block_diverges(b)) && branches.iter().all(|(_, b)| block_diverges(b))
        }
        TStmt::While { cond, body } => matches!(cond.kind, TExprKind::Bool(true)) && !contains_break(body),
        // Matches are exhaustive: one of the cases always runs.
        TStmt::Match { arms, .. } => !arms.is_empty() && arms.iter().all(|a| block_diverges(&a.body)),
        _ => false,
    }
}

/// Does control never continue to the next statement (for unreachable-code lints)?
fn ends_flow(s: &TStmt) -> bool {
    match s {
        TStmt::Break | TStmt::Continue => true,
        TStmt::If { branches, else_block: Some(eb) } => {
            branches.iter().all(|(_, b)| b.iter().any(ends_flow)) && eb.iter().any(ends_flow)
        }
        TStmt::Match { arms, .. } => !arms.is_empty() && arms.iter().all(|a| a.body.iter().any(ends_flow)),
        other => stmt_diverges(other),
    }
}

/// Does this loop body contain a `break` that targets the enclosing loop?
/// (Also inside expressions: `val v = x ?: break`.)
fn contains_break(stmts: &[TStmt]) -> bool {
    stmts.iter().any(|s| match s {
        TStmt::Break => true,
        TStmt::Let(_, e) | TStmt::Expr(e) | TStmt::Return(Some(e)) => expr_breaks(e),
        TStmt::Assign { place, value, .. } => {
            expr_breaks(value)
                || match place {
                    Place::Local(_) => false,
                    Place::Field { obj, .. } => expr_breaks(obj),
                    Place::Index { list, index, .. } => expr_breaks(list) || expr_breaks(index),
                }
        }
        TStmt::If { branches, else_block } => {
            branches.iter().any(|(c, b)| expr_breaks(c) || contains_break(b)) || else_block.as_ref().is_some_and(|b| contains_break(b))
        }
        // A nested loop's header is evaluated outside of it, but breaks in its body target it.
        TStmt::While { cond, .. } => expr_breaks(cond),
        TStmt::ForRange { start, end, .. } => expr_breaks(start) || expr_breaks(end),
        TStmt::ForList { list, .. } => expr_breaks(list),
        TStmt::Match { scrutinee, arms } => {
            expr_breaks(scrutinee) || arms.iter().any(|a| a.guard.as_ref().is_some_and(expr_breaks) || contains_break(&a.body))
        }
        TStmt::Continue | TStmt::Return(None) => false,
    })
}

/// Does evaluating `e` possibly `break` out of the enclosing loop (`x ?: break`)?
fn expr_breaks(e: &TExpr) -> bool {
    let any = |es: &[TExpr]| es.iter().any(expr_breaks);
    match &e.kind {
        TExprKind::Jump(s) => contains_break(std::slice::from_ref(&**s)),
        TExprKind::Int(_) | TExprKind::Float(_) | TExprKind::Bool(_) | TExprKind::Str(_) | TExprKind::Local(_) | TExprKind::Null => false,
        TExprKind::Call(_, args)
        | TExprKind::Builtin(_, args)
        | TExprKind::StructNew(_, args)
        | TExprKind::EnumNew(_, _, args)
        | TExprKind::ListLit(args)
        | TExprKind::FString(args) => any(args),
        TExprKind::Binary(_, a, b) | TExprKind::And(a, b) | TExprKind::Or(a, b) | TExprKind::Index(a, b) | TExprKind::Elvis(a, b) => {
            expr_breaks(a) || expr_breaks(b)
        }
        TExprKind::SafeCall { recv, body, .. } => expr_breaks(recv) || expr_breaks(body),
        TExprKind::Neg(x) | TExprKind::Not(x) | TExprKind::Field(x, ..) | TExprKind::IsNull(x) | TExprKind::Wrap(x) | TExprKind::Unwrap(x) | TExprKind::NotNull(x) => {
            expr_breaks(x)
        }
    }
}

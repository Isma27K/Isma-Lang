//! Semantic analysis: name resolution, strict type checking, mutability,
//! definite-return analysis. Produces the typed AST.

use crate::ast::*;
use crate::diagnostics::Diagnostic;
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

struct LocalBinding {
    id: LocalId,
    mutable: bool,
    span: Span,
}

pub struct Checker {
    structs: Vec<TStruct>,
    struct_spans: Vec<Span>,
    struct_index: HashMap<String, StructId>,
    funcs: Vec<FuncSig>,
    func_index: HashMap<String, FuncId>,
    methods: HashMap<(StructId, String), FuncId>,
    diags: Vec<Diagnostic>,
    // per-function state
    locals: Vec<TLocal>,
    scopes: Vec<HashMap<String, LocalBinding>>,
    loop_depth: usize,
    ret_ty: Type,
}

pub fn check(program: &Program) -> Result<TProgram, Vec<Diagnostic>> {
    let mut c = Checker {
        structs: Vec::new(),
        struct_spans: Vec::new(),
        struct_index: HashMap::new(),
        funcs: Vec::new(),
        func_index: HashMap::new(),
        methods: HashMap::new(),
        diags: Vec::new(),
        locals: Vec::new(),
        scopes: Vec::new(),
        loop_depth: 0,
        ret_ty: Type::Unit,
    };
    let funcs = c.program(program);
    if c.diags.is_empty() {
        Ok(TProgram { structs: c.structs, funcs })
    } else {
        Err(c.diags)
    }
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

    // ---------- declarations ----------

    fn program(&mut self, program: &Program) -> Vec<TFunc> {
        // Pass 1: struct names.
        for item in &program.items {
            if let Item::Struct(s) = item {
                let name = &s.name.name;
                if RESERVED_TYPES.contains(&name.as_str()) {
                    self.err(Diagnostic::error(format!("`{name}` is a built-in type name"), s.name.span));
                    continue;
                }
                if let Some(&prev) = self.struct_index.get(name) {
                    let prev_span = self.struct_spans[prev];
                    self.err(
                        Diagnostic::error(format!("struct `{name}` is defined more than once"), s.name.span)
                            .with_label(prev_span, "first defined here"),
                    );
                    continue;
                }
                let id = self.structs.len();
                self.structs.push(TStruct { name: name.clone(), fields: Vec::new() });
                self.struct_spans.push(s.name.span);
                self.struct_index.insert(name.clone(), id);
            }
        }

        // Pass 2: struct fields, function + method signatures.
        let mut bodies: Vec<(&FnDecl, Option<StructId>)> = Vec::new();
        for item in &program.items {
            match item {
                Item::Struct(s) => {
                    let Some(&sid) = self.struct_index.get(&s.name.name) else { continue };
                    if self.structs[sid].name != s.name.name || self.struct_spans[sid] != s.name.span {
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
                        fields.push(TField { name: f.name.name.clone(), ty, mutable: f.mutable });
                    }
                    self.structs[sid].fields = fields;
                    for m in &s.methods {
                        if seen.contains_key(m.name.name.as_str()) {
                            self.err(Diagnostic::error(
                                format!("`{}` is both a field and a method of `{}`", m.name.name, s.name.name),
                                m.name.span,
                            ));
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
                        bodies.push((m, Some(sid)));
                    }
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
                    if self.struct_index.contains_key(name) {
                        self.err(Diagnostic::error(format!("`{name}` is already the name of a struct"), f.name.span));
                        continue;
                    }
                    if let Some(&prev) = self.func_index.get(name) {
                        let prev_span = self.funcs[prev].span;
                        self.err(
                            Diagnostic::error(format!("function `{name}` is defined more than once"), f.name.span)
                                .with_label(prev_span, "first defined here")
                                .with_note("Isma has no function overloading"),
                        );
                        continue;
                    }
                    let fid = self.declare_fn(f, None);
                    self.func_index.insert(name.clone(), fid);
                    bodies.push((f, None));
                }
            }
        }

        // `main` checks.
        match self.func_index.get("main") {
            None => self.err(
                Diagnostic::error("no `main` function found", Span::default())
                    .with_help("add an entry point:\n\nfn main():\n    print(\"hello\")"),
            ),
            Some(&id) => {
                let f = &self.funcs[id];
                if !f.params.is_empty() || f.ret != Type::Unit {
                    let span = f.span;
                    self.err(Diagnostic::error("`main` must take no parameters and return nothing", span));
                }
            }
        }

        // Pass 3: bodies.
        let mut out = Vec::new();
        for (decl, owner) in bodies {
            out.push(self.fn_body(decl, owner, out.len()));
        }
        out
    }

    fn declare_fn(&mut self, f: &FnDecl, owner: Option<StructId>) -> FuncId {
        let mut params = Vec::new();
        if let Some(sid) = owner {
            params.push(("self".to_string(), Type::Struct(sid)));
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
        let (name, symbol) = match owner {
            Some(sid) => {
                let sname = &self.structs[sid].name;
                (format!("{sname}.{}", f.name.name), format!("isma_m_{sname}_{}", f.name.name))
            }
            None if f.name.name == "main" => ("main".into(), "isma_main".into()),
            None => (f.name.name.clone(), format!("isma_fn_{}", f.name.name)),
        };
        let id = self.funcs.len();
        self.funcs.push(FuncSig { name, symbol, params, ret, span: f.name.span });
        id
    }

    fn resolve_type(&mut self, t: &TypeExpr) -> Type {
        let name = t.name.name.as_str();
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
                if let Some(&sid) = self.struct_index.get(name) {
                    if !expect_args(self, 0) {
                        return Type::Error;
                    }
                    return Type::Struct(sid);
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
                let names: Vec<String> = RESERVED_TYPES.iter().map(|s| s.to_string()).chain(self.struct_index.keys().cloned()).collect();
                if let Some(h) = hint.map(String::from).or_else(|| suggest(name, names.iter().map(|s| s.as_str()))) {
                    d = d.with_help(format!("did you mean `{h}`?"));
                }
                self.err(d);
                Type::Error
            }
        }
    }

    // ---------- function bodies ----------

    fn fn_body(&mut self, f: &FnDecl, owner: Option<StructId>, fid: FuncId) -> TFunc {
        self.locals.clear();
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
            let id = self.new_local(name, ty.clone());
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
        let sig = &self.funcs[fid];
        TFunc {
            name: sig.name.clone(),
            symbol: sig.symbol.clone(),
            params,
            ret: sig.ret.clone(),
            locals: std::mem::take(&mut self.locals),
            body,
            exported: sig.symbol == "isma_main",
        }
    }

    fn new_local(&mut self, name: &str, ty: Type) -> LocalId {
        self.locals.push(TLocal { name: name.to_string(), ty });
        self.locals.len() - 1
    }

    fn declare_local(&mut self, name: &Ident, ty: Type, mutable: bool) -> LocalId {
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
        let id = self.new_local(&name.name, ty);
        self.scopes.last_mut().unwrap().insert(name.name.clone(), LocalBinding { id, mutable, span: name.span });
        id
    }

    fn lookup(&self, name: &str) -> Option<&LocalBinding> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    fn block(&mut self, stmts: &[Stmt], new_scope: bool) -> Vec<TStmt> {
        if new_scope {
            self.scopes.push(HashMap::new());
        }
        let out = stmts.iter().filter_map(|s| self.stmt(s)).collect();
        if new_scope {
            self.scopes.pop();
        }
        out
    }

    fn expect_bool(&mut self, e: &Expr, what: &str) -> TExpr {
        let t = self.expr(e, Some(&Type::Bool));
        if !t.ty.compatible(&Type::Bool) {
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
                let v = self.expr(value, declared.as_ref());
                if v.ty == Type::Unit {
                    self.err(
                        Diagnostic::error("cannot bind a value of type `Unit`", value.span)
                            .with_primary("this expression produces no value"),
                    );
                }
                let ty = match declared {
                    Some(d) => {
                        self.expect_type(&v, &d, ty.as_ref().map(|t| t.span));
                        d
                    }
                    None => v.ty.clone(),
                };
                let id = self.declare_local(name, ty, *mutable);
                TStmt::Let(id, v)
            }
            StmtKind::Assign { target, op, value } => self.assign(target, *op, value, s.span)?,
            StmtKind::Expr(e) => {
                let t = self.expr(e, None);
                if !matches!(e.kind, ExprKind::Call { .. } | ExprKind::Error) && !t.ty.is_error() {
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
                let mut tb = Vec::new();
                for (cond, body) in branches {
                    let c = self.expect_bool(cond, "`if` condition");
                    let b = self.block(body, true);
                    tb.push((c, b));
                }
                let eb = else_block.as_ref().map(|b| self.block(b, true));
                TStmt::If { branches: tb, else_block: eb }
            }
            StmtKind::While { cond, body } => {
                let c = self.expect_bool(cond, "`while` condition");
                self.loop_depth += 1;
                let b = self.block(body, true);
                self.loop_depth -= 1;
                TStmt::While { cond: c, body: b }
            }
            StmtKind::For { var, iter, body } => match iter {
                ForIter::Range { start, end, inclusive } => {
                    let ts = self.expr(start, Some(&Type::Int));
                    let te = self.expr(end, Some(&Type::Int));
                    for t in [&ts, &te] {
                        if !t.ty.compatible(&Type::Int) {
                            let found = self.tn(&t.ty);
                            self.err(
                                Diagnostic::error(format!("range bounds must be `Int`, found `{found}`"), t.span)
                                    .with_primary(format!("this is `{found}`")),
                            );
                        }
                    }
                    self.scopes.push(HashMap::new());
                    let v = self.declare_local(var, Type::Int, false);
                    self.loop_depth += 1;
                    let b = self.block(body, true);
                    self.loop_depth -= 1;
                    self.scopes.pop();
                    TStmt::ForRange { var: v, start: ts, end: te, inclusive: *inclusive, body: b }
                }
                ForIter::Expr(e) => {
                    let tl = self.expr(e, None);
                    let elem = match &tl.ty {
                        Type::List(t) => (**t).clone(),
                        Type::Error => Type::Error,
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
                    let v = self.declare_local(var, elem, false);
                    self.loop_depth += 1;
                    let b = self.block(body, true);
                    self.loop_depth -= 1;
                    self.scopes.pop();
                    TStmt::ForList { var: v, list: tl, body: b }
                }
            },
            StmtKind::Break | StmtKind::Continue => {
                let is_break = matches!(s.kind, StmtKind::Break);
                if self.loop_depth == 0 {
                    let kw = if is_break { "break" } else { "continue" };
                    self.err(Diagnostic::error(format!("`{kw}` outside of a loop"), s.span).with_primary("not inside `while` or `for`"));
                }
                if is_break { TStmt::Break } else { TStmt::Continue }
            }
            StmtKind::Return(value) => {
                let ret = self.ret_ty.clone();
                match value {
                    None => {
                        if ret != Type::Unit && !ret.is_error() {
                            let rt = self.tn(&ret);
                            self.err(Diagnostic::error(format!("`return` without a value in a function returning `{rt}`"), s.span));
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
                            self.expect_type(&t, &ret, None);
                            TStmt::Return(Some(t))
                        }
                    }
                }
            }
            StmtKind::Pass => return None,
        })
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
        let v = self.expr(value, Some(&place_ty));
        match op {
            None => self.expect_type(&v, &place_ty, None),
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
        d = match (expected, &e.ty) {
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
        if !index.ty.compatible(&Type::Int) {
            let found = self.tn(&index.ty);
            self.err(Diagnostic::error(format!("list index must be `Int`, found `{found}`"), index.span));
        }
        match &list.ty {
            Type::List(t) => (**t).clone(),
            Type::Error => Type::Error,
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
                    let id = b.id;
                    return Self::mk(TExprKind::Local(id), self.locals[id].ty.clone(), span);
                }
                let d = if self.func_index.contains_key(name) || BUILTINS.contains(&name.as_str()) {
                    Diagnostic::error(format!("`{name}` is a function, not a value"), span)
                        .with_help(format!("call it: `{name}(...)`"))
                        .with_note("functions are not first-class values yet")
                } else if self.struct_index.contains_key(name) {
                    Diagnostic::error(format!("`{name}` is a struct, not a value"), span)
                        .with_help(format!("construct one with `{name}(...)`"))
                } else {
                    let mut d = Diagnostic::error(format!("cannot find value `{name}` in this scope"), span).with_primary("not found in this scope");
                    let candidates: Vec<String> = self.scopes.iter().flat_map(|s| s.keys().cloned()).collect();
                    let hint = match name.as_str() {
                        "True" => Some("true".to_string()),
                        "False" => Some("false".to_string()),
                        "None" | "null" | "nil" => {
                            d = d.with_note("Isma has no null; every value is always initialized");
                            None
                        }
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
                        if !t.ty.is_numeric() && !t.ty.is_error() {
                            let found = self.tn(&t.ty);
                            self.err(Diagnostic::error(format!("cannot negate a value of type `{found}`"), span));
                            return Self::error_expr(span);
                        }
                        let ty = t.ty.clone();
                        Self::mk(TExprKind::Neg(Box::new(t)), ty, span)
                    }
                    UnOp::Not => {
                        if !t.ty.compatible(&Type::Bool) {
                            let found = self.tn(&t.ty);
                            self.err(Diagnostic::error(format!("`not` requires a `Bool`, found `{found}`"), inner.span));
                        }
                        Self::mk(TExprKind::Not(Box::new(t)), Type::Bool, span)
                    }
                }
            }
            ExprKind::Call { callee, args } => self.call(callee, args, span, expected),
            ExprKind::Field(obj, field) => {
                let t = self.expr(obj, None);
                match &t.ty {
                    Type::Struct(sid) => {
                        let sid = *sid;
                        match self.structs[sid].fields.iter().position(|f| f.name == field.name) {
                            Some(idx) => {
                                let ty = self.structs[sid].fields[idx].ty.clone();
                                Self::mk(TExprKind::Field(Box::new(t), sid, idx), ty, span)
                            }
                            None => {
                                self.no_field(&Type::Struct(sid), field);
                                Self::error_expr(span)
                            }
                        }
                    }
                    Type::Error => Self::error_expr(span),
                    other => {
                        let other = other.clone();
                        let mut d_ty = other.clone();
                        if matches!(other, Type::List(_) | Type::Str) && field.name == "len" {
                            d_ty = Type::Error;
                            self.err(
                                Diagnostic::error("`len` is a method", field.span).with_help("call it with `.len()`"),
                            );
                        }
                        if !d_ty.is_error() {
                            self.no_field(&other, field);
                        }
                        Self::error_expr(span)
                    }
                }
            }
            ExprKind::Index(list, index) => {
                let tl = self.expr(list, None);
                let ti = self.expr(index, Some(&Type::Int));
                let elem = self.index_elem_type(&tl, &ti, span);
                Self::mk(TExprKind::Index(Box::new(tl), Box::new(ti)), elem, span)
            }
            ExprKind::List(elems) => {
                let expected_elem = match expected {
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
                        Some(et) => {
                            if !t.ty.compatible(et) {
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
                Self::mk(TExprKind::ListLit(out), Type::List(Box::new(et)), span)
            }
        }
    }

    /// Convert a value to String for f-strings.
    fn stringify(&mut self, t: TExpr, span: Span) -> TExpr {
        match &t.ty {
            Type::Str => t,
            Type::Int | Type::Float | Type::Bool => Self::mk(TExprKind::Builtin(Builtin::ToStr, vec![t]), Type::Str, span),
            Type::Error => t,
            other => {
                let found = self.tn(other);
                self.err(
                    Diagnostic::error(format!("cannot format a value of type `{found}` in an f-string"), span)
                        .with_note("only `Int`, `Float`, `Bool` and `String` can be formatted"),
                );
                Self::error_expr(span)
            }
        }
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr, span: Span) -> TExpr {
        if matches!(op, BinOp::And | BinOp::Or) {
            let tl = self.expect_bool(l, &format!("left operand of `{}`", op.symbol()));
            let tr = self.expect_bool(r, &format!("right operand of `{}`", op.symbol()));
            let kind = if op == BinOp::And {
                TExprKind::And(Box::new(tl), Box::new(tr))
            } else {
                TExprKind::Or(Box::new(tl), Box::new(tr))
            };
            return Self::mk(kind, Type::Bool, span);
        }
        let tl = self.expr(l, None);
        let tr = self.expr(r, Some(&tl.ty));
        if tl.ty.is_error() || tr.ty.is_error() {
            let ty = if op.is_comparison() { Type::Bool } else { Type::Error };
            return Self::mk(TExprKind::Binary(op, Box::new(tl), Box::new(tr)), ty, span);
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
                (Type::Struct(_) | Type::List(_), _) if op.is_comparison() => {
                    d.with_note("`==` is only defined for `Int`, `Float`, `Bool` and `String`")
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
            self.expect_type(&t, &pty, None);
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
                if let Some(&fid) = self.func_index.get(name) {
                    let params = self.funcs[fid].params.clone();
                    let ret = self.funcs[fid].ret.clone();
                    let what = format!("function `{name}`");
                    return match self.match_args(&what, &params, args, span) {
                        Some(targs) => Self::mk(TExprKind::Call(fid, targs), ret, span),
                        None => Self::mk(TExprKind::Int(0), if ret == Type::Unit { Type::Unit } else { Type::Error }, span),
                    };
                }
                if let Some(&sid) = self.struct_index.get(name) {
                    let params: Vec<(String, Type)> = self.structs[sid].fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect();
                    let what = format!("struct `{name}`");
                    return match self.match_args(&what, &params, args, span) {
                        Some(targs) => Self::mk(TExprKind::StructNew(sid, targs), Type::Struct(sid), span),
                        None => Self::mk(TExprKind::Int(0), Type::Struct(sid), span),
                    };
                }
                if BUILTINS.contains(&name.as_str()) {
                    return self.builtin_call(name, args, span);
                }
                let mut d = Diagnostic::error(format!("cannot find function `{name}` in this scope"), callee.span).with_primary("not found");
                let names: Vec<String> = self
                    .func_index
                    .keys()
                    .cloned()
                    .chain(BUILTINS.iter().map(|s| s.to_string()))
                    .chain(self.struct_index.keys().cloned())
                    .collect();
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
                let recv = self.expr(obj, None);
                self.method_call(recv, method, args, span)
            }
            _ => {
                let _ = self.expr(callee, None);
                self.err(Diagnostic::error("this expression is not callable", callee.span));
                Self::error_expr(span)
            }
        }
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
            let t = self.expr(&a.value, param.as_ref());
            match &param {
                Some(p) => self.expect_type(&t, p, None),
                None => {
                    // print accepts any primitive
                    if !t.ty.is_primitive() && !t.ty.is_error() {
                        let found = self.tn(&t.ty);
                        self.err(
                            Diagnostic::error(format!("`print` cannot print a value of type `{found}`"), a.value.span)
                                .with_note("`print` accepts `Int`, `Float`, `Bool` and `String`")
                                .with_help("print individual fields, or use an f-string"),
                        );
                    }
                }
            }
            targs.push(t);
        }
        Self::mk(TExprKind::Builtin(b, targs), ret, span)
    }

    fn method_call(&mut self, recv: TExpr, method: &Ident, args: &[Arg], span: Span) -> TExpr {
        let m = method.name.as_str();
        let rty = recv.ty.clone();
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
            (Type::Str, "to_float") => simple(self, recv, Builtin::StrToFloat, Type::Float, args),
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
                self.expect_type(&v, &elem, None);
                Self::mk(TExprKind::Builtin(Builtin::ListPush, vec![recv, v]), Type::Unit, span)
            }
            (Type::Struct(sid), _) => {
                let sid = *sid;
                match self.methods.get(&(sid, m.to_string())) {
                    Some(&fid) => {
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
                        let mut d = Diagnostic::error(format!("no method `{m}` on struct `{sname}`"), method.span).with_primary("method not found");
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
                    Type::Str => "`len()`, `to_int()`, `to_float()`, `to_string()`",
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
        _ => false,
    }
}

/// Does this loop body contain a `break` that targets the enclosing loop?
fn contains_break(stmts: &[TStmt]) -> bool {
    stmts.iter().any(|s| match s {
        TStmt::Break => true,
        TStmt::If { branches, else_block } => {
            branches.iter().any(|(_, b)| contains_break(b)) || else_block.as_ref().is_some_and(|b| contains_break(b))
        }
        // breaks inside nested loops target those loops
        _ => false,
    })
}

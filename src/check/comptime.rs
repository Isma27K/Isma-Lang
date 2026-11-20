//! Code that runs while compiling: `comptime val`, `comptime if`,
//! `comptime for`, `comptime match`, `comptime fn`, spread arguments
//! (`T(decode(f)? for f in fields(T))`) and `compile_error`.
//!
//! **Where it runs.** Generic code is compiled again for every set of types
//! it's used with (see `generics.rs`): that's where reflection gets its
//! answers. In each copy everything is known, so a `comptime for` over
//! `fields(T)` repeats its body once per field, a `comptime if` keeps only
//! the branch its condition picks, and a `comptime match T` only the case
//! that fits. In generic code as written (checked once, with the type
//! parameters abstract), those answers aren't known yet: the code that
//! depends on them is checked *quietly* (for the editor, with no errors and
//! no needs recorded) and really checked in every copy instead, like in C++
//! templates or Zig. What doesn't depend on the type parameters is decided
//! right away, even there.
//!
//! **Compile-time mode.** The expressions that are only evaluated while
//! compiling (the value of a `comptime val`, a `comptime if`'s condition, a
//! `comptime for`'s list, attribute arguments, the body of a `comptime fn`)
//! are checked in compile-time mode (`ct.mode > 0`): there they may use
//! what only exists while compiling (`f.attribute(a)` on any `Field`,
//! calls to `comptime fn`s), as `TExprKind::Meta`. The interpreter
//! (`interp.rs`) runs them; they never reach the code that's compiled.
//!
//! **Values back to run time.** A `comptime val` or a `comptime for`'s
//! variable holds a value known while compiling (`ct_locals`). Code that
//! runs reads it like any local, so it's also set at run time, to the value
//! turned back into code (`materialize`), but only if run-time code reads it.

use super::interp::{Ctl, Interp, Value};
use super::reflect::{AttrUse, PendingAttr};
use super::*;
use std::rc::Rc;

/// A function's AST, kept to check its body when compile-time code needs it
/// before its turn. (The programs outlive the checking: see `program`.)
#[derive(Clone, Copy)]
pub(super) struct AstFn {
    pub decl: *const FnDecl,
    pub owner: Option<StructId>,
    pub ns: usize,
}

/// The checker's state for compile-time code.
#[derive(Default)]
pub(super) struct CtState {
    /// Attribute declarations (their ids in the struct table).
    pub attr_decls: HashSet<StructId>,
    pub pending_attrs: Vec<PendingAttr>,
    /// Every attribute written, and per declaration the ones on it.
    pub attr_uses: Vec<AttrUse>,
    pub attr_index: HashMap<AttrTarget, Vec<usize>>,
    /// Per function: where each parameter is declared (for `at = p`).
    pub param_spans: HashMap<FuncId, Vec<Span>>,
    /// Per named function passed as a `Function`: its type; and back.
    pub fn_items: HashMap<FuncId, StructId>,
    pub item_fns: HashMap<StructId, FuncId>,
    /// Type parameters bounded by `Function`.
    pub fn_item_params: HashSet<ParamId>,
    /// `comptime fn`s (declarations).
    pub comptime_fns: HashSet<FuncId>,
    /// Code that only runs while compiling: never compiled to run.
    pub comptime_only: HashSet<FuncId>,
    /// Functions whose bodies are being checked, innermost last.
    pub checking: Vec<FuncId>,
    /// Every function with a body (by declaration).
    pub asts: HashMap<FuncId, AstFn>,
    /// Checked bodies lent to the interpreter (put back in `out` at the end).
    pub bodies: HashMap<FuncId, Rc<TFunc>>,
    /// Per copy of generic code: where it was first needed (and in which function).
    pub inst_origin: HashMap<FuncId, (Span, FuncId)>,
    /// Inside compile-time code (> 0).
    pub mode: usize,
    /// Inside code checked quietly (> 0): see the module comment.
    pub quiet: usize,
    /// While matching a type pattern: the types its binders stand for (`U` in `case U?:`).
    pub binds: Vec<(String, Type)>,
}

/// The result of trying to work something out while compiling.
enum Known {
    Yes(Value),
    /// Not yet: generic code as written.
    Later,
    /// It failed, and that was reported.
    Failed,
}

impl Checker {
    // ---------- evaluating ----------

    /// Evaluate `e` (of the function being checked) while compiling.
    pub(super) fn ct_eval(&mut self, e: &TExpr) -> Result<Value, Ctl> {
        let mut known: Vec<Option<Value>> = vec![None; self.locals.len()];
        for (id, v) in &self.ct_locals {
            if *id < known.len() {
                known[*id] = Some(v.clone());
            }
        }
        Interp::new(self).eval_here(e, known)
    }

    /// Is `span` in the standard library's code?
    fn in_std(&self, span: Span) -> bool {
        self.ns_sources.iter().enumerate().filter(|(_, (b, _))| *b <= span.start).max_by_key(|(_, (b, _))| *b).is_some_and(|(ns, _)| self.ns_kinds[ns] == FileKind::Std)
    }

    /// A failure while running compile-time code started at `span`: shown
    /// there too (and only there, if it happened in the standard library).
    pub(super) fn ct_failure(&self, mut d: Diagnostic, span: Span) -> Diagnostic {
        if self.in_std(d.span) {
            d.span = span;
            d.labels.retain(|(s, _)| !self.in_std(*s));
            if d.primary_label.is_empty() {
                d.primary_label = "run while compiling here".into();
            }
        } else if d.span != span && !d.labels.iter().any(|(s, _)| *s == span) {
            d = d.with_label(span, "run while compiling here");
        }
        d
    }

    /// Report why evaluating stopped (at `span`); an error expression.
    pub(super) fn report_ctl(&mut self, e: Ctl, span: Span) -> TExpr {
        match e {
            Ctl::Fail(d) => {
                let d = self.ct_failure(*d, span);
                self.err(d)
            }
            Ctl::CompileError { message, at, raised } => self.raise_compile_error(message, at, raised),
            Ctl::Unknown(at) => {
                let what = self.snippet(at).map(|s| format!("`{s}`")).unwrap_or_else(|| "this".into());
                self.err(
                    Diagnostic::error(format!("{what} isn't known while compiling"), at)
                        .with_primary("only known when the program runs")
                        .with_note("compile-time code can use literals, constants, `comptime val`s, the variables of `comptime for`, and what functions compute from those")
                        .with_label(span, "needed while compiling here"),
                );
            }
            Ctl::Quiet | Ctl::Break | Ctl::Continue | Ctl::Return(_) => {}
        }
        Self::error_expr(span)
    }

    /// Work out `e` while compiling, if it can be here.
    fn known(&mut self, e: &TExpr) -> Known {
        if e.ty.is_error() {
            return Known::Failed;
        }
        match self.ct_eval(e) {
            Ok(v) => Known::Yes(v),
            Err(Ctl::Unknown(_)) if self.in_template || self.ct.quiet > 0 => Known::Later,
            Err(c) => {
                if self.ct.quiet == 0 {
                    self.report_ctl(c, e.span);
                }
                Known::Failed
            }
        }
    }

    /// Check `f` quietly: no errors, warnings or needs (generic code whose
    /// compile-time answers aren't known yet; see the module comment).
    pub(super) fn quiet<R>(&mut self, f: impl FnOnce(&mut Checker) -> R) -> R {
        let saved = self.cur_generic.take();
        let (nd, nlam, nlocals) = (self.diags.len(), self.lambda_funcs.len(), self.locals.len());
        self.ct.quiet += 1;
        let r = f(self);
        self.ct.quiet -= 1;
        self.diags.truncate(nd);
        self.lambda_funcs.truncate(nlam);
        self.cur_generic = saved;
        // (Its variables aren't really unused: the copies use them.)
        let n = self.local_meta.len();
        for m in &mut self.local_meta[nlocals.min(n)..] {
            m.used = true;
            m.reassigned = true;
        }
        r
    }

    /// Check the arguments of a call that can't be checked yet, quietly.
    pub(super) fn quiet_args(&mut self, args: &[Arg]) {
        self.quiet(|c| {
            for a in args {
                match &a.value.kind {
                    ExprKind::Spread { value, var, iter, cond } => {
                        c.ct.mode += 1;
                        let it = c.expr(iter, None);
                        c.ct.mode -= 1;
                        let elem = match it.ty {
                            Type::List(e) => *e,
                            _ => Type::Error,
                        };
                        c.scopes.push(HashMap::new());
                        c.declare_local(var, elem, LocalKind::LoopVar);
                        if let Some(cond) = cond {
                            c.ct.mode += 1;
                            let _ = c.expr(cond, Some(&Type::Bool));
                            c.ct.mode -= 1;
                        }
                        let _ = c.expr(value, None);
                        c.scopes.pop();
                    }
                    _ => {
                        let _ = c.expr(&a.value, None);
                    }
                }
            }
        });
    }

    // ---------- compile-time locals ----------

    /// Declare compile-time local `name` holding `v` (of type `ty`).
    fn ct_local(&mut self, name: &Ident, ty: Type, v: Value, kind: LocalKind) -> LocalId {
        let id = self.declare_local(name, ty, kind);
        if let Some(shown) = shown_value(&v) {
            let n = self.ide.hovers.iter().rposition(|h| h.span == name.span);
            if let Some(h) = n.map(|i| &mut self.ide.hovers[i]) {
                h.code = format!("comptime {} = {shown}", h.code.trim_start_matches("comptime "));
            }
        }
        self.ct_locals.insert(id, v);
        id
    }

    /// Run-time code read compile-time local `id` (at `span`).
    pub(super) fn ct_read(&mut self, id: LocalId, span: Span) {
        if self.ct.mode == 0 && self.ct_locals.contains_key(&id) {
            self.ct_reads.entry(id).or_insert((0, span)).0 += 1;
        }
    }

    fn ct_runtime_reads(&self, id: LocalId) -> Option<Span> {
        if matches!(self.ct_locals.get(&id), Some(Value::Failed)) {
            return None;
        }
        self.ct_reads.get(&id).filter(|(n, _)| *n > 0).map(|(_, s)| *s)
    }

    /// The run-time value of compile-time local `id` (if run-time code reads it).
    fn ct_let(&mut self, id: LocalId, span: Span) -> Option<TExpr> {
        let at = self.ct_runtime_reads(id)?;
        let v = self.ct_locals.get(&id)?.clone();
        let ty = self.locals[id].ty.clone();
        match self.materialize(&v, &ty, span) {
            Ok(t) => Some(t),
            Err(why) => {
                let name = self.locals[id].name.clone();
                self.err(
                    Diagnostic::error(format!("`{name}` is only known while compiling: {why}"), at)
                        .with_primary("read by code that runs")
                        .with_note("a value made while compiling reaches the running program only if it's made of numbers, strings, lists, maps and structs"),
                );
                None
            }
        }
    }

    /// At the end of a block: `comptime val`s that run-time code never
    /// reads need no run-time value.
    pub(super) fn drop_unread_ct_lets(&mut self, out: &mut Vec<TStmt>) {
        if self.ct_locals.is_empty() {
            return;
        }
        let mut keep = Vec::with_capacity(out.len());
        for s in out.drain(..) {
            if let TStmt::Let(id, _) = &s
                && self.ct_locals.contains_key(id)
            {
                let id = *id;
                if self.ct_runtime_reads(id).is_none() {
                    continue;
                }
                if let Some(why) = self.ct_unkept.remove(&id) {
                    let (name, at) = (self.locals[id].name.clone(), self.ct_reads[&id].1);
                    self.err(Diagnostic::error(format!("`{name}` is only known while compiling: {why}"), at).with_primary("read by code that runs"));
                }
            }
            keep.push(s);
        }
        *out = keep;
    }

    // ---------- statements ----------

    pub(super) fn comptime_stmt(&mut self, c: &ComptimeStmt, span: Span) -> Option<TStmt> {
        match c {
            ComptimeStmt::Let { name, ty, value } => {
                let ann = ty.as_ref().map(|t| self.resolve_type(t));
                self.ct.mode += 1;
                let t = self.expr(value, ann.as_ref());
                let t = match &ann {
                    Some(a) => self.coerce(t, a, ty.as_ref().map(|t| t.span)),
                    None => t,
                };
                self.ct.mode -= 1;
                let lty = ann.unwrap_or_else(|| t.ty.clone());
                if lty == Type::Unit || lty == Type::Null {
                    self.err(Diagnostic::error("a `comptime val` needs a value", value.span).with_primary(if lty == Type::Null { "`null` of which type?" } else { "this gives no value" }));
                }
                match self.known(&t) {
                    Known::Yes(v) => {
                        let id = self.ct_local(name, lty.clone(), v.clone(), LocalKind::Val);
                        let value = match self.materialize(&v, &lty, value.span) {
                            Ok(m) => m,
                            Err(why) => {
                                self.ct_unkept.insert(id, why);
                                Self::error_expr(value.span)
                            }
                        };
                        Some(TStmt::Let(id, value))
                    }
                    Known::Later => {
                        let id = self.declare_local(name, lty, LocalKind::Val);
                        Some(TStmt::Let(id, t))
                    }
                    Known::Failed => {
                        // (Reported: what uses it stops quietly.)
                        let id = self.declare_local(name, lty, LocalKind::Val);
                        self.ct_locals.insert(id, Value::Failed);
                        self.local_meta[id].used = true;
                        Some(TStmt::Let(id, Self::error_expr(value.span)))
                    }
                }
            }
            ComptimeStmt::If { branches, else_block } => {
                for (i, (cond, body)) in branches.iter().enumerate() {
                    self.ct.mode += 1;
                    let c = self.expect_bool(cond, "a `comptime if` condition");
                    self.ct.mode -= 1;
                    match self.known(&c) {
                        Known::Yes(Value::Bool(true)) => {
                            let b = self.block(body, true);
                            return Some(TStmt::Block(b));
                        }
                        Known::Yes(_) => continue,
                        Known::Failed => return None,
                        Known::Later => {
                            // Generic code: each copy picks; here, every branch quietly.
                            let rest: Vec<&Block> = branches[i..].iter().map(|(_, b)| b).chain(else_block.iter()).collect();
                            let conds: Vec<&Expr> = branches[i + 1..].iter().map(|(c, _)| c).collect();
                            return Some(self.quiet_branches(&rest, &conds, else_block.is_some()));
                        }
                    }
                }
                else_block.as_ref().map(|b| TStmt::Block(self.block(b, true)))
            }
            ComptimeStmt::For { var, second, iter, body } => Some(self.comptime_for(var, second.as_ref(), iter, body, span)),
            ComptimeStmt::Match { ty, arms } => Some(self.comptime_match(ty, arms, span)),
        }
    }

    /// Branches whose choice depends on generic types, checked quietly: a
    /// statement that ends the flow if they all do (so `return` in every
    /// case still counts in generic code).
    fn quiet_branches(&mut self, bodies: &[&Block], conds: &[&Expr], exhaustive: bool) -> TStmt {
        let diverges = self.quiet(|c| {
            for cond in conds {
                c.ct.mode += 1;
                let _ = c.expect_bool(cond, "a `comptime if` condition");
                c.ct.mode -= 1;
            }
            let mut all = exhaustive;
            for b in bodies {
                let t = c.block(b, true);
                all &= block_diverges(&t);
            }
            all
        });
        if diverges {
            let span = bodies.first().and_then(|b| b.first()).map(|s| s.span).unwrap_or_default();
            TStmt::Expr(Self::mk(TExprKind::Builtin(Builtin::Panic, vec![Self::mk(TExprKind::Str(String::new()), Type::Str, span)]), Type::Unit, span))
        } else {
            TStmt::Block(Vec::new())
        }
    }

    /// `comptime for x in list:` / `comptime for i, x in list:`.
    fn comptime_for(&mut self, var: &Ident, second: Option<&Ident>, iter: &Expr, body: &Block, span: Span) -> TStmt {
        self.ct.mode += 1;
        let it = self.expr(iter, None);
        self.ct.mode -= 1;
        let elem = match &it.ty {
            Type::List(e) => (**e).clone(),
            Type::Error => return TStmt::Block(Vec::new()),
            t => {
                let tn = self.tn(t);
                self.err(Diagnostic::error(format!("`comptime for` goes over a list, not a `{tn}`"), iter.span).with_help("loop over `fields(T)`, `params(H)`, or a list worked out while compiling"));
                return TStmt::Block(Vec::new());
            }
        };
        // `comptime for i, f in fields(T)`: the index, then the element.
        let (index, item) = match second {
            Some(s) => (Some(var), s),
            None => (None, var),
        };
        let items = match self.known(&it) {
            Known::Yes(Value::List(l)) => l.borrow().clone(),
            Known::Yes(_) | Known::Failed => return TStmt::Block(Vec::new()),
            Known::Later => {
                self.quiet(|c| {
                    c.scopes.push(HashMap::new());
                    if let Some(i) = index {
                        c.declare_local(i, Type::Int, LocalKind::LoopVar);
                    }
                    c.declare_local(item, elem.clone(), LocalKind::LoopVar);
                    c.loop_depth += 1;
                    let _ = c.block(body, false);
                    c.loop_depth -= 1;
                    c.scopes.pop();
                });
                return TStmt::Block(Vec::new());
            }
        };
        let _ = span;
        let mut iters = Vec::new();
        for (k, v) in items.into_iter().enumerate() {
            self.scopes.push(HashMap::new());
            let ix = index.map(|i| self.ct_local(i, Type::Int, Value::Int(k as i64), LocalKind::LoopVar));
            let id = self.ct_local(item, elem.clone(), v, LocalKind::LoopVar);
            self.loop_depth += 1;
            let mut b = self.block(body, false);
            self.loop_depth -= 1;
            self.scopes.pop();
            // What run-time code reads of them, set first.
            for l in [Some(id), ix].into_iter().flatten().rev() {
                if let Some(t) = self.ct_let(l, item.span) {
                    b.insert(0, TStmt::Let(l, t));
                }
            }
            iters.push(b);
        }
        TStmt::Unrolled(iters)
    }

    /// `comptime match T:` with types as cases.
    fn comptime_match(&mut self, ty: &TypeExpr, arms: &[TypeArm], span: Span) -> TStmt {
        let t = self.resolve_type(ty);
        if t.is_error() {
            return TStmt::Block(Vec::new());
        }
        if t.has_param(&self.struct_has_param) {
            // Generic code as written: each copy picks its case; here, every case quietly.
            let exhaustive = arms.iter().any(|a| a.pats.iter().any(|p| p.name.name == "_" && !p.nullable && p.module.is_none()));
            let diverges = self.quiet(|c| {
                let mut all = exhaustive;
                for a in arms {
                    c.ct.binds.clear();
                    for p in &a.pats {
                        let _ = c.type_fits(p, &Type::Error);
                    }
                    let binds = std::mem::take(&mut c.ct.binds);
                    let n = c.type_env.len();
                    c.type_env.extend(binds);
                    let b = c.block(&a.body, true);
                    c.type_env.truncate(n);
                    all &= block_diverges(&b);
                }
                all
            });
            if diverges {
                return TStmt::Expr(Self::mk(TExprKind::Builtin(Builtin::Panic, vec![Self::mk(TExprKind::Str(String::new()), Type::Str, span)]), Type::Unit, span));
            }
            return TStmt::Block(Vec::new());
        }
        for a in arms {
            let mut fits = None;
            for p in &a.pats {
                self.ct.binds.clear();
                match self.type_fits(p, &t) {
                    Ok(true) if fits.is_none() => fits = Some(std::mem::take(&mut self.ct.binds)),
                    Ok(_) => {}
                    Err(()) => return TStmt::Block(Vec::new()),
                }
            }
            if let Some(binds) = fits {
                // `case U?:`: `U` is the type inside, in this case's code.
                let n = self.type_env.len();
                self.type_env.extend(binds);
                let b = self.block(&a.body, true);
                self.type_env.truncate(n);
                return TStmt::Block(b);
            }
        }
        let tn = self.tn(&t);
        self.err(
            Diagnostic::error(format!("no case of this `comptime match` fits `{tn}`"), ty.span)
                .with_primary(format!("this is `{tn}` here"))
                .with_help("add a case for it, or `case _:` for every other type"),
        );
        let _ = span;
        TStmt::Block(Vec::new())
    }

    /// Does type `t` fit type pattern `p` (`Int`, `List[_]`, `_?`, an interface)?
    /// `Err` after an error (reported).
    fn type_fits(&mut self, p: &TypeExpr, t: &Type) -> Result<bool, ()> {
        let wild = p.module.is_none() && p.name.name == "_" && p.args.is_empty() && p.func.is_none();
        if p.nullable {
            let inner = TypeExpr { nullable: false, ..p.clone() };
            return match t {
                Type::Nullable(x) => self.type_fits(&inner, x),
                Type::Error => self.type_fits(&inner, t).map(|_| false),
                _ => Ok(false),
            };
        }
        if wild {
            return Ok(true);
        }
        // `U` in `case U?:` (one capital letter, maybe digits, that isn't a
        // type here): stands for the type found there.
        let binder = p.module.is_none()
            && p.args.is_empty()
            && p.func.is_none()
            && p.name.name.starts_with(|c: char| c.is_ascii_uppercase())
            && p.name.name[1..].chars().all(|c| c.is_ascii_digit())
            && !self.type_env.iter().any(|(n, _)| *n == p.name.name)
            && self.visible_struct(&p.name.name).is_none();
        if binder {
            let name = p.name.name.clone();
            if let Some((_, prev)) = self.ct.binds.iter().find(|(n, _)| *n == name) {
                return Ok(prev == t || t.is_error());
            }
            self.ct.binds.push((name, t.clone()));
            self.hover(p.name.span, format!("type {}", p.name.name), Some("stands for the type found here, in this case's code".into()));
            return Ok(true);
        }
        if matches!(t, Type::Nullable(_)) {
            // (Only `T?` and `_` fit a nullable type.)
            let _ = self.type_fits(p, &Type::Error);
            return Ok(false);
        }
        let args = |c: &mut Checker, targs: &[Type]| -> Result<bool, ()> {
            if p.args.is_empty() {
                return Ok(true);
            }
            if p.args.len() != targs.len() {
                return Ok(false);
            }
            let mut all = true;
            for (q, a) in p.args.iter().zip(targs) {
                all &= c.type_fits(q, a)?;
            }
            Ok(all)
        };
        let name = p.name.name.as_str();
        if p.module.is_none() && p.func.is_none() {
            match name {
                "List" => {
                    if !p.args.is_empty() && p.args.len() != 1 {
                        self.err(Diagnostic::error("`List` takes one type argument", p.span));
                        return Err(());
                    }
                    return match t {
                        Type::List(e) => args(self, &[(**e).clone()]),
                        _ => args(self, &[Type::Error]).map(|_| false),
                    };
                }
                "Map" => {
                    if !p.args.is_empty() && p.args.len() != 2 {
                        self.err(Diagnostic::error("`Map` takes two type arguments", p.span));
                        return Err(());
                    }
                    return match t {
                        Type::Map(k, v) => args(self, &[(**k).clone(), (**v).clone()]),
                        _ => args(self, &[Type::Error, Type::Error]).map(|_| false),
                    };
                }
                n if n == TUPLE_TYPE => {
                    let arity = p.args.len();
                    let tid = self.tuples.get(arity).copied().flatten();
                    return match t {
                        Type::Struct(s) if tid == Some(self.struct_origin[*s]) => {
                            let targs = self.struct_args[*s].clone();
                            args(self, &targs)
                        }
                        _ => args(self, &vec![Type::Error; arity]).map(|_| false),
                    };
                }
                _ => {}
            }
        }
        // A generic type written bare (`Box`) or with patterns (`Box[_]`).
        let sid = if p.func.is_none() {
            match &p.module {
                None => self.visible_struct(name),
                Some(m) => self.imported_module(&m.name).and_then(|ns| self.struct_index.get(&(ns, name.to_string())).copied()),
            }
        } else {
            None
        };
        if let Some(sid) = sid
            && (!self.struct_params[sid].is_empty() || p.args.iter().any(|a| a.name.name == "_"))
        {
            self.pending_struct_hovers.push((p.name.span, sid));
            if matches!(self.def_types[sid], Type::Iface(_)) {
                // An interface with type arguments: those must be types.
                let full = self.resolve_type(&TypeExpr { nullable: false, ..p.clone() });
                return Ok(match full {
                    Type::Iface(i) if !t.is_error() => self.fit(t, i).is_ok(),
                    _ => false,
                });
            }
            return match t {
                Type::Struct(s) | Type::Enum(s, _) if self.struct_origin[*s] == sid => {
                    let targs = self.struct_args[*s].clone();
                    args(self, &targs)
                }
                _ => {
                    let n = self.struct_params[sid].len();
                    args(self, &vec![Type::Error; n]).map(|_| false)
                }
            };
        }
        let want = self.resolve_type(&TypeExpr { nullable: false, ..p.clone() });
        Ok(match want {
            Type::Error => return Err(()),
            _ if t.is_error() => false,
            // An interface: any type that fits it.
            Type::Iface(i) => *t == Type::Iface(i) || self.fit(t, i).is_ok(),
            w => w == *t,
        })
    }

    // ---------- spreads ----------

    /// The arguments of a call with a spread (`decode(f)? for f in
    /// fields(T)`), checked against `params`: one per parameter (`None`
    /// where none is given). `Err(None)`: not known yet (generic code);
    /// `Err(Some(()))`: an error, reported.
    pub(super) fn spread_args(&mut self, what: &str, params: &[(String, Type)], args: &[Arg], span: Span) -> Result<Vec<Option<TExpr>>, Option<()>> {
        let mut slots: Vec<Option<TExpr>> = (0..params.len()).map(|_| None).collect();
        let mut ok = true;
        let mut next = 0;
        let place = |c: &mut Checker, slots: &[Option<TExpr>], name: Option<&str>, next: &mut usize, at: Span| -> Option<usize> {
            let k = match name {
                Some(n) => match params.iter().position(|(p, _)| p == n) {
                    Some(k) => k,
                    None => {
                        c.err(Diagnostic::error(format!("{what} has no parameter or field `{n}`"), at));
                        return None;
                    }
                },
                None => {
                    while *next < slots.len() && slots[*next].is_some() {
                        *next += 1;
                    }
                    if *next >= slots.len() {
                        c.err(Diagnostic::error(format!("too many arguments for {what}"), at).with_note(format!("it takes {}", params.len())));
                        return None;
                    }
                    *next
                }
            };
            if slots[k].is_some() {
                c.err(Diagnostic::error(format!("`{}` is given twice", params[k].0), at));
                return None;
            }
            Some(k)
        };
        for a in args {
            let ExprKind::Spread { value, var, iter, cond } = &a.value.kind else {
                let Some(k) = place(self, &slots, a.name.as_ref().map(|n| n.name.as_str()), &mut next, a.value.span) else {
                    let _ = self.expr(&a.value, None);
                    ok = false;
                    continue;
                };
                let pt = params[k].1.clone();
                let t = self.expr(&a.value, Some(&pt));
                slots[k] = Some(self.coerce(t, &pt, None));
                continue;
            };
            if let Some(n) = &a.name {
                self.err(Diagnostic::error("a spread gives several arguments: it can't have a name", n.span).with_help(format!("remove `{} =`", n.name)));
                ok = false;
            }
            self.ct.mode += 1;
            let it = self.expr(iter, None);
            self.ct.mode -= 1;
            let elem = match &it.ty {
                Type::List(e) => (**e).clone(),
                Type::Error => {
                    ok = false;
                    continue;
                }
                t => {
                    let tn = self.tn(t);
                    self.err(Diagnostic::error(format!("a spread goes over a list, not a `{tn}`"), iter.span).with_help("spread over `fields(T)`, `params(H)`, or a list worked out while compiling"));
                    ok = false;
                    continue;
                }
            };
            let items = match self.known(&it) {
                Known::Yes(Value::List(l)) => l.borrow().clone(),
                Known::Later => return Err(None),
                _ => {
                    ok = false;
                    continue;
                }
            };
            for item in items {
                self.scopes.push(HashMap::new());
                let id = self.ct_local(var, elem.clone(), item.clone(), LocalKind::LoopVar);
                self.local_meta[id].used = true;
                let keep = match cond {
                    None => true,
                    Some(c) => {
                        self.ct.mode += 1;
                        let tc = self.expect_bool(c, "the condition of a spread");
                        self.ct.mode -= 1;
                        match self.known(&tc) {
                            Known::Yes(Value::Bool(b)) => b,
                            Known::Later => {
                                self.scopes.pop();
                                return Err(None);
                            }
                            _ => {
                                ok = false;
                                false
                            }
                        }
                    }
                };
                if keep {
                    // A field or parameter goes where its name says.
                    let name = match &item {
                        Value::Field(sid, i) => Some(self.structs[*sid].fields[*i].name.clone()),
                        Value::Param(fid, i) => Some(self.funcs[*fid].params[*i].0.clone()),
                        _ => None,
                    };
                    match place(self, &slots, name.as_deref(), &mut next, value.span) {
                        Some(k) => {
                            let pt = params[k].1.clone();
                            let t = self.expr(value, Some(&pt));
                            let t = self.coerce(t, &pt, None);
                            let t = match self.ct_let(id, var.span) {
                                Some(init) => {
                                    let ty = t.ty.clone();
                                    Self::mk(TExprKind::Let(id, Box::new(init), Box::new(t)), ty, value.span)
                                }
                                None => t,
                            };
                            slots[k] = Some(t);
                        }
                        None => ok = false,
                    }
                }
                self.scopes.pop();
            }
        }
        let _ = span;
        if ok { Ok(slots) } else { Err(Some(())) }
    }

    /// `T(... for f in fields(T))`: build struct `sid` from a spread.
    pub(super) fn spread_construct(&mut self, sid: StructId, name: &str, args: &[Arg], span: Span) -> TExpr {
        if self.structs[sid].generic {
            if self.in_template || self.ct.quiet > 0 {
                self.quiet_args(args);
                return Self::mk(TExprKind::Int(0), self.def_types[sid].clone(), span);
            }
            self.err(Diagnostic::error(format!("give `{name}`'s type arguments: a spread can't work them out"), span).with_help(format!("write `{name}[Int](...)`, with its types")));
            self.quiet_args(args);
            return Self::error_expr(span);
        }
        let params: Vec<(String, Type)> = self.structs[sid].fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect();
        let what = format!("struct `{name}`");
        match self.spread_args(&what, &params, args, span) {
            Ok(slots) => {
                let origin = self.struct_origin[sid];
                let mut vals = Vec::new();
                for (k, s) in slots.into_iter().enumerate() {
                    match s {
                        Some(t) => vals.push(t),
                        None => match self.defaults.get(&(origin, k)).copied() {
                            Some(d) => {
                                let f = self.member_instance(d, sid);
                                let ty = self.funcs[f].ret.clone();
                                vals.push(Self::mk(TExprKind::Call(f, Vec::new()), ty, span));
                            }
                            None => {
                                self.err(
                                    Diagnostic::error(format!("no value for field `{}` of `{name}`", params[k].0), span)
                                        .with_primary(format!("`{}` is left out", params[k].0))
                                        .with_note("a field the spread leaves out must have a default value, or be given by name"),
                                );
                                return Self::error_expr(span);
                            }
                        },
                    }
                }
                Self::mk(TExprKind::StructNew(sid, vals), self.def_types[sid].clone(), span)
            }
            Err(None) => {
                self.quiet_args(args);
                Self::mk(TExprKind::Int(0), self.def_types[sid].clone(), span)
            }
            Err(Some(())) => Self::error_expr(span),
        }
    }

    /// `T(...)` where `T` is a type parameter: built in each copy of the code.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn param_construct(&mut self, name: &str, t: Type, callee_span: Span, args: &[Arg], explicit: Option<&[TypeExpr]>, expected: Option<&Type>, span: Span) -> TExpr {
        match t {
            Type::Param(_) => {
                // Generic code as written: what `T` is, and so what it takes, isn't known yet.
                self.quiet_args(args);
                Self::mk(TExprKind::Int(0), t, span)
            }
            Type::Struct(sid) if self.structs[sid].closure.is_none() => {
                let shown = self.structs[sid].name.clone();
                self.construct(sid, &shown, callee_span, args, explicit, expected, span)
            }
            other => {
                let tn = self.tn(&other);
                self.err(Diagnostic::error(format!("`{name}` is `{tn}` here: it can't be built from fields"), callee_span).with_primary("not a struct"));
                self.quiet_args(args);
                Self::error_expr(span)
            }
        }
    }

    // ---------- function bodies for the interpreter ----------

    /// The checked body of `fid`, for the interpreter: checked now if its turn hasn't come yet.
    pub(super) fn body_for(&mut self, fid: FuncId, at: Span) -> Result<Rc<TFunc>, Ctl> {
        if let Some(b) = self.ct.bodies.get(&fid) {
            return Ok(b.clone());
        }
        if let Some(tf) = self.out.get_mut(fid).and_then(Option::take) {
            let b = Rc::new(tf);
            self.ct.bodies.insert(fid, b.clone());
            return Ok(b);
        }
        if let Some(i) = self.lambda_funcs.iter().position(|(id, _)| *id == fid) {
            let (_, tf) = self.lambda_funcs.remove(i);
            let b = Rc::new(tf);
            self.ct.bodies.insert(fid, b.clone());
            return Ok(b);
        }
        let name = self.funcs[fid].name.clone();
        if self.ct.checking.contains(&fid) {
            return Err(Ctl::Fail(Box::new(
                Diagnostic::error(format!("`{name}` is needed while compiling itself"), at)
                    .with_note("its body runs code while compiling that calls it, before it's finished"),
            )));
        }
        let decl = self.fn_decl_of(fid);
        let Some(ast) = self.ct.asts.get(&decl).copied() else {
            return Err(Ctl::Fail(Box::new(Diagnostic::error(format!("`{name}` can't run while compiling"), at).with_note("its code isn't available here"))));
        };
        if self.funcs[fid].instance.is_some() && !self.is_concrete_instance(fid) {
            return Err(Ctl::Unknown(at));
        }
        if let Some(cur) = self.cur_fn {
            self.ct.inst_origin.entry(fid).or_insert((at, cur));
        }
        let nerr = self.error_count();
        self.check_body_now(fid, ast);
        if self.error_count() > nerr {
            return Err(Ctl::Quiet);
        }
        match self.out.get_mut(fid).and_then(Option::take) {
            Some(tf) => {
                let b = Rc::new(tf);
                self.ct.bodies.insert(fid, b.clone());
                Ok(b)
            }
            None => Err(Ctl::Quiet),
        }
    }

    /// Check the body of `fid` now, with the function being checked set aside.
    fn check_body_now(&mut self, fid: FuncId, ast: AstFn) {
        let mut st = FnState::default();
        self.swap_state(&mut st);
        let lambdas = std::mem::take(&mut self.lambda_stack);
        let lambda_funcs = std::mem::take(&mut self.lambda_funcs);
        let saved = (
            self.cur_fn,
            self.in_template,
            std::mem::take(&mut self.type_env),
            self.cur_generic.take(),
            self.ns,
            std::mem::take(&mut self.lambda_assigned),
            self.ct.mode,
            self.ct.quiet,
            std::mem::take(&mut self.unit_ok),
            self.declaring_default.take(),
            std::mem::take(&mut self.extra_default_ifaces),
        );
        self.ct.mode = 0;
        self.ct.quiet = 0;
        self.ns = ast.ns;
        self.ct.checking.push(fid);
        // SAFETY: `asts` points into the programs being checked, which
        // outlive `program()` (and are dropped from `asts` before it returns).
        let decl: &FnDecl = unsafe { &*ast.decl };
        if self.funcs[fid].instance.is_some() {
            self.build_instance(fid, decl, ast.owner);
        } else {
            let tf = self.fn_body(decl, ast.owner, fid);
            self.record_calls(&tf.body, None);
            put_func(&mut self.out, fid, tf);
            for (lid, lf) in std::mem::take(&mut self.lambda_funcs) {
                put_func(&mut self.out, lid, lf);
            }
        }
        self.ct.checking.pop();
        self.swap_state(&mut st);
        self.lambda_stack = lambdas;
        self.lambda_funcs = lambda_funcs;
        (
            self.cur_fn,
            self.in_template,
            self.type_env,
            self.cur_generic,
            self.ns,
            self.lambda_assigned,
            self.ct.mode,
            self.ct.quiet,
            self.unit_ok,
            self.declaring_default,
            self.extra_default_ifaces,
        ) = saved;
    }

    /// Where code from the program itself first needs the copy of generic
    /// code being checked (following copies needed by other copies).
    pub(super) fn user_site(&self) -> Option<Span> {
        let mut f = self.cur_fn?;
        let mut site = None;
        for _ in 0..64 {
            if self.funcs[f].instance.is_none() && !self.ct.comptime_only.contains(&f) {
                break;
            }
            let Some(&(span, caller)) = self.ct.inst_origin.get(&f) else { break };
            site = Some(span);
            if caller == f {
                break;
            }
            f = caller;
        }
        site
    }

    /// A call of a `comptime fn` in code that runs: it runs while compiling
    /// instead, and its result goes in as a constant.
    pub(super) fn comptime_call(&mut self, call: TExpr, name_span: Span) -> TExpr {
        let TExprKind::Call(fid, _) = &call.kind else { return call };
        let decl = self.fn_decl_of(*fid);
        if !self.ct.comptime_fns.contains(&decl) || self.ct.mode > 0 || call.ty.is_error() {
            return call;
        }
        if self.in_template || self.ct.quiet > 0 {
            return call;
        }
        let span = call.span;
        match self.ct_eval(&call) {
            Ok(v) => {
                let ty = call.ty.clone();
                match self.materialize(&v, &ty, span) {
                    Ok(t) => t,
                    Err(why) => {
                        let name = self.funcs[decl].name.clone();
                        self.err(Diagnostic::error(format!("what `comptime fn {name}` gives can't be kept for when the program runs: {why}"), span).with_primary("used by code that runs"));
                        Self::error_expr(span)
                    }
                }
            }
            Err(Ctl::Unknown(at)) => {
                let name = self.funcs[decl].name.clone();
                self.err(
                    Diagnostic::error(format!("`{name}` is a `comptime fn`: its arguments must be known while compiling"), at)
                        .with_primary("only known when the program runs")
                        .with_label(name_span, "runs while compiling")
                        .with_note("a `comptime fn` only runs while compiling: called from code that runs, it's run while compiling and its result put in"),
                );
                Self::error_expr(span)
            }
            Err(e) => self.report_ctl(e, span),
        }
    }
}

/// A value as shown in a hover (`3`, `"a"`), for simple values.
fn shown_value(v: &Value) -> Option<String> {
    Some(match v {
        Value::Int(n) => n.to_string(),
        Value::Float(x) => super::interp::format_float(*x),
        Value::Bool(b) => b.to_string(),
        Value::Str(s) if s.len() <= 60 => format!("{:?}", s.as_ref()),
        _ => return None,
    })
}

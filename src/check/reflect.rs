//! Compile-time reflection and attributes.
//!
//! `fields(T)` lists the fields of struct `T` and `params(H)` the
//! parameters of a function, as `Field` and `Parameter` values (prelude
//! structs). Loop over them with `comptime for`: each copy of the loop body
//! knows which field it's about, so `f.get(value)` reads that field, with
//! its own type. There's no type information at run time: everything here
//! is worked out while compiling, and what the running program needs of it
//! is turned back into plain values (`materialize`).
//!
//! An attribute is declared like a small struct (`attribute get(path:
//! String)`) and written on a declaration (`@get("/users/{id}")`). Its
//! arguments are checked like a struct's, and its value is worked out
//! (by the interpreter, `interp.rs`) the first time compile-time code reads
//! it: `attribute(H, get)`, `f.attribute(rename)`, `f.has(skip)`.
//!
//! A type parameter bounded by `Function` (`fn route[H: Function](h: H)`)
//! stands for one particular named function: each function passed gets a
//! type of its own (a struct with no fields, `fn show_user`), so every copy
//! of `route` knows which function it has, and can read its parameters and
//! attributes and call it directly.

use super::interp::{Ctl, Interp, StructVal, Value, list_val, str_val};
use super::*;
use std::cell::RefCell;
use std::rc::Rc;

/// An attribute written on a declaration: `@get("/users/{id}")`.
pub(super) struct AttrUse {
    /// The attribute (its declaration, in the struct table).
    pub attr: StructId,
    /// From `@` to the end.
    pub span: Span,
    /// Its value, built: the checked expression and how many locals it uses
    /// (`None` after an error in its arguments).
    pub build: Option<Rc<(TExpr, usize)>>,
    /// Worked out (`Err`: it failed, and that was reported).
    pub value: Option<Result<Value, ()>>,
}

/// An attribute seen while declaring things, checked once every type and
/// function is known.
pub(super) struct PendingAttr {
    pub target: AttrTarget,
    pub attr: Attribute,
    pub ns: usize,
}

/// What `fields(X)`, `attribute(X, a)`... are asked about.
pub(super) enum Subject {
    Type(Type),
    Fn(FuncId),
    /// A `Field` or `Parameter` value (checked).
    Desc(TExpr),
    /// Not known yet (a type parameter, in generic code).
    Unknown,
}

/// Types an attribute's fields may have: values known while compiling,
/// which the running program can be given as constants.
fn attr_field_ok(t: &Type) -> bool {
    match t {
        Type::Int | Type::Float | Type::Bool | Type::Str | Type::Enum(_, false) | Type::Error => true,
        Type::Nullable(x) | Type::List(x) => attr_field_ok(x),
        _ => false,
    }
}

impl Checker {
    // ---------- declarations ----------

    /// Pass 2: the fields of `attribute name(...)`.
    pub(super) fn declare_attribute<'p>(&mut self, sid: StructId, a: &'p AttributeDecl, ns: usize, bodies: &mut Vec<(&'p FnDecl, Option<StructId>, usize, FuncId)>) {
        let mut seen: HashMap<&str, Span> = HashMap::new();
        let mut fields = Vec::new();
        let mut defaults: Vec<(usize, &FnDecl)> = Vec::new();
        for f in &a.fields {
            if let Some(prev) = seen.get(f.name.name.as_str()) {
                self.err(Diagnostic::error(format!("field `{}` is declared more than once", f.name.name), f.name.span).with_label(*prev, "first declared here"));
                continue;
            }
            seen.insert(&f.name.name, f.name.span);
            let ty = self.resolve_type(&f.ty);
            if !attr_field_ok(&ty) {
                let tn = self.tn(&ty);
                self.err(
                    Diagnostic::error(format!("an attribute's field can't be a `{tn}`"), f.ty.span)
                        .with_primary("not a value known while compiling")
                        .with_note("an attribute's fields can be `Int`, `Float`, `Bool`, `String`, enums without data, and `T?` or `List[T]` of those"),
                );
            }
            let tn = self.tn(&ty);
            let default = f.default.as_ref().map(|d| {
                defaults.push((fields.len(), &**d));
                match d.body.first().map(|st| &st.kind) {
                    Some(StmtKind::Return(Some(v))) => self.snippet(v.span),
                    _ => None,
                }
                .unwrap_or_else(|| "...".into())
            });
            self.ide.structs[sid].fields.push(FieldSymbol { name: f.name.name.clone(), ty: tn.clone(), mutable: false, span: f.name.span, default: default.clone() });
            self.field_spans[sid].push(f.name.span);
            let shown = match &default {
                Some(d) => format!("{tn} = {d}"),
                None => tn.clone(),
            };
            let aname = self.structs[sid].name.clone();
            self.hover(f.name.span, format!("{}: {shown}", f.name.name), Some(format!("field of attribute `{aname}`")));
            fields.push(TField { name: f.name.name.clone(), ty, mutable: false });
        }
        self.structs[sid].fields = fields;
        self.fields_ready[sid] = true;
        for (idx, d) in defaults {
            let fid = self.declare_fn(d, Some(sid));
            self.defaults.insert((sid, idx), fid);
            bodies.push((d, Some(sid), ns, fid));
        }
    }

    /// Keep the attributes written on something, to check them later.
    pub(super) fn collect_attrs(&mut self, target: AttrTarget, attrs: &[Attribute], ns: usize) {
        for a in attrs {
            self.ct.pending_attrs.push(PendingAttr { target, attr: a.clone(), ns });
        }
    }

    /// The attributes on function `fid` and its parameters.
    pub(super) fn collect_fn_attrs(&mut self, f: &FnDecl, fid: FuncId, ns: usize) {
        self.collect_attrs(AttrTarget::Fn(fid), &f.attrs, ns);
        let skip = f.self_param.is_some() as usize;
        for (i, p) in f.params.iter().enumerate() {
            self.collect_attrs(AttrTarget::Param(fid, i + skip), &p.attrs, ns);
        }
    }

    /// The attribute named by `module.name` or `name`, in the code being checked.
    #[allow(clippy::result_large_err)]
    fn find_attr(&mut self, module: Option<&Ident>, name: &Ident) -> Result<StructId, Diagnostic> {
        let sid = match module {
            Some(m) => {
                let Some(module_ns) = self.imported_module(&m.name) else {
                    return Err(Diagnostic::error(format!("cannot find module `{}`", m.name), m.span).with_help(format!("add `import {}` at the top of the file", m.name)));
                };
                self.module_hover(module_ns, m.span);
                self.struct_index.get(&(module_ns, name.name.clone())).copied()
            }
            None => self.visible_struct(&name.name),
        };
        match sid {
            Some(sid) if self.ct.attr_decls.contains(&sid) => {
                self.pending_struct_hovers.push((name.span, sid));
                self.ide.definitions.push((name.span, self.struct_spans[sid]));
                Ok(sid)
            }
            Some(sid) => {
                let tn = self.structs[sid].name.clone();
                Err(Diagnostic::error(format!("`{tn}` isn't an attribute"), name.span)
                    .with_primary("not an attribute")
                    .with_note(format!("attributes are declared with `attribute`, like `attribute {}(path: String)`", name.name)))
            }
            None => {
                let ns = self.ns;
                let known: Vec<String> = self.ct.attr_decls.iter().filter(|s| self.struct_ns[**s] == ns || Some(self.struct_ns[**s]) == self.prelude_ns).map(|s| self.structs[*s].name.clone()).collect();
                let mut d = Diagnostic::error(format!("cannot find attribute `{}`", name.name), name.span).with_primary("not found");
                if let Some(s) = suggest(&name.name, known.iter().map(|s| s.as_str())) {
                    d = d.with_help(format!("did you mean `@{s}`?"));
                } else {
                    d = d.with_note("an attribute must be declared (`attribute name(...)`) in this file, or imported from the module that declares it");
                }
                Err(d)
            }
        }
    }

    /// Before the bodies: check the arguments of every attribute written.
    pub(super) fn check_attr_uses(&mut self) {
        let saved_ns = self.ns;
        for p in std::mem::take(&mut self.ct.pending_attrs) {
            self.ns = p.ns;
            let a = &p.attr;
            let attr = match self.find_attr(a.module.as_ref(), &a.name) {
                Ok(sid) => sid,
                Err(d) => {
                    self.err(d);
                    continue;
                }
            };
            if let Some(&prev) = self.ct.attr_index.get(&p.target).and_then(|v| v.iter().find(|&&i| self.ct.attr_uses[i].attr == attr)) {
                let prev = self.ct.attr_uses[prev].span;
                self.err(Diagnostic::error(format!("`@{}` is written twice here", a.name.name), a.span).with_primary("written again").with_label(prev, "first written here"));
                continue;
            }
            // Its arguments, like a struct's, in a function of their own, worked out while compiling.
            let mut outer = FnState::default();
            self.swap_state(&mut outer);
            let saved = (self.cur_generic.take(), self.in_template, std::mem::take(&mut self.type_env), self.cur_fn.take());
            self.in_template = false;
            self.ct.mode += 1;
            let nerr = self.error_count();
            let t = self.construct(attr, &a.name.name, a.name.span, &a.args, None, None, a.span);
            let ok = self.error_count() == nerr;
            self.ct.mode -= 1;
            let nlocals = self.locals.len();
            self.swap_state(&mut outer);
            (self.cur_generic, self.in_template, self.type_env, self.cur_fn) = saved;
            for (lid, lf) in std::mem::take(&mut self.lambda_funcs) {
                self.ct.comptime_only.insert(lid);
                put_func(&mut self.out, lid, lf);
            }
            let i = self.ct.attr_uses.len();
            self.ct.attr_uses.push(AttrUse { attr, span: a.span, build: ok.then(|| Rc::new((t, nlocals))), value: None });
            self.ct.attr_index.entry(p.target).or_default().push(i);
        }
        self.ns = saved_ns;
    }

    /// The value of attribute `attr` on `target` (`null` if it isn't there).
    pub(super) fn attr_value(&mut self, target: AttrTarget, attr: StructId, span: Span) -> Result<Value, Ctl> {
        let Some(i) = self.ct.attr_index.get(&target).and_then(|v| v.iter().copied().find(|&i| self.ct.attr_uses[i].attr == attr)) else {
            return Ok(Value::Null);
        };
        self.attr_use_value(i, Some(span))
    }

    /// Work out the value of attribute use `i` (once).
    fn attr_use_value(&mut self, i: usize, wanted_at: Option<Span>) -> Result<Value, Ctl> {
        match &self.ct.attr_uses[i].value {
            Some(Ok(v)) => return Ok(v.clone()),
            Some(Err(())) => return Err(Ctl::Quiet),
            None => {}
        }
        let at = self.ct.attr_uses[i].span;
        let Some(build) = self.ct.attr_uses[i].build.clone() else {
            self.ct.attr_uses[i].value = Some(Err(()));
            return Err(Ctl::Quiet);
        };
        let r = Interp::new(self).eval_here(&build.0, vec![None; build.1]);
        match r {
            Ok(Value::Struct(sv)) => {
                let fields = sv.fields.borrow().clone();
                let v = Value::Struct(Rc::new(StructVal { sid: sv.sid, fields: RefCell::new(fields), origin: Some(at) }));
                self.ct.attr_uses[i].value = Some(Ok(v.clone()));
                Ok(v)
            }
            Ok(v) => {
                self.ct.attr_uses[i].value = Some(Ok(v.clone()));
                Ok(v)
            }
            Err(e) => {
                self.ct.attr_uses[i].value = Some(Err(()));
                let mut d = match e {
                    Ctl::Fail(d) => self.ct_failure(*d, at),
                    Ctl::Unknown(s) => Diagnostic::error("an attribute's value must be known while compiling", s).with_primary("not known while compiling"),
                    Ctl::CompileError { message, at: a, raised } => Diagnostic::error(message, a.unwrap_or(raised)),
                    Ctl::Quiet => return Err(Ctl::Quiet),
                    Ctl::Break | Ctl::Continue | Ctl::Return(_) => Diagnostic::error("internal error: an attribute's value", at),
                };
                d = d.with_label(at, "while working out this attribute");
                if let Some(w) = wanted_at
                    && w != at
                {
                    d = d.with_label(w, "read here");
                }
                self.err(d);
                Err(Ctl::Quiet)
            }
        }
    }

    /// At the end: every attribute's value is worked out, so a mistake in one
    /// nobody reads is still reported.
    pub(super) fn finish_attrs(&mut self) {
        for i in 0..self.ct.attr_uses.len() {
            let _ = self.attr_use_value(i, None);
        }
    }

    // ---------- descriptors ----------

    /// The prelude's `Field` and `Parameter` structs.
    pub(super) fn meta_types(&self) -> (Option<StructId>, Option<StructId>) {
        (self.prelude_type("Field"), self.prelude_type("Parameter"))
    }

    pub(super) fn is_descriptor(&self, t: &Type) -> bool {
        let (f, p) = self.meta_types();
        matches!(t, Type::Struct(s) if Some(*s) == f || Some(*s) == p)
    }

    /// The field values of the `Field` describing field `i` of struct `sid`.
    pub(super) fn field_info(&mut self, sid: StructId, i: usize) -> Vec<Value> {
        let Some(fsid) = self.meta_types().0 else { return Vec::new() };
        let f = self.structs[sid].fields[i].clone();
        let tuple = self.tuples.iter().flatten().any(|&t| t == self.struct_origin[sid]);
        let name = if tuple { f.name.trim_start_matches('_').to_string() } else { f.name.clone() };
        let has_default = self.defaults.contains_key(&(self.struct_origin[sid], i));
        let tn = self.tn(&f.ty);
        self.structs[fsid]
            .fields
            .iter()
            .map(|m| match m.name.as_str() {
                "name" => str_val(&name),
                "type_name" => str_val(&tn),
                "mutable" => Value::Bool(f.mutable),
                "has_default" => Value::Bool(has_default),
                _ => Value::Null,
            })
            .collect()
    }

    /// The field values of the `Parameter` describing parameter `i` of function `fid`.
    pub(super) fn param_info(&mut self, fid: FuncId, i: usize) -> Vec<Value> {
        let Some(psid) = self.meta_types().1 else { return Vec::new() };
        let (name, ty) = self.funcs[fid].params[i].clone();
        let tn = self.tn(&ty);
        self.structs[psid]
            .fields
            .iter()
            .map(|m| match m.name.as_str() {
                "name" => str_val(&name),
                "type_name" => str_val(&tn),
                _ => Value::Null,
            })
            .collect()
    }

    /// The generic declaration of a copy of generic code (or `fid` itself).
    pub(super) fn fn_decl_of(&self, fid: FuncId) -> FuncId {
        match &self.funcs[fid].instance {
            Some((d, _)) => *d,
            None => fid,
        }
    }

    /// Where a value known while compiling comes from, for `compile_error(..., at = x)`.
    pub(super) fn origin_of(&self, v: &Value) -> Option<Span> {
        match v {
            Value::Struct(sv) => sv.origin,
            Value::Field(sid, i) => self.field_spans.get(self.struct_origin[*sid]).and_then(|s| s.get(*i)).copied(),
            Value::Param(fid, i) => self.ct.param_spans.get(&self.fn_decl_of(*fid)).and_then(|s| s.get(*i)).copied(),
            _ => None,
        }
    }

    // ---------- values back into code ----------

    /// Code giving value `v` (of type `ty`), worked out while compiling, to the running program.
    pub(super) fn materialize(&mut self, v: &Value, ty: &Type, span: Span) -> Result<TExpr, String> {
        if let Type::Nullable(inner) = ty {
            if matches!(v, Value::Null) {
                return Ok(Self::mk(TExprKind::Null, ty.clone(), span));
            }
            let x = self.materialize(v, inner, span)?;
            return Ok(self.coerce(x, ty, None));
        }
        let mk = |k: TExprKind, t: Type| Self::mk(k, t, span);
        Ok(match v {
            Value::Int(n) => mk(TExprKind::Int(*n), Type::Int),
            Value::Float(x) => mk(TExprKind::Float(*x), Type::Float),
            Value::Bool(b) => mk(TExprKind::Bool(*b), Type::Bool),
            Value::Str(s) => mk(TExprKind::Str(s.to_string()), Type::Str),
            Value::Null => mk(TExprKind::Null, ty.clone().nullable()),
            Value::Unit => return Err("it's no value (`Unit`)".into()),
            Value::Bytes(_) => return Err("it's `Bytes`".into()),
            Value::Closure(..) => return Err("it's a function".into()),
            Value::Failed => return Err("working it out failed".into()),
            Value::List(items) => {
                let elem = match ty {
                    Type::List(e) => (**e).clone(),
                    _ => return Err("internal: a list of an unknown type".into()),
                };
                let items = items.borrow().clone();
                let mut out = Vec::new();
                for x in &items {
                    out.push(self.materialize(x, &elem, span)?);
                }
                mk(TExprKind::ListLit(out), ty.clone())
            }
            Value::Map(m) => {
                let (kt, vt) = match ty {
                    Type::Map(k, v) => ((**k).clone(), (**v).clone()),
                    _ => return Err("internal: a map of an unknown type".into()),
                };
                let entries = m.borrow().entries.clone();
                let mut out = Vec::new();
                for (k, x) in &entries {
                    out.push((self.materialize(k, &kt, span)?, self.materialize(x, &vt, span)?));
                }
                mk(TExprKind::MapLit(out), ty.clone())
            }
            Value::Struct(sv) => {
                let sid = sv.sid;
                if self.structs[sid].closure.is_some() {
                    // A box: a value used as an interface value.
                    if matches!(self.structs[sid].closure, Some(Type::Iface(_))) {
                        let inner_ty = self.structs[sid].fields[0].ty.clone();
                        let inner = sv.fields.borrow()[0].clone();
                        let x = self.materialize(&inner, &inner_ty, span)?;
                        return Ok(mk(TExprKind::Boxed(sid, Box::new(x)), ty.clone()));
                    }
                    return Err("it's a function".into());
                }
                if self.ct.item_fns.contains_key(&sid) {
                    return Ok(mk(TExprKind::StructNew(sid, Vec::new()), Type::Struct(sid)));
                }
                let fields = sv.fields.borrow().clone();
                let tys: Vec<Type> = self.structs[sid].fields.iter().map(|f| f.ty.clone()).collect();
                let mut out = Vec::new();
                for (x, t) in fields.iter().zip(&tys) {
                    out.push(self.materialize(x, t, span)?);
                }
                mk(TExprKind::StructNew(sid, out), self.def_types[sid].clone())
            }
            Value::Enum(sid, k, fields) => {
                let tys: Vec<Type> = self.structs[*sid].variants()[*k].fields.iter().map(|f| f.ty.clone()).collect();
                let mut out = Vec::new();
                for (x, t) in fields.iter().zip(&tys) {
                    out.push(self.materialize(x, t, span)?);
                }
                mk(TExprKind::EnumNew(*sid, *k, out), self.def_types[*sid].clone())
            }
            Value::Field(sid, i) => {
                let Some(fsid) = self.meta_types().0 else { return Err("internal: no `Field`".into()) };
                let info = self.field_info(*sid, *i);
                let v = Value::Struct(Rc::new(StructVal { sid: fsid, fields: RefCell::new(info), origin: None }));
                return self.materialize(&v, &Type::Struct(fsid), span);
            }
            Value::Param(fid, i) => {
                let Some(psid) = self.meta_types().1 else { return Err("internal: no `Parameter`".into()) };
                let info = self.param_info(*fid, *i);
                let v = Value::Struct(Rc::new(StructVal { sid: psid, fields: RefCell::new(info), origin: None }));
                return self.materialize(&v, &Type::Struct(psid), span);
            }
        })
    }

    /// `materialize`, reporting a failure as an error at `span`.
    fn constant(&mut self, v: &Value, ty: &Type, what: &str, span: Span) -> TExpr {
        match self.materialize(v, ty, span) {
            Ok(t) => t,
            Err(why) => {
                self.err(Diagnostic::error(format!("{what} can't be kept for when the program runs: {why}"), span).with_note("only values like numbers, strings, lists, maps and structs of those can"));
                Self::error_expr(span)
            }
        }
    }

    // ---------- reflection functions ----------

    /// `fields`, `params`, `attribute`, `type_name` and `compile_error`,
    /// unless the program defines functions of those names. `None`: not one.
    pub(super) fn reflect_call(&mut self, name: &str, callee_span: Span, args: &[Arg], span: Span) -> Option<TExpr> {
        if !matches!(name, "fields" | "params" | "attribute" | "type_name" | "compile_error") {
            return None;
        }
        if let Some((code, doc)) = crate::ide::builtin_function(name) {
            self.hover(callee_span, code.to_string(), Some(doc.to_string()));
        }
        Some(match name {
            "fields" => self.reflect_list(args, span, true),
            "params" => self.reflect_list(args, span, false),
            "attribute" => self.reflect_attribute(args, span),
            "type_name" => self.reflect_type_name(args, span),
            _ => self.compile_error_call(args, span),
        })
    }

    fn reflect_args(&mut self, args: &[Arg], n: usize, what: &str, span: Span) -> bool {
        if let Some(a) = args.iter().find(|a| a.name.is_some() && !(what == "compile_error" && a.name.as_ref().is_some_and(|n| n.name == "at"))) {
            let n = a.name.as_ref().unwrap();
            self.err(Diagnostic::error(format!("`{what}` takes no named arguments"), n.span).with_help(format!("remove `{} =`", n.name)));
            return false;
        }
        if args.len() != n {
            self.err(Diagnostic::error(format!("`{what}` takes {n} argument{} but {} were given", if n == 1 { "" } else { "s" }, args.len()), span));
            return false;
        }
        true
    }

    /// What `X` names in `fields(X)`, `attribute(X, a)`...: a type, a function, or a `Field`/`Parameter` value.
    pub(super) fn reflect_subject(&mut self, e: &Expr) -> Option<Subject> {
        let te = match &e.kind {
            ExprKind::Name(n) => {
                if self.lookup(n).is_some() || self.is_outer_local(n) {
                    let t = self.expr(e, None);
                    if t.ty.is_error() {
                        return None;
                    }
                    if !self.is_descriptor(&t.ty) {
                        let tn = self.tn(&t.ty);
                        self.err(Diagnostic::error(format!("`{n}` is a variable (a `{tn}`), not a type or a function"), e.span).with_note("this needs a type (`User`, `T`), a function, or a `Field` or `Parameter`"));
                        return None;
                    }
                    return Some(Subject::Desc(t));
                }
                if let Some((_, t)) = self.type_env.iter().rev().find(|(p, _)| p == n) {
                    let t = t.clone();
                    return Some(self.subject_of_type(t));
                }
                if self.visible_struct(n).is_none()
                    && !RESERVED_TYPES.contains(&n.as_str())
                    && let Some(fid) = self.visible_func(n)
                {
                    self.fn_use_hover(fid, e.span);
                    return Some(Subject::Fn(fid));
                }
                TypeExpr { module: None, name: Ident { name: n.clone(), span: e.span }, args: Vec::new(), nullable: false, func: None, span: e.span }
            }
            ExprKind::Field(m, n) => {
                let ExprKind::Name(mn) = &m.kind else {
                    self.err(Diagnostic::error("expected a type or a function", e.span));
                    return None;
                };
                // `Api.health`: a static method.
                if let Some(fid) = self.static_named(m, n) {
                    return Some(Subject::Fn(fid));
                }
                if let Some(module_ns) = self.imported_module(mn)
                    && self.lookup(mn).is_none()
                    && !self.struct_index.contains_key(&(module_ns, n.name.clone()))
                    && let Some(&fid) = self.func_index.get(&(module_ns, n.name.clone()))
                {
                    self.module_hover(module_ns, m.span);
                    self.fn_use_hover(fid, n.span);
                    return Some(Subject::Fn(fid));
                }
                TypeExpr { module: Some(Ident { name: mn.clone(), span: m.span }), name: n.clone(), args: Vec::new(), nullable: false, func: None, span: e.span }
            }
            ExprKind::TypeApply(base, targs) => match &base.kind {
                ExprKind::Name(n) => TypeExpr { module: None, name: Ident { name: n.clone(), span: base.span }, args: targs.clone(), nullable: false, func: None, span: e.span },
                _ => {
                    self.err(Diagnostic::error("expected a type or a function", e.span));
                    return None;
                }
            },
            _ => {
                self.err(Diagnostic::error("expected a type or a function", e.span).with_note("write the type's name (`User`, `T`, `List[Int]`) or the function's"));
                let _ = self.expr(e, None);
                return None;
            }
        };
        let t = self.resolve_type(&te);
        if t.is_error() {
            return None;
        }
        Some(self.subject_of_type(t))
    }

    /// `Type.name` naming a static method (with its hover).
    fn static_named(&mut self, ty: &Expr, name: &Ident) -> Option<FuncId> {
        let (sid, _) = self.quietly(|c| c.type_ref(ty)).0?;
        let fid = *self.statics.get(&(sid, name.name.clone()))?;
        let _ = self.type_ref(ty);
        self.fn_use_hover(fid, name.span);
        Some(fid)
    }

    fn subject_of_type(&self, t: Type) -> Subject {
        if let Type::Struct(s) = &t
            && let Some(&fid) = self.ct.item_fns.get(s)
        {
            return Subject::Fn(fid);
        }
        if t.has_param(&self.struct_has_param) {
            return Subject::Unknown;
        }
        Subject::Type(t)
    }

    /// `fields(T)` (`fields` true) or `params(H)`.
    fn reflect_list(&mut self, args: &[Arg], span: Span, fields: bool) -> TExpr {
        let what = if fields { "fields" } else { "params" };
        let (fsid, psid) = self.meta_types();
        let Some(elem) = (if fields { fsid } else { psid }) else { return Self::error_expr(span) };
        let ty = Type::List(Box::new(Type::Struct(elem)));
        if !self.reflect_args(args, 1, what, span) {
            return Self::error_expr(span);
        }
        let Some(subject) = self.reflect_subject(&args[0].value) else { return Self::error_expr(span) };
        let at = args[0].value.span;
        let op = match (subject, fields) {
            (Subject::Unknown, _) => {
                return match self.ct.mode {
                    0 => Self::mk(TExprKind::ListLit(Vec::new()), ty, span),
                    _ => Self::mk(TExprKind::Meta(MetaOp::Fields(Type::Error), Vec::new()), ty, span),
                };
            }
            (Subject::Type(Type::Struct(sid)), true) if self.structs[sid].closure.is_none() => MetaOp::Fields(Type::Struct(sid)),
            (Subject::Fn(fid), false) => MetaOp::Params(fid),
            (Subject::Type(t), true) => {
                let tn = self.tn(&t);
                let mut d = Diagnostic::error(format!("`fields` needs a struct, but this is `{tn}`"), at).with_primary("not a struct");
                if matches!(t, Type::Enum(..)) {
                    d = d.with_note("an enum's variants each have fields of their own");
                }
                self.err(d);
                return Self::error_expr(span);
            }
            (Subject::Fn(_), true) => {
                self.err(Diagnostic::error("`fields` needs a struct, but this is a function", at).with_help("a function's parameters are `params(...)`"));
                return Self::error_expr(span);
            }
            (Subject::Type(t), false) => {
                let tn = self.tn(&t);
                self.err(
                    Diagnostic::error(format!("`params` needs a function, but this is `{tn}`"), at)
                        .with_note("pass a function's name, or a type parameter bounded by `Function` (`fn route[H: Function](h: H)`)"),
                );
                return Self::error_expr(span);
            }
            (Subject::Desc(_), _) => {
                self.err(Diagnostic::error(format!("`{what}` needs a {}", if fields { "struct" } else { "function" }), at));
                return Self::error_expr(span);
            }
        };
        if self.ct.mode > 0 {
            return Self::mk(TExprKind::Meta(op, Vec::new()), ty, span);
        }
        let items: Vec<Value> = match op {
            MetaOp::Fields(Type::Struct(sid)) => (0..self.structs[sid].fields.len()).map(|i| Value::Field(sid, i)).collect(),
            MetaOp::Params(fid) => (0..self.funcs[fid].params.len()).map(|i| Value::Param(fid, i)).collect(),
            _ => Vec::new(),
        };
        self.constant(&list_val(items), &ty, "the list", span)
    }

    /// The attribute named by `e` in `attribute(X, a)`, `f.has(a)`.
    fn attr_arg(&mut self, e: &Expr) -> Option<StructId> {
        let r = match &e.kind {
            ExprKind::Name(n) => self.find_attr(None, &Ident { name: n.clone(), span: e.span }),
            ExprKind::Field(m, n) if matches!(m.kind, ExprKind::Name(_)) => {
                let ExprKind::Name(mn) = &m.kind else { unreachable!() };
                self.find_attr(Some(&Ident { name: mn.clone(), span: m.span }), n)
            }
            _ => Err(Diagnostic::error("expected the name of an attribute", e.span).with_help("write it without `@` or arguments: `attribute(H, get)`")),
        };
        match r {
            Ok(s) => Some(s),
            Err(d) => {
                self.err(d);
                None
            }
        }
    }

    /// `attribute(X, a)`.
    fn reflect_attribute(&mut self, args: &[Arg], span: Span) -> TExpr {
        if !self.reflect_args(args, 2, "attribute", span) {
            return Self::error_expr(span);
        }
        let subject = self.reflect_subject(&args[0].value);
        let Some(attr) = self.attr_arg(&args[1].value) else { return Self::error_expr(span) };
        let ty = Type::Struct(attr).nullable();
        let target = match subject {
            None => return Self::error_expr(span),
            Some(Subject::Desc(t)) => return self.desc_attr(t, attr, false, span),
            Some(Subject::Unknown) => None,
            Some(Subject::Fn(fid)) => Some(AttrTarget::Fn(self.fn_decl_of(fid))),
            Some(Subject::Type(t)) => match t.non_null() {
                Type::Struct(s) | Type::Enum(s, _) | Type::Iface(s) => Some(AttrTarget::Type(self.struct_origin[*s])),
                _ => {
                    let tn = self.tn(&t);
                    self.err(Diagnostic::error(format!("`{tn}` has no attributes"), args[0].value.span).with_note("attributes are written on functions, structs, enums, fields and parameters"));
                    return Self::error_expr(span);
                }
            },
        };
        if self.ct.mode > 0 {
            return Self::mk(TExprKind::Meta(MetaOp::AttrOf(target, attr), Vec::new()), ty, span);
        }
        let Some(target) = target else { return Self::mk(TExprKind::Null, ty, span) };
        match self.attr_value(target, attr, span) {
            Ok(v) => self.constant(&v, &ty, "the attribute", span),
            Err(e) => self.report_ctl(e, span),
        }
    }

    /// `type_name(T)`.
    fn reflect_type_name(&mut self, args: &[Arg], span: Span) -> TExpr {
        if !self.reflect_args(args, 1, "type_name", span) {
            return Self::error_expr(span);
        }
        let name = match self.reflect_subject(&args[0].value) {
            None => return Self::error_expr(span),
            Some(Subject::Unknown) => String::new(),
            Some(Subject::Fn(fid)) => self.funcs[self.fn_decl_of(fid)].name.clone(),
            Some(Subject::Type(t)) => self.tn(&t),
            Some(Subject::Desc(_)) => {
                self.err(Diagnostic::error("`type_name` needs a type", args[0].value.span).with_help("a `Field` has its type's name in `f.type_name`"));
                return Self::error_expr(span);
            }
        };
        Self::mk(TExprKind::Str(name), Type::Str, span)
    }

    /// `compile_error(message)` / `compile_error(message, at = x)`.
    fn compile_error_call(&mut self, args: &[Arg], span: Span) -> TExpr {
        let unit = |span| Self::mk(TExprKind::Builtin(Builtin::Panic, vec![Self::mk(TExprKind::Str(String::new()), Type::Str, span)]), Type::Unit, span);
        let msg = args.iter().find(|a| a.name.is_none());
        let at = args.iter().find(|a| a.name.as_ref().is_some_and(|n| n.name == "at"));
        let n = args.len();
        if msg.is_none() || n > 2 || (n == 2 && at.is_none()) {
            self.err(Diagnostic::error("`compile_error` takes a message, and optionally `at = x`", span).with_help("compile_error(\"no route here\", at = route)"));
            for a in args {
                let _ = self.expr(&a.value, None);
            }
            return unit(span);
        }
        self.ct.mode += 1;
        let m = self.expr(&msg.unwrap().value, Some(&Type::Str));
        let m = self.coerce(m, &Type::Str, None);
        let a = at.map(|a| self.expr(&a.value, None));
        self.ct.mode -= 1;
        if self.ct.mode > 0 {
            let mut all = vec![m];
            all.extend(a);
            return Self::mk(TExprKind::Meta(MetaOp::CompileError, all), Type::Unit, span);
        }
        if self.in_template || self.ct.quiet > 0 {
            return unit(span);
        }
        // Code that's compiled: the error is reached.
        let message = match self.ct_eval(&m) {
            Ok(v) => String::from_utf8_lossy(match &v {
                Value::Str(s) => s.as_bytes(),
                _ => b"",
            })
            .into_owned(),
            Err(e) => {
                self.report_ctl(e, m.span);
                return unit(span);
            }
        };
        let at = match a.map(|a| self.ct_eval(&a)) {
            Some(Ok(v)) => self.origin_of(&v),
            Some(Err(e)) => {
                self.report_ctl(e, span);
                return unit(span);
            }
            None => None,
        };
        self.raise_compile_error(message, at, span);
        unit(span)
    }

    /// Report `compile_error(message)` raised at `raised`: at `at` if given,
    /// else where the code that raised it is used from the program's own code.
    pub(super) fn raise_compile_error(&mut self, message: String, at: Option<Span>, raised: Span) {
        let site = at.or_else(|| self.user_site());
        let mut d = Diagnostic::error(message, site.unwrap_or(raised)).with_primary(match (at, site) {
            (Some(_), _) => "here",
            (None, Some(_)) => "in this use",
            (None, None) => "reported here",
        });
        if site.is_some_and(|s| s != raised) {
            let who = self.cur_fn.map(|f| self.fn_display(self.fn_decl_of(f))).unwrap_or_default();
            d = d.with_label(raised, if who.is_empty() { "reported here".to_string() } else { format!("reported by `{who}` here") });
        }
        self.err(d);
    }

    // ---------- `f.get(x)`, `f.attribute(a)`, `f.has(a)` ----------

    pub(super) fn descriptor_method(&mut self, recv: &TExpr, method: &Ident) -> bool {
        self.is_descriptor(&recv.ty) && matches!(method.name.as_str(), "get" | "attribute" | "has")
    }

    /// `f.get(x)`, `f.attribute(a)` or `f.has(a)` on a `Field` or `Parameter`.
    pub(super) fn descriptor_call(&mut self, recv: TExpr, method: &Ident, args: &[Arg], span: Span) -> TExpr {
        let m = method.name.as_str();
        let doc = match m {
            "get" => ("fn get(self, value: T) -> F", "The value of this field in `value`, with the field's own type (`f` must be known while compiling: use it in `comptime for f in fields(T)`)."),
            "attribute" => ("fn attribute(self, a) -> a?", "Attribute `@a(...)` on this field or parameter, or `null`."),
            _ => ("fn has(self, a) -> Bool", "Does this field or parameter have attribute `@a`?"),
        };
        self.hover(method.span, doc.0.to_string(), Some(doc.1.to_string()));
        if args.len() != 1 || args[0].name.is_some() {
            self.err(Diagnostic::error(format!("`{m}` takes one argument"), span));
            for a in args {
                let _ = self.expr(&a.value, None);
            }
            return Self::error_expr(span);
        }
        if m == "get" {
            return self.field_get(recv, &args[0].value, span);
        }
        let Some(attr) = self.attr_arg(&args[0].value) else { return Self::error_expr(span) };
        self.desc_attr(recv, attr, m == "has", span)
    }

    /// The value of `t` if it's known while compiling (here, in this copy of the code).
    fn known_now(&mut self, t: &TExpr) -> Option<Value> {
        if self.in_template && t.ty.has_param(&self.struct_has_param) {
            return None;
        }
        self.ct_eval(t).ok()
    }

    /// Code reading a compile-time local was folded away: it's not read at run time after all.
    pub(super) fn unread(&mut self, t: &TExpr) {
        if let TExprKind::Local(id) = t.kind
            && self.ct.mode == 0
            && let Some(n) = self.ct_reads.get_mut(&id)
        {
            n.0 = n.0.saturating_sub(1);
        }
    }

    /// `f.attribute(a)` (`has` false) or `f.has(a)`.
    fn desc_attr(&mut self, recv: TExpr, attr: StructId, has: bool, span: Span) -> TExpr {
        let ty = if has { Type::Bool } else { Type::Struct(attr).nullable() };
        if self.ct.mode > 0 {
            let op = if has { MetaOp::DescHas(attr) } else { MetaOp::DescAttr(attr) };
            return Self::mk(TExprKind::Meta(op, vec![recv]), ty, span);
        }
        let target = match self.known_now(&recv) {
            Some(Value::Field(sid, i)) => AttrTarget::Field(self.struct_origin[sid], i),
            Some(Value::Param(fid, i)) => AttrTarget::Param(self.fn_decl_of(fid), i),
            _ if self.in_template || self.ct.quiet > 0 => return Self::mk(if has { TExprKind::Bool(false) } else { TExprKind::Null }, ty, span),
            _ => {
                let what = if has { "has" } else { "attribute" };
                self.err(
                    Diagnostic::error(format!("`{what}(...)` needs to know which field or parameter this is while compiling"), recv.span)
                        .with_primary("not known while compiling")
                        .with_help("use it on the variable of a `comptime for` (`comptime for f in fields(T)`), in a `comptime val`, or in a `comptime fn`"),
                );
                return Self::error_expr(span);
            }
        };
        self.unread(&recv);
        match self.attr_value(target, attr, span) {
            Ok(v) if has => Self::mk(TExprKind::Bool(!matches!(v, Value::Null)), Type::Bool, span),
            Ok(v) => self.constant(&v, &ty, "the attribute", span),
            Err(e) => self.report_ctl(e, span),
        }
    }

    /// `f.get(x)`: field `f` of `x`.
    fn field_get(&mut self, recv: TExpr, x: &Expr, span: Span) -> TExpr {
        let (fsid, _) = self.meta_types();
        if recv.ty != Type::Struct(fsid.unwrap_or(usize::MAX)) {
            self.err(Diagnostic::error("a `Parameter` has no value to get", span).with_note("`get` reads a field of a struct value: use it on a `Field`"));
            let _ = self.expr(x, None);
            return Self::error_expr(span);
        }
        match self.known_now(&recv) {
            Some(Value::Field(sid, i)) => {
                self.unread(&recv);
                let st = Type::Struct(sid);
                let t = self.expr(x, Some(&st));
                if !t.ty.compatible(&st) {
                    let (want, found) = (self.tn(&st), self.tn(&t.ty));
                    self.err(Diagnostic::error(format!("`{}` is a field of `{want}`, but this is {}", self.structs[sid].fields[i].name, a_type(&found)), t.span).with_primary(format!("expected `{want}`")));
                    return Self::error_expr(span);
                }
                let fty = self.structs[sid].fields[i].ty.clone();
                Self::mk(TExprKind::Field(Box::new(t), sid, i), fty, span)
            }
            _ if self.in_template || self.ct.quiet > 0 => {
                let _ = self.expr(x, None);
                Self::error_expr(span)
            }
            _ => {
                self.err(
                    Diagnostic::error("`get` needs to know which field this is while compiling", recv.span)
                        .with_primary("not known while compiling")
                        .with_note("the value it gives has the field's own type, so the field must be known")
                        .with_help("loop over the fields with `comptime for f in fields(T):`"),
                );
                let _ = self.expr(x, None);
                Self::error_expr(span)
            }
        }
    }

    // ---------- functions as types: `[H: Function]` ----------

    /// The type standing for named function `fid` (a struct without fields).
    pub(super) fn fn_item_type(&mut self, fid: FuncId) -> StructId {
        if let Some(&s) = self.ct.fn_items.get(&fid) {
            return s;
        }
        let span = self.funcs[fid].span;
        let s = self.closure_struct(span, false);
        let name = format!("fn {}", self.funcs[fid].name);
        self.structs[s].name = name.clone();
        self.ide.structs[s].name = format!("<{name}>");
        self.ct.fn_items.insert(fid, s);
        self.ct.item_fns.insert(s, fid);
        s
    }

    /// An argument for a parameter of type `H` bounded by `Function`: a
    /// named function, as a value of its own type. `None`: not a named function.
    pub(super) fn fn_item_arg(&mut self, e: &Expr) -> Option<TExpr> {
        let fid = match &e.kind {
            ExprKind::Name(n) if self.lookup(n).is_none() && !self.is_outer_local(n) => {
                let fid = self.visible_func(n)?;
                self.fn_use_hover(fid, e.span);
                fid
            }
            ExprKind::Field(m, n) => {
                let ExprKind::Name(mn) = &m.kind else { return None };
                if self.lookup(mn).is_some() {
                    return None;
                }
                if let Some(fid) = self.static_named(m, n) {
                    let s = self.fn_item_type(fid);
                    return Some(Self::mk(TExprKind::StructNew(s, Vec::new()), Type::Struct(s), e.span));
                }
                let module_ns = self.imported_module(mn)?;
                let fid = *self.func_index.get(&(module_ns, n.name.clone()))?;
                self.module_hover(module_ns, m.span);
                self.fn_use_hover(fid, n.span);
                fid
            }
            _ => return None,
        };
        if !self.funcs[fid].type_params.is_empty() {
            let name = self.funcs[fid].name.clone();
            self.err(Diagnostic::error(format!("`{name}` is generic: it can't be passed as a `Function`"), e.span).with_help("pass a function that isn't generic (wrap it in one if needed)"));
            return Some(Self::error_expr(e.span));
        }
        let s = self.fn_item_type(fid);
        Some(Self::mk(TExprKind::StructNew(s, Vec::new()), Type::Struct(s), e.span))
    }

    /// After inference: a type parameter bounded by `Function` must stand for a named function.
    pub(super) fn check_fn_items(&mut self, tparams: &[ParamId], bound: &[Type], what: &str, span: Span) {
        for (p, t) in tparams.iter().zip(bound) {
            if !self.ct.fn_item_params.contains(p) || t.is_error() {
                continue;
            }
            let ok = match t {
                Type::Struct(s) => self.ct.item_fns.contains_key(s),
                Type::Param(q) => self.ct.fn_item_params.contains(q),
                _ => false,
            };
            if !ok {
                let (pn, tn) = (self.type_params[*p].name.clone(), self.tn(t));
                self.err(
                    Diagnostic::error(format!("{what} needs a named function for `{pn}`, found `{tn}`"), span)
                        .with_note(format!("`{pn}: Function` stands for one particular function, so the code can read its parameters and attributes"))
                        .with_help("pass a function by its name"),
                );
            }
        }
    }

    /// Calling `h` whose type stands for a named function (or for one, in generic code).
    pub(super) fn item_call(&mut self, f: &TExpr, args: &[Arg], span: Span) -> Option<TExpr> {
        match &f.ty {
            Type::Struct(s) if self.ct.item_fns.contains_key(s) => {
                let fid = self.ct.item_fns[s];
                let name = self.funcs[fid].name.clone();
                if args.iter().any(|a| matches!(a.value.kind, ExprKind::Spread { .. })) {
                    let params = self.funcs[fid].params.clone();
                    let ret = self.funcs[fid].ret.clone();
                    let what = format!("function `{name}`");
                    return Some(match self.spread_args(&what, &params, args, span) {
                        Ok(slots) => {
                            let mut vals = Vec::new();
                            for (k, s) in slots.into_iter().enumerate() {
                                match s {
                                    Some(t) => vals.push(t),
                                    None => {
                                        self.err(Diagnostic::error(format!("no value for parameter `{}` of `{name}`", params[k].0), span));
                                        return Some(Self::error_expr(span));
                                    }
                                }
                            }
                            Self::mk(TExprKind::Call(fid, vals), ret, span)
                        }
                        Err(None) => {
                            self.quiet_args(args);
                            Self::error_expr(span)
                        }
                        Err(Some(())) => Self::error_expr(span),
                    });
                }
                let what = format!("function `{name}`");
                Some(self.call_fn(fid, &what, None, args, None, None, f.span, span))
            }
            Type::Param(p) if self.ct.fn_item_params.contains(p) => {
                self.quiet_args(args);
                Some(Self::error_expr(span))
            }
            _ => None,
        }
    }
}

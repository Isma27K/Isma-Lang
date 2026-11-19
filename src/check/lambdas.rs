//! First-class functions: function types, lambdas and the values they
//! capture, named functions used as values, and calls through function values.
//!
//! A function value is a closure: the code to run, plus the values a lambda
//! captured from the code around it. A `val`, parameter or loop variable is
//! copied in when the lambda is created (it can't change). A `var` is shared:
//! the first time a lambda captures one, the variable moves into a *cell*, a
//! small struct with one mutable field, and the function and every lambda
//! using it read and write that field (`apply_cells` rewrites the function's
//! own uses once its body is checked). Lists and structs are shared, as
//! everywhere else, so a lambda can change their contents too.
//!
//! Each lambda's body becomes a function of its own (closure code) whose
//! first parameter is the closure, a struct with one immutable field per
//! captured value. Every parameter of closure code is borrowed: a call
//! through a value can't know which function it reaches.
//!
//! The list methods that take functions (`map`, `filter`...) don't go
//! through any of this when given a lambda directly: see `list_fns.rs`.

use super::*;

/// The per-function state of the checker, set aside while a lambda's body is checked.
pub(super) struct FnState {
    pub locals: Vec<TLocal>,
    local_meta: Vec<LocalMeta>,
    scopes: Vec<HashMap<String, LocalBinding>>,
    nonnull: Vec<NullPath>,
    loop_depth: usize,
    ret_ty: Type,
    in_lambda_expr: usize,
    cells: HashMap<LocalId, StructId>,
    cell_assigned: HashMap<LocalId, Span>,
}

impl Default for FnState {
    fn default() -> Self {
        FnState {
            locals: Vec::new(),
            local_meta: Vec::new(),
            scopes: vec![HashMap::new()],
            nonnull: Vec::new(),
            loop_depth: 0,
            ret_ty: Type::Unit,
            in_lambda_expr: 0,
            cells: HashMap::new(),
            cell_assigned: HashMap::new(),
        }
    }
}

/// A lambda whose body is being checked.
pub(super) struct LambdaFrame {
    /// The state of the code around the lambda.
    outer: FnState,
    /// The closure (the lambda's first parameter) and its struct.
    env: LocalId,
    sid: StructId,
    /// Captured names, in field order, each with how the code around the
    /// lambda reads it (evaluated when the lambda is created), and for a
    /// `var`, its cell's struct (the capture is the cell).
    captures: Vec<(String, TExpr, Option<StructId>)>,
}

impl Checker {
    // ---------- types ----------

    /// `fn(Int, String) -> Bool`
    pub(super) fn resolve_fn_type(&mut self, f: &FnTypeExpr) -> Type {
        let params = f.params.iter().map(|p| self.resolve_type(p)).collect();
        let ret = f.ret.as_ref().map(|r| self.resolve_ret_type(r)).unwrap_or(Type::Unit);
        Type::Func(params, Box::new(ret))
    }

    // ---------- state ----------

    pub(super) fn swap_state(&mut self, s: &mut FnState) {
        std::mem::swap(&mut self.locals, &mut s.locals);
        std::mem::swap(&mut self.local_meta, &mut s.local_meta);
        std::mem::swap(&mut self.scopes, &mut s.scopes);
        std::mem::swap(&mut self.nonnull, &mut s.nonnull);
        std::mem::swap(&mut self.loop_depth, &mut s.loop_depth);
        std::mem::swap(&mut self.ret_ty, &mut s.ret_ty);
        std::mem::swap(&mut self.in_lambda_expr, &mut s.in_lambda_expr);
        std::mem::swap(&mut self.cells, &mut s.cells);
        std::mem::swap(&mut self.cell_assigned, &mut s.cell_assigned);
    }

    /// A struct holding one lambda's captured values (fields are added as
    /// captures are found). Never seen by users: the editor skips it.
    pub(super) fn closure_struct(&mut self, span: Span, generic: bool) -> StructId {
        let id = self.structs.len();
        let name = format!("<lambda {id}>");
        self.structs.push(TStruct { name: name.clone(), fields: Vec::new(), variants: None, generic, closure: None, vtable: Vec::new(), drop: None });
        self.def_types.push(Type::Struct(id));
        self.struct_spans.push(span);
        self.struct_ns.push(self.ns);
        self.struct_origin.push(id);
        self.struct_args.push(Vec::new());
        self.struct_has_param.push(false);
        self.struct_params.push(Vec::new());
        self.fields_ready.push(true);
        self.field_spans.push(Vec::new());
        self.ide.structs.push(StructSymbol {
            name,
            name_span: span,
            full: span,
            fields: Vec::new(),
            variants: None,
            methods: Vec::new(),
            ns: self.ns,
            doc: None,
            type_params: Vec::new(),
            type_args: Vec::new(),
            instance: true,
            iface: false,
            inherited: Vec::new(),
        });
        id
    }

    /// Register closure code `tf` (a lambda's body or a function wrapper) as
    /// function `fid`. Generic code as written only keeps a placeholder.
    pub(super) fn add_closure_code(&mut self, tf: TFunc, span: Span, ty: &Type) -> FuncId {
        let fid = self.funcs.len();
        let params = tf.params.iter().map(|&p| (tf.locals[p].name.clone(), tf.locals[p].ty.clone())).collect();
        self.funcs.push(FuncSig {
            name: tf.name.clone(),
            symbol: tf.symbol.clone(),
            params,
            ret: tf.ret.clone(),
            span,
            type_params: Vec::new(),
            tparams: Vec::new(),
            instance: None,
        });
        let signature = self.tn(ty);
        self.ide.functions.push(FnSymbol { name: tf.name.clone(), signature, name_span: span, full: span, owner: None, ns: self.ns, doc: None, instance: true, is_static: false });
        self.record_calls(&tf.body, if tf.generic { self.cur_generic } else { None });
        if !tf.generic {
            self.lambda_funcs.push((fid, tf));
        }
        fid
    }

    // ---------- lambdas ----------

    /// Check a lambda. `expected`: the type it should have, which gives the
    /// types of parameters written without one (and of what it returns).
    /// `unknown`: type parameters of a generic call still being inferred:
    /// parts of `expected` mentioning them aren't known yet.
    pub(super) fn lambda(&mut self, lam: &Lambda, span: Span, expected: Option<&Type>, unknown: &[ParamId]) -> TExpr {
        let n = lam.params.len();
        let mut exp = match expected.map(Type::non_null) {
            Some(Type::Func(ps, r)) => Some((ps.clone(), (**r).clone())),
            _ => None,
        };
        if let Some((ps, _)) = &exp
            && ps.len() != n
        {
            let et = self.tn(expected.unwrap().non_null());
            self.err(
                Diagnostic::error(format!("this lambda takes {n} parameter{}, but `{et}` takes {}", if n == 1 { "" } else { "s" }, ps.len()), lam.head)
                    .with_primary(format!("expected {} parameter{}", ps.len(), if ps.len() == 1 { "" } else { "s" })),
            );
            exp = None;
        }
        let known = |c: &Checker, t: &Type| !unknown.iter().any(|p| c.mentions(t, *p));
        let mut params: Vec<(Ident, Type)> = Vec::new();
        for (i, p) in lam.params.iter().enumerate() {
            let ty = match &p.ty {
                Some(t) => self.resolve_type(t),
                None => match &exp {
                    Some((ps, _)) if known(self, &ps[i]) => {
                        if p.name.name != "_" {
                            let hint = format!(": {}", self.tn(&ps[i]));
                            self.ide.inlay_hints.push((p.name.span.end, hint));
                        }
                        ps[i].clone()
                    }
                    _ => {
                        let name = &p.name.name;
                        self.err(
                            Diagnostic::error(format!("cannot infer the type of parameter `{name}`"), p.name.span)
                                .with_primary("which type is this?")
                                .with_help(format!("write it: `fn({name}: Int) -> ...`, or pass the lambda where a function type is expected")),
                        );
                        Type::Error
                    }
                },
            };
            params.push((p.name.clone(), ty));
        }
        let target = match &lam.ret {
            Some(t) => Some(self.resolve_ret_type(t)),
            None => exp.as_ref().map(|(_, r)| r.clone()).filter(|r| known(self, r)),
        };

        // The body is checked as a function of its own.
        let generic = self.in_template;
        let sid = self.closure_struct(lam.head, generic);
        let mut outer = FnState::default();
        self.swap_state(&mut outer);
        let env = self.locals.len();
        self.local_meta.push(LocalMeta { name: "_env".into(), span: lam.head, kind: LocalKind::Param, used: true, reassigned: false });
        self.locals.push(TLocal { name: "_env".into(), ty: Type::Struct(sid) });
        self.lambda_stack.push(LambdaFrame { outer, env, sid, captures: Vec::new() });
        let mut ids = vec![env];
        let mut seen: HashMap<String, Span> = HashMap::new();
        for (name, ty) in &params {
            if let Some(prev) = seen.get(&name.name) {
                self.err(Diagnostic::error(format!("parameter `{}` is declared more than once", name.name), name.span).with_label(*prev, "first declared here"));
            }
            if name.name != "_" {
                seen.insert(name.name.clone(), name.span);
            }
            let id = self.new_local(&name.name, ty.clone(), name.span, LocalKind::Param);
            if name.name != "_" {
                self.scopes[0].insert(name.name.clone(), LocalBinding { id, mutable: false, span: name.span });
            }
            ids.push(id);
        }
        let (body, ret) = match &lam.body {
            LambdaBody::Expr(e) => {
                self.in_lambda_expr = 1;
                let t = self.expr(e, target.as_ref().filter(|t| **t != Type::Unit));
                match target {
                    // A value where none is expected is just dropped (`fn(x) -> list.push(x)` as an `fn(Int)`).
                    Some(Type::Unit) => (vec![TStmt::Expr(t)], Type::Unit),
                    Some(r) => {
                        let t = self.coerce(t, &r, None);
                        (vec![TStmt::Return(Some(t))], r)
                    }
                    None if t.ty == Type::Unit => (vec![TStmt::Expr(t)], Type::Unit),
                    None if t.ty == Type::Null => {
                        self.err(
                            Diagnostic::error("cannot tell what this lambda returns from `null` alone", t.span)
                                .with_help("pass it where a function type is expected, e.g. `val f: fn(Int) -> String? = ...`"),
                        );
                        (vec![TStmt::Return(Some(t))], Type::Error)
                    }
                    None => {
                        let r = t.ty.clone();
                        (vec![TStmt::Return(Some(t))], r)
                    }
                }
            }
            LambdaBody::Block(stmts) => {
                let ret = target.unwrap_or(Type::Unit);
                self.ret_ty = ret.clone();
                let body = self.block(stmts, false);
                if ret != Type::Unit && !ret.is_error() && !block_diverges(&body) {
                    let rt = self.tn(&ret);
                    let at = lam.ret.as_ref().map(|r| r.span).unwrap_or(lam.head);
                    self.err(
                        Diagnostic::error("this lambda might not return a value", at)
                            .with_primary(format!("it must return `{rt}` on every path"))
                            .with_help("add a `return` at the end of its block"),
                    );
                }
                (body, ret)
            }
        };
        let mut body = body;
        self.apply_cells(&mut body);
        self.lint_locals();
        let frame = self.lambda_stack.pop().expect("lambda frame");
        let mut inner = frame.outer;
        self.swap_state(&mut inner);

        let fty = Type::Func(params.iter().map(|(_, t)| t.clone()).collect(), Box::new(ret.clone()));
        self.structs[sid].closure = Some(fty.clone());
        let shown = self.tn(&fty);
        self.hover(lam.head, shown, Some("a lambda".into()));
        let id = self.funcs.len();
        let owner = self.cur_fn.map(|f| self.funcs[f].name.clone()).unwrap_or_default();
        let tf = TFunc {
            name: format!("{owner}.<lambda>"),
            symbol: format!("tarn_lambda.{id}"),
            params: ids,
            ret,
            locals: inner.locals,
            body,
            exported: false,
            track_caller: false,
            generic,
            closure: true,
            is_drop: false,
        };
        let func = self.add_closure_code(tf, lam.head, &fty);
        let captures: Vec<TExpr> = frame.captures.into_iter().map(|(_, e, _)| e).collect();
        let sid = (!captures.is_empty()).then_some(sid);
        Self::mk(TExprKind::Closure { func, sid, captures }, fty, span)
    }

    // ---------- captures ----------

    /// Is `name` a variable of the code around the lambda being checked?
    pub(super) fn is_outer_local(&self, name: &str) -> bool {
        self.lambda_stack.iter().any(|f| f.captures.iter().any(|(n, ..)| n == name) || f.outer.scopes.iter().any(|s| s.contains_key(name)))
    }

    /// `name` isn't a local of the lambda being checked: if it belongs to the
    /// code around it, capture it, and read it (from its cell, for a `var`).
    pub(super) fn capture(&mut self, name: &str, span: Span) -> Option<TExpr> {
        let k = self.lambda_stack.len().checked_sub(1)?;
        let (e, cell) = self.capture_into(k, name, span)?;
        Some(match cell {
            Some(c) => {
                let ty = self.structs[c].fields[0].ty.clone();
                Self::mk(TExprKind::Field(Box::new(e), c, 0), ty, span)
            }
            None => e,
        })
    }

    /// `name = ...` inside a lambda, for a `name` from around it: the place
    /// to assign (the field of its cell), or `None` after an error.
    pub(super) fn captured_place(&mut self, name: &str, span: Span) -> Option<(Place, Type)> {
        let k = self.lambda_stack.len().checked_sub(1)?;
        let (e, cell) = self.capture_into(k, name, span)?;
        let Some(c) = cell else {
            if !e.ty.is_error() {
                let decl = self.outer_decl(name);
                let mut d = Diagnostic::error(format!("cannot assign twice to immutable variable `{name}`"), span).with_primary("assigned inside a lambda");
                if let Some(decl) = decl {
                    d = d.with_label(decl, format!("`{name}` is declared with `val` here"));
                }
                self.err(d.with_help(format!("make it mutable with `var {name}`: the lambda then shares it")));
            }
            return None;
        };
        self.mark_cell_assigned(name, span);
        let ty = self.structs[c].fields[0].ty.clone();
        Some((Place::Field { obj: e, sid: c, idx: 0 }, ty))
    }

    /// Where `name`, a variable of the code around the lambdas, is declared.
    fn outer_decl(&self, name: &str) -> Option<Span> {
        self.lambda_stack.iter().rev().find_map(|f| f.outer.scopes.iter().rev().find_map(|s| s.get(name)).map(|b| b.span))
    }

    /// A lambda assigns captured `var` `name`: tell the code that owns it.
    fn mark_cell_assigned(&mut self, name: &str, span: Span) {
        for f in self.lambda_stack.iter_mut().rev() {
            if let Some(b) = f.outer.scopes.iter().rev().find_map(|s| s.get(name)) {
                let id = b.id;
                f.outer.cell_assigned.entry(id).or_insert(span);
                f.outer.local_meta[id].reassigned = true;
                return;
            }
        }
    }

    /// Read `name` inside the body of lambda `k` (of `lambda_stack`), from the
    /// code around it, capturing it there (and in enclosing lambdas, if it
    /// comes from further out). For a `var`, the capture is its cell (also
    /// returned); otherwise it's the value.
    fn capture_into(&mut self, k: usize, name: &str, span: Span) -> Option<(TExpr, Option<StructId>)> {
        let frame = &self.lambda_stack[k];
        if let Some(i) = frame.captures.iter().position(|(n, ..)| n == name) {
            let (ty, cell) = (frame.captures[i].1.ty.clone(), frame.captures[i].2);
            let (env, sid) = (frame.env, frame.sid);
            let field = Self::mk(TExprKind::Field(Box::new(Self::mk(TExprKind::Local(env), Type::Struct(sid), span)), sid, i), ty, span);
            return Some((if cell.is_some() { field } else { self.narrow(field) }, cell));
        }
        // Look it up in the code around lambda `k`, with that code's state in place.
        let mut outer = std::mem::take(&mut self.lambda_stack[k].outer);
        self.swap_state(&mut outer);
        let found = self.lookup(name).map(|b| (b.id, b.span, b.mutable));
        let value = match found {
            Some((id, decl, true)) => {
                // A `var`: shared through its cell.
                self.local_use(id, decl, span, false);
                self.local_meta[id].used = true;
                let ty = self.locals[id].ty.clone();
                let cell = match self.cells.get(&id) {
                    Some(c) => *c,
                    None => {
                        let c = self.cell_struct(name, &ty, decl);
                        self.cells.insert(id, c);
                        c
                    }
                };
                Some((Self::mk(TExprKind::Local(id), Type::Struct(cell), span), Some(cell)))
            }
            Some((id, decl, false)) => Some((self.read_local(id, decl, span), None)),
            None if k > 0 => self.capture_into(k - 1, name, span),
            None => None,
        };
        self.swap_state(&mut outer);
        self.lambda_stack[k].outer = outer;
        let (value, cell) = value?;
        if value.ty.is_error() {
            return Some((value, None));
        }
        let frame = &mut self.lambda_stack[k];
        let i = frame.captures.len();
        let (env, sid, ty) = (frame.env, frame.sid, value.ty.clone());
        frame.captures.push((name.to_string(), value, cell));
        self.structs[sid].fields.push(TField { name: name.to_string(), ty: ty.clone(), mutable: false });
        let field = Self::mk(TExprKind::Field(Box::new(Self::mk(TExprKind::Local(env), Type::Struct(sid), span)), sid, i), ty, span);
        Some((if cell.is_some() { field } else { self.narrow(field) }, cell))
    }

    /// The cell holding `var` `name` (of type `ty`) once a lambda captures it.
    fn cell_struct(&mut self, name: &str, ty: &Type, span: Span) -> StructId {
        let generic = self.in_template;
        let c = self.closure_struct(span, generic);
        let shown = format!("<var {name}>");
        self.structs[c].name = shown.clone();
        self.ide.structs[c].name = shown;
        self.structs[c].fields.push(TField { name: "value".into(), ty: ty.clone(), mutable: true });
        c
    }

    /// Once a function's (or lambda's) body is checked: its `var`s that
    /// lambdas captured live in cells, so their declarations make the cell,
    /// and every other use reads or writes the cell's field.
    pub(super) fn apply_cells(&mut self, body: &mut [TStmt]) {
        if self.cells.is_empty() {
            return;
        }
        let cells = self.cells.clone();
        let mut reported = HashSet::new();
        let mut cx = CellRewrite { cells: &cells, assigned: &self.cell_assigned, errors: Vec::new(), reported: &mut reported, locals: &self.locals };
        for s in body.iter_mut() {
            cx.stmt(s);
        }
        let errors = std::mem::take(&mut cx.errors);
        for d in errors {
            self.err(d);
        }
        for (&id, &c) in &cells {
            self.locals[id].ty = Type::Struct(c);
        }
    }

    // ---------- named functions as values ----------

    /// Function `fid` used as a value (`xs.map(double)`). A generic one gets
    /// its type arguments from `explicit` (`max[Int]`) or from the function
    /// type `expected` here.
    pub(super) fn fn_value(&mut self, fid: FuncId, name: &str, explicit: Option<&[TypeExpr]>, expected: Option<&Type>, span: Span) -> TExpr {
        let tparams = self.funcs[fid].tparams.clone();
        let params: Vec<Type> = self.funcs[fid].params.iter().map(|(_, t)| t.clone()).collect();
        let ret = self.funcs[fid].ret.clone();
        let fid = if tparams.is_empty() {
            if let Some(ex) = explicit {
                for t in ex {
                    let _ = self.resolve_type(t);
                }
                self.err(Diagnostic::error(format!("function `{name}` isn't generic, so it takes no type arguments"), span).with_help("remove the `[...]`"));
                return Self::error_expr(span);
            }
            fid
        } else {
            let mut bound: Vec<Option<Type>> = vec![None; tparams.len()];
            if let Some(ex) = explicit {
                let tys: Vec<Type> = ex.iter().map(|t| self.resolve_type(t)).collect();
                if tys.len() != tparams.len() {
                    let names: Vec<String> = tparams.iter().map(|p| format!("`{}`", self.type_params[*p].name)).collect();
                    self.err(Diagnostic::error(format!("function `{name}` takes {} type argument(s) ({}), found {}", tparams.len(), and_list(&names), tys.len()), span));
                    return Self::error_expr(span);
                }
                bound = tys.into_iter().map(Some).collect();
            } else if let Some(Type::Func(ps, r)) = expected.map(Type::non_null)
                && ps.len() == params.len()
            {
                for (pat, a) in params.iter().zip(ps) {
                    self.unify(pat, a, &tparams, &mut bound, false);
                }
                self.unify(&ret, r, &tparams, &mut bound, false);
            }
            if let Some(i) = bound.iter().position(|b| b.is_none()) {
                let tn = self.type_params[tparams[i]].name.clone();
                self.err(
                    Diagnostic::error(format!("cannot infer the type `{tn}` for function `{name}` used as a value"), span)
                        .with_primary(format!("which type is `{tn}` here?"))
                        .with_help(format!("give it in brackets: `{name}[Int]`, or use it where a function type is expected")),
                );
                return Self::error_expr(span);
            }
            let bound: Vec<Type> = bound.into_iter().map(Option::unwrap).collect();
            let (decl, base) = self.funcs[fid].instance.clone().expect("generic functions have an instance entry");
            let map: Vec<(ParamId, Type)> = tparams.iter().copied().zip(bound).collect();
            let full: Vec<Type> = base.iter().map(|t| self.subst(t, &map)).collect();
            self.fn_instance(decl, full)
        };
        let ty = Type::Func(self.funcs[fid].params.iter().map(|(_, t)| t.clone()).collect(), Box::new(self.funcs[fid].ret.clone()));
        let func = self.fn_wrapper(fid, span);
        Self::mk(TExprKind::Closure { func, sid: None, captures: Vec::new() }, ty, span)
    }

    /// `users.list` (no call): method `name` of `recv` (of struct or enum
    /// `sid`), as a function value that remembers `recv`.
    pub(super) fn method_value(&mut self, recv: TExpr, sid: StructId, name: &Ident, span: Span) -> TExpr {
        let tname = self.structs[sid].name.clone();
        if is_private(&name.name) && self.struct_ns[sid] != self.ns {
            self.err(Diagnostic::error(format!("method `{}` of `{tname}` is private", name.name), name.span));
            return Self::error_expr(span);
        }
        let Some(fid) = self.method(sid, &name.name) else { return Self::error_expr(span) };
        if self.structs[sid].drop == Some(fid) || is_operator_name(&name.name) {
            self.err(Diagnostic::error(format!("`{}` can't be used as a value", name.name), name.span));
            return Self::error_expr(span);
        }
        self.fn_use_hover(fid, name.span);
        if let Some(&p) = self.funcs[fid].tparams.first() {
            let (pn, m) = (self.type_params[p].name.clone(), name.name.clone());
            self.err(
                Diagnostic::error(format!("method `{m}` is generic, so it can't be used as a value"), name.span)
                    .with_primary(format!("which type is `{pn}`?"))
                    .with_help(format!("wrap it in a lambda that says: `fn(x) -> value.{m}(x)`")),
            );
            return Self::error_expr(span);
        }
        let params: Vec<Type> = self.funcs[fid].params[1..].iter().map(|(_, t)| t.clone()).collect();
        let ret = self.funcs[fid].ret.clone();
        let (fname, fsym) = (format!("{}.<bound>", self.funcs[fid].name), format!("{}.bound", self.funcs[fid].symbol));
        self.bound_value(recv, (fid, usize::MAX), params, ret, fname, fsym, span, move |recv, mut args| {
            args.insert(0, recv);
            TExprKind::Call(fid, args)
        })
    }

    /// `shape.area` (no call) on an interface value: the method in its table, as a function value.
    pub(super) fn iface_method_value(&mut self, recv: TExpr, iface: StructId, name: &Ident, span: Span) -> TExpr {
        let Some((slot, decl)) = self.iface_slot(iface, &name.name) else { return Self::error_expr(span) };
        let code = self.ide.functions[decl].signature.clone();
        self.hover(name.span, code, Some(format!("method of interface `{}`", self.structs[iface].name)));
        let params: Vec<Type> = self.funcs[decl].params[1..].iter().map(|(_, t)| t.clone()).collect();
        let ret = self.funcs[decl].ret.clone();
        let (fname, fsym) = (format!("{}.<bound>", self.funcs[decl].name), format!("tarn_iface.{iface}.{slot}.bound"));
        self.bound_value(recv, (iface, slot), params, ret, fname, fsym, span, move |recv, args| TExprKind::CallMethod(Box::new(recv), slot, args))
    }

    /// A function value that calls something (`call`, given the receiver and
    /// the arguments) on `recv`, which it captures. Its code is made once per
    /// `key` (outside generic code as written).
    #[allow(clippy::too_many_arguments)]
    fn bound_value(&mut self, recv: TExpr, key: (usize, usize), params: Vec<Type>, ret: Type, name: String, symbol: String, span: Span, call: impl Fn(TExpr, Vec<TExpr>) -> TExprKind) -> TExpr {
        let fty = Type::Func(params.clone(), Box::new(ret.clone()));
        let recv_ty = recv.ty.clone();
        let cached = if self.in_template { None } else { self.bound_methods.get(&key).copied() };
        let (func, sid) = match cached {
            Some(k) => k,
            None => {
                let generic = self.in_template;
                let sid = self.closure_struct(span, generic);
                self.structs[sid].fields.push(TField { name: "self".into(), ty: recv_ty.clone(), mutable: false });
                self.structs[sid].closure = Some(fty.clone());
                let mut locals = vec![TLocal { name: "_env".into(), ty: Type::Struct(sid) }];
                locals.extend(params.iter().enumerate().map(|(i, t)| TLocal { name: format!("_arg{i}"), ty: t.clone() }));
                let me = Self::mk(TExprKind::Field(Box::new(Self::mk(TExprKind::Local(0), Type::Struct(sid), span)), sid, 0), recv_ty.clone(), span);
                let args: Vec<TExpr> = params.iter().enumerate().map(|(i, t)| Self::mk(TExprKind::Local(i + 1), t.clone(), span)).collect();
                let c = Self::mk(call(me, args), ret.clone(), span);
                let body = if ret == Type::Unit { vec![TStmt::Expr(c)] } else { vec![TStmt::Return(Some(c))] };
                let tf = TFunc {
                    name,
                    symbol,
                    params: (0..=params.len()).collect(),
                    ret,
                    locals,
                    body,
                    exported: false,
                    track_caller: false,
                    generic,
                    closure: true,
                    is_drop: false,
                };
                let func = self.add_closure_code(tf, span, &fty);
                if !generic {
                    self.bound_methods.insert(key, (func, sid));
                }
                (func, sid)
            }
        };
        Self::mk(TExprKind::Closure { func, sid: Some(sid), captures: vec![recv] }, fty, span)
    }

    /// The closure code that calls function `fid` (made once per function).
    fn fn_wrapper(&mut self, fid: FuncId, span: Span) -> FuncId {
        if let Some(&w) = self.fn_wrappers.get(&fid) {
            return w;
        }
        let params = self.funcs[fid].params.clone();
        let ret = self.funcs[fid].ret.clone();
        let fty = Type::Func(params.iter().map(|(_, t)| t.clone()).collect(), Box::new(ret.clone()));
        let mut locals = vec![TLocal { name: "_env".into(), ty: fty.clone() }];
        locals.extend(params.iter().map(|(n, t)| TLocal { name: n.clone(), ty: t.clone() }));
        let args: Vec<TExpr> = params.iter().enumerate().map(|(i, (_, t))| Self::mk(TExprKind::Local(i + 1), t.clone(), span)).collect();
        let call = Self::mk(TExprKind::Call(fid, args), ret.clone(), span);
        let body = if ret == Type::Unit { vec![TStmt::Expr(call)] } else { vec![TStmt::Return(Some(call))] };
        let generic = self.funcs[fid].instance.is_some() && !self.is_concrete_instance(fid);
        let tf = TFunc {
            name: format!("{}.<value>", self.funcs[fid].name),
            symbol: format!("{}.value", self.funcs[fid].symbol),
            params: (0..=params.len()).collect(),
            ret,
            locals,
            body,
            exported: false,
            track_caller: false,
            generic,
            closure: true,
            is_drop: false,
        };
        let w = self.add_closure_code(tf, span, &fty);
        self.fn_wrappers.insert(fid, w);
        self.wrapped.insert(w, fid);
        w
    }

    /// The function a constant function value calls, if it is a named function.
    pub(super) fn named_fn(&self, e: &TExpr) -> Option<FuncId> {
        match &e.kind {
            TExprKind::Closure { func, captures, .. } if captures.is_empty() => self.wrapped.get(func).copied(),
            _ => None,
        }
    }

    // ---------- calls ----------

    /// `f(args)` where `f` is a function value.
    pub(super) fn call_value(&mut self, f: TExpr, args: &[Arg], span: Span) -> TExpr {
        let check_rest = |c: &mut Checker, args: &[Arg]| {
            for a in args {
                let _ = c.expr(&a.value, None);
            }
        };
        let (params, ret) = match f.ty.clone() {
            Type::Func(ps, r) => (ps, *r),
            Type::Error => {
                check_rest(self, args);
                return Self::error_expr(span);
            }
            Type::Nullable(inner) if matches!(*inner, Type::Func(..)) => {
                self.null_use_error(&f, NullUse::Call);
                check_rest(self, args);
                return Self::error_expr(span);
            }
            other => {
                let tn = self.tn(&other);
                let what = self.snippet(f.span).map(|s| format!("`{s}`")).unwrap_or_else(|| "this".into());
                self.err(Diagnostic::error(format!("{what} is {}, not a function", a_type(&tn)), f.span).with_primary("can't be called"));
                check_rest(self, args);
                return Self::error_expr(span);
            }
        };
        if let Some(n) = args.iter().find_map(|a| a.name.as_ref()) {
            self.err(
                Diagnostic::error("a function value takes its arguments by position", n.span)
                    .with_primary("no names here")
                    .with_note("a function type like `fn(Int, String)` doesn't name its parameters")
                    .with_help(format!("remove `{} =`", n.name)),
            );
        }
        if args.len() != params.len() {
            let ft = self.tn(&f.ty);
            self.err(
                Diagnostic::error(format!("this function takes {} argument(s) but {} were given", params.len(), args.len()), span)
                    .with_primary(format!("expected {} argument(s)", params.len()))
                    .with_note(format!("it is `{ft}`")),
            );
            check_rest(self, args);
            return Self::mk(TExprKind::Int(0), if ret == Type::Unit { Type::Unit } else { Type::Error }, span);
        }
        let mut out = Vec::new();
        for (a, p) in args.iter().zip(&params) {
            let t = self.expr(&a.value, Some(p));
            out.push(self.coerce(t, p, None));
        }
        Self::mk(TExprKind::CallValue(Box::new(f), out), ret, span)
    }
}

/// Rewrites a function body for its captured `var`s (see `apply_cells`).
struct CellRewrite<'a> {
    cells: &'a HashMap<LocalId, StructId>,
    assigned: &'a HashMap<LocalId, Span>,
    errors: Vec<Diagnostic>,
    reported: &'a mut HashSet<LocalId>,
    locals: &'a [TLocal],
}

impl CellRewrite<'_> {
    fn cell_ref(id: LocalId, c: StructId, span: Span) -> TExpr {
        TExpr { kind: TExprKind::Local(id), ty: Type::Struct(c), span }
    }

    fn stmts(&mut self, stmts: &mut [TStmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &mut TStmt) {
        match s {
            TStmt::Let(id, e) => {
                self.expr(e);
                if let Some(&c) = self.cells.get(id) {
                    let span = e.span;
                    let v = std::mem::replace(e, TExpr { kind: TExprKind::Int(0), ty: Type::Unit, span });
                    *e = TExpr { kind: TExprKind::StructNew(c, vec![v]), ty: Type::Struct(c), span };
                }
            }
            TStmt::Assign { place, value, .. } => {
                match place {
                    Place::Local(id) => {
                        if let Some(&c) = self.cells.get(id) {
                            *place = Place::Field { obj: Self::cell_ref(*id, c, value.span), sid: c, idx: 0 };
                        }
                    }
                    Place::Field { obj, .. } => self.expr(obj),
                    Place::Index { list: a, index: b, .. } | Place::Custom { obj: a, index: b, .. } | Place::MapEntry { map: a, key: b, .. } => {
                        self.expr(a);
                        self.expr(b);
                    }
                }
                self.expr(value);
            }
            TStmt::Expr(e) | TStmt::Return(Some(e)) => self.expr(e),
            TStmt::If { branches, else_block } => {
                for (c, b) in branches {
                    self.expr(c);
                    self.stmts(b);
                }
                if let Some(b) = else_block {
                    self.stmts(b);
                }
            }
            TStmt::While { cond, body } => {
                self.expr(cond);
                self.stmts(body);
            }
            TStmt::ForRange { start, end, body, .. } => {
                self.expr(start);
                self.expr(end);
                self.stmts(body);
            }
            TStmt::ForList { list, body, .. } | TStmt::ForMap { map: list, body, .. } => {
                self.expr(list);
                self.stmts(body);
            }
            TStmt::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                self.arms(arms);
            }
            TStmt::Break | TStmt::Continue | TStmt::Return(None) => {}
        }
    }

    fn arms(&mut self, arms: &mut [TArm]) {
        for a in arms {
            if let Some(g) = &mut a.guard {
                self.expr(g);
            }
            self.stmts(&mut a.body);
            if let Some(v) = &mut a.value {
                self.expr(v);
            }
        }
    }

    fn exprs(&mut self, es: &mut [TExpr]) {
        for e in es {
            self.expr(e);
        }
    }

    fn expr(&mut self, e: &mut TExpr) {
        match &mut e.kind {
            TExprKind::Local(id) => {
                if let Some(&c) = self.cells.get(id)
                    && e.ty != Type::Struct(c)
                {
                    let r = Self::cell_ref(*id, c, e.span);
                    e.kind = TExprKind::Field(Box::new(r), c, 0);
                }
            }
            // A smart cast of a `var` that a lambda can change isn't safe.
            TExprKind::Unwrap(x) => {
                if let TExprKind::Local(id) = x.kind
                    && let Some(&at) = self.assigned.get(&id)
                    && self.reported.insert(id)
                {
                    let name = self.locals[id].name.clone();
                    self.errors.push(
                        Diagnostic::error(format!("`{name}` can't be smart-cast: a lambda can change it"), x.span)
                            .with_primary(format!("`{name}` was checked for `null`, but it's shared with a lambda"))
                            .with_label(at, "the lambda assigns it here")
                            .with_note("the lambda could run between the check and this use, so the check proves nothing")
                            .with_help(format!("copy it into a `val` first (`val {name}_now = {name}`) and check and use that")),
                    );
                }
                self.expr(x);
            }
            TExprKind::Int(_) | TExprKind::Float(_) | TExprKind::Bool(_) | TExprKind::Str(_) | TExprKind::Null => {}
            TExprKind::Call(_, args) | TExprKind::Builtin(_, args) | TExprKind::StructNew(_, args) | TExprKind::EnumNew(_, _, args) | TExprKind::ListLit(args) | TExprKind::FString(args) => {
                self.exprs(args)
            }
            TExprKind::Binary(_, a, b) | TExprKind::And(a, b) | TExprKind::Or(a, b) | TExprKind::Index(a, b) | TExprKind::Elvis(a, b) | TExprKind::Let(_, a, b) => {
                self.expr(a);
                self.expr(b);
            }
            TExprKind::Neg(x) | TExprKind::Not(x) | TExprKind::Field(x, ..) | TExprKind::IsNull(x) | TExprKind::Wrap(x) | TExprKind::NotNull(x) | TExprKind::Boxed(_, x) => {
                self.expr(x)
            }
            TExprKind::MapLit(entries) => {
                for (k, v) in entries {
                    self.expr(k);
                    self.expr(v);
                }
            }
            TExprKind::ListFind { list, value, test, .. } => {
                self.expr(list);
                self.expr(value);
                self.expr(test);
            }
            TExprKind::ListFormat { list, each, .. } => {
                self.expr(list);
                self.expr(each);
            }
            TExprKind::MapFormat { map, key_str, value_str, .. } => {
                self.expr(map);
                self.expr(key_str);
                self.expr(value_str);
            }
            TExprKind::Try { value, on_err, .. } => {
                self.expr(value);
                self.expr(on_err);
            }
            TExprKind::SafeCall { recv, body, .. } => {
                self.expr(recv);
                self.expr(body);
            }
            TExprKind::Jump(s) => self.stmt(s),
            TExprKind::Closure { captures, .. } => self.exprs(captures),
            TExprKind::CallValue(f, args) | TExprKind::CallMethod(f, _, args) => {
                self.expr(f);
                self.exprs(args);
            }
            TExprKind::Block(stmts, v) => {
                self.stmts(stmts);
                self.expr(v);
            }
            TExprKind::Match(x, arms) => {
                self.expr(x);
                self.arms(arms);
            }
        }
    }
}

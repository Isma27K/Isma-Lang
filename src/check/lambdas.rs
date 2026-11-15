//! First-class functions: function types, lambdas and the values they
//! capture, named functions used as values, and calls through function values.
//!
//! A function value is a closure: the code to run, plus the values a lambda
//! captured from the code around it. Captures are copies made when the
//! lambda is created, so a lambda may only capture names that can't change
//! afterwards (`val`s, parameters, loop variables): capturing a `var` is an
//! error rather than a silently stale copy. Lists and structs are shared, as
//! everywhere else, so a lambda can still change their contents.
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
    /// lambda reads it (evaluated when the lambda is created).
    captures: Vec<(String, TExpr)>,
}

impl Checker {
    // ---------- types ----------

    /// `fn(Int, String) -> Bool`
    pub(super) fn resolve_fn_type(&mut self, f: &FnTypeExpr) -> Type {
        let params = f.params.iter().map(|p| self.resolve_type(p)).collect();
        let ret = f.ret.as_ref().map(|r| self.resolve_type(r)).unwrap_or(Type::Unit);
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
    }

    /// A struct holding one lambda's captured values (fields are added as
    /// captures are found). Never seen by users: the editor skips it.
    pub(super) fn closure_struct(&mut self, span: Span, generic: bool) -> StructId {
        let id = self.structs.len();
        let name = format!("<lambda {id}>");
        self.structs.push(TStruct { name: name.clone(), fields: Vec::new(), variants: None, generic, closure: None, vtable: Vec::new() });
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
        self.ide.functions.push(FnSymbol { name: tf.name.clone(), signature, name_span: span, full: span, owner: None, ns: self.ns, doc: None, instance: true });
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
            Some(t) => Some(self.resolve_type(t)),
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
        };
        let func = self.add_closure_code(tf, lam.head, &fty);
        let captures: Vec<TExpr> = frame.captures.into_iter().map(|(_, e)| e).collect();
        let sid = (!captures.is_empty()).then_some(sid);
        Self::mk(TExprKind::Closure { func, sid, captures }, fty, span)
    }

    // ---------- captures ----------

    /// Is `name` a variable of the code around the lambda being checked?
    pub(super) fn is_outer_local(&self, name: &str) -> bool {
        self.lambda_stack.iter().any(|f| f.captures.iter().any(|(n, _)| n == name) || f.outer.scopes.iter().any(|s| s.contains_key(name)))
    }

    /// `name` isn't a local of the lambda being checked: if it belongs to the
    /// code around it, capture it.
    pub(super) fn capture(&mut self, name: &str, span: Span) -> Option<TExpr> {
        let k = self.lambda_stack.len().checked_sub(1)?;
        self.capture_into(k, name, span)
    }

    /// Read `name` inside the body of lambda `k` (of `lambda_stack`), from the
    /// code around it, capturing it there (and in enclosing lambdas, if it
    /// comes from further out).
    fn capture_into(&mut self, k: usize, name: &str, span: Span) -> Option<TExpr> {
        let frame = &self.lambda_stack[k];
        if let Some(i) = frame.captures.iter().position(|(n, _)| n == name) {
            let ty = frame.captures[i].1.ty.clone();
            let (env, sid) = (frame.env, frame.sid);
            return Some(self.narrow(Self::mk(TExprKind::Field(Box::new(Self::mk(TExprKind::Local(env), Type::Struct(sid), span)), sid, i), ty, span)));
        }
        // Look it up in the code around lambda `k`, with that code's state in place.
        let mut outer = std::mem::take(&mut self.lambda_stack[k].outer);
        self.swap_state(&mut outer);
        let found = self.lookup(name).map(|b| (b.id, b.span, b.mutable));
        let value = match found {
            Some((id, decl, mutable)) => {
                let e = self.read_local(id, decl, span);
                if mutable {
                    self.err(
                        Diagnostic::error(format!("a lambda can't capture `{name}`, a `var`"), span)
                            .with_primary("captured here")
                            .with_label(decl, "declared with `var` here")
                            .with_note(format!("a lambda keeps a copy of each value it uses, so it would never see later changes to `{name}`"))
                            .with_help(format!("copy it into a `val` before the lambda (`val {name}_now = {name}`), or keep it in a list or struct: the lambda shares those")),
                    );
                    Some(Self::error_expr(span))
                } else {
                    Some(e)
                }
            }
            None if k > 0 => self.capture_into(k - 1, name, span),
            None => None,
        };
        self.swap_state(&mut outer);
        self.lambda_stack[k].outer = outer;
        let value = value?;
        if value.ty.is_error() {
            return Some(value);
        }
        let frame = &mut self.lambda_stack[k];
        let i = frame.captures.len();
        let (env, sid, ty) = (frame.env, frame.sid, value.ty.clone());
        frame.captures.push((name.to_string(), value));
        self.structs[sid].fields.push(TField { name: name.to_string(), ty: ty.clone(), mutable: false });
        Some(self.narrow(Self::mk(TExprKind::Field(Box::new(Self::mk(TExprKind::Local(env), Type::Struct(sid), span)), sid, i), ty, span)))
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

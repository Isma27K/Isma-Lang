//! List methods that take a function: `map`, `filter`, `any`, `all`, `find`,
//! `count`, `fold`, `sort_by`, `min_by` and `max_by`.
//!
//! Each becomes a loop in the caller's own code (a `TExprKind::Block`). A
//! lambda written right in the call is inlined into that loop: its
//! parameters become locals of the caller and its body runs in place, so
//! `xs.map(fn(x) -> x * 2)` allocates the new list and nothing else, and
//! calls nothing. A named function is called directly, and any other
//! function value through the value.

use super::*;

/// The function given to a list method, ready to be called in its loop.
enum FnArg {
    /// A lambda written in the call: its body, checked with its parameters
    /// as locals of the current function.
    Inline { params: Vec<LocalId>, body: TExpr },
    /// A named function, called directly.
    Direct(FuncId),
    /// Any other function value, evaluated once (into this local) before the loop.
    Value(LocalId),
}

/// The list methods taking a function.
pub(super) const LIST_FNS: &[&str] = &["map", "filter", "any", "all", "find", "count", "fold", "sort_by", "min_by", "max_by"];

impl Checker {
    /// `xs.map(f)` and the other methods in `LIST_FNS`.
    pub(super) fn list_fn(&mut self, recv: TExpr, method: &Ident, args: &[Arg], expected: Option<&Type>, span: Span) -> TExpr {
        let m = method.name.as_str();
        let n = if m == "fold" { 2 } else { 1 };
        if !self.arity(args, n, &format!("method `{m}`"), span) {
            return Self::error_expr(span);
        }
        match self.list_fn_inner(recv, m, args, expected, span) {
            Some(e) => e,
            None => Self::error_expr(span),
        }
    }

    fn list_fn_inner(&mut self, recv: TExpr, m: &str, args: &[Arg], expected: Option<&Type>, span: Span) -> Option<TExpr> {
        let lt = recv.ty.clone();
        let Type::List(elem) = &lt else { unreachable!("list method on {lt:?}") };
        let elem = (**elem).clone();
        let local = |id: LocalId, ty: &Type| Self::mk(TExprKind::Local(id), ty.clone(), span);
        let assign = |id: LocalId, value: TExpr| TStmt::Assign { place: Place::Local(id), op: None, value, span };
        let if_then = |cond: TExpr, body: Vec<TStmt>| TStmt::If { branches: vec![(cond, body)], else_block: None };
        // The list is evaluated first, once.
        let mut setup = Vec::new();
        let xs = self.hidden_local(lt.clone());
        setup.push(TStmt::Let(xs, recv));
        let each = |var: LocalId, body: Vec<TStmt>| TStmt::ForList { var, index: None, list: Self::mk(TExprKind::Local(xs), lt.clone(), span), body };
        let (result, ty) = match m {
            "map" => {
                let hint = match expected.map(Type::non_null) {
                    Some(Type::List(u)) => Some((**u).clone()),
                    _ => None,
                };
                let (f, u) = self.fn_arg(&args[0], m, std::slice::from_ref(&elem), hint.as_ref(), &mut setup)?;
                if u == Type::Unit || u == Type::Null {
                    let what = if u == Type::Unit { "returns nothing" } else { "only returns `null`" };
                    self.err(
                        Diagnostic::error(format!("the function given to `map` {what}"), args[0].value.span)
                            .with_note("`map` makes a new list of what the function returns for each element")
                            .with_help("to just do something with each element, use a `for` loop"),
                    );
                    return None;
                }
                let out_ty = Type::List(Box::new(u.clone()));
                let out = self.hidden_local(out_ty.clone());
                setup.push(TStmt::Let(out, Self::mk(TExprKind::ListLit(Vec::new()), out_ty.clone(), span)));
                let x = self.loop_var(&f, 0, &elem);
                let v = self.call_fn_arg(f, vec![local(x, &elem)], &u, span);
                let push = Self::mk(TExprKind::Builtin(Builtin::ListPush, vec![local(out, &out_ty), v]), Type::Unit, span);
                setup.push(each(x, vec![TStmt::Expr(push)]));
                (local(out, &out_ty), out_ty)
            }
            "filter" => {
                let (f, _) = self.fn_arg(&args[0], m, std::slice::from_ref(&elem), Some(&Type::Bool), &mut setup)?;
                let out = self.hidden_local(lt.clone());
                setup.push(TStmt::Let(out, Self::mk(TExprKind::ListLit(Vec::new()), lt.clone(), span)));
                let x = self.loop_var(&f, 0, &elem);
                let keep = self.call_fn_arg(f, vec![local(x, &elem)], &Type::Bool, span);
                let push = Self::mk(TExprKind::Builtin(Builtin::ListPush, vec![local(out, &lt), local(x, &elem)]), Type::Unit, span);
                setup.push(each(x, vec![if_then(keep, vec![TStmt::Expr(push)])]));
                (local(out, &lt), lt.clone())
            }
            // Stop at the first element that decides the answer.
            "any" | "all" => {
                let all = m == "all";
                let (f, _) = self.fn_arg(&args[0], m, std::slice::from_ref(&elem), Some(&Type::Bool), &mut setup)?;
                let res = self.hidden_local(Type::Bool);
                setup.push(TStmt::Let(res, Self::mk(TExprKind::Bool(all), Type::Bool, span)));
                let x = self.loop_var(&f, 0, &elem);
                let c = self.call_fn_arg(f, vec![local(x, &elem)], &Type::Bool, span);
                let decides = if all { Self::mk(TExprKind::Not(Box::new(c)), Type::Bool, span) } else { c };
                setup.push(each(x, vec![if_then(decides, vec![assign(res, Self::mk(TExprKind::Bool(!all), Type::Bool, span)), TStmt::Break])]));
                (local(res, &Type::Bool), Type::Bool)
            }
            "find" => {
                let (f, _) = self.fn_arg(&args[0], m, std::slice::from_ref(&elem), Some(&Type::Bool), &mut setup)?;
                let rt = elem.clone().nullable();
                let res = self.hidden_local(rt.clone());
                setup.push(TStmt::Let(res, Self::mk(TExprKind::Null, rt.clone(), span)));
                let x = self.loop_var(&f, 0, &elem);
                let c = self.call_fn_arg(f, vec![local(x, &elem)], &Type::Bool, span);
                let found = self.coerce(local(x, &elem), &rt, None);
                setup.push(each(x, vec![if_then(c, vec![assign(res, found), TStmt::Break])]));
                (local(res, &rt), rt)
            }
            "count" => {
                let (f, _) = self.fn_arg(&args[0], m, std::slice::from_ref(&elem), Some(&Type::Bool), &mut setup)?;
                let n = self.hidden_local(Type::Int);
                setup.push(TStmt::Let(n, Self::mk(TExprKind::Int(0), Type::Int, span)));
                let x = self.loop_var(&f, 0, &elem);
                let c = self.call_fn_arg(f, vec![local(x, &elem)], &Type::Bool, span);
                let inc = TStmt::Assign { place: Place::Local(n), op: Some(CompoundOp::Builtin(BinOp::Add)), value: Self::mk(TExprKind::Int(1), Type::Int, span), span };
                setup.push(each(x, vec![if_then(c, vec![inc])]));
                (local(n, &Type::Int), Type::Int)
            }
            // `xs.fold(initial, fn(acc, x) -> ...)`: the accumulator's type is the initial value's.
            "fold" => {
                let init = self.expr(&args[0].value, expected);
                let at = match expected {
                    Some(e) if !e.is_error() => e.clone(),
                    _ => init.ty.clone(),
                };
                if at == Type::Null || at == Type::Unit {
                    self.err(
                        Diagnostic::error("cannot tell the type of `fold`'s result from this initial value", args[0].value.span)
                            .with_help("annotate the variable that receives the result, e.g. `val best: Int? = xs.fold(null, ...)`"),
                    );
                    return None;
                }
                let init = self.coerce(init, &at, None);
                let acc = self.hidden_local(at.clone());
                setup.push(TStmt::Let(acc, init));
                let (f, _) = self.fn_arg(&args[1], m, &[at.clone(), elem.clone()], Some(&at), &mut setup)?;
                let x = self.loop_var(&f, 1, &elem);
                let next = self.call_fn_arg(f, vec![local(acc, &at), local(x, &elem)], &at, span);
                setup.push(each(x, vec![assign(acc, next)]));
                (local(acc, &at), at)
            }
            // Keys are computed once per element, then the list is sorted by them (stably).
            "sort_by" => {
                let (f, k) = self.fn_arg(&args[0], m, std::slice::from_ref(&elem), None, &mut setup)?;
                if !self.sort_key(&k, m, &args[0]) {
                    return None;
                }
                let kl = Type::List(Box::new(k.clone()));
                let keys = self.hidden_local(kl.clone());
                setup.push(TStmt::Let(keys, Self::mk(TExprKind::ListLit(Vec::new()), kl.clone(), span)));
                let x = self.loop_var(&f, 0, &elem);
                let key = self.call_fn_arg(f, vec![local(x, &elem)], &k, span);
                setup.push(each(x, vec![TStmt::Expr(Self::mk(TExprKind::Builtin(Builtin::ListPush, vec![local(keys, &kl), key]), Type::Unit, span))]));
                (Self::mk(TExprKind::Builtin(Builtin::ListSortBy, vec![local(xs, &lt), local(keys, &kl)]), Type::Unit, span), Type::Unit)
            }
            // The first element with the smallest (largest) key, or `null` for an empty list.
            "min_by" | "max_by" => {
                let (f, k) = self.fn_arg(&args[0], m, std::slice::from_ref(&elem), None, &mut setup)?;
                if !self.sort_key(&k, m, &args[0]) {
                    return None;
                }
                let rt = elem.clone().nullable();
                let found = self.hidden_local(Type::Bool);
                let best = self.hidden_local(k.clone());
                let res = self.hidden_local(rt.clone());
                let zero = match k {
                    Type::Int => TExprKind::Int(0),
                    Type::Float => TExprKind::Float(0.0),
                    _ => TExprKind::Str(String::new()),
                };
                setup.push(TStmt::Let(found, Self::mk(TExprKind::Bool(false), Type::Bool, span)));
                setup.push(TStmt::Let(best, Self::mk(zero, k.clone(), span)));
                setup.push(TStmt::Let(res, Self::mk(TExprKind::Null, rt.clone(), span)));
                let x = self.loop_var(&f, 0, &elem);
                let kx = self.hidden_local(k.clone());
                let key = self.call_fn_arg(f, vec![local(x, &elem)], &k, span);
                let op = if m == "min_by" { BinOp::Lt } else { BinOp::Gt };
                let better = Self::mk(TExprKind::Binary(op, Box::new(local(kx, &k)), Box::new(local(best, &k))), Type::Bool, span);
                let first = Self::mk(TExprKind::Not(Box::new(local(found, &Type::Bool))), Type::Bool, span);
                let take = Self::mk(TExprKind::Or(Box::new(first), Box::new(better)), Type::Bool, span);
                let value = self.coerce(local(x, &elem), &rt, None);
                setup.push(each(
                    x,
                    vec![
                        TStmt::Let(kx, key),
                        if_then(take, vec![assign(found, Self::mk(TExprKind::Bool(true), Type::Bool, span)), assign(best, local(kx, &k)), assign(res, value)]),
                    ],
                ));
                (local(res, &rt), rt)
            }
            _ => unreachable!("not a list function: {m}"),
        };
        Some(Self::mk(TExprKind::Block(setup, Box::new(result)), ty, span))
    }

    /// Can values of type `k` be compared to sort by them?
    fn sort_key(&mut self, k: &Type, m: &str, arg: &Arg) -> bool {
        if matches!(k, Type::Int | Type::Float | Type::Str | Type::Error) {
            return true;
        }
        let tn = self.tn(k);
        self.err(
            Diagnostic::error(format!("`{m}` needs a key of type `Int`, `Float` or `String`, found `{tn}`"), arg.value.span)
                .with_primary(format!("this returns `{tn}`"))
                .with_help("return something comparable from the element: a number or a string"),
        );
        false
    }

    /// The function argument `arg` of list method `m`, which calls it with
    /// values of types `params` and wants back a `ret` (if known). Code to run
    /// before the loop goes into `setup`. Gives the function and what it returns.
    fn fn_arg(&mut self, arg: &Arg, m: &str, params: &[Type], ret: Option<&Type>, setup: &mut Vec<TStmt>) -> Option<(FnArg, Type)> {
        if let ExprKind::Lambda(lam) = &arg.value.kind {
            if lam.params.len() != params.len() {
                let (n, k) = (params.len(), lam.params.len());
                self.err(
                    Diagnostic::error(format!("`{m}` calls its function with {n} value{}, but this lambda takes {k}", if n == 1 { "" } else { "s" }), lam.head)
                        .with_primary(format!("expected {n} parameter{}", if n == 1 { "" } else { "s" })),
                );
                return None;
            }
            if let LambdaBody::Expr(body) = &lam.body {
                return Some(self.inline_lambda(lam, body, params, ret, arg.value.span));
            }
        }
        let expected = ret.map(|r| Type::Func(params.to_vec(), Box::new(r.clone())));
        let v = self.expr(&arg.value, expected.as_ref());
        match v.ty.clone() {
            Type::Func(ps, r) => {
                let fits = ps.len() == params.len() && ps.iter().zip(params).all(|(a, b)| a.compatible(b)) && ret.is_none_or(|rt| r.compatible(rt));
                if !fits {
                    let want = match ret {
                        Some(rt) => self.tn(&Type::Func(params.to_vec(), Box::new(rt.clone()))),
                        None => format!("{} -> ...", self.tn(&Type::Func(params.to_vec(), Box::new(Type::Unit)))),
                    };
                    let found = self.tn(&v.ty);
                    self.err(
                        Diagnostic::error(format!("`{m}` needs a function like `{want}`, found `{found}`"), v.span).with_primary(format!("this is `{found}`")),
                    );
                    return None;
                }
                if let Some(fid) = self.named_fn(&v) {
                    return Some((FnArg::Direct(fid), *r));
                }
                let l = self.hidden_local(v.ty.clone());
                setup.push(TStmt::Let(l, v));
                Some((FnArg::Value(l), *r))
            }
            Type::Error => None,
            Type::Nullable(inner) if matches!(*inner, Type::Func(..)) => {
                self.null_use_error(&v, NullUse::Value);
                None
            }
            other => {
                let tn = self.tn(&other);
                self.err(
                    Diagnostic::error(format!("`{m}` takes a function, found {}", a_type(&tn)), v.span)
                        .with_primary("not a function")
                        .with_help(format!("pass a lambda: `xs.{m}(fn(x) -> ...)`")),
                );
                None
            }
        }
    }

    /// A lambda written in a list method's call, inlined: its parameters are
    /// locals of the current function, bound in the method's loop.
    fn inline_lambda(&mut self, lam: &Lambda, body: &Expr, params: &[Type], ret: Option<&Type>, span: Span) -> (FnArg, Type) {
        self.scopes.push(HashMap::new());
        let mut ids = Vec::new();
        for (p, ty) in lam.params.iter().zip(params) {
            match &p.ty {
                Some(t) => {
                    let written = self.resolve_type(t);
                    if !written.compatible(ty) {
                        let (w, g) = (self.tn(&written), self.tn(ty));
                        self.err(
                            Diagnostic::error(format!("mismatched types: this parameter is `{w}`, but it is given `{g}`"), t.span)
                                .with_primary(format!("the values here are `{g}`")),
                        );
                    }
                }
                None if p.name.name != "_" => {
                    let hint = format!(": {}", self.tn(ty));
                    self.ide.inlay_hints.push((p.name.span.end, hint));
                }
                None => {}
            }
            ids.push(self.declare_local(&p.name, ty.clone(), LocalKind::Param));
        }
        self.in_lambda_expr += 1;
        let t = self.expr(body, ret);
        self.in_lambda_expr -= 1;
        let t = match ret {
            Some(r) => self.coerce(t, r, None),
            None => t,
        };
        self.scopes.pop();
        let rt = t.ty.clone();
        let fty = Type::Func(params.to_vec(), Box::new(rt.clone()));
        let shown = self.tn(&fty);
        self.hover(lam.head, shown, Some("a lambda (inlined into the loop)".into()));
        self.ide.expr_types.push((span, fty));
        (FnArg::Inline { params: ids, body: t }, rt)
    }

    /// The loop variable for the element passed as argument `i` of `f`: an
    /// inlined lambda's own parameter when it has one, else a new local.
    fn loop_var(&mut self, f: &FnArg, i: usize, elem: &Type) -> LocalId {
        match f {
            FnArg::Inline { params, .. } => params[i],
            _ => self.hidden_local(elem.clone()),
        }
    }

    /// `f(args)`, returning a `ret`.
    fn call_fn_arg(&mut self, f: FnArg, args: Vec<TExpr>, ret: &Type, span: Span) -> TExpr {
        match f {
            FnArg::Inline { params, body } => {
                // Bind each parameter to its argument (the loop variable already is one).
                let mut out = body;
                for (p, a) in params.into_iter().zip(args).rev() {
                    if !matches!(a.kind, TExprKind::Local(l) if l == p) {
                        let ty = out.ty.clone();
                        out = Self::mk(TExprKind::Let(p, Box::new(a), Box::new(out)), ty, span);
                    }
                }
                out
            }
            FnArg::Direct(fid) => Self::mk(TExprKind::Call(fid, args), ret.clone(), span),
            FnArg::Value(l) => {
                let fty = self.locals[l].ty.clone();
                Self::mk(TExprKind::CallValue(Box::new(Self::mk(TExprKind::Local(l), fty, span)), args), ret.clone(), span)
            }
        }
    }
}

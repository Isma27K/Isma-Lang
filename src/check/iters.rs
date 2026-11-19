//! Iterators for your own types: `for x in value` works on any value whose
//! type has `fn next(self) -> T?` (an iterator), or `fn iter(self)` giving
//! one (a collection, which can then be looped over more than once). The
//! loop calls `next()` until it returns `null`.
//!
//! The prelude's `interface Iterator[T]` names that shape and gives every
//! iterator that declares it default methods (`map`, `filter`, `to_list`...),
//! but the loop itself only needs the methods: like the rest of Tarn's
//! interfaces, nothing has to say it fits.

use super::*;

impl Checker {
    /// The type `t.name()` would have (with no arguments), if `t` has such a method.
    fn probe_method(&mut self, t: &Type, name: &str, span: Span) -> Option<Type> {
        let probe = Self::mk(TExprKind::Int(0), t.clone(), span);
        let (r, failed) = self.quietly(|c| c.method_call(probe, &Ident { name: name.into(), span }, &[], None, None, span));
        (!failed && !r.ty.is_error()).then_some(r.ty)
    }

    /// `for x in value:` (`for i, x in value:`) on a value of a struct, enum,
    /// interface or type parameter: a loop over what its `next()` gives.
    pub(super) fn for_iter(&mut self, var: &Ident, second: Option<&Ident>, pats: &[Option<Pattern>; 2], tl: TExpr, body: &[Stmt], span: Span) -> TStmt {
        let fail = |c: &mut Checker, body: &[Stmt]| {
            c.scopes.push(HashMap::new());
            for v in [Some(var), second].into_iter().flatten() {
                c.declare_local(v, Type::Error, LocalKind::LoopVar);
            }
            c.loop_depth += 1;
            let _ = c.block(body, true);
            c.loop_depth -= 1;
            c.scopes.pop();
            TStmt::Expr(Self::error_expr(span))
        };
        if tl.ty.is_error() || tl.ty.is_nullable() {
            return fail(self, body);
        }
        let tn = self.tn(&tl.ty);
        // A collection gives a fresh iterator; an iterator is its own.
        let (src, src_ty) = match self.probe_method(&tl.ty, "iter", span) {
            Some(it) if it != Type::Unit && self.probe_method(&it, "next", span).is_some() => {
                let call = self.method_call(tl, &Ident { name: "iter".into(), span }, &[], None, None, span);
                (call, it)
            }
            _ => {
                let ty = tl.ty.clone();
                (tl, ty)
            }
        };
        let Some(next_ty) = self.probe_method(&src_ty, "next", span) else {
            let mut d = Diagnostic::error(format!("cannot iterate over a value of type `{tn}`"), span)
                .with_primary("not iterable")
                .with_note("`for x in value` calls the value's `next()` until it returns `null` (after its `iter()`, if it has one)");
            if let Some(sid) = user_type(&src_ty).filter(|s| self.can_extend(*s) && !self.is_tuple(*s)) {
                let kw = if self.structs[sid].variants.is_some() { "enum" } else { "struct" };
                d = d.with_help(format!("give `{kw} {}` a method `fn next(self) -> T?` that gives each value, then `null`", short_name(&tn)));
            } else if let Type::Struct(sid) = &src_ty
                && self.is_tuple(*sid)
            {
                d = d.with_help("take it apart instead: `val (a, b) = pair`");
            }
            self.err(d);
            return fail(self, body);
        };
        if !next_ty.is_nullable() {
            let shown = self.tn(&next_ty);
            self.err(
                Diagnostic::error(format!("`next()` of `{}` returns `{shown}`, so a loop over it would never end", self.tn(&src_ty)), span)
                    .with_primary("its `next` can't say there are no more")
                    .with_help(format!("make it return `{shown}?`, and `null` after the last value")),
            );
            return fail(self, body);
        }
        let elem = next_ty.non_null().clone();
        let it = self.hidden_local(src_ty.clone());
        let n = self.hidden_local(next_ty.clone());
        let it_ref = Self::mk(TExprKind::Local(it), src_ty.clone(), span);
        let next = self.method_call(it_ref, &Ident { name: "next".into(), span }, &[], None, None, span);

        self.scopes.push(HashMap::new());
        let (ivar, evar) = match second {
            Some(sv) => (Some(var), sv),
            None => (None, var),
        };
        let destructured = pats.iter().any(Option::is_some);
        if let Some(iv) = ivar
            && iv.name != "_"
        {
            self.ide.inlay_hints.push((iv.span.end, ": Int".into()));
        }
        if evar.name != "_" && !destructured && !elem.is_error() {
            let hint = format!(": {}", self.tn(&elem));
            self.ide.inlay_hints.push((evar.span.end, hint));
        }
        self.forget_assigned_in(body);
        let before = self.nonnull.clone();
        let index = ivar.map(|iv| self.declare_local(iv, Type::Int, LocalKind::LoopVar));
        let v = self.declare_local(evar, elem.clone(), LocalKind::LoopVar);
        let local = |id: LocalId, ty: &Type| Self::mk(TExprKind::Local(id), ty.clone(), span);
        // Each round: the next value, or the end.
        let mut round = vec![
            TStmt::Let(n, next),
            TStmt::If { branches: vec![(Self::mk(TExprKind::IsNull(Box::new(local(n, &next_ty))), Type::Bool, span), vec![TStmt::Break])], else_block: None },
        ];
        if let Some(i) = index {
            let one = Self::mk(TExprKind::Int(1), Type::Int, span);
            let sum = Self::mk(TExprKind::Binary(BinOp::Add, Box::new(local(i, &Type::Int)), Box::new(one)), Type::Int, span);
            round.push(TStmt::Assign { place: Place::Local(i), op: None, value: sum, span });
        }
        round.push(TStmt::Let(v, Self::mk(TExprKind::Unwrap(Box::new(local(n, &next_ty))), elem.clone(), span)));
        let epat = if second.is_some() { &pats[1] } else { &pats[0] };
        if let Some(p) = epat {
            self.local_meta[v].used = true;
            round.extend(self.destructure(p, local(v, &elem), LocalKind::LoopVar));
        }
        if second.is_some() && pats[0].is_some() {
            self.err(Diagnostic::error("the index of `for i, x in ...` is an `Int`, not a tuple", var.span).with_primary("can't take an `Int` apart"));
        }
        self.loop_depth += 1;
        round.extend(self.block(body, true));
        self.loop_depth -= 1;
        self.scopes.pop();
        self.nonnull = before;
        let mut out = vec![TStmt::Let(it, src)];
        if let Some(i) = index {
            out.push(TStmt::Let(i, Self::mk(TExprKind::Int(-1), Type::Int, span)));
        }
        out.push(TStmt::While { cond: Self::mk(TExprKind::Bool(true), Type::Bool, span), body: round });
        // (One statement: the iterator's variable is set first.)
        TStmt::If { branches: vec![(Self::mk(TExprKind::Bool(true), Type::Bool, span), out)], else_block: None }
    }
}

//! Tuples: `(Int, String)` values written `(1, "a")`, read with `t.0` and
//! taken apart with `val (n, s) = t`, `for (k, v) in pairs:` and `case (0, y):`.
//!
//! A tuple of `n` elements is the prelude's generic struct `Tuple<n>`
//! (`Tuple2[A, B]` up to `Tuple6`), so it's a small immutable heap object
//! like any struct, and its `==`, `<=>`, `hash` and `to_string` are methods
//! written in Tarn there. The compiler only adds the syntax, names the types
//! by their elements, and lets `.0` read the hidden fields `_0`, `_1`...

use super::*;

impl Checker {
    /// The tuple struct for `n` elements, if there is one.
    fn tuple_struct(&self, n: usize) -> Option<StructId> {
        self.tuples.get(n).copied().flatten()
    }

    /// Is `sid` a tuple type (an instance of a prelude tuple struct)?
    pub(super) fn is_tuple(&self, sid: StructId) -> bool {
        let origin = self.struct_origin[sid];
        self.tuples.contains(&Some(origin))
    }

    /// The element types of `t`, if it's a tuple (and the struct behind it).
    pub(super) fn tuple_elems(&self, t: &Type) -> Option<(StructId, Vec<Type>)> {
        match t {
            Type::Struct(sid) if self.is_tuple(*sid) => Some((*sid, self.structs[*sid].fields.iter().map(|f| f.ty.clone()).collect())),
            _ => None,
        }
    }

    /// `(Int, String)`
    pub(super) fn tuple_type(&mut self, elems: Vec<Type>, span: Span) -> Type {
        let n = elems.len();
        let Some(tid) = self.tuple_struct(n) else {
            self.err(
                Diagnostic::error(format!("a tuple can have at most {MAX_TUPLE} elements, not {n}"), span)
                    .with_primary("too many elements")
                    .with_help("name the parts: declare a struct with a field for each"),
            );
            return Type::Error;
        };
        if elems.iter().any(Type::is_error) {
            return Type::Error;
        }
        match self.struct_instance(tid, elems) {
            Some(i) => self.def_types[i].clone(),
            None => Type::Error,
        }
    }

    /// `(1, "a")`: each element takes the type expected for it, if a tuple
    /// type is expected (so `(Circle(1.0), 2)` can be a `(Shape, Int)`).
    pub(super) fn tuple_value(&mut self, elems: &[Expr], expected: Option<&Type>, span: Span) -> TExpr {
        let want: Option<Vec<Type>> = expected.map(Type::non_null).and_then(|t| self.tuple_elems(t)).map(|(_, e)| e).filter(|e| e.len() == elems.len());
        let mut vals = Vec::new();
        let mut tys = Vec::new();
        for (i, e) in elems.iter().enumerate() {
            let w = want.as_ref().map(|w| w[i].clone());
            let t = self.expr(e, w.as_ref());
            let t = match &w {
                Some(w) => self.coerce(t, w, None),
                None => t,
            };
            let ty = match &t.ty {
                Type::Unit => {
                    self.err(Diagnostic::error("a tuple's elements must have a value", e.span).with_primary("this gives nothing"));
                    Type::Error
                }
                Type::Null => {
                    self.err(
                        Diagnostic::error("cannot tell the type of this element from `null` alone", e.span)
                            .with_help("give the variable a type, e.g. `val p: (Int, String?) = (1, null)`"),
                    );
                    Type::Error
                }
                t => t.clone(),
            };
            tys.push(ty);
            vals.push(t);
        }
        match self.tuple_type(tys, span) {
            Type::Struct(sid) => Self::mk(TExprKind::StructNew(sid, vals), Type::Struct(sid), span),
            _ => Self::error_expr(span),
        }
    }

    /// `t.0` on a tuple `t` (of struct `sid`).
    pub(super) fn tuple_field(&mut self, t: TExpr, sid: StructId, field: &Ident, span: Span) -> TExpr {
        let n = self.structs[sid].fields.len();
        let tn = self.structs[sid].name.clone();
        // (The prelude's own methods read the fields by name: `self._0`.)
        let name = if Some(self.ns) == self.prelude_ns { field.name.strip_prefix('_').unwrap_or(&field.name) } else { &field.name };
        let Some(idx) = name.parse::<usize>().ok().filter(|i| *i < n) else {
            let mut d = Diagnostic::error(format!("`{tn}` has no element `{}`", field.name), field.span).with_primary("unknown element");
            d = d.with_note(format!("its elements are `.0` to `.{}`", n - 1));
            self.err(d);
            return Self::error_expr(span);
        };
        let ty = self.structs[sid].fields[idx].ty.clone();
        let shown = self.tn(&ty);
        self.hover(field.span, format!("{tn}.{idx}: {shown}"), Some(format!("element {idx} of the tuple")));
        Self::mk(TExprKind::Field(Box::new(t), sid, idx), ty, span)
    }

    /// `case (x, 0):` against a value of type `ty`.
    pub(super) fn tuple_pattern(&mut self, p: &Pattern, elems: &[Pattern], ty: &Type, cx: &mut PatCx) -> TPat {
        match self.tuple_elems(ty) {
            Some((sid, tys)) if tys.len() == elems.len() => {
                let subs = elems.iter().zip(&tys).map(|(e, t)| self.check_pattern(e, t, cx)).collect();
                TPat { kind: TPatKind::Struct(sid, subs), ty: ty.clone() }
            }
            found => {
                let tn = self.tn(ty);
                let what = match found {
                    Some((_, tys)) => format!("the value is a tuple of {} elements, `{tn}`", tys.len()),
                    None => format!("the value is {}", a_type(&tn)),
                };
                self.err(Diagnostic::error(format!("this pattern is a tuple of {} elements, but {what}", elems.len()), p.span).with_primary("doesn't fit"));
                cx.errors = true;
                for e in elems {
                    let _ = self.check_pattern(e, &Type::Error, cx);
                }
                TPat { kind: TPatKind::Wild, ty: ty.clone() }
            }
        }
    }

    /// `val (q, r) = value` (or a loop variable `(k, v)`): declares the names
    /// in `pat` as `kind` and gives the statements that set them from `value`.
    pub(super) fn destructure(&mut self, pat: &Pattern, value: TExpr, kind: LocalKind) -> Vec<TStmt> {
        let mut out = Vec::new();
        self.destructure_into(pat, value, kind, &mut out);
        out
    }

    fn destructure_into(&mut self, pat: &Pattern, value: TExpr, kind: LocalKind, out: &mut Vec<TStmt>) {
        match &pat.kind {
            PatternKind::Bind(name) => {
                if name.name != "_" && !value.ty.is_error() {
                    let hint = format!(": {}", self.tn(&value.ty));
                    self.ide.inlay_hints.push((name.span.end, hint));
                }
                let id = self.declare_local(name, value.ty.clone(), kind);
                out.push(TStmt::Let(id, value));
            }
            PatternKind::Wild => {}
            PatternKind::Tuple(elems) => {
                let span = value.span;
                let ty = value.ty.clone();
                match self.tuple_elems(&ty) {
                    Some((sid, tys)) if tys.len() == elems.len() => {
                        let tmp = self.hidden_local(ty.clone());
                        out.push(TStmt::Let(tmp, value));
                        for (i, (e, t)) in elems.iter().zip(tys).enumerate() {
                            let field = Self::mk(TExprKind::Field(Box::new(Self::mk(TExprKind::Local(tmp), ty.clone(), span)), sid, i), t, span);
                            self.destructure_into(e, field, kind, out);
                        }
                    }
                    found => {
                        if !ty.is_error() {
                            let tn = self.tn(&ty);
                            let mut d = Diagnostic::error(format!("can't take this apart into {} names", elems.len()), pat.span);
                            d = match found {
                                Some((_, tys)) => d.with_primary(format!("the value is a tuple of {} elements, `{tn}`", tys.len())),
                                None if ty.is_nullable() && self.tuple_elems(ty.non_null()).is_some() => {
                                    d.with_primary(format!("the value is `{tn}`: it might be `null`")).with_help("check it for `null` first, or give a default with `?:`")
                                }
                                None => d.with_primary(format!("the value is {}, not a tuple", a_type(&tn))),
                            };
                            self.err(d);
                        }
                        for e in elems {
                            self.destructure_into(e, Self::error_expr(span), kind, out);
                        }
                    }
                }
            }
            _ => {
                self.err(
                    Diagnostic::error("only names, `_` and tuples of them can be taken apart here", pat.span)
                        .with_primary("this pattern might not match")
                        .with_help("use `match` to test a value against patterns like this one"),
                );
            }
        }
    }
}

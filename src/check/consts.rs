//! Top-level constants: `val pi = 3.141592653589793`.
//!
//! A constant is a number, `Bool` or `String` worked out while compiling,
//! from literals, operators and other constants (of its own module or
//! imported). Every use is replaced by the value itself, so constants cost
//! nothing at run time and there is no order of initialization to get wrong.

use super::*;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ConstVal {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

impl ConstVal {
    fn ty(&self) -> Type {
        match self {
            ConstVal::Int(_) => Type::Int,
            ConstVal::Float(_) => Type::Float,
            ConstVal::Bool(_) => Type::Bool,
            ConstVal::Str(_) => Type::Str,
        }
    }

    /// As Tarn source: `3.0`, `"a\"b"`.
    fn code(&self) -> String {
        match self {
            ConstVal::Int(v) => v.to_string(),
            ConstVal::Float(v) => float_code(*v),
            ConstVal::Bool(b) => b.to_string(),
            ConstVal::Str(s) => format!("{s:?}"),
        }
    }
}

/// A float as Tarn prints it (`3.0`, `0.1`, `inf`).
fn float_code(v: f64) -> String {
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if v.is_nan() {
        return "nan".into();
    }
    let s = format!("{v:?}");
    if s.contains('.') || s.contains('e') { s } else { format!("{s}.0") }
}

pub(super) struct ConstDef {
    pub name: String,
    pub ns: usize,
    pub span: Span,
    /// `None` until worked out (or after an error).
    pub value: Option<ConstVal>,
    pub ty: Type,
}

/// Where a constant's value comes from, while working them out.
enum State {
    Todo,
    InProgress,
    Done,
}

impl Checker {
    /// Pass 2: `val name = ...` at the top of module `ns`.
    pub(super) fn declare_const(&mut self, c: &ConstDecl, ns: usize) {
        let name = &c.name.name;
        let key = (ns, name.clone());
        let taken = if let Some(&prev) = self.const_index.get(&key) {
            Some(self.consts[prev].span)
        } else if let Some(&sid) = self.struct_index.get(&key) {
            Some(self.struct_spans[sid])
        } else {
            self.func_index.get(&key).map(|&fid| self.funcs[fid].span)
        };
        if let Some(prev) = taken {
            self.err(Diagnostic::error(format!("`{name}` is defined more than once"), c.name.span).with_label(prev, "first defined here"));
            return;
        }
        if BUILTINS.contains(&name.as_str()) {
            self.err(Diagnostic::error(format!("`{name}` is a built-in function and cannot be redefined"), c.name.span));
            return;
        }
        let id = self.consts.len();
        self.consts.push(ConstDef { name: name.clone(), ns, span: c.name.span, value: None, ty: Type::Error });
        self.const_index.insert(key, id);
        let doc = self.doc_comment(c.span.start);
        self.ide.consts.push(ConstSymbol { name: name.clone(), ns, span: c.name.span, signature: format!("val {name}"), doc });
    }

    /// Work out every constant (each one once, following references between them).
    pub(super) fn eval_consts(&mut self, units: &[(usize, &Program)]) {
        let mut decls: Vec<(usize, &ConstDecl)> = Vec::new();
        for &(ns, program) in units {
            for item in &program.items {
                if let Item::Const(c) = item
                    && self.const_index.get(&(ns, c.name.name.clone())).is_some_and(|&id| self.consts[id].span == c.name.span)
                {
                    decls.push((ns, c));
                }
            }
        }
        let mut state: Vec<State> = (0..self.consts.len()).map(|_| State::Todo).collect();
        let saved = self.ns;
        for i in 0..decls.len() {
            let id = self.const_index[&(decls[i].0, decls[i].1.name.name.clone())];
            self.eval_const(id, &decls, &mut state);
        }
        self.ns = saved;
    }

    fn eval_const(&mut self, id: usize, decls: &[(usize, &ConstDecl)], state: &mut Vec<State>) {
        match state[id] {
            State::Done => return,
            State::InProgress => {
                let name = self.consts[id].name.clone();
                self.err(
                    Diagnostic::error(format!("the constant `{name}` is defined in terms of itself"), self.consts[id].span)
                        .with_primary("its value depends on its own value"),
                );
                state[id] = State::Done;
                return;
            }
            State::Todo => {}
        }
        state[id] = State::InProgress;
        let Some(&(ns, decl)) = decls.iter().find(|(ns, d)| *ns == self.consts[id].ns && d.name.span == self.consts[id].span) else {
            state[id] = State::Done;
            return;
        };
        let saved = self.ns;
        self.ns = ns;
        let declared = decl.ty.as_ref().map(|t| (self.resolve_type(t), t.span));
        let value = self.const_expr(&decl.value, decls, state);
        let value = match (value, &declared) {
            (Some(v), Some((t, tspan))) if !t.is_error() && v.ty() != *t => {
                let (want, got) = (self.tn(t), self.tn(&v.ty()));
                let mut d = Diagnostic::error(format!("mismatched types: expected `{want}`, found `{got}`"), decl.value.span)
                    .with_primary(format!("this is `{got}`"))
                    .with_label(*tspan, "the type is declared here");
                if let (Type::Float, ConstVal::Int(n)) = (t, &v) {
                    d = d.with_help(format!("write `{n}.0`"));
                }
                self.err(d);
                None
            }
            (Some(v), Some((t, _))) if !matches!(t, Type::Int | Type::Float | Type::Bool | Type::Str | Type::Error) => {
                let _ = v;
                None
            }
            (v, _) => v,
        };
        if let Some((t, tspan)) = &declared
            && !matches!(t, Type::Int | Type::Float | Type::Bool | Type::Str | Type::Error)
        {
            let shown = self.tn(t);
            self.err(
                Diagnostic::error(format!("a constant can't be a `{shown}`"), *tspan)
                    .with_note("constants are numbers, `Bool`s and `String`s, worked out when compiling")
                    .with_help("make a function that returns it instead"),
            );
        }
        self.ns = saved;
        let name = self.consts[id].name.clone();
        if let Some(v) = &value {
            let ty = v.ty();
            let tn = self.tn(&ty);
            self.ide.consts[id].signature = format!("val {name}: {tn} = {}", v.code());
            self.hover(decl.name.span, self.ide.consts[id].signature.clone(), self.ide.consts[id].doc.clone());
            self.consts[id].ty = ty;
        }
        self.consts[id].value = value;
        state[id] = State::Done;
    }

    /// The value of a constant expression (`None` after reporting an error).
    fn const_expr(&mut self, e: &Expr, decls: &[(usize, &ConstDecl)], state: &mut Vec<State>) -> Option<ConstVal> {
        use ConstVal::*;
        let overflow = |c: &mut Checker, span: Span| {
            c.err(Diagnostic::error("this constant overflows `Int`", span).with_note("`Int` is 64-bit: from -9223372036854775808 to 9223372036854775807"));
            None
        };
        match &e.kind {
            ExprKind::Int(v) => Some(Int(*v)),
            ExprKind::Float(v) => Some(Float(*v)),
            ExprKind::Bool(b) => Some(Bool(*b)),
            ExprKind::Str(s) => Some(Str(s.clone())),
            ExprKind::Name(n) => {
                let Some(id) = self.visible_const(n) else {
                    self.err(
                        Diagnostic::error(format!("cannot find constant `{n}`"), e.span)
                            .with_primary("not a constant")
                            .with_note("a constant's value can only use literals, operators and other constants"),
                    );
                    return None;
                };
                self.const_ref(id, e.span, decls, state)
            }
            ExprKind::Field(obj, field) => {
                if let ExprKind::Name(m) = &obj.kind
                    && let Some(module_ns) = self.imported_module(m)
                    && let Some(&id) = self.const_index.get(&(module_ns, field.name.clone()))
                {
                    if is_private(&field.name) && module_ns != self.ns {
                        self.err(Diagnostic::error(format!("`{m}.{}` is private to the `{m}` module", field.name), field.span));
                        return None;
                    }
                    return self.const_ref(id, e.span, decls, state);
                }
                self.not_constant(e.span)
            }
            ExprKind::Unary(op, inner) => {
                let v = self.const_expr(inner, decls, state)?;
                match (op, v) {
                    (UnOp::Neg, Int(i)) => i.checked_neg().map(Int).or_else(|| overflow(self, e.span)),
                    (UnOp::Neg, Float(f)) => Some(Float(-f)),
                    (UnOp::Not, Bool(b)) => Some(Bool(!b)),
                    (_, v) => {
                        let tn = self.tn(&v.ty());
                        let sym = if *op == UnOp::Neg { "-" } else { "not" };
                        self.err(Diagnostic::error(format!("`{sym}` cannot be applied to `{tn}`"), e.span));
                        None
                    }
                }
            }
            ExprKind::Binary(op, l, r) => {
                let a = self.const_expr(l, decls, state)?;
                let b = self.const_expr(r, decls, state)?;
                let res = match (&a, &b) {
                    (Int(x), Int(y)) => {
                        let (x, y) = (*x, *y);
                        if matches!(op, BinOp::Div | BinOp::Mod) && y == 0 {
                            self.err(Diagnostic::error("division by zero in a constant", e.span));
                            return None;
                        }
                        let v = match op {
                            BinOp::Add => x.checked_add(y).map(Int),
                            BinOp::Sub => x.checked_sub(y).map(Int),
                            BinOp::Mul => x.checked_mul(y).map(Int),
                            BinOp::Div => x.checked_div(y).map(Int),
                            BinOp::Mod => x.checked_rem(y).map(Int),
                            BinOp::Cmp => Some(Int((x > y) as i64 - (x < y) as i64)),
                            _ => Some(Bool(compare(*op, x.cmp(&y)))),
                        };
                        return v.or_else(|| overflow(self, e.span));
                    }
                    (Float(x), Float(y)) => {
                        let (x, y) = (*x, *y);
                        match op {
                            BinOp::Add => Some(Float(x + y)),
                            BinOp::Sub => Some(Float(x - y)),
                            BinOp::Mul => Some(Float(x * y)),
                            BinOp::Div => Some(Float(x / y)),
                            BinOp::Mod => Some(Float(x % y)),
                            BinOp::Cmp => Some(Int((x > y) as i64 - (x < y) as i64)),
                            _ => x.partial_cmp(&y).map(|o| Bool(compare(*op, o))).or(Some(Bool(*op == BinOp::Ne))),
                        }
                    }
                    (Str(x), Str(y)) => match op {
                        BinOp::Add => Some(Str(format!("{x}{y}"))),
                        BinOp::Cmp => Some(Int(x.cmp(y) as i64)),
                        BinOp::Mul | BinOp::Sub | BinOp::Div | BinOp::Mod | BinOp::And | BinOp::Or => None,
                        _ => Some(Bool(compare(*op, x.cmp(y)))),
                    },
                    (Bool(x), Bool(y)) => match op {
                        BinOp::And => Some(Bool(*x && *y)),
                        BinOp::Or => Some(Bool(*x || *y)),
                        BinOp::Eq => Some(Bool(x == y)),
                        BinOp::Ne => Some(Bool(x != y)),
                        _ => None,
                    },
                    _ => None,
                };
                if res.is_none() {
                    let (x, y) = (self.tn(&a.ty()), self.tn(&b.ty()));
                    let mut d = Diagnostic::error(format!("operator `{}` cannot be applied to `{x}` and `{y}`", op.symbol()), e.span)
                        .with_label(l.span, format!("this is `{x}`"))
                        .with_label(r.span, format!("this is `{y}`"));
                    if matches!((&a, &b), (Int(_), Float(_)) | (Float(_), Int(_))) {
                        d = d.with_help("write the `Int` as a `Float`, e.g. `2.0` instead of `2`");
                    }
                    self.err(d);
                }
                res
            }
            _ => self.not_constant(e.span),
        }
    }

    fn not_constant(&mut self, span: Span) -> Option<ConstVal> {
        self.err(
            Diagnostic::error("a top-level `val` must be a constant", span)
                .with_primary("not known when compiling")
                .with_note("a constant is a number, `Bool` or `String` made of literals, operators and other constants")
                .with_help("compute it in a function instead, e.g. `fn table() -> List[Int]:`"),
        );
        None
    }

    fn const_ref(&mut self, id: usize, span: Span, decls: &[(usize, &ConstDecl)], state: &mut Vec<State>) -> Option<ConstVal> {
        let saved = self.ns;
        self.eval_const(id, decls, state);
        self.ns = saved;
        self.ide.definitions.push((span, self.consts[id].span));
        self.consts[id].value.clone()
    }

    /// The constant `name` refers to in the current module: its own, or one imported with `from m import name`.
    pub(super) fn visible_const(&mut self, name: &str) -> Option<usize> {
        if let Some(&id) = self.const_index.get(&(self.ns, name.to_string())) {
            return Some(id);
        }
        let n = self.names.get_mut(&(self.ns, name.to_string()))?;
        let Imported::Const(id) = n.target else { return None };
        n.used = true;
        Some(id)
    }

    pub(super) fn const_hover(&mut self, id: usize, span: Span) {
        let c = &self.ide.consts[id];
        let from = (c.ns != self.ns).then(|| format!("from module `{}`", self.ns_names[c.ns]));
        let doc = match (from, &c.doc) {
            (Some(f), Some(d)) => Some(format!("{f}\n\n{d}")),
            (f, d) => f.or(d.clone()),
        };
        self.hover(span, c.signature.clone(), doc);
        self.ide.definitions.push((span, self.consts[id].span));
    }

    /// A use of constant `id`: its value.
    pub(super) fn const_value(&mut self, id: usize, span: Span) -> TExpr {
        self.const_hover(id, span);
        match self.consts[id].value.clone() {
            Some(ConstVal::Int(v)) => Self::mk(TExprKind::Int(v), Type::Int, span),
            Some(ConstVal::Float(v)) => Self::mk(TExprKind::Float(v), Type::Float, span),
            Some(ConstVal::Bool(v)) => Self::mk(TExprKind::Bool(v), Type::Bool, span),
            Some(ConstVal::Str(v)) => Self::mk(TExprKind::Str(v), Type::Str, span),
            None => Self::error_expr(span),
        }
    }

    /// `math.pi`: a constant of an imported module, if `field` is one.
    pub(super) fn module_const(&mut self, module_ns: usize, module: &str, field: &Ident, span: Span) -> Option<TExpr> {
        let &id = self.const_index.get(&(module_ns, field.name.clone()))?;
        if is_private(&field.name) && module_ns != self.ns {
            self.err(
                Diagnostic::error(format!("`{module}.{}` is private to the `{module}` module", field.name), field.span)
                    .with_primary("private")
                    .with_note("names starting with `_` are internal to their module"),
            );
            return Some(Self::error_expr(span));
        }
        let mut t = self.const_value(id, field.span);
        t.span = span;
        Some(t)
    }
}

fn compare(op: BinOp, o: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match op {
        BinOp::Eq => o == Equal,
        BinOp::Ne => o != Equal,
        BinOp::Lt => o == Less,
        BinOp::Le => o != Greater,
        BinOp::Gt => o == Greater,
        BinOp::Ge => o != Less,
        _ => false,
    }
}

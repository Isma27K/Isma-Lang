//! Generics: `fn max[T](a: T, b: T) -> T`, `struct Box[T]`, `enum Tree[T]`.
//!
//! Generic code is checked once, with its type parameters left abstract
//! (`Type::Param`). Instead of declared bounds, the checker records what the
//! code does with values of each parameter (compares them, adds them,
//! prints them...): those are the parameter's *needs*. A call passes them on:
//! `max3[T]` calling `max(a, b)` needs whatever `max` needs. Once every body
//! has been checked, each use of generic code with concrete types is checked
//! against the needs (`max(p, q)` with `Point`s needs `Point` to have
//! `fn <=>`), and every generic function is copied for each set of concrete
//! types it's used with (monomorphization) by checking its body again with
//! the type parameters bound to those types. The back end only ever sees
//! those copies, so generic code is exactly as fast as hand-written code.

use super::*;
use std::collections::HashSet;

/// The deepest a type argument may nest (`List[List[...]]`) before a generic
/// function or type is considered to grow without end.
const MAX_TYPE_DEPTH: usize = 24;

/// A type parameter (`T` in `fn max[T]`).
pub(super) struct ParamInfo {
    pub name: String,
    pub span: Span,
    /// What declares it: `max`, or `Box` (for display).
    pub owner: String,
}

/// Something generic code does with values of a type parameter, which the
/// types it's used with must support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Need {
    /// `a == b` (and `!=`) on two `T`s.
    Eq,
    /// `a == b` where a side is a `T?`.
    EqNullable,
    /// `<`, `<=`, `>`, `>=`, `<=>`.
    Ord,
    /// `+ - * / %` on two `T`s, giving a `T`.
    Arith(BinOp),
    /// `-a`, giving a `T`.
    Neg,
    /// `print(a)` and `f"{a}"` (also on a `T?`).
    Format,
    /// `a.to_string()`.
    ToStr,
}

impl Need {
    /// "`<` and `>`", for "needs `T` to support ...".
    fn shown(self) -> String {
        match self {
            Need::Eq => "`==`".into(),
            Need::EqNullable => "`==` between values that may be `null`".into(),
            Need::Ord => "`<` and `>`".into(),
            Need::Arith(op) => format!("`{}`", op.symbol()),
            Need::Neg => "`-` (negation)".into(),
            Need::Format => "printing".into(),
            Need::ToStr => "`to_string()`".into(),
        }
    }

    /// "`T` values are compared here"
    fn used(self, p: &str) -> String {
        match self {
            Need::Eq | Need::EqNullable => format!("`{p}` values are compared with `==` here"),
            Need::Ord => format!("`{p}` values are compared here"),
            Need::Arith(BinOp::Add) => format!("`{p}` values are added here"),
            Need::Arith(BinOp::Sub) => format!("`{p}` values are subtracted here"),
            Need::Arith(BinOp::Mul) => format!("`{p}` values are multiplied here"),
            Need::Arith(BinOp::Div) => format!("`{p}` values are divided here"),
            Need::Arith(_) => format!("`%` is used on `{p}` values here"),
            Need::Neg => format!("a `{p}` is negated here"),
            Need::Format => format!("a `{p}` is printed here"),
            Need::ToStr => format!("`to_string()` is called on a `{p}` here"),
        }
    }
}

/// A need of one type parameter of a generic declaration, and where the code needs it.
#[derive(Debug, Clone)]
pub(super) struct NeedUse {
    pub param: ParamId,
    pub need: Need,
    pub span: Span,
    /// When passed on from a call: the function called.
    pub via: Option<FuncId>,
}

/// A use of generic code with some types: `fid` is the copy used (its
/// `instance` says of what, for which types) at `span`.
pub(super) struct Obligation {
    pub fid: FuncId,
    pub span: Span,
    /// The generic declaration whose (abstract) body makes the call, if any.
    pub in_generic: Option<FuncId>,
}

/// Why a type lacks what a need asks for.
pub(super) struct Fail {
    /// The type that falls short (maybe nested inside the one that was passed).
    pub ty: Type,
    pub need: Need,
    /// Generic code on the way to it: (where, what it does).
    pub trail: Vec<(Span, String)>,
}

/// Every function called in `stmts`, with the call's span.
fn calls_in(stmts: &[TStmt], out: &mut Vec<(FuncId, Span)>) {
    for s in stmts {
        match s {
            TStmt::Let(_, e) | TStmt::Expr(e) | TStmt::Return(Some(e)) => calls_in_expr(e, out),
            TStmt::Assign { place, op, value, span } => {
                if let Some(CompoundOp::Call(f)) = op {
                    out.push((*f, *span));
                }
                match place {
                    Place::Local(_) => {}
                    Place::Field { obj, .. } => calls_in_expr(obj, out),
                    Place::Index { list, index, .. } => {
                        calls_in_expr(list, out);
                        calls_in_expr(index, out);
                    }
                    Place::Custom { obj, index, get, set, .. } => {
                        calls_in_expr(obj, out);
                        calls_in_expr(index, out);
                        out.extend(get.iter().chain([set]).map(|f| (*f, *span)));
                    }
                }
                calls_in_expr(value, out);
            }
            TStmt::If { branches, else_block } => {
                for (c, b) in branches {
                    calls_in_expr(c, out);
                    calls_in(b, out);
                }
                if let Some(b) = else_block {
                    calls_in(b, out);
                }
            }
            TStmt::While { cond, body } => {
                calls_in_expr(cond, out);
                calls_in(body, out);
            }
            TStmt::ForRange { start, end, body, .. } => {
                calls_in_expr(start, out);
                calls_in_expr(end, out);
                calls_in(body, out);
            }
            TStmt::ForList { list, body, .. } => {
                calls_in_expr(list, out);
                calls_in(body, out);
            }
            TStmt::Match { scrutinee, arms } => {
                calls_in_expr(scrutinee, out);
                for a in arms {
                    if let Some(g) = &a.guard {
                        calls_in_expr(g, out);
                    }
                    calls_in(&a.body, out);
                }
            }
            TStmt::Break | TStmt::Continue | TStmt::Return(None) => {}
        }
    }
}

fn calls_in_expr(e: &TExpr, out: &mut Vec<(FuncId, Span)>) {
    match &e.kind {
        TExprKind::Call(f, args) => {
            out.push((*f, e.span));
            args.iter().for_each(|a| calls_in_expr(a, out));
        }
        TExprKind::Builtin(_, args) | TExprKind::StructNew(_, args) | TExprKind::EnumNew(_, _, args) | TExprKind::ListLit(args) | TExprKind::FString(args) => {
            args.iter().for_each(|a| calls_in_expr(a, out))
        }
        TExprKind::Binary(_, a, b) | TExprKind::And(a, b) | TExprKind::Or(a, b) | TExprKind::Index(a, b) | TExprKind::Elvis(a, b) => {
            calls_in_expr(a, out);
            calls_in_expr(b, out);
        }
        TExprKind::SafeCall { recv, body, .. } => {
            calls_in_expr(recv, out);
            calls_in_expr(body, out);
        }
        TExprKind::Neg(x) | TExprKind::Not(x) | TExprKind::Field(x, ..) | TExprKind::IsNull(x) | TExprKind::Wrap(x) | TExprKind::Unwrap(x) | TExprKind::NotNull(x) => {
            calls_in_expr(x, out)
        }
        TExprKind::Jump(s) => calls_in(std::slice::from_ref(&**s), out),
        TExprKind::Int(_) | TExprKind::Float(_) | TExprKind::Bool(_) | TExprKind::Str(_) | TExprKind::Local(_) | TExprKind::Null => {}
    }
}

/// A name made of the characters allowed in a generated symbol.
fn symbol_safe(s: &str) -> String {
    s.chars().filter(|c| *c != ' ').map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '.' }).collect()
}

impl Checker {
    // ---------- declarations ----------

    /// Register the type parameters `[T, U]` of `owner` (a function or type
    /// name). `outer`: those already in scope (a method's struct's).
    pub(super) fn declare_type_params(&mut self, params: &[Ident], owner: &str, outer: &[ParamId]) -> Vec<ParamId> {
        let mut out = Vec::new();
        for p in params {
            let name = &p.name;
            let what = format!("type parameter of `{owner}`");
            if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
                self.err(
                    Diagnostic::error(format!("type parameter `{name}` must start with an uppercase letter"), p.span)
                        .with_primary("type names are capitalized")
                        .with_help(if capitalize(name) == "T" { "e.g. `T`".to_string() } else { format!("e.g. `{}`, or the usual `T`", capitalize(name)) }),
                );
            } else if RESERVED_TYPES.contains(&name.as_str()) {
                self.err(Diagnostic::error(format!("`{name}` is a built-in type, so it can't name a type parameter"), p.span).with_help("pick another name, like `T`"));
            } else if let Some(&sid) = self.struct_index.get(&(self.ns, name.clone())) {
                let prev = self.struct_spans[sid];
                self.err(
                    Diagnostic::error(format!("`{name}` is already the name of a type"), p.span)
                        .with_label(prev, "defined here")
                        .with_help("pick another name for the type parameter, like `T`"),
                );
            } else if let Some(&q) = outer.iter().find(|q| self.type_params[**q].name == *name) {
                let prev = self.type_params[q].span;
                let of = self.type_params[q].owner.clone();
                self.err(
                    Diagnostic::error(format!("`{name}` is already a type parameter of `{of}`"), p.span)
                        .with_label(prev, "declared here")
                        .with_note(format!("methods of `{of}` can use `{name}` directly")),
                );
                // From here on, `T` means the outer one.
                continue;
            } else if let Some(&q) = out.iter().find(|q: &&ParamId| self.type_params[**q].name == *name) {
                let prev = self.type_params[q].span;
                self.err(Diagnostic::error(format!("type parameter `{name}` is declared twice"), p.span).with_label(prev, "first declared here"));
                continue;
            }
            let id = self.type_params.len();
            self.type_params.push(ParamInfo { name: name.clone(), span: p.span, owner: owner.to_string() });
            self.ide.params.push(name.clone());
            self.hover(p.span, name.clone(), Some(what));
            out.push(id);
        }
        out
    }

    /// The type parameters of generic type `sid`, in scope for its fields.
    pub(super) fn struct_env(&self, sid: StructId) -> Vec<(String, Type)> {
        self.struct_params[sid].iter().map(|p| (self.type_params[*p].name.clone(), Type::Param(*p))).collect()
    }

    /// Warn about type parameters of `sid` that none of its fields uses.
    pub(super) fn unused_type_params(&mut self, sid: StructId) {
        let s = &self.structs[sid];
        let tys: Vec<Type> = s.fields.iter().chain(s.variants().iter().flat_map(|v| &v.fields)).map(|f| f.ty.clone()).collect();
        for p in self.struct_params[sid].clone() {
            if !RESERVED_TYPES.contains(&self.type_params[p].name.as_str()) && !tys.iter().any(|t| self.mentions(t, p)) {
                let name = self.type_params[p].name.clone();
                self.diags.push(
                    Diagnostic::warning(format!("type parameter `{name}` is never used"), self.type_params[p].span)
                        .with_primary(format!("no field mentions `{name}`"))
                        .with_help("remove it, or give the type a field that uses it"),
                );
            }
        }
    }

    /// The type parameter names in scope: those of `fid` (and its owner), bound
    /// to its type arguments when `fid` is a copy of generic code.
    pub(super) fn env_of(&self, fid: FuncId) -> Vec<(String, Type)> {
        let Some((decl, args)) = &self.funcs[fid].instance else { return Vec::new() };
        self.funcs[*decl].type_params.iter().zip(args).map(|(p, t)| (self.type_params[*p].name.clone(), t.clone())).collect()
    }

    /// `T` as written in a type, when it names a type parameter in scope.
    pub(super) fn type_param_ref(&mut self, t: &TypeExpr) -> Option<Type> {
        if t.module.is_some() {
            return None;
        }
        let ty = self.type_env.iter().rev().find(|(n, _)| *n == t.name.name)?.1.clone();
        if let Type::Param(p) = ty {
            let (span, owner) = (self.type_params[p].span, self.type_params[p].owner.clone());
            self.hover(t.name.span, t.name.name.clone(), Some(format!("type parameter of `{owner}`")));
            self.ide.definitions.push((t.name.span, span));
        }
        if !t.args.is_empty() {
            self.err(Diagnostic::error(format!("type parameter `{}` doesn't take type arguments", t.name.name), t.span));
            return Some(Type::Error);
        }
        Some(ty)
    }

    /// `Box[Int]` as written in a type: generic type `sid` with the type
    /// arguments of `t` (shown as `shown` in messages).
    pub(super) fn apply_type_args(&mut self, sid: StructId, t: &TypeExpr, shown: &str) -> Type {
        let n = self.struct_params[sid].len();
        if n == 0 {
            if !t.args.is_empty() {
                self.err(Diagnostic::error(format!("type `{shown}` does not take type arguments"), t.span));
                return Type::Error;
            }
            return self.def_types[sid].clone();
        }
        let names: Vec<String> = self.struct_params[sid].iter().map(|p| self.type_params[*p].name.clone()).collect();
        if t.args.len() != n {
            let msg = if t.args.is_empty() {
                format!("`{shown}` needs type arguments")
            } else {
                format!("`{shown}` takes {n} type argument{}, found {}", if n == 1 { "" } else { "s" }, t.args.len())
            };
            self.err(
                Diagnostic::error(msg, t.span)
                    .with_primary(format!("`{shown}` is generic"))
                    .with_help(format!("say what it holds: `{shown}[{}]`, with a type for {}", names.join(", "), and_list(&names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>()))),
            );
            return Type::Error;
        }
        let args: Vec<Type> = t.args.iter().map(|a| self.resolve_type(a)).collect();
        if args.iter().any(|a| a.is_error()) {
            return Type::Error;
        }
        match self.struct_instance(sid, args) {
            Some(i) => self.def_types[i].clone(),
            None => Type::Error,
        }
    }

    // ---------- instances ----------

    pub(super) fn type_depth(&self, t: &Type) -> usize {
        match t {
            Type::List(e) | Type::Nullable(e) => 1 + self.type_depth(e),
            Type::Struct(id) | Type::Enum(id, _) => 1 + self.struct_args[*id].iter().map(|a| self.type_depth(a)).max().unwrap_or(0),
            _ => 1,
        }
    }

    /// Does type parameter `p` occur in `t`?
    pub(super) fn mentions(&self, t: &Type, p: ParamId) -> bool {
        match t {
            Type::Param(q) => *q == p,
            Type::List(e) | Type::Nullable(e) => self.mentions(e, p),
            Type::Struct(id) | Type::Enum(id, _) => self.struct_has_param[*id] && self.struct_args[*id].iter().any(|a| self.mentions(a, p)),
            _ => false,
        }
    }

    /// Replace type parameters in `t` (`List[T]` with `T` = `Int` is `List[Int]`).
    pub(super) fn subst(&mut self, t: &Type, map: &[(ParamId, Type)]) -> Type {
        match t {
            Type::Param(p) => map.iter().find(|(q, _)| q == p).map(|(_, t)| t.clone()).unwrap_or_else(|| t.clone()),
            Type::List(e) => Type::List(Box::new(self.subst(e, map))),
            // `T?` with `T` = `Int?` is `Int?`.
            Type::Nullable(e) => self.subst(e, map).nullable(),
            Type::Struct(id) | Type::Enum(id, _) if self.struct_has_param[*id] => {
                let origin = self.struct_origin[*id];
                let args: Vec<Type> = self.struct_args[*id].clone().iter().map(|a| self.subst(a, map)).collect();
                match self.struct_instance(origin, args) {
                    Some(i) => self.def_types[i].clone(),
                    None => Type::Error,
                }
            }
            _ => t.clone(),
        }
    }

    /// Generic type `origin` with type arguments `args` (`Box[Int]`): a struct
    /// (or enum) of its own, made once and reused. `None` (reported) when
    /// the type arguments grow without end.
    pub(super) fn struct_instance(&mut self, origin: StructId, args: Vec<Type>) -> Option<StructId> {
        let params = &self.struct_params[origin];
        if args.len() != params.len() || args.iter().zip(params).all(|(a, p)| *a == Type::Param(*p)) {
            return Some(origin);
        }
        if let Some(&id) = self.struct_instances.get(&(origin, args.clone())) {
            return Some(id);
        }
        if args.iter().any(|a| self.type_depth(a) > MAX_TYPE_DEPTH) {
            if self.endless.insert(origin) {
                let name = self.ide.structs[origin].name.clone();
                self.err(
                    Diagnostic::error(format!("`{name}` contains itself with ever-bigger type arguments"), self.struct_spans[origin])
                        .with_primary("this type never stops growing")
                        .with_note(format!("a field like `next: {name}[List[T]]` makes `{name}[Int]` hold a `{name}[List[Int]]`, which holds a `{name}[List[List[Int]]]`, and so on"))
                        .with_help(format!("inside `{name}`, use it with its own type parameters: `next: {name}[{}]?`", self.ide.structs[origin].type_params.join(", "))),
                );
            }
            return None;
        }
        let id = self.structs.len();
        let base = self.ide.structs[origin].name.clone();
        let shown: Vec<String> = args.iter().map(|a| self.tn(a)).collect();
        let name = format!("{base}[{}]", shown.join(", "));
        let has_param = args.iter().any(|a| a.has_param(&self.struct_has_param));
        let variants = self.structs[origin].variants.as_ref().map(|_| Vec::new());
        self.structs.push(TStruct { name: name.clone(), fields: Vec::new(), variants, generic: has_param });
        self.def_types.push(match self.def_types[origin] {
            Type::Enum(_, boxed) => Type::Enum(id, boxed),
            _ => Type::Struct(id),
        });
        self.struct_spans.push(self.struct_spans[origin]);
        self.struct_ns.push(self.struct_ns[origin]);
        self.struct_origin.push(origin);
        self.struct_args.push(args.clone());
        self.struct_has_param.push(has_param);
        self.struct_params.push(Vec::new());
        self.fields_ready.push(false);
        self.field_spans.push(Vec::new());
        let mut sym = self.ide.structs[origin].clone();
        sym.name = name;
        sym.fields = Vec::new();
        sym.variants = sym.variants.map(|_| Vec::new());
        sym.methods = Vec::new();
        sym.type_args = shown;
        sym.instance = true;
        self.ide.structs.push(sym);
        self.struct_instances.insert((origin, args), id);
        if self.fields_ready[origin] {
            self.fill_instance(id);
        } else {
            self.pending_fields.push(id);
        }
        Some(id)
    }

    /// Give instance `id` the fields (or variants) of its generic type, with its type arguments.
    pub(super) fn fill_instance(&mut self, id: StructId) {
        let origin = self.struct_origin[id];
        let map: Vec<(ParamId, Type)> = self.struct_params[origin].iter().copied().zip(self.struct_args[id].clone()).collect();
        let mut fields = Vec::new();
        for f in self.structs[origin].fields.clone() {
            let ty = self.subst(&f.ty, &map);
            fields.push(TField { ty, ..f });
        }
        let variants = match self.structs[origin].variants.clone() {
            Some(vs) => {
                let mut out = Vec::new();
                for v in vs {
                    let mut fs = Vec::new();
                    for f in v.fields {
                        let ty = self.subst(&f.ty, &map);
                        fs.push(TField { ty, ..f });
                    }
                    out.push(TVariant { name: v.name, fields: fs });
                }
                Some(out)
            }
            None => None,
        };
        let fsyms: Vec<FieldSymbol> =
            fields.iter().zip(self.ide.structs[origin].fields.clone()).map(|(f, s)| FieldSymbol { ty: self.tn(&f.ty), ..s }).collect();
        let vsyms = variants.as_ref().map(|vs| {
            let ename = self.structs[id].name.clone();
            let decl = self.ide.structs[origin].variants.clone().unwrap_or_default();
            vs.iter().zip(decl).map(|(v, s)| VariantSymbol { signature: self.variant_signature(&ename, v), ..s }).collect::<Vec<_>>()
        });
        self.structs[id].fields = fields;
        self.structs[id].variants = variants;
        self.ide.structs[id].fields = fsyms;
        self.ide.structs[id].variants = vsyms;
        self.field_spans[id] = self.field_spans[origin].clone();
        self.fields_ready[id] = true;
    }

    /// Generic function `decl` with a type for each of its type parameters
    /// (`max[Int]`, or `Box[Int].get`): a function of its own, made once.
    /// `args` may still contain type parameters (inside generic code).
    pub(super) fn fn_instance(&mut self, decl: FuncId, args: Vec<Type>) -> FuncId {
        let tps = self.funcs[decl].type_params.clone();
        if args.len() != tps.len() || args.iter().zip(&tps).all(|(a, p)| *a == Type::Param(*p)) {
            return decl;
        }
        if let Some(&f) = self.fn_instances.get(&(decl, args.clone())) {
            return f;
        }
        let map: Vec<(ParamId, Type)> = tps.iter().copied().zip(args.iter().cloned()).collect();
        let mut params = Vec::new();
        for (n, t) in self.funcs[decl].params.clone() {
            let t = self.subst(&t, &map);
            params.push((n, t));
        }
        let ret = self.subst(&self.funcs[decl].ret.clone(), &map);
        // Its own type parameters that are still to be picked (a method of `Box[Int]` that is generic itself).
        let tparams: Vec<ParamId> = self.funcs[decl].tparams.iter().copied().filter(|p| map.iter().any(|(q, t)| q == p && *t == Type::Param(*p))).collect();
        let shown: Vec<String> = args.iter().map(|a| self.tn(a)).collect();
        let name = format!("{}[{}]", self.funcs[decl].name, shown.join(", "));
        let mut symbol = format!("{}.{}", self.funcs[decl].symbol, symbol_safe(&shown.join(".")));
        if !self.symbols.insert(symbol.clone()) {
            symbol = format!("{symbol}.{}", self.funcs.len());
            self.symbols.insert(symbol.clone());
        }
        let id = self.funcs.len();
        let mut sym = self.ide.functions[decl].clone();
        let is_method = sym.owner.is_some();
        sym.signature = self.signature(&sym.name, &tparams, &params, &ret, is_method);
        sym.instance = true;
        self.ide.functions.push(sym);
        let span = self.funcs[decl].span;
        self.funcs.push(FuncSig { name, symbol, params, ret, span, type_params: Vec::new(), tparams, instance: Some((decl, args.clone())) });
        self.fn_instances.insert((decl, args), id);
        id
    }

    /// Is `fid` a copy of generic code for concrete types (so it gets a body of its own)?
    pub(super) fn is_concrete_instance(&self, fid: FuncId) -> bool {
        matches!(&self.funcs[fid].instance, Some((_, args)) if !args.iter().any(|a| a.has_param(&self.struct_has_param)))
    }

    /// Method `decl` of a generic type, as a method of `sid` (an instance of that type).
    pub(super) fn member_instance(&mut self, decl: FuncId, sid: StructId) -> FuncId {
        if self.struct_origin[sid] == sid {
            return decl;
        }
        let mut args = self.struct_args[sid].clone();
        args.extend(self.funcs[decl].tparams.iter().map(|p| Type::Param(*p)));
        self.fn_instance(decl, args)
    }

    /// Method `name` of struct or enum `sid` (of an instance: the generic type's, for its type arguments).
    pub(super) fn method(&mut self, sid: StructId, name: &str) -> Option<FuncId> {
        let decl = *self.methods.get(&(self.struct_origin[sid], name.to_string()))?;
        Some(self.member_instance(decl, sid))
    }

    pub(super) fn has_method(&self, sid: StructId, name: &str) -> bool {
        self.methods.contains_key(&(self.struct_origin[sid], name.to_string()))
    }

    pub(super) fn method_names(&self, sid: StructId) -> Vec<String> {
        let origin = self.struct_origin[sid];
        self.methods.keys().filter(|(s, _)| *s == origin).map(|(_, n)| n.clone()).collect()
    }

    /// The operator functions of kind `kind` of `sid`.
    pub(super) fn ops(&mut self, sid: StructId, kind: OpKind) -> Vec<FuncId> {
        let decls = self.operators.get(&(self.struct_origin[sid], kind)).cloned().unwrap_or_default();
        decls.into_iter().map(|d| self.member_instance(d, sid)).collect()
    }

    pub(super) fn has_ops(&self, sid: StructId, kind: OpKind) -> bool {
        self.operators.contains_key(&(self.struct_origin[sid], kind))
    }

    // ---------- calls ----------

    /// Which parameter each argument of a call goes to. Misplaced, unknown and
    /// repeated arguments are reported (and checked, for editor support);
    /// `ok` is false then, or when the count is wrong.
    fn arg_slots<'a>(&mut self, what: &str, params: &[(String, Type)], args: &'a [Arg], span: Span) -> (Vec<(usize, &'a Arg)>, bool) {
        let mut taken = vec![false; params.len()];
        let mut slots = Vec::new();
        let mut ok = true;
        let mut seen_named = false;
        for (i, a) in args.iter().enumerate() {
            let idx = match &a.name {
                None => {
                    if seen_named {
                        self.err(Diagnostic::error("positional arguments must come before named arguments", a.value.span));
                        ok = false;
                        let _ = self.expr(&a.value, None);
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
                        Some(idx) if taken[idx] => {
                            self.err(Diagnostic::error(format!("argument `{}` is given more than once", n.name), n.span));
                            ok = false;
                            let _ = self.expr(&a.value, None);
                            continue;
                        }
                        Some(idx) => idx,
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
            taken[idx] = true;
            slots.push((idx, a));
        }
        if args.len() > params.len() {
            self.err(
                Diagnostic::error(format!("{what} takes {} argument(s) but {} were given", params.len(), args.len()), span)
                    .with_primary(format!("expected {} argument(s)", params.len())),
            );
            ok = false;
        } else if taken.iter().any(|t| !t) && ok {
            let missing: Vec<String> = params.iter().zip(&taken).filter(|(_, t)| !**t).map(|((n, t), _)| format!("`{n}: {}`", self.tn(t))).collect();
            self.err(
                Diagnostic::error(format!("{what} is missing argument(s): {}", missing.join(", ")), span)
                    .with_primary(format!("expected {} argument(s), found {}", params.len(), args.len())),
            );
            ok = false;
        } else if taken.iter().any(|t| !t) {
            ok = false;
        }
        (slots, ok)
    }

    /// `pat` with the bound type parameters replaced, if none of `tparams` is left in it.
    fn known_type(&mut self, pat: &Type, tparams: &[ParamId], bound: &[Option<Type>]) -> Option<Type> {
        let mut map = Vec::new();
        for (p, b) in tparams.iter().zip(bound) {
            match b {
                Some(t) => map.push((*p, t.clone())),
                None if self.mentions(pat, *p) => return None,
                None => {}
            }
        }
        Some(self.subst(pat, &map))
    }

    /// Learn type parameters (`tparams`, bound so far in `bound`) by matching a
    /// parameter type `pat` against the type of the value given for it.
    /// `weak`: only bind parameters that are still unknown.
    fn unify(&self, pat: &Type, actual: &Type, tparams: &[ParamId], bound: &mut [Option<Type>], weak: bool) {
        match (pat, actual) {
            (_, Type::Error) => {}
            (Type::Param(p), a) => {
                let Some(i) = tparams.iter().position(|q| q == p) else { return };
                match &bound[i] {
                    None if matches!(a, Type::Null | Type::Unit) => {}
                    None => bound[i] = Some(a.clone()),
                    Some(_) if weak => {}
                    // `pick(1, null)`: `T` is `Int?`.
                    Some(b) if *a == Type::Null => bound[i] = Some(b.clone().nullable()),
                    Some(b) if !a.assignable(b) && b.assignable(a) => bound[i] = Some(a.clone()),
                    Some(_) => {}
                }
            }
            (Type::Nullable(_), Type::Null) => {}
            (Type::Nullable(p), Type::Nullable(a)) => self.unify(p, a, tparams, bound, weak),
            (Type::Nullable(p), a) => self.unify(p, a, tparams, bound, weak),
            (Type::List(p), Type::List(a)) => self.unify(p, a, tparams, bound, weak),
            (Type::Struct(ps) | Type::Enum(ps, _), Type::Struct(asid) | Type::Enum(asid, _))
                if self.struct_has_param[*ps] && self.struct_origin[*ps] == self.struct_origin[*asid] =>
            {
                for (p, a) in self.struct_args[*ps].iter().zip(&self.struct_args[*asid]) {
                    self.unify(p, a, tparams, bound, weak);
                }
            }
            _ => {}
        }
    }

    /// Does `e` take its type from where it's used: `null`, `[]`, or a variant
    /// of a generic enum without fields to tell its type arguments (`Tree.Leaf`)?
    fn needs_context(&mut self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Null => true,
            ExprKind::List(es) => es.is_empty(),
            ExprKind::Field(obj, _) => {
                let sid = match &obj.kind {
                    ExprKind::Name(n) if self.lookup(n).is_none() => self.struct_index.get(&(self.ns, n.clone())).copied().or_else(|| match self.names.get(&(self.ns, n.clone())) {
                        Some(NameImport { target: Imported::Struct(s), .. }) => Some(*s),
                        _ => None,
                    }),
                    ExprKind::Field(m, n) => match &m.kind {
                        ExprKind::Name(mn) if self.lookup(mn).is_none() => {
                            self.imports.get(&(self.ns, mn.clone())).and_then(|i| self.struct_index.get(&(i.module_ns, n.name.clone())).copied())
                        }
                        _ => None,
                    },
                    _ => None,
                };
                sid.is_some_and(|s| !self.struct_params[s].is_empty() && self.structs[s].variants.is_some())
            }
            _ => false,
        }
    }

    /// Check the arguments of a call to something generic over `tparams`
    /// (`params` and `ret` mention them): the type parameters come from
    /// `explicit` type arguments, or from the arguments, or else from the type
    /// the result is `expected` to have. Gives the arguments in parameter
    /// order and a type for each type parameter; `None` after an error.
    /// With no `tparams`, this is plain argument checking.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn infer_args(
        &mut self,
        what: &str,
        tparams: &[ParamId],
        params: &[(String, Type)],
        ret: &Type,
        args: &[Arg],
        explicit: Option<&[TypeExpr]>,
        expected: Option<&Type>,
        span: Span,
    ) -> Option<(Vec<TExpr>, Vec<Type>)> {
        let mut bound: Vec<Option<Type>> = vec![None; tparams.len()];
        if let Some(ex) = explicit {
            let tys: Vec<Type> = ex.iter().map(|t| self.resolve_type(t)).collect();
            if tparams.is_empty() || ex.len() != tparams.len() {
                let n = tparams.len();
                let d = if n == 0 {
                    Diagnostic::error(format!("{what} isn't generic, so it takes no type arguments"), span).with_help("remove the `[...]`")
                } else {
                    let names: Vec<String> = tparams.iter().map(|p| format!("`{}`", self.type_params[*p].name)).collect();
                    Diagnostic::error(format!("{what} takes {n} type argument{} ({}), found {}", if n == 1 { "" } else { "s" }, and_list(&names), ex.len()), span)
                };
                self.err(d);
                for a in args {
                    let _ = self.expr(&a.value, None);
                }
                return None;
            }
            for (b, t) in bound.iter_mut().zip(tys) {
                *b = Some(t);
            }
        }
        let (slots, ok) = self.arg_slots(what, params, args, span);
        // `[]`, `null` and `Tree.Leaf` need to know their type: they wait for the others.
        let later: Vec<bool> = slots.iter().map(|(_, a)| self.needs_context(&a.value)).collect();
        let mut checked: Vec<Option<TExpr>> = (0..params.len()).map(|_| None).collect();
        for pass in 0..2 {
            if pass == 1
                && let Some(exp) = expected
            {
                self.unify(ret, exp, tparams, &mut bound, true);
            }
            for (k, &(idx, a)) in slots.iter().enumerate() {
                if later[k] != (pass == 1) {
                    continue;
                }
                let pat = params[idx].1.clone();
                let known = self.known_type(&pat, tparams, &bound);
                let t = self.expr(&a.value, known.as_ref());
                self.unify(&pat, &t.ty, tparams, &mut bound, false);
                // With the parameter's type known, check the argument right away (as for any call).
                let t = match &known {
                    Some(k) if tparams.is_empty() => self.coerce(t, k, None),
                    _ => t,
                };
                checked[idx] = Some(t);
            }
        }
        if !ok {
            return None;
        }
        if let Some(i) = bound.iter().position(|b| b.is_none()) {
            let name = self.type_params[tparams[i]].name.clone();
            let mut d = Diagnostic::error(format!("cannot infer the type `{name}` for {what}"), span).with_primary(format!("which type is `{name}` here?"));
            if args.iter().any(|a| matches!(a.value.kind, ExprKind::Null)) {
                d = d.with_note(format!("`null` alone doesn't say which type `{name}` is"));
            }
            d = d.with_help("give it in brackets, e.g. `[Int]` after the name, or add a type annotation to the variable that receives the result");
            self.err(d);
            return None;
        }
        let bound: Vec<Type> = bound.into_iter().map(|b| b.unwrap()).collect();
        let map: Vec<(ParamId, Type)> = tparams.iter().copied().zip(bound.iter().cloned()).collect();
        let mut out = Vec::new();
        for (i, t) in checked.into_iter().enumerate() {
            let t = t.unwrap();
            if tparams.is_empty() {
                out.push(t);
            } else {
                let pt = self.subst(&params[i].1, &map);
                out.push(self.coerce(t, &pt, None));
            }
        }
        Some((out, bound))
    }

    /// A call of function `fid` (a method when `recv` is the receiver):
    /// checks the arguments and, for generic code, picks the copy for the
    /// types it's called with. `name_span`: where the function is named (for its hover).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn call_fn(
        &mut self,
        fid: FuncId,
        what: &str,
        recv: Option<TExpr>,
        args: &[Arg],
        explicit: Option<&[TypeExpr]>,
        expected: Option<&Type>,
        name_span: Span,
        span: Span,
    ) -> TExpr {
        let skip = recv.is_some() as usize;
        let params = self.funcs[fid].params[skip..].to_vec();
        let tparams = self.funcs[fid].tparams.clone();
        let ret = self.funcs[fid].ret.clone();
        let fallback = |ret: &Type| Self::mk(TExprKind::Int(0), if *ret == Type::Unit { Type::Unit } else { Type::Error }, span);
        let Some((targs, bound)) = self.infer_args(what, &tparams, &params, &ret, args, explicit, expected, span) else {
            return fallback(&ret);
        };
        let callee = if tparams.is_empty() {
            fid
        } else {
            let (decl, base) = self.funcs[fid].instance.clone().expect("generic functions have an instance entry");
            let map: Vec<(ParamId, Type)> = tparams.iter().copied().zip(bound.iter().cloned()).collect();
            let full: Vec<Type> = base.iter().map(|t| self.subst(t, &map)).collect();
            let inst = self.fn_instance(decl, full);
            // Hover: the generic signature, and what the type parameters are here.
            let shown: Vec<String> = tparams.iter().zip(&bound).map(|(p, t)| format!("`{}` is `{}`", self.type_params[*p].name, self.tn(t))).collect();
            self.generic_hovers.push((decl, name_span));
            let i = self.ide.hovers.iter().rposition(|h| h.span == name_span);
            if let Some(i) = i {
                let h = &mut self.ide.hovers[i];
                let here = format!("here {}", and_list(&shown));
                h.doc = Some(match h.doc.take() {
                    Some(d) => format!("{here}\n\n{d}"),
                    None => here,
                });
            }
            inst
        };
        let mut all = Vec::new();
        all.extend(recv);
        all.extend(targs);
        let ret = self.funcs[callee].ret.clone();
        Self::mk(TExprKind::Call(callee, all), ret, span)
    }

    /// Build the copies of generic code that concrete code uses (and the
    /// ones those use, and so on), by checking each body again with the type
    /// parameters bound to concrete types.
    pub(super) fn instantiate(&mut self, templates: &HashMap<FuncId, (&FnDecl, Option<StructId>, usize)>, out: &mut Vec<Option<TFunc>>) {
        let mut built = 0;
        while let Some(fid) = self.queue.pop() {
            let (decl, args) = self.funcs[fid].instance.clone().unwrap();
            let Some(&(ast, owner, ns)) = templates.get(&decl) else { continue };
            built += 1;
            if built > 50_000 || args.iter().any(|a| self.type_depth(a) > MAX_TYPE_DEPTH) {
                if self.endless_fns.insert(decl) {
                    let name = self.fn_display(decl);
                    self.err(
                        Diagnostic::error(format!("`{name}` needs ever more copies of itself"), self.funcs[decl].span)
                            .with_primary("each copy for some types calls one for bigger types")
                            .with_note("generic code is compiled once for every set of types it's used with, so the types it calls itself with must not keep growing"),
                    );
                }
                return;
            }
            self.ns = ns;
            let marks = (
                self.ide.hovers.len(),
                self.ide.definitions.len(),
                self.ide.expr_types.len(),
                self.ide.inlay_hints.len(),
                self.ide.locals.len(),
                self.pending_struct_hovers.len(),
            );
            let (ndiags, nobl) = (self.diags.len(), self.obligations.len());
            let tf = self.fn_body(ast, owner, fid);
            self.ide.hovers.truncate(marks.0);
            self.ide.definitions.truncate(marks.1);
            self.ide.expr_types.truncate(marks.2);
            self.ide.inlay_hints.truncate(marks.3);
            self.ide.locals.truncate(marks.4);
            self.pending_struct_hovers.truncate(marks.5);
            // Warnings were reported on the generic code itself. Errors here
            // mean the generic checks missed something: show where.
            let tps = self.funcs[decl].type_params.clone();
            let shown: Vec<String> = tps.iter().zip(&args).map(|(p, t)| format!("`{}` = `{}`", self.type_params[*p].name, self.tn(t))).collect();
            let note = format!("while compiling `{}` for {}", self.fn_display(decl), and_list(&shown));
            let errors: Vec<Diagnostic> = self.diags.drain(ndiags..).filter(|d| d.is_error()).map(|d| d.with_note(note.clone())).collect();
            let failed = !errors.is_empty();
            self.diags.extend(errors);
            self.record_calls(&tf.body, None);
            self.check_obligations(nobl);
            put_func(out, fid, tf);
            if failed || self.error_count() > 0 {
                return;
            }
        }
    }

    // ---------- needs ----------

    /// Generic code uses a value of type `t` in a way that needs `need`: when
    /// `t` is a type parameter (or `T?`), that becomes a need of the parameter.
    pub(super) fn require(&mut self, t: &Type, need: Need, span: Span) {
        let Some(g) = self.cur_generic else { return };
        let Type::Param(p) = *t.non_null() else { return };
        let list = self.needs.entry(g).or_default();
        if !list.iter().any(|n| n.param == p && n.need == need) {
            list.push(NeedUse { param: p, need, span, via: None });
        }
    }

    /// `a op b` where an operand's type is a type parameter.
    pub(super) fn param_binary(&mut self, op: BinOp, tl: TExpr, tr: TExpr, lspan: Span, rspan: Span, span: Span) -> TExpr {
        let base = tl.ty.non_null().clone();
        if base == *tr.ty.non_null() {
            if tl.ty.is_nullable() || tr.ty.is_nullable() {
                if matches!(op, BinOp::Eq | BinOp::Ne) {
                    self.require(&base, Need::EqNullable, span);
                    let nt = base.nullable();
                    let (a, b) = (self.coerce(tl, &nt, None), self.coerce(tr, &nt, None));
                    return Self::mk(TExprKind::Binary(op, Box::new(a), Box::new(b)), Type::Bool, span);
                }
                let culprit = if tl.ty.is_nullable() { &tl } else { &tr };
                self.null_use_error(culprit, NullUse::Value);
                return Self::mk(TExprKind::Binary(op, Box::new(tl), Box::new(tr)), result_on_error(op), span);
            }
            let ty = match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                    self.require(&base, Need::Arith(op), span);
                    base
                }
                BinOp::Eq | BinOp::Ne => {
                    self.require(&base, Need::Eq, span);
                    Type::Bool
                }
                BinOp::Cmp => {
                    self.require(&base, Need::Ord, span);
                    Type::Int
                }
                _ => {
                    self.require(&base, Need::Ord, span);
                    Type::Bool
                }
            };
            return Self::mk(TExprKind::Binary(op, Box::new(tl), Box::new(tr)), ty, span);
        }
        let (a, b) = (self.tn(&tl.ty), self.tn(&tr.ty));
        let sym = op.symbol();
        let p = if matches!(tl.ty.non_null(), Type::Param(_)) { a.trim_end_matches('?').to_string() } else { b.trim_end_matches('?').to_string() };
        self.err(
            Diagnostic::error(format!("operator `{sym}` cannot be applied to `{a}` and `{b}`"), span)
                .with_label(lspan, format!("this is `{a}`"))
                .with_label(rspan, format!("this is `{b}`"))
                .with_note(format!("`{p}` could be any type, so in generic code an operator works on two values of the same type parameter, like `{p} {sym} {p}`")),
        );
        Self::mk(TExprKind::Binary(op, Box::new(tl), Box::new(tr)), result_on_error(op), span)
    }

    /// Remember the calls of a checked body: each use of generic code is an
    /// obligation, and copies for concrete types get queued to be built.
    pub(super) fn record_calls(&mut self, body: &[TStmt], in_generic: Option<FuncId>) {
        let mut calls = Vec::new();
        calls_in(body, &mut calls);
        for (fid, span) in calls {
            let Some((decl, args)) = self.funcs[fid].instance.clone() else { continue };
            if let Some(g) = in_generic
                && decl == g
                && !args.iter().zip(&self.funcs[g].type_params).all(|(a, p)| *a == Type::Param(*p))
                && args.iter().any(|a| a.has_param(&self.struct_has_param))
                && self.endless_fns.insert(g)
            {
                let name = self.fn_display(g);
                let tps = self.funcs[g].type_params.clone();
                let shown: Vec<String> = tps
                    .iter()
                    .zip(&args)
                    .filter(|(p, a)| **a != Type::Param(**p))
                    .map(|(p, a)| format!("`{}` = `{}`", self.type_params[*p].name, self.tn(a)))
                    .collect();
                self.err(
                    Diagnostic::error(format!("`{name}` calls itself with {}", and_list(&shown)), span)
                        .with_primary("a call with bigger types")
                        .with_note(format!("every copy of `{name}` would need yet another one, without end"))
                        .with_help("call it with its own type parameters"),
                );
            }
            self.obligations.push(Obligation { fid, span, in_generic });
            if in_generic.is_none() && self.is_concrete_instance(fid) && self.queued.insert(fid) {
                self.queue.push(fid);
            }
        }
    }

    /// `max`, `Box.get`, "`+` of `Vec2`": how generic function `decl` is named in messages.
    pub(super) fn fn_display(&self, decl: FuncId) -> String {
        let sym = &self.ide.functions[decl];
        match sym.owner {
            Some(o) => format!("{}.{}", self.ide.structs[o].name, sym.name),
            None => self.funcs[decl].name.clone(),
        }
    }

    /// The needs of generic code that calls other generic code include the
    /// callee's (for the types passed to it). Computed to a fixed point, so
    /// recursion is fine.
    pub(super) fn close_needs(&mut self) {
        loop {
            let mut changed = false;
            for i in 0..self.obligations.len() {
                let Some(g) = self.obligations[i].in_generic else { continue };
                let (fid, span) = (self.obligations[i].fid, self.obligations[i].span);
                let (decl, args) = self.funcs[fid].instance.clone().unwrap();
                let tps = self.funcs[decl].type_params.clone();
                for nu in self.needs.get(&decl).cloned().unwrap_or_default() {
                    let Some(k) = tps.iter().position(|p| *p == nu.param) else { continue };
                    let Ok(residual) = self.satisfies(&args[k], nu.need, 0) else { continue };
                    for (q, m) in residual {
                        let list = self.needs.entry(g).or_default();
                        if !list.iter().any(|n| n.param == q && n.need == m) {
                            list.push(NeedUse { param: q, need: m, span, via: Some(decl) });
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Does `t` support `need`? `Ok` with what that still needs from type
    /// parameters inside `t` (nothing, for concrete types).
    pub(super) fn satisfies(&mut self, t: &Type, need: Need, depth: usize) -> Result<Vec<(ParamId, Need)>, Fail> {
        let fail = |t: &Type| Err(Fail { ty: t.clone(), need, trail: Vec::new() });
        if depth > MAX_TYPE_DEPTH {
            return Ok(Vec::new());
        }
        match t {
            Type::Error => Ok(Vec::new()),
            Type::Param(p) => Ok(vec![(*p, need)]),
            Type::Nullable(inner) => match (need, &**inner) {
                (Need::Format, _) => self.satisfies(inner, Need::Format, depth + 1),
                (Need::Eq | Need::EqNullable, Type::Param(p)) => Ok(vec![(*p, Need::EqNullable)]),
                (Need::Eq | Need::EqNullable, inner) => self.satisfies(inner, Need::EqNullable, depth + 1),
                _ => fail(t),
            },
            Type::Int | Type::Float => Ok(Vec::new()),
            Type::Bool => match need {
                Need::Eq | Need::EqNullable | Need::Format | Need::ToStr => Ok(Vec::new()),
                _ => fail(t),
            },
            Type::Str => match need {
                Need::Eq | Need::EqNullable | Need::Ord | Need::Arith(BinOp::Add) | Need::Format | Need::ToStr => Ok(Vec::new()),
                _ => fail(t),
            },
            Type::Struct(sid) | Type::Enum(sid, _) => {
                let sid = *sid;
                let plain_enum = matches!(t, Type::Enum(_, false));
                // Enums without data have `==` and printing built in.
                if plain_enum && matches!(need, Need::Eq | Need::EqNullable) && !self.has_ops(sid, OpKind::Eq) {
                    return Ok(Vec::new());
                }
                if plain_enum && matches!(need, Need::Format | Need::ToStr) && !self.has_method(sid, "to_string") {
                    return Ok(Vec::new());
                }
                let fid = match need {
                    Need::Eq => self.ops(sid, OpKind::Eq).first().copied(),
                    Need::EqNullable => None,
                    Need::Ord => self.ops(sid, OpKind::Cmp).first().copied(),
                    Need::Arith(op) => {
                        let cands = self.ops(sid, OpKind::of_binop(op));
                        cands.into_iter().find(|&f| self.funcs[f].params[1].1 == *t && self.funcs[f].ret == *t)
                    }
                    Need::Neg => self.ops(sid, OpKind::Neg).into_iter().find(|&f| self.funcs[f].ret == *t),
                    Need::Format | Need::ToStr => self.user_to_string(t),
                };
                let Some(fid) = fid else { return fail(t) };
                // A method of a generic type needs what its code needs.
                let Some((decl, args)) = self.funcs[fid].instance.clone() else { return Ok(Vec::new()) };
                let tps = self.funcs[decl].type_params.clone();
                let mut out = Vec::new();
                for nu in self.needs.get(&decl).cloned().unwrap_or_default() {
                    let Some(k) = tps.iter().position(|p| *p == nu.param) else { continue };
                    match self.satisfies(&args[k], nu.need, depth + 1) {
                        Ok(r) => out.extend(r),
                        Err(mut f) => {
                            let p = self.type_params[nu.param].name.clone();
                            let what = format!("`{}` needs `{p}` to support {}", self.fn_display(decl), nu.need.shown());
                            f.trail.insert(0, (nu.span, what));
                            return Err(f);
                        }
                    }
                }
                Ok(out)
            }
            _ => fail(t),
        }
    }

    /// Report uses of generic code (`obligations[from..]`) with types that
    /// lack what the code needs.
    pub(super) fn check_obligations(&mut self, from: usize) {
        let mut reported: HashSet<(Span, FuncId)> = HashSet::new();
        for i in from..self.obligations.len() {
            let (fid, span) = (self.obligations[i].fid, self.obligations[i].span);
            let (decl, args) = self.funcs[fid].instance.clone().unwrap();
            let tps = self.funcs[decl].type_params.clone();
            for nu in self.needs.get(&decl).cloned().unwrap_or_default() {
                let Some(k) = tps.iter().position(|p| *p == nu.param) else { continue };
                let Err(fail) = self.satisfies(&args[k], nu.need, 0) else { continue };
                if !reported.insert((span, decl)) {
                    break;
                }
                let d = self.need_error(decl, &nu, &args[k], fail, span);
                self.err(d);
                break;
            }
        }
    }

    fn need_error(&mut self, decl: FuncId, nu: &NeedUse, arg: &Type, fail: Fail, span: Span) -> Diagnostic {
        let name = self.fn_display(decl);
        let p = self.type_params[nu.param].name.clone();
        let shown = self.tn(arg);
        let mut d = Diagnostic::error(format!("`{name}` needs `{p}` to support {}, but `{shown}` doesn't", nu.need.shown()), span)
            .with_primary(format!("here `{p}` is `{shown}`"));
        d = match nu.via {
            None => d.with_label(nu.span, nu.need.used(&p)),
            Some(callee) => {
                let callee = self.fn_display(callee);
                d.with_label(nu.span, format!("`{p}` goes to `{callee}` here, which needs {}", nu.need.shown()))
            }
        };
        for (s, msg) in &fail.trail {
            d = d.with_label(*s, msg.clone());
        }
        let ft = fail.ty.clone();
        let fshown = self.tn(&ft);
        if ft != *arg {
            d = d.with_note(format!("`{fshown}` doesn't support {}", fail.need.shown()));
        }
        self.need_help(d, &ft, fail.need)
    }

    /// How to give type `t` what `need` asks for.
    fn need_help(&mut self, d: Diagnostic, t: &Type, need: Need) -> Diagnostic {
        let shown = self.tn(t);
        match t {
            Type::Nullable(_) => d
                .with_note(format!("{} might be `null`, and generic code can't use `null` that way", a_type(&shown)))
                .with_help("use a non-null type, or handle `null` before calling (e.g. with `?:`)"),
            Type::Struct(sid) | Type::Enum(sid, _) if self.can_extend(*sid) && need != Need::EqNullable => {
                let sid = *sid;
                let kw = if self.structs[sid].variants.is_some() { "enum" } else { "struct" };
                let short = short_name(&shown).to_string();
                let me = if self.struct_origin[sid] == sid { short.clone() } else { shown.clone() };
                let m = match need {
                    Need::Eq => format!("fn ==(self, other: {me}) -> Bool"),
                    Need::Ord => format!("fn <=>(self, other: {me}) -> Int"),
                    Need::Arith(op) => format!("fn {}(self, other: {me}) -> {me}", op.symbol()),
                    Need::Neg => format!("fn -(self) -> {me}"),
                    _ => "fn to_string(self) -> String".to_string(),
                };
                let mut d = d.with_help(format!("give `{kw} {short}` a method `{m}`"));
                if let Need::Arith(op) = need
                    && self.has_ops(sid, OpKind::of_binop(op))
                {
                    d = d.with_note(format!("`{shown}` has `fn {}`, but not one that takes and returns a `{shown}`", op.symbol()));
                }
                if need == Need::Ord {
                    d = d.with_note("`<=>` returns a negative number, 0 or a positive number; it gives `<`, `<=`, `>` and `>=`");
                }
                d
            }
            Type::Struct(_) | Type::Enum(..) if need == Need::EqNullable => {
                d.with_note(format!("`{shown}` has its own `==`, which needs two non-null values"))
            }
            Type::List(_) => d.with_note("lists can't be compared, added or printed directly"),
            _ => d,
        }
    }

    /// Append what each type parameter must support to the hovers of generic functions.
    pub(super) fn generic_docs(&mut self) {
        let mut texts: HashMap<FuncId, String> = HashMap::new();
        for (fid, f) in self.funcs.iter().enumerate() {
            if f.tparams.is_empty() || f.instance.as_ref().is_some_and(|(d, _)| *d != fid) {
                continue;
            }
            let needs = self.needs.get(&fid).cloned().unwrap_or_default();
            let mut lines = Vec::new();
            for &p in &f.tparams {
                let mut shown: Vec<String> = Vec::new();
                for n in needs.iter().filter(|n| n.param == p) {
                    let s = n.need.shown();
                    if !shown.contains(&s) {
                        shown.push(s);
                    }
                }
                let name = &self.type_params[p].name;
                lines.push(if shown.is_empty() {
                    format!("`{name}` can be any type")
                } else {
                    format!("`{name}` can be any type that supports {}", and_list(&shown))
                });
            }
            texts.insert(fid, lines.join("\n\n"));
        }
        for h in &mut self.ide.hovers {
            let Some((fid, _)) = self.generic_hovers.iter().find(|(_, s)| *s == h.span) else { continue };
            let Some(t) = texts.get(fid) else { continue };
            h.doc = Some(match h.doc.take() {
                Some(d) => format!("{d}\n\n{t}"),
                None => t.clone(),
            });
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

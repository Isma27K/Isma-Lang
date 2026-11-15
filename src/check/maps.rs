//! `Map[K, V]`, `x in xs`, `hash(...)` and searching lists.
//!
//! A map's keys must support `==` and `hash`: `Int`, `Bool`, `String` and
//! enums without data do, and so does a struct or enum that defines both
//! `fn ==(self, other: T) -> Bool` and `fn hash(self) -> Int`. The functions
//! a user type uses are recorded in `map_keys` for the back end, which calls
//! them from the hash table.

use super::*;

/// What a map test or search looks for in its container.
enum Container {
    Map(Type),
    List(Type),
    Str,
}

impl Checker {
    /// `fn hash(self) -> Int` of a struct or enum.
    pub(super) fn user_hash(&mut self, sid: StructId) -> Option<FuncId> {
        let fid = self.method(sid, "hash")?;
        (self.funcs[fid].params.len() == 1 && self.funcs[fid].ret == Type::Int).then_some(fid)
    }

    /// Can values of type `k` be map keys? Reports why not when `report` is
    /// set. In generic code, a key type parameter needs `==` and `hash`.
    pub(super) fn map_key(&mut self, k: &Type, span: Span, report: bool) -> bool {
        let shown = self.tn(k);
        let problem = match k {
            Type::Int | Type::Bool | Type::Str | Type::Error => return true,
            Type::Param(_) => {
                self.require(k, Need::Key, span);
                return true;
            }
            // An enum without data is its variant's number: keys compare by variant.
            Type::Enum(_, false) => return true,
            Type::Struct(sid) | Type::Enum(sid, _) => {
                let sid = *sid;
                let eq = self.find_operator(sid, OpKind::Eq, None).ok();
                let hash = self.user_hash(sid);
                if let (Some(eq), Some(hash)) = (eq, hash) {
                    if !k.has_param(&self.struct_has_param) && !self.map_keys.iter().any(|m| m.sid == sid) {
                        self.map_keys.push(MapKey { sid, hash, eq });
                        // Methods of generic types are compiled for each instance used.
                        for fid in [hash, eq] {
                            if self.funcs[fid].instance.is_none() {
                                continue;
                            }
                            self.obligations.push(Obligation { fid, span, in_generic: None });
                            if self.is_concrete_instance(fid) && self.queued.insert(fid) {
                                self.queue.push(fid);
                            }
                        }
                    }
                    return true;
                }
                if !report {
                    return false;
                }
                let kw = if self.structs[sid].variants.is_some() { "enum" } else { "struct" };
                let short = short_name(&shown).to_string();
                let me = if self.struct_origin[sid] == sid { short.clone() } else { shown.clone() };
                let missing: Vec<String> = [
                    eq.is_none().then(|| format!("fn ==(self, other: {me}) -> Bool")),
                    hash.is_none().then(|| "fn hash(self) -> Int".to_string()),
                ]
                .into_iter()
                .flatten()
                .collect();
                let mut d = Diagnostic::error(format!("`{shown}` can't be a map key"), span)
                    .with_primary(format!("`{shown}` has no {}", and_list(&missing.iter().map(|m| format!("`{}`", m.split('(').next().unwrap_or(m))).collect::<Vec<_>>())))
                    .with_note("a map finds keys by their hash and compares them with `==`");
                if self.can_extend(sid) {
                    let fields: Vec<String> = self.structs[sid].fields.iter().take(3).map(|f| format!("self.{}", f.name)).collect();
                    let fields = if fields.is_empty() { "...".to_string() } else { fields.join(", ") };
                    let example = if hash.is_none() { format!("\n\n    fn hash(self) -> Int:\n        return hash({fields})      # the fields that `==` compares") } else { String::new() };
                    d = d.with_help(format!("give `{kw} {short}` {}{example}", and_list(&missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>())));
                }
                self.err(d);
                return false;
            }
            Type::Float => "`Float`s make unreliable keys: `0.1 + 0.2 != 0.3`, and NaN isn't equal to itself",
            Type::Nullable(_) | Type::Null => "keys can't be `null`",
            Type::List(_) | Type::Map(..) => "a key must not change while it's in the map, and lists and maps can change",
            Type::Unit => "`Unit` has no values",
        };
        if report {
            let mut d = Diagnostic::error(format!("`{shown}` can't be a map key"), span).with_primary("not a valid key type").with_note(problem);
            d = match k {
                Type::Float => d.with_help("use `Int` or `String` keys (e.g. scale and round the number)"),
                Type::Nullable(inner) => d.with_help(format!("use `{}` keys, and leave null keys out", self.tn(inner))),
                _ => d.with_help("keys can be `Int`, `Bool`, `String`, enums, and types with `fn ==` and `fn hash`"),
            };
            self.err(d);
        }
        false
    }

    /// `Map[K, V]` in a type annotation.
    pub(super) fn resolve_map_type(&mut self, t: &TypeExpr) -> Type {
        if t.args.len() != 2 {
            self.err(
                Diagnostic::error(format!("type `Map` takes 2 type arguments (keys and values), found {}", t.args.len()), t.span)
                    .with_help("write it like `Map[String, Int]`"),
            );
            for a in &t.args {
                let _ = self.resolve_type(a);
            }
            return Type::Error;
        }
        let k = self.resolve_type(&t.args[0]);
        let v = self.resolve_type(&t.args[1]);
        if v == Type::Unit {
            self.err(Diagnostic::error("map values must have a type other than `Unit`", t.args[1].span));
            return Type::Error;
        }
        // A bad key type is reported once; the map itself still has a type.
        let k = if self.map_key(&k, t.args[0].span, true) { k } else { Type::Error };
        Type::Map(Box::new(k), Box::new(v))
    }

    /// The type shared by `values`, checked against `expected` when it's known
    /// (else inferred from the first value, `Int` and `null` making `Int?`).
    /// `what` is "key" or "value". `None` after an error.
    fn infer_common(&mut self, values: &[&Expr], expected: Option<&Type>, what: &str) -> (Vec<TExpr>, Option<Type>) {
        let mut out = Vec::new();
        let mut ty = expected.cloned();
        let mut ok = true;
        for e in values {
            let t = self.expr(e, ty.as_ref());
            match &ty {
                None => {
                    if t.ty == Type::Unit {
                        self.err(Diagnostic::error(format!("map {what}s must have a value"), e.span));
                        ok = false;
                    }
                    ty = Some(t.ty.clone());
                }
                Some(et) if expected.is_none() && !t.ty.assignable(et) && join_types(et, &t.ty).is_some() => ty = join_types(et, &t.ty),
                Some(et) => {
                    if !t.ty.assignable(et) {
                        let (a, b) = (self.tn(et), self.tn(&t.ty));
                        let mut d = Diagnostic::error(format!("map {what}s must all have the same type: expected `{a}`, found `{b}`"), e.span)
                            .with_primary(format!("this is `{b}`"));
                        if expected.is_none() {
                            d = d.with_label(values[0].span, format!("the {what} type was inferred as `{a}` from this"));
                        }
                        self.err(d);
                        ok = false;
                    }
                }
            }
            out.push(t);
        }
        let Some(et) = ty.filter(|_| ok) else { return (out, None) };
        let out = out.into_iter().map(|t| if t.ty.assignable(&et) { self.coerce(t, &et, None) } else { t }).collect();
        (out, Some(et))
    }

    /// `{"a": 1, "b": 2}`
    pub(super) fn map_literal(&mut self, entries: &[(Expr, Expr)], expected: Option<&Type>, span: Span) -> TExpr {
        let (ek, ev) = match expected.map(Type::non_null) {
            Some(Type::Map(k, v)) => (Some((**k).clone()), Some((**v).clone())),
            _ => (None, None),
        };
        if entries.is_empty() {
            return match (ek, ev) {
                (Some(k), Some(v)) => {
                    self.map_key(&k, span, false);
                    Self::mk(TExprKind::MapLit(Vec::new()), Type::Map(Box::new(k), Box::new(v)), span)
                }
                _ => {
                    self.err(
                        Diagnostic::error("cannot infer the key and value types of an empty map", span)
                            .with_help("add a type annotation, e.g. `var counts: Map[String, Int] = {}`"),
                    );
                    Self::error_expr(span)
                }
            };
        }
        // Keys and values are evaluated in the order they're written.
        let keys: Vec<&Expr> = entries.iter().map(|(k, _)| k).collect();
        let vals: Vec<&Expr> = entries.iter().map(|(_, v)| v).collect();
        let (tk, kt) = self.infer_common(&keys, ek.as_ref(), "key");
        let (tv, vt) = self.infer_common(&vals, ev.as_ref(), "value");
        let (Some(kt), Some(vt)) = (kt, vt) else { return Self::error_expr(span) };
        for (t, what) in [(&kt, "key"), (&vt, "value")] {
            if *t == Type::Null {
                self.err(
                    Diagnostic::error(format!("cannot infer the {what} type of a map of `null`s"), span)
                        .with_help("add a type annotation, e.g. `val m: Map[String, Int?] = {\"a\": null}`"),
                );
                return Self::error_expr(span);
            }
        }
        if !self.map_key(&kt, entries[0].0.span, true) {
            return Self::error_expr(span);
        }
        // `{"a": 1, "a": 2}`: the second one wins, which is rarely intended.
        let mut seen: Vec<(String, Span)> = Vec::new();
        for (t, (k, _)) in tk.iter().zip(entries) {
            let lit = match &t.kind {
                TExprKind::Int(v) => v.to_string(),
                TExprKind::Str(s) => format!("{s:?}"),
                TExprKind::Bool(b) => b.to_string(),
                _ => continue,
            };
            match seen.iter().find(|(l, _)| *l == lit) {
                Some((_, first)) => self.diags.push(
                    Diagnostic::warning(format!("duplicate key {lit} in a map literal"), k.span)
                        .with_primary("this value replaces the earlier one")
                        .with_label(*first, "first used here")
                        .with_help("remove one of the entries"),
                ),
                None => seen.push((lit, k.span)),
            }
        }
        let ty = Type::Map(Box::new(kt), Box::new(vt));
        Self::mk(TExprKind::MapLit(tk.into_iter().zip(tv).collect()), ty, span)
    }

    /// The key of `m[k]`, `m.get(k)`... checked against the map's key type.
    fn map_key_arg(&mut self, k: &Type, e: &Expr) -> TExpr {
        let t = self.expr(e, Some(k));
        if t.ty.is_nullable() && !k.is_nullable() && t.ty.non_null().compatible(k) {
            self.null_use_error(&t, NullUse::Value);
            return Self::error_expr(e.span);
        }
        self.map_key(k, e.span, false);
        self.coerce(t, k, None)
    }

    /// `m[k]`: panics if `k` isn't in the map.
    pub(super) fn map_index(&mut self, m: TExpr, index: &Expr, span: Span) -> TExpr {
        let Type::Map(k, v) = m.ty.clone() else { unreachable!() };
        let key = self.map_key_arg(&k, index);
        Self::mk(TExprKind::Builtin(Builtin::MapGet, vec![m, key]), *v, span)
    }

    /// `m[k] = v` (and `m[k] += v`).
    pub(super) fn map_place(&mut self, m: TExpr, index: &Expr, span: Span) -> (Place, Type) {
        let Type::Map(k, v) = m.ty.clone() else { unreachable!() };
        let key = self.map_key_arg(&k, index);
        (Place::MapEntry { map: m, key, span }, *v)
    }

    /// A method of `Map[K, V]`.
    pub(super) fn map_method(&mut self, recv: TExpr, method: &Ident, args: &[Arg], span: Span) -> TExpr {
        let Type::Map(k, v) = recv.ty.clone() else { unreachable!() };
        let (k, v) = (*k, *v);
        let m = method.name.as_str();
        let what = format!("method `{m}`");
        let mk = Self::mk;
        let none = |c: &mut Checker, b: Builtin, ret: Type, recv: TExpr| -> TExpr {
            if !c.arity(args, 0, &what, span) {
                return Self::error_expr(span);
            }
            mk(TExprKind::Builtin(b, vec![recv]), ret, span)
        };
        match m {
            "len" => none(self, Builtin::MapLen, Type::Int, recv),
            "is_empty" => {
                let len = none(self, Builtin::MapLen, Type::Int, recv);
                self.is_zero(len, span)
            }
            "clear" => none(self, Builtin::MapClear, Type::Unit, recv),
            "keys" => none(self, Builtin::MapKeys, Type::List(Box::new(k)), recv),
            "values" => none(self, Builtin::MapValues, Type::List(Box::new(v)), recv),
            "contains" | "remove" => {
                if !self.arity(args, 1, &what, span) {
                    return Self::error_expr(span);
                }
                let key = self.map_key_arg(&k, &args[0].value);
                let (b, ret) = if m == "contains" { (Builtin::MapContains, Type::Bool) } else { (Builtin::MapRemove, v.nullable()) };
                mk(TExprKind::Builtin(b, vec![recv, key]), ret, span)
            }
            "get" => {
                if args.len() != 1 && args.len() != 2 {
                    self.arity(args, 1, &what, span);
                    return Self::error_expr(span);
                }
                if self.reject_named(args, &what) {
                    return Self::error_expr(span);
                }
                let key = self.map_key_arg(&k, &args[0].value);
                if args.len() == 1 {
                    return mk(TExprKind::Builtin(Builtin::MapGetOrNull, vec![recv, key]), v.nullable(), span);
                }
                let d = self.expr(&args[1].value, Some(&v));
                let d = self.coerce(d, &v, None);
                mk(TExprKind::Builtin(Builtin::MapGetOr, vec![recv, key, d]), v, span)
            }
            _ => {
                let tn = self.tn(&recv.ty);
                let names = ["len", "is_empty", "get", "contains", "remove", "keys", "values", "clear"];
                let mut d = Diagnostic::error(format!("no method `{m}` on type `{tn}`"), method.span)
                    .with_primary("method not found")
                    .with_note("methods available on maps: `len()`, `is_empty()`, `get(key)`, `get(key, default)`, `contains(key)`, `remove(key)`, `keys()`, `values()`, `clear()`");
                let hint = match m {
                    "put" | "set" | "insert" => Some("m[key] = value".to_string()),
                    "has" | "contains_key" | "has_key" => Some("contains".to_string()),
                    "pop" | "delete" => Some("remove".to_string()),
                    "size" | "length" | "count" => Some("len".to_string()),
                    "get_or" | "get_or_default" | "get_or_else" => Some("get(key, default)".to_string()),
                    "items" | "entries" => {
                        d = d.with_note("to loop over keys and values, use `for k, v in m:`");
                        None
                    }
                    _ => suggest(m, names.into_iter()),
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
        }
    }

    /// `e == 0` (for `is_empty()`).
    pub(super) fn is_zero(&mut self, e: TExpr, span: Span) -> TExpr {
        if e.ty.is_error() {
            return e;
        }
        Self::mk(TExprKind::Binary(BinOp::Eq, Box::new(e), Box::new(Self::mk(TExprKind::Int(0), Type::Int, span))), Type::Bool, span)
    }

    /// `a == b` between two values of type `t` (for searching lists), or `None`
    /// if `t` has no `==`.
    fn equality(&mut self, t: &Type, a: TExpr, b: TExpr, span: Span) -> Option<TExpr> {
        let bin = |a: TExpr, b: TExpr| Self::mk(TExprKind::Binary(BinOp::Eq, Box::new(a), Box::new(b)), Type::Bool, span);
        match t.non_null() {
            Type::Param(_) => {
                self.require(t, if t.is_nullable() { Need::EqNullable } else { Need::Eq }, span);
                Some(bin(a, b))
            }
            Type::Int | Type::Float | Type::Bool | Type::Str => Some(bin(a, b)),
            Type::Enum(sid, false) if !self.has_ops(*sid, OpKind::Eq) => Some(bin(a, b)),
            Type::Struct(sid) | Type::Enum(sid, _) if !t.is_nullable() => {
                let fid = self.find_operator(*sid, OpKind::Eq, None).ok()?;
                let param = self.funcs[fid].params[1].1.clone();
                let b = self.coerce(b, &param, None);
                Some(Self::mk(TExprKind::Call(fid, vec![a, b]), Type::Bool, span))
            }
            _ => None,
        }
    }

    /// Search list `list` for `value`: `Bool` (`contains`), or `Int?` (`index_of`).
    fn list_find(&mut self, list: TExpr, value: TExpr, index: bool, span: Span) -> TExpr {
        let Type::List(elem) = list.ty.clone() else { unreachable!() };
        let elem = *elem;
        let e = self.hidden_local(elem.clone());
        let n = self.hidden_local(elem.clone());
        let el = Self::mk(TExprKind::Local(e), elem.clone(), span);
        let nl = Self::mk(TExprKind::Local(n), elem.clone(), span);
        let Some(test) = self.equality(&elem, el, nl, span) else {
            let shown = self.tn(&elem);
            let what = if index { "index_of" } else { "contains" };
            let mut d = Diagnostic::error(format!("`{what}` compares elements with `==`, but `{shown}` doesn't support it"), span)
                .with_primary(format!("these are `{shown}`s"));
            if let Some(sid) = user_type(elem.non_null()).filter(|s| self.can_extend(*s) && !elem.is_nullable()) {
                let kw = if self.structs[sid].variants.is_some() { "enum" } else { "struct" };
                let short = short_name(&shown).to_string();
                d = d.with_help(format!("give `{kw} {short}` a method `fn ==(self, other: {short}) -> Bool`"));
            } else if elem.is_nullable() && user_type(elem.non_null()).is_some() {
                d = d.with_note("a type's own `fn ==` needs two non-null values");
            }
            self.err(d);
            return Self::error_expr(span);
        };
        let ty = if index { Type::Int.nullable() } else { Type::Bool };
        let value = if value.ty.assignable(&elem) { self.coerce(value, &elem, None) } else { value };
        Self::mk(TExprKind::ListFind { list: Box::new(list), value: Box::new(value), elem: e, needle: n, test: Box::new(test), index }, ty, span)
    }

    /// `xs.contains(x)` / `xs.index_of(x)`.
    pub(super) fn list_search_method(&mut self, recv: TExpr, method: &Ident, args: &[Arg], span: Span) -> TExpr {
        let Type::List(elem) = recv.ty.clone() else { unreachable!() };
        let index = method.name == "index_of";
        if !self.arity(args, 1, &format!("method `{}`", method.name), span) {
            return Self::error_expr(span);
        }
        let v = self.expr(&args[0].value, Some(&elem));
        if !v.ty.assignable(&elem) && !v.ty.is_error() {
            let (a, b) = (self.tn(&elem), self.tn(&v.ty));
            self.err(Diagnostic::error(format!("expected `{a}`, found `{b}`"), args[0].value.span).with_primary(format!("this is `{b}`")).with_note(format!("the list holds `{a}`s")));
            return Self::error_expr(span);
        }
        self.list_find(recv, v, index, span)
    }

    /// `x in c` / `x not in c`: a key of a map, an element of a list, or a
    /// substring of a string.
    pub(super) fn in_expr(&mut self, item: &Expr, container: &Expr, negated: bool, span: Span) -> TExpr {
        // Left to right: the item first (into a hidden local unless it's trivial).
        let ti = self.expr(item, None);
        let tc = self.expr(container, None);
        let kind = match &tc.ty {
            Type::Map(k, _) => Container::Map((**k).clone()),
            Type::List(e) => Container::List((**e).clone()),
            Type::Str => Container::Str,
            Type::Error => return Self::error_expr(span),
            Type::Nullable(inner) if matches!(**inner, Type::Map(..) | Type::List(_) | Type::Str) => {
                self.null_use_error(&tc, NullUse::Value);
                return Self::error_expr(span);
            }
            other => {
                let shown = self.tn(other);
                let mut d = Diagnostic::error(format!("`in` needs a map, a list or a string on its right, found `{shown}`"), container.span)
                    .with_primary(format!("this is `{shown}`"));
                if *other == Type::Int {
                    d = d.with_help("to check a range, compare: `a <= x and x < b`");
                }
                self.err(d);
                return Self::error_expr(span);
            }
        };
        let want = match &kind {
            Container::Map(k) => k.clone(),
            Container::List(e) => e.clone(),
            Container::Str => Type::Str,
        };
        let ti = if ti.ty.assignable(&want) { self.coerce(ti, &want, None) } else { ti };
        if !ti.ty.compatible(&want) && !ti.ty.is_error() {
            let (a, b) = (self.tn(&ti.ty), self.tn(&tc.ty));
            let what = match &kind {
                Container::Map(_) => "keys",
                Container::List(_) => "elements",
                Container::Str => "substrings",
            };
            let mut d = Diagnostic::error(format!("`{a}` can't be in a `{b}`"), item.span)
                .with_label(item.span, format!("this is `{a}`"))
                .with_note(format!("the {what} of a `{b}` are `{}`s", self.tn(&want)));
            if ti.ty.is_nullable() && ti.ty.non_null().compatible(&want) {
                d = d.with_help("check for `null` first");
            }
            self.err(d);
            return Self::error_expr(span);
        }
        let trivial = matches!(ti.kind, TExprKind::Int(_) | TExprKind::Float(_) | TExprKind::Bool(_) | TExprKind::Str(_) | TExprKind::Local(_) | TExprKind::Null);
        let (bind, item_ref) = if trivial {
            (None, ti)
        } else {
            let id = self.hidden_local(ti.ty.clone());
            let r = Self::mk(TExprKind::Local(id), ti.ty.clone(), ti.span);
            (Some((id, ti)), r)
        };
        let test = match kind {
            Container::Map(k) => {
                self.map_key(&k, item.span, false);
                Self::mk(TExprKind::Builtin(Builtin::MapContains, vec![tc, item_ref]), Type::Bool, span)
            }
            Container::List(_) => self.list_find(tc, item_ref, false, span),
            Container::Str => Self::mk(TExprKind::Builtin(Builtin::StrContains, vec![tc, item_ref]), Type::Bool, span),
        };
        let test = if negated { Self::mk(TExprKind::Not(Box::new(test)), Type::Bool, span) } else { test };
        match bind {
            None => test,
            Some((id, value)) => Self::mk(TExprKind::Let(id, Box::new(value), Box::new(test)), Type::Bool, span),
        }
    }

    /// `hash(a, b, ...)`: one `Int` mixing the hashes of every argument.
    pub(super) fn hash_call(&mut self, args: &[Arg], span: Span) -> TExpr {
        if args.is_empty() {
            self.err(
                Diagnostic::error("`hash` takes at least one argument", span)
                    .with_help("pass the values to hash, e.g. `hash(self.x, self.y)`"),
            );
            return Self::error_expr(span);
        }
        if self.reject_named(args, "`hash`") {
            return Self::error_expr(span);
        }
        let mut parts = Vec::new();
        for a in args {
            let t = self.expr(&a.value, None);
            match self.hash_part(t, a.value.span) {
                Some(p) => parts.push(p),
                None => return Self::error_expr(span),
            }
        }
        Self::mk(TExprKind::Builtin(Builtin::HashCombine, parts), Type::Int, span)
    }

    /// The hash of one value, as an `Int` expression.
    fn hash_part(&mut self, t: TExpr, span: Span) -> Option<TExpr> {
        let ty = t.ty.clone();
        let builtin = |t: TExpr| Self::mk(TExprKind::Builtin(Builtin::HashOf, vec![t]), Type::Int, span);
        match ty.non_null() {
            Type::Error => Some(t),
            Type::Param(_) => {
                self.require(&ty, Need::Hash, span);
                Some(builtin(t))
            }
            Type::Int | Type::Float | Type::Bool | Type::Str => Some(builtin(t)),
            Type::Enum(sid, false) if !self.has_method(*sid, "hash") => Some(builtin(t)),
            Type::Struct(sid) | Type::Enum(sid, _) if self.user_hash(*sid).is_some() => {
                let fid = self.user_hash(*sid).unwrap();
                let inner = ty.non_null().clone();
                if !ty.is_nullable() {
                    return Some(Self::mk(TExprKind::Call(fid, vec![t]), Type::Int, span));
                }
                // `x?.hash() ?: 0`
                let binder = self.hidden_local(inner.clone());
                let body = Self::mk(TExprKind::Call(fid, vec![Self::mk(TExprKind::Local(binder), inner, span)]), Type::Int, span);
                let safe = Self::mk(TExprKind::SafeCall { recv: Box::new(t), binder, body: Box::new(body) }, Type::Int.nullable(), span);
                Some(Self::mk(TExprKind::Elvis(Box::new(safe), Box::new(Self::mk(TExprKind::Int(0), Type::Int, span))), Type::Int, span))
            }
            other => {
                let shown = self.tn(&ty);
                let mut d = Diagnostic::error(format!("cannot hash a value of type `{shown}`"), span).with_primary(format!("this is `{shown}`"));
                d = match user_type(other).filter(|s| self.can_extend(*s)) {
                    Some(sid) => {
                        let kw = if self.structs[sid].variants.is_some() { "enum" } else { "struct" };
                        d.with_help(format!("give `{kw} {}` a method `fn hash(self) -> Int`", short_name(&shown)))
                    }
                    None => d.with_note("numbers, `Bool`, `String`, enums, and types with `fn hash(self) -> Int` can be hashed"),
                };
                self.err(d);
                None
            }
        }
    }

    /// `for k in m:` / `for k, v in m:`
    pub(super) fn for_map(&mut self, key: &Ident, value: Option<&Ident>, m: TExpr, body: &[Stmt], span: Span) -> TStmt {
        let Type::Map(kt, vt) = m.ty.clone() else { unreachable!() };
        self.scopes.push(HashMap::new());
        for (id, t) in [(Some(key), &*kt), (value, &*vt)] {
            if let Some(id) = id
                && id.name != "_"
            {
                let hint = format!(": {}", self.tn(t));
                self.ide.inlay_hints.push((id.span.end, hint));
            }
        }
        self.forget_assigned_in(body);
        let before = self.nonnull.clone();
        let k = self.declare_local(key, *kt, LocalKind::LoopVar);
        let v = value.map(|v| self.declare_local(v, *vt, LocalKind::LoopVar));
        self.loop_depth += 1;
        let b = self.block(body, true);
        self.loop_depth -= 1;
        self.scopes.pop();
        self.nonnull = before;
        TStmt::ForMap { key: k, value: v, map: m, body: b, span }
    }
}

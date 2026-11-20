//! Interfaces: `interface Shape:` lists methods (`fn area(self) -> Float`)
//! that a type must have to be a `Shape`. Nothing has to say it is one: any
//! type with those methods, with the same parameter names and types and the
//! same return type, fits. (`struct Circle(Shape):` asks for that to be
//! checked where `Circle` is defined, with errors there.)
//!
//! An interface is used in two ways:
//!
//! - As a *bound* on a type parameter, `fn total[T: Shape](xs: List[T])`:
//!   generic code can call `Shape`'s methods on a `T`. A bound is one more
//!   thing the code needs from `T` (see `generics.rs`), and the code is still
//!   copied for each type it's used with, so the calls are direct.
//! - As a *type*, `List[Shape]`: a value of any type that fits, so one list
//!   can hold circles and squares. A value becomes a `Shape` in a *box*: a
//!   small heap object holding the value and the *method table* of its type,
//!   one piece of closure code (see `lambdas.rs`) per method of `Shape`,
//!   calling that method on the boxed value. `s.area()` calls the code in
//!   `area`'s slot of the table.
//!
//! Requiring the same parameter names (as Python's protocols do) means a
//! call like `s.scale(by=2.0)` reads the same on every type that fits, which
//! generic code relies on when it's checked again for each type.

use super::*;

/// An interface (by its id in the struct table; a generic one's instances,
/// like `Source[Int]`, have their own, with their types filled in).
#[derive(Default, Clone)]
pub(super) struct IfaceInfo {
    /// Its methods in the order of its method tables, each with its
    /// declaration: its own, then those of the interfaces it includes.
    /// Default methods without type parameters of their own are here too
    /// (a type can replace them, so they're called through the table).
    pub methods: Vec<(String, FuncId)>,
    /// Its own methods.
    pub own: Vec<(String, FuncId)>,
    /// `NamedShape(Shape, Named)`: the interfaces it includes, and where each is named.
    pub extends: Vec<(StructId, Span)>,
    /// Every default method (its own and included ones): name, declaration,
    /// and the interface (instance) that declares it.
    pub defaults: Vec<(String, FuncId, StructId)>,
}

/// Why a type doesn't fit an interface.
pub(super) struct Misfit {
    /// The interface's method it lacks or gets wrong.
    pub method: Option<FuncId>,
    /// "`Circle` has no method `area`"
    pub reason: String,
    /// Whether the type could be given the method (a struct or enum of the program).
    pub fixable: Option<StructId>,
    /// The type has a method of that name, which differs.
    pub differs: bool,
}

/// A bound to check once every method is known: the type, the interface,
/// where the type is used, and (for messages) the generic type and its parameter.
pub(super) struct PendingBound {
    pub ty: Type,
    pub iface: StructId,
    pub span: Span,
    pub owner: String,
    pub param: ParamId,
}

impl Checker {
    // ---------- declarations ----------

    /// `interface Shape:` (pass 2): its own methods' signatures, and the
    /// interfaces it includes.
    pub(super) fn declare_interface<'p>(&mut self, sid: StructId, decl: &'p InterfaceDecl, ns: usize, bodies: &mut Vec<(&'p FnDecl, Option<StructId>, usize, FuncId)>) {
        let iname = self.structs[sid].name.clone();
        let mut info = IfaceInfo::default();
        for m in &decl.methods {
            let name = &m.name.name;
            if is_operator_name(name) {
                self.err(
                    Diagnostic::error(format!("an interface can't ask for `fn {name}`"), m.sig_span)
                        .with_primary("operator function")
                        .with_note("generic code doesn't need it listed: comparing two `T`s with `<` already makes `T` need `<=>`, adding them needs `+`, and so on"),
                );
                continue;
            }
            if m.self_param.is_none() {
                self.err(
                    Diagnostic::error(format!("interface method `{name}` must take `self` as its first parameter"), m.sig_span)
                        .with_help(format!("write `fn {name}(self{}...`", if m.params.is_empty() { ")" } else { ", " })),
                );
                continue;
            }
            if let Some(tp) = m.type_params.first() {
                self.err(
                    Diagnostic::error(format!("interface method `{name}` can't have type parameters yet"), tp.name.span)
                        .with_primary("not supported yet")
                        .with_help("use concrete types in an interface's methods"),
                );
                continue;
            }
            if let Some((_, prev)) = info.own.iter().find(|(n, _)| n == name) {
                let prev_span = self.funcs[*prev].span;
                self.err(Diagnostic::error(format!("method `{name}` is listed more than once"), m.name.span).with_label(prev_span, "first listed here"));
                continue;
            }
            let fid = self.declare_fn(m, Some(sid));
            self.ide.functions[fid].doc = Some(match self.ide.functions[fid].doc.take() {
                Some(d) => format!("required by interface `{iname}`\n\n{d}"),
                None => format!("required by interface `{iname}`"),
            });
            if let Some(h) = self.ide.hovers.iter_mut().rev().find(|h| h.span == m.name.span) {
                h.doc = self.ide.functions[fid].doc.clone();
            }
            self.methods.insert((sid, name.clone()), fid);
            self.ide.structs[sid].methods.push(fid);
            info.own.push((name.clone(), fid));
        }
        // Default methods: generic code over a hidden `Self` that fits the interface.
        for m in &decl.defaults {
            let name = &m.name.name;
            if is_operator_name(name) {
                self.err(Diagnostic::error(format!("an interface can't have `fn {name}`"), m.sig_span).with_primary("operator function"));
                continue;
            }
            if m.self_param.is_none() {
                self.err(
                    Diagnostic::error(format!("default method `{name}` must take `self` as its first parameter"), m.sig_span)
                        .with_note("a default method runs for each type that fits the interface, on a value of it"),
                );
                continue;
            }
            if let Some(prev) = info.own.iter().find(|(n, _)| n == name).map(|(_, f)| *f).or_else(|| info.defaults.iter().find(|(n, ..)| n == name).map(|(_, f, _)| *f)) {
                let prev_span = self.funcs[prev].span;
                self.err(Diagnostic::error(format!("method `{name}` is listed more than once"), m.name.span).with_label(prev_span, "first listed here"));
                continue;
            }
            let sp = self.type_params.len();
            self.type_params.push(ParamInfo { name: "Self".into(), span: m.name.span, owner: iname.clone(), bounds: vec![(sid, m.name.span)] });
            self.ide.params.push("Self".into());
            self.declaring_default = Some(sp);
            let fid = self.declare_fn(m, Some(sid));
            self.declaring_default = None;
            self.ide.functions[fid].doc = Some(match self.ide.functions[fid].doc.take() {
                Some(d) => format!("default method of interface `{iname}`\n\n{d}"),
                None => format!("default method of interface `{iname}`"),
            });
            if let Some(h) = self.ide.hovers.iter_mut().rev().find(|h| h.span == m.name.span) {
                h.doc = self.ide.functions[fid].doc.clone();
            }
            self.methods.insert((sid, name.clone()), fid);
            self.ide.structs[sid].methods.push(fid);
            self.default_decls.insert(fid);
            // One without type parameters of its own can be replaced by a type: it's in the method tables.
            if m.type_params.is_empty() {
                info.own.push((name.clone(), fid));
            }
            info.defaults.push((name.clone(), fid, sid));
            bodies.push((m, Some(sid), ns, fid));
        }
        for t in &decl.extends {
            match self.resolve_type(t) {
                Type::Iface(j) => info.extends.push((j, t.span)),
                Type::Error => {}
                other => {
                    let tn = self.tn(&other);
                    self.err(
                        Diagnostic::error(format!("`{tn}` isn't an interface"), t.span)
                            .with_primary("not an interface")
                            .with_note(format!("`interface {}(...)` lists interfaces whose methods it includes", short_name(&iname))),
                    );
                }
            }
        }
        self.ifaces.insert(sid, info);
    }

    /// Once every interface's own methods are known: each one's full list of
    /// methods (with those of the interfaces it includes), and the methods
    /// that types promise with `struct Circle(Shape):`.
    pub(super) fn finish_ifaces(&mut self) {
        let mut ids: Vec<StructId> = self.ifaces.keys().copied().collect();
        ids.sort();
        let mut done: HashSet<StructId> = HashSet::new();
        for id in ids {
            self.flatten_iface(id, &mut Vec::new(), &mut done);
        }
        // Instances of generic interfaces made so far (`Source[Int]` in a signature).
        let instances: Vec<StructId> =
            (0..self.structs.len()).filter(|&i| matches!(self.def_types[i], Type::Iface(_)) && self.struct_origin[i] != i).collect();
        for i in instances {
            self.fill_iface_instance(i);
        }
    }

    /// Instance `inst` of a generic interface (`Source[Int]`): its methods,
    /// defaults and included interfaces, with its type arguments filled in.
    pub(super) fn fill_iface_instance(&mut self, inst: StructId) {
        let origin = self.struct_origin[inst];
        if origin == inst || self.ifaces.contains_key(&inst) {
            return;
        }
        let Some(o) = self.ifaces.get(&origin).cloned() else { return };
        // (Put something in first: filling it in can make more instances.)
        self.ifaces.insert(inst, IfaceInfo::default());
        let map: Vec<(ParamId, Type)> = self.struct_params[origin].iter().copied().zip(self.struct_args[inst].clone()).collect();
        let member = |c: &mut Checker, f: FuncId| match c.funcs[f].instance.clone() {
            Some((decl, args)) => {
                let args: Vec<Type> = args.iter().map(|a| c.subst(a, &map)).collect();
                c.fn_instance(decl, args)
            }
            None => f,
        };
        let iface = |c: &mut Checker, j: StructId| match c.subst(&Type::Iface(j), &map) {
            Type::Iface(x) => x,
            _ => j,
        };
        let info = IfaceInfo {
            methods: o.methods.iter().map(|(n, f)| (n.clone(), member(self, *f))).collect(),
            own: o.own.iter().map(|(n, f)| (n.clone(), member(self, *f))).collect(),
            extends: o.extends.iter().map(|(j, s)| (iface(self, *j), *s)).collect(),
            defaults: o.defaults.iter().map(|(n, d, j)| (n.clone(), *d, iface(self, *j))).collect(),
        };
        self.ifaces.insert(inst, info);
    }

    /// Fill in the full method list of interface `id` (`path`: the interfaces
    /// being filled in that include it, to catch one including itself).
    fn flatten_iface(&mut self, id: StructId, path: &mut Vec<StructId>, done: &mut HashSet<StructId>) {
        if done.contains(&id) {
            return;
        }
        if path.contains(&id) {
            let name = self.structs[id].name.clone();
            self.err(
                Diagnostic::error(format!("interface `{name}` includes itself"), self.struct_spans[id])
                    .with_primary("through the interfaces it includes")
                    .with_help("remove one of the interfaces from the list in parentheses"),
            );
            done.insert(id);
            return;
        }
        path.push(id);
        let extends = self.ifaces[&id].extends.clone();
        let mut methods = self.ifaces[&id].own.clone();
        let mut defaults = self.ifaces[&id].defaults.clone();
        for (j, span) in extends {
            // (`Peekable[T](Source[T])` includes an instance: its interface is done first.)
            let origin = self.struct_origin[j];
            self.flatten_iface(origin, path, done);
            self.fill_iface_instance(j);
            for (name, d, owner) in self.ifaces.get(&j).map(|i| i.defaults.clone()).unwrap_or_default() {
                if !defaults.iter().any(|(n, ..)| *n == name) && !methods.iter().any(|(n, _)| *n == name) {
                    defaults.push((name, d, owner));
                }
            }
            for (name, fid) in self.ifaces.get(&j).map(|i| i.methods.clone()).unwrap_or_default() {
                match methods.iter().find(|(n, _)| *n == name) {
                    Some((_, prev)) if *prev == fid => {}
                    Some((_, prev)) => {
                        let prev = *prev;
                        if self.same_signature(prev, fid).is_err() {
                            let (iname, jname) = (self.structs[id].name.clone(), self.structs[j].name.clone());
                            let (a, b) = (self.ide.functions[prev].signature.clone(), self.ide.functions[fid].signature.clone());
                            self.err(
                                Diagnostic::error(format!("`{iname}` gets two different methods called `{name}`"), span)
                                    .with_primary(format!("`{jname}` has `{b}`"))
                                    .with_label(self.funcs[prev].span, format!("this is `{a}`"))
                                    .with_note("a type can only have one method of each name"),
                            );
                        }
                    }
                    None => methods.push((name, fid)),
                }
            }
        }
        path.pop();
        for (name, fid) in &methods {
            self.methods.entry((id, name.clone())).or_insert(*fid);
            if !self.ide.structs[id].methods.contains(fid) {
                self.ide.structs[id].methods.push(*fid);
            }
        }
        let info = self.ifaces.get_mut(&id).unwrap();
        info.methods = methods;
        info.defaults = defaults;
        done.insert(id);
    }

    /// Do two methods take the same parameters (by name and type, besides
    /// `self`) and return the same type? `Err` says how they differ, in
    /// words for "`Circle.area` ..." (`what`, `found` is the method of the type).
    fn same_signature(&self, want: FuncId, found: FuncId) -> Result<(), String> {
        let (wp, fp) = (&self.funcs[want].params[1..], &self.funcs[found].params[1..]);
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        if !self.funcs[found].tparams.is_empty() {
            return Err("has type parameters of its own".into());
        }
        if wp.len() != fp.len() {
            return Err(format!("takes {} parameter{} besides `self`, not {}", fp.len(), plural(fp.len()), wp.len()));
        }
        for ((wn, wt), (fname, ft)) in wp.iter().zip(fp) {
            if !wt.compatible(ft) {
                return Err(format!("takes `{fname}: {}`, not `{wn}: {}`", self.tn(ft), self.tn(wt)));
            }
            if wn != fname {
                return Err(format!("calls its parameter `{fname}`, not `{wn}`"));
            }
        }
        let (wr, fr) = (&self.funcs[want].ret, &self.funcs[found].ret);
        if !wr.compatible(fr) {
            let shown = |t: &Type| if *t == Type::Unit { "nothing".to_string() } else { format!("`{}`", self.tn(t)) };
            return Err(format!("returns {}, not {}", shown(fr), shown(wr)));
        }
        Ok(())
    }

    /// `T: Shape + Named` (`tps`, declared as `pids`): the interfaces each must be.
    pub(super) fn resolve_bounds(&mut self, pids: &[ParamId], tps: &[TypeParam]) {
        let named = |c: &Checker, p: ParamId| tps.iter().find(|tp| tp.name.span == c.type_params[p].span);
        for &p in pids {
            let Some(tp) = named(self, p) else { continue };
            let mut bounds = Vec::new();
            for b in &tp.bounds {
                // What other languages call the operators' bounds.
                let known = |c: &mut Checker| b.module.is_some() || c.visible_struct(&b.name.name).is_some();
                if ["Comparable", "Ord", "PartialOrd", "Eq", "PartialEq", "Equatable", "Hashable", "Hash", "Numeric", "Number", "Num"].contains(&b.name.name.as_str())
                    && !known(self)
                {
                    let (bn, pn) = (b.name.name.clone(), tp.name.name.clone());
                    self.err(
                        Diagnostic::error(format!("Tarn has no `{bn}`: operators need no bound"), b.span)
                            .with_primary("remove this bound")
                            .with_note(format!("Tarn works out what `{pn}` must support from how the code uses it: comparing two `{pn}`s with `<` makes it need `<=>`, adding them needs `+`, using one as a map key needs `==` and `hash`"))
                            .with_help("bounds are interfaces, for calling methods on a `T`: `[T: Shape]`"),
                    );
                    continue;
                }
                // `H: Function`: `H` stands for one particular named function.
                if b.name.name == "Function" && b.module.is_none() && b.args.is_empty() && b.func.is_none() && !known(self) {
                    self.ct.fn_item_params.insert(p);
                    self.hover(b.span, "Function".into(), Some("A type parameter bounded by `Function` stands for one particular named function: the code can read its parameters (`params(H)`) and attributes (`attribute(H, a)`), and calls it directly.".into()));
                    continue;
                }
                match self.resolve_type(b) {
                    Type::Iface(i) => {
                        if !bounds.iter().any(|(j, _)| *j == i) {
                            bounds.push((i, b.span));
                        }
                    }
                    Type::Error => {}
                    other => {
                        let (tn, pn) = (self.tn(&other), tp.name.name.clone());
                        self.err(
                            Diagnostic::error(format!("`{tn}` isn't an interface, so it can't be a bound"), b.span)
                                .with_primary("not an interface")
                                .with_note(format!("a bound like `{pn}: Shape` says which interface `{pn}` must fit; operators need no bound: comparing two `{pn}`s with `<` already makes `{pn}` need `<=>`")),
                        );
                    }
                }
            }
            if bounds.is_empty() {
                continue;
            }
            let shown: Vec<String> = bounds.iter().map(|(i, _)| self.structs[*i].name.clone()).collect();
            let code = format!("{}: {}", self.type_params[p].name, shown.join(" + "));
            let span = self.type_params[p].span;
            if let Some(h) = self.ide.hovers.iter_mut().rev().find(|h| h.span == span) {
                h.code = code;
            }
            self.type_params[p].bounds = bounds;
        }
    }

    /// The bounds of generic function `fid`'s own type parameters, as things its code needs.
    pub(super) fn bound_needs(&mut self, fid: FuncId) {
        for p in self.funcs[fid].tparams.clone() {
            for (i, span) in self.type_params[p].bounds.clone() {
                self.needs.entry(fid).or_default().push(NeedUse { param: p, need: Need::Iface(i), span, via: None });
            }
        }
    }

    /// `struct Circle(Shape):`: check that `sid` fits the interfaces it names.
    pub(super) fn check_implements(&mut self, sid: StructId, list: &[(Type, Span)]) {
        let me = self.def_types[sid].clone();
        let tname = short_name(&self.structs[sid].name).to_string();
        // Their default methods are then methods of this type too.
        let declared: Vec<StructId> = list.iter().filter_map(|(t, _)| if let Type::Iface(i) = t { Some(*i) } else { None }).collect();
        self.declared_ifaces.insert(sid, declared);
        for (t, span) in list {
            let Type::Iface(i) = *t else {
                if !t.is_error() {
                    let tn = self.tn(t);
                    self.err(
                        Diagnostic::error(format!("`{tn}` isn't an interface"), *span)
                            .with_primary("not an interface")
                            .with_note(format!("the names in `{tname}(...)` are interfaces `{tname}` promises to fit")),
                    );
                }
                continue;
            };
            if let Err(m) = self.fit(&me, i) {
                let iname = self.structs[i].name.clone();
                let mut d = Diagnostic::error(format!("`{tname}` says it's {}, but {}", a_type(&iname), m.reason), *span).with_primary(format!("`{tname}` promises to fit `{iname}`"));
                if let Some(want) = m.method {
                    d = d.with_label(self.funcs[want].span, format!("`{iname}` needs `{}`", self.ide.functions[want].signature));
                }
                let d = self.misfit_help(d, &m);
                self.err(d);
            }
        }
    }

    // ---------- fitting ----------

    /// Does type `t` fit interface `iface`? `Ok` with what that needs from
    /// type parameters (when `t` is or contains one, in generic code).
    pub(super) fn fit(&mut self, t: &Type, iface: StructId) -> Result<Vec<(ParamId, Need)>, Misfit> {
        let tn = self.tn(t);
        let misfit = |reason: String| Err(Misfit { method: None, reason, fixable: None, differs: false });
        let methods = self.ifaces.get(&iface).map(|i| i.methods.clone()).unwrap_or_default();
        match t {
            Type::Error => Ok(Vec::new()),
            Type::Iface(j) if *j == iface => Ok(Vec::new()),
            Type::Param(p) => Ok(vec![(*p, Need::Iface(iface))]),
            Type::Nullable(_) | Type::Null => misfit(format!("`{tn}` might be `null`, and `null` has no methods")),
            Type::Func(..) => misfit(format!("`{tn}` is a function, which has no methods")),
            Type::Unit => misfit("it's nothing (`Unit`)".into()),
            Type::Iface(j) => {
                let j = *j;
                for (name, want) in methods {
                    let found = self.ifaces.get(&j).and_then(|i| i.methods.iter().find(|(n, _)| *n == name)).map(|(_, f)| *f);
                    let Some(found) = found else {
                        if self.is_default(want) {
                            continue;
                        }
                        return Err(Misfit { method: Some(want), reason: format!("`{tn}` has no method `{name}`"), fixable: None, differs: false });
                    };
                    if let Err(how) = self.same_signature(want, found) {
                        return Err(Misfit { method: Some(want), reason: format!("`{tn}.{name}` {how}"), fixable: None, differs: true });
                    }
                }
                Ok(Vec::new())
            }
            Type::Struct(sid) | Type::Enum(sid, _) => {
                let sid = *sid;
                let fixable = self.can_extend(sid).then_some(self.struct_origin[sid]);
                let short = short_name(&tn).to_string();
                let mut out = Vec::new();
                for (name, want) in methods {
                    // An enum without data prints as its variant's name.
                    if matches!(t, Type::Enum(_, false))
                        && name == "to_string"
                        && !self.has_method(sid, &name)
                        && self.funcs[want].params.len() == 1
                        && self.funcs[want].ret == Type::Str
                    {
                        continue;
                    }
                    let Some(found) = self.method(sid, &name) else {
                        // A default method fills in what the type doesn't have.
                        if self.is_default(want) {
                            continue;
                        }
                        if self.structs[sid].fields.iter().any(|f| f.name == name) {
                            let reason = format!("`{name}` is a field of `{short}`, not a method (a type can't have both: rename the field, then add the method)");
                            return Err(Misfit { method: Some(want), reason, fixable: None, differs: false });
                        }
                        return Err(Misfit { method: Some(want), reason: format!("`{short}` has no method `{name}`"), fixable, differs: false });
                    };
                    if let Err(how) = self.same_signature(want, found) {
                        return Err(Misfit { method: Some(want), reason: format!("`{short}.{name}` {how}"), fixable, differs: true });
                    }
                    // A method of a generic type needs what its code needs (inside generic code).
                    if let Ok(r) = self.callee_needs(found, 0) {
                        out.extend(r);
                    }
                }
                Ok(out)
            }
            // Built-in types fit methods without parameters that they have.
            _ => {
                for (name, want) in methods {
                    if self.is_default(want) {
                        continue;
                    }
                    if self.funcs[want].params.len() > 1 {
                        return Err(Misfit {
                            method: Some(want),
                            reason: format!("methods of built-in types can only stand for interface methods without parameters, and `{name}` takes some"),
                            fixable: None,
                            differs: false,
                        });
                    }
                    let ret = self.funcs[want].ret.clone();
                    let span = self.funcs[want].span;
                    let probe = Self::mk(TExprKind::Int(0), t.clone(), span);
                    let (got, failed) = self.quietly(|c| c.method_call(probe, &Ident { name: name.clone(), span }, &[], None, None, span).ty);
                    if failed {
                        return Err(Misfit { method: Some(want), reason: format!("`{tn}` has no method `{name}`"), fixable: None, differs: false });
                    }
                    if !got.compatible(&ret) {
                        let shown = |c: &Checker, t: &Type| if *t == Type::Unit { "nothing".to_string() } else { format!("`{}`", c.tn(t)) };
                        let reason = format!("`{tn}.{name}()` returns {}, not {}", shown(self, &got), shown(self, &ret));
                        return Err(Misfit { method: Some(want), reason, fixable: None, differs: true });
                    }
                }
                Ok(Vec::new())
            }
        }
    }

    /// Run `f` without leaving anything behind: no diagnostics, no editor
    /// information. Says whether it reported an error.
    pub(super) fn quietly<R>(&mut self, f: impl FnOnce(&mut Checker) -> R) -> (R, bool) {
        let marks = (
            self.diags.len(),
            self.ide.hovers.len(),
            self.ide.definitions.len(),
            self.ide.expr_types.len(),
            self.ide.inlay_hints.len(),
            self.ide.locals.len(),
            self.pending_struct_hovers.len(),
            self.generic_hovers.len(),
        );
        let r = f(self);
        let failed = self.diags[marks.0..].iter().any(|d| d.is_error());
        self.diags.truncate(marks.0);
        self.ide.hovers.truncate(marks.1);
        self.ide.definitions.truncate(marks.2);
        self.ide.expr_types.truncate(marks.3);
        self.ide.inlay_hints.truncate(marks.4);
        self.ide.locals.truncate(marks.5);
        self.pending_struct_hovers.truncate(marks.6);
        self.generic_hovers.truncate(marks.7);
        (r, failed)
    }

    /// "give `struct Circle` a method `fn area(self) -> Float`"
    pub(super) fn misfit_help(&self, d: Diagnostic, m: &Misfit) -> Diagnostic {
        match (m.fixable, m.method) {
            (Some(sid), Some(want)) => {
                let kw = if self.structs[sid].variants.is_some() { "enum" } else { "struct" };
                let short = short_name(&self.structs[sid].name).to_string();
                let sig = &self.ide.functions[want].signature;
                if m.differs {
                    d.with_help(format!("change `{short}.{}` to match: `{sig}`", self.ide.functions[want].name))
                } else {
                    d.with_help(format!("give `{kw} {short}` a method `{sig}`"))
                }
            }
            _ => d,
        }
    }

    /// Can a value of type `from` be used where a `to` is expected: as it is,
    /// or put in a box when `to` is an interface type that it fits?
    pub(super) fn can_coerce(&mut self, from: &Type, to: &Type) -> bool {
        if from.assignable(to) {
            return true;
        }
        let Type::Iface(i) = *to.non_null() else { return false };
        let from = if to.is_nullable() { from.non_null().clone() } else { from.clone() };
        if from.is_nullable() || matches!(from, Type::Null | Type::Unit) {
            return false;
        }
        self.fit(&from, i).is_ok()
    }

    /// An interface both `a` and `b` fit (for a hint when they meet in one list).
    pub(super) fn common_iface(&mut self, a: &Type, b: &Type) -> Option<StructId> {
        if !matches!(a, Type::Struct(_) | Type::Enum(..)) || !matches!(b, Type::Struct(_) | Type::Enum(..)) {
            return None;
        }
        let mut ids: Vec<StructId> = self.ifaces.keys().copied().filter(|i| !self.ifaces[i].methods.is_empty()).collect();
        // Interfaces of this file first.
        ids.sort_by_key(|i| (self.struct_ns[*i] != self.ns, *i));
        ids.into_iter().find(|&i| self.fit(a, i).is_ok() && self.fit(b, i).is_ok())
    }

    /// A bound on generic type `owner`'s parameter `param`, for type `t` used
    /// at `span` (`Zoo[Int]`): reported if `t` doesn't fit; in generic code,
    /// what that needs from the code's own type parameters is recorded.
    pub(super) fn check_bound(&mut self, b: PendingBound) {
        if !self.sigs_ready {
            self.pending_bounds.push(b);
            return;
        }
        match self.fit(&b.ty, b.iface) {
            Ok(residual) => {
                for (q, need) in residual {
                    self.require(&Type::Param(q), need, b.span);
                }
            }
            Err(m) => {
                let (iname, tn, pn) = (self.structs[b.iface].name.clone(), self.tn(&b.ty), self.type_params[b.param].name.clone());
                let bound = self.type_params[b.param].bounds.iter().find(|(i, _)| *i == b.iface).map(|(_, s)| *s).unwrap_or(self.type_params[b.param].span);
                let d = Diagnostic::error(format!("`{}` needs `{pn}` to be {}, but `{tn}` isn't", b.owner, a_type(&iname)), b.span)
                    .with_primary(format!("here `{pn}` is `{tn}`"))
                    .with_label(bound, format!("`{pn}` must be {} here", a_type(&iname)))
                    .with_note(m.reason.clone());
                let d = self.misfit_help(d, &m);
                self.err(d);
            }
        }
    }

    /// The bounds on the type parameters of generic type `origin`, for its
    /// instance with type arguments `args`, used at `span`.
    pub(super) fn check_type_bounds(&mut self, origin: StructId, args: &[Type], span: Span) {
        let params = self.struct_params[origin].clone();
        let owner = short_name(&self.structs[origin].name).to_string();
        // (A bound can mention the type's other parameters: `I: Iterator[T]`.)
        let map: Vec<(ParamId, Type)> = params.iter().copied().zip(args.iter().cloned()).collect();
        for (p, t) in params.iter().zip(args) {
            for (i, _) in self.type_params[*p].bounds.clone() {
                let iface = match self.subst(&Type::Iface(i), &map) {
                    Type::Iface(j) => j,
                    _ => i,
                };
                self.check_bound(PendingBound { ty: t.clone(), iface, span, owner: owner.clone(), param: *p });
            }
        }
    }

    // ---------- interface values ----------

    /// `e` used where interface type `expected` (`Shape` or `Shape?`) is wanted.
    pub(super) fn box_as_iface(&mut self, e: TExpr, iface: StructId, expected: &Type, annotation: Option<Span>) -> TExpr {
        let span = e.span;
        let from = e.ty.clone();
        if from.is_nullable() && !expected.is_nullable() {
            let inner = from.non_null().clone();
            if self.fit(&inner, iface).is_ok() {
                // A `Circle?` where a `Shape` is wanted: the problem is the `null`.
                let (exp, found) = (self.tn(expected), self.tn(&from));
                let snip = self.snippet(span).unwrap_or_else(|| "value".into());
                let mut d = Diagnostic::error(format!("mismatched types: expected `{exp}`, found `{found}`"), span)
                    .with_primary("this might be `null`")
                    .with_help(format!("give a default with `{snip} ?: ...`, check `if {snip} != null:` first, or assert it isn't with `{snip}!!`"));
                if let Some(a) = annotation {
                    d = d.with_label(a, "expected because of this annotation");
                }
                self.err(d);
            } else {
                self.expect_type(&e, expected, annotation);
            }
            return e;
        }
        let inner = from.non_null().clone();
        match self.fit(&inner, iface) {
            Ok(residual) => {
                for (q, need) in residual {
                    self.require(&Type::Param(q), need, span);
                }
                let bx = self.box_struct(iface, &inner, span);
                let ity = Type::Iface(iface);
                if from.is_nullable() {
                    // `c?` as a `Shape?`: boxed unless it's null.
                    let binder = self.hidden_local(inner.clone());
                    let it = Self::mk(TExprKind::Local(binder), inner, span);
                    let body = Self::mk(TExprKind::Boxed(bx, Box::new(it)), ity.clone(), span);
                    return Self::mk(TExprKind::SafeCall { recv: Box::new(e), binder, body: Box::new(body) }, ity.nullable(), span);
                }
                let boxed = Self::mk(TExprKind::Boxed(bx, Box::new(e)), ity.clone(), span);
                if expected.is_nullable() { Self::mk(TExprKind::Wrap(Box::new(boxed)), ity.nullable(), span) } else { boxed }
            }
            Err(m) => {
                let (exp, found, iname) = (self.tn(expected), self.tn(&from), self.structs[iface].name.clone());
                let mut d = Diagnostic::error(format!("mismatched types: expected `{exp}`, found `{found}`"), span)
                    .with_primary(format!("`{}` isn't {}", self.tn(&inner), a_type(&iname)))
                    .with_note(m.reason.clone());
                if let Some(a) = annotation {
                    d = d.with_label(a, "expected because of this annotation");
                }
                let d = self.misfit_help(d, &m);
                self.err(d);
                e
            }
        }
    }

    /// The box for values of type `ty` used as values of interface `iface`
    /// (made once), with its method table.
    fn box_struct(&mut self, iface: StructId, ty: &Type, span: Span) -> StructId {
        if let Some(&b) = self.boxes.get(&(iface, ty.clone())) {
            return b;
        }
        // (A box for a concrete type is real even when generic code makes it:
        // other code may use the same one.)
        let generic = ty.has_param(&self.struct_has_param);
        let b = self.closure_struct(span, generic);
        let name = format!("<{} as {}>", self.tn(ty), self.structs[iface].name);
        self.structs[b].name = name.clone();
        self.ide.structs[b].name = name;
        self.structs[b].fields.push(TField { name: "value".into(), ty: ty.clone(), mutable: false });
        self.structs[b].closure = Some(Type::Iface(iface));
        self.boxes.insert((iface, ty.clone()), b);
        if !generic {
            let n = self.ifaces.get(&iface).map(|i| i.methods.len()).unwrap_or(0);
            let table: Vec<FuncId> = (0..n).map(|slot| self.box_method(b, iface, slot, ty, span)).collect();
            self.structs[b].vtable = table;
        }
        b
    }

    /// The closure code in slot `slot` of the method table of box `b`: it
    /// calls that method of interface `iface` on the boxed value (of type `ty`).
    fn box_method(&mut self, b: StructId, iface: StructId, slot: usize, ty: &Type, span: Span) -> FuncId {
        let (name, decl) = self.ifaces[&iface].methods[slot].clone();
        let params: Vec<Type> = self.funcs[decl].params[1..].iter().map(|(_, t)| t.clone()).collect();
        let ret = self.funcs[decl].ret.clone();
        // Checked as a function of its own, with nothing from around it in scope.
        let mut outer = FnState::default();
        self.swap_state(&mut outer);
        let lambdas = std::mem::take(&mut self.lambda_stack);
        let saved = (self.cur_generic.take(), self.in_template, std::mem::take(&mut self.type_env), self.cur_fn);
        self.in_template = false;
        let me = self.locals.len();
        self.local_meta.push(LocalMeta { name: "_box".into(), span, kind: LocalKind::Param, used: true, reassigned: false });
        self.locals.push(TLocal { name: "_box".into(), ty: Type::Struct(b) });
        let mut ids = vec![me];
        let mut args = Vec::new();
        for (i, t) in params.iter().enumerate() {
            let n = format!("_arg{i}");
            let id = self.locals.len();
            self.local_meta.push(LocalMeta { name: n.clone(), span, kind: LocalKind::Param, used: true, reassigned: false });
            self.locals.push(TLocal { name: n.clone(), ty: t.clone() });
            self.scopes[0].insert(n.clone(), LocalBinding { id, mutable: false, span });
            ids.push(id);
            args.push(Arg { name: None, value: Expr { kind: ExprKind::Name(n), span } });
        }
        let value = Self::mk(TExprKind::Field(Box::new(Self::mk(TExprKind::Local(me), Type::Struct(b), span)), b, 0), ty.clone(), span);
        // (A type that only fits the interface still gets its default methods here.)
        let extra = std::mem::replace(&mut self.extra_default_ifaces, vec![iface]);
        let (call, failed) = self.quietly(|c| c.method_call(value, &Ident { name: name.clone(), span }, &args, None, None, span));
        self.extra_default_ifaces = extra;
        let body = if ret == Type::Unit { vec![TStmt::Expr(call)] } else { vec![TStmt::Return(Some(call))] };
        self.swap_state(&mut outer);
        let inner = outer;
        self.lambda_stack = lambdas;
        (self.cur_generic, self.in_template, self.type_env, self.cur_fn) = saved;
        if failed {
            let (tn, iname) = (self.tn(ty), self.structs[iface].name.clone());
            self.err(Diagnostic::error(format!("internal error: can't call `{name}` on a `{tn}` used as {}", a_type(&iname)), span));
        }
        let tf = TFunc {
            name: format!("{}.{name}", self.structs[b].name),
            symbol: format!("tarn_box.{b}.{slot}"),
            params: ids,
            ret: ret.clone(),
            locals: inner.locals,
            body,
            exported: false,
            track_caller: false,
            generic: false,
            closure: true,
            is_drop: false,
        };
        self.add_closure_code(tf, span, &Type::Func(params, Box::new(ret)))
    }

    /// The slot of method `name` in the method tables of interface `iface`.
    pub(super) fn iface_slot(&self, iface: StructId, name: &str) -> Option<(usize, FuncId)> {
        self.ifaces.get(&iface)?.methods.iter().position(|(n, _)| n == name).map(|k| (k, self.ifaces[&iface].methods[k].1))
    }

    /// `to_string` of interface `iface`, if it has one taking nothing and returning a `String`.
    pub(super) fn iface_to_string(&self, iface: StructId) -> Option<usize> {
        let (k, fid) = self.iface_slot(iface, "to_string")?;
        (self.funcs[fid].params.len() == 1 && self.funcs[fid].ret == Type::Str).then_some(k)
    }

    /// `s.area()` where `s` is a value of interface type `iface`: a call through its method table.
    pub(super) fn iface_call(&mut self, recv: TExpr, iface: StructId, method: &Ident, args: &[Arg], explicit: Option<&[TypeExpr]>, span: Span) -> TExpr {
        let m = method.name.as_str();
        let iname = self.structs[iface].name.clone();
        let Some((slot, decl)) = self.iface_slot(iface, m) else {
            // A default method with type parameters of its own: its code runs on the interface value.
            if let Some((d, owner)) = self.iface_default(iface, m).or_else(|| self.default_for(&Type::Iface(iface), m)) {
                return self.default_call(recv, d, owner, method, args, explicit, span);
            }
            let names: Vec<String> = self.ifaces.get(&iface).map(|i| i.methods.iter().map(|(n, _)| n.clone()).collect()).unwrap_or_default();
            let mut d = Diagnostic::error(format!("no method `{m}` on interface `{iname}`"), method.span).with_primary("method not found");
            if let Some(s) = suggest(m, names.iter().map(|s| s.as_str())) {
                d = d.with_help(format!("did you mean `{s}`?"));
            } else if names.is_empty() {
                d = d.with_note(format!("`{iname}` has no methods"));
            } else {
                d = d.with_note(format!("a `{iname}` can be any type that fits it, so only its methods can be used: {}", and_list(&names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>())));
            }
            if m == "to_string" && self.can_extend(iface) {
                d = d.with_help(format!("add `fn to_string(self) -> String` to `interface {}` (every type that fits it then needs one)", short_name(&iname)));
            }
            self.err(d);
            for a in args {
                let _ = self.expr(&a.value, None);
            }
            return Self::error_expr(span);
        };
        let code = self.ide.functions[decl].signature.clone();
        self.hover(method.span, code, Some(format!("method of interface `{iname}`")));
        self.ide.definitions.push((method.span, self.funcs[decl].span));
        self.dispatch(recv, slot, decl, m, args, explicit, span)
    }

    /// A call of interface method `decl` (in slot `slot`) on `recv`.
    #[allow(clippy::too_many_arguments)]
    fn dispatch(&mut self, recv: TExpr, slot: usize, decl: FuncId, m: &str, args: &[Arg], explicit: Option<&[TypeExpr]>, span: Span) -> TExpr {
        let params = self.funcs[decl].params[1..].to_vec();
        let ret = self.funcs[decl].ret.clone();
        let what = format!("method `{m}`");
        match self.infer_args(&what, &[], &params, &ret, args, explicit, None, span) {
            Some((targs, _)) => Self::mk(TExprKind::CallMethod(Box::new(recv), slot, targs), ret, span),
            None => Self::mk(TExprKind::Int(0), if ret == Type::Unit { Type::Unit } else { Type::Error }, span),
        }
    }

    /// `x.area()` in generic code, where `x` is a `T` with bound `Shape`. (The
    /// code is checked again for each type it's used with; this is the generic version.)
    pub(super) fn bound_call(&mut self, recv: TExpr, p: ParamId, method: &Ident, args: &[Arg], explicit: Option<&[TypeExpr]>, span: Span) -> Option<TExpr> {
        let m = method.name.as_str();
        // A default method of a bound: its code for this `T`. (Each type's
        // own copy of the generic code calls the type's own method, if it has one.)
        let default = self.type_params[p].bounds.iter().find_map(|(i, _)| self.iface_default(*i, m));
        if let Some((d, owner)) = default {
            return Some(self.default_call(recv, d, owner, method, args, explicit, span));
        }
        let found = self.type_params[p].bounds.iter().find_map(|(i, _)| self.iface_slot(*i, m).map(|(k, f)| (*i, k, f)));
        let (iface, slot, decl) = found?;
        let code = self.ide.functions[decl].signature.clone();
        let doc = format!("method of interface `{}`, a bound of `{}`", self.structs[iface].name, self.type_params[p].name);
        self.hover(method.span, code, Some(doc));
        self.ide.definitions.push((method.span, self.funcs[decl].span));
        Some(self.dispatch(recv, slot, decl, m, args, explicit, span))
    }
}

impl Checker {
    // ---------- default methods ----------

    /// Is `fid` a default method of an interface (or a copy of one)?
    pub(super) fn is_default(&self, fid: FuncId) -> bool {
        self.default_decls.contains(&fid) || self.funcs[fid].instance.as_ref().is_some_and(|(d, _)| self.default_decls.contains(d))
    }

    /// Default method `name` of interface `iface`: its declaration, and the
    /// interface (instance) that declares it.
    pub(super) fn iface_default(&self, iface: StructId, name: &str) -> Option<(FuncId, StructId)> {
        self.ifaces.get(&iface)?.defaults.iter().find(|(n, ..)| n == name).map(|(_, d, o)| (*d, *o))
    }

    /// Default method `decl` of interface `owner` (an instance, for a generic
    /// one), running for `self_ty`: a copy of its code with `Self` = `self_ty`
    /// (its own type parameters still to be picked at the call).
    pub(super) fn default_instance(&mut self, decl: FuncId, owner: StructId, self_ty: Type) -> FuncId {
        let tps = self.funcs[decl].type_params.clone();
        let mut args = self.struct_args[owner].clone();
        let n = args.len();
        args.push(self_ty);
        args.extend(tps[n + 1..].iter().map(|p| Type::Param(*p)));
        self.fn_instance(decl, args)
    }

    /// A call of default method `decl` (of interface `owner`) on `recv`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn default_call(&mut self, recv: TExpr, decl: FuncId, owner: StructId, method: &Ident, args: &[Arg], explicit: Option<&[TypeExpr]>, span: Span) -> TExpr {
        let fid = self.default_instance(decl, owner, recv.ty.clone());
        let code = self.ide.functions[decl].signature.clone();
        let doc = format!("default method of interface `{}`", self.structs[owner].name);
        self.hover(method.span, code, Some(doc));
        self.ide.definitions.push((method.span, self.funcs[decl].span));
        let what = format!("method `{}`", method.name);
        self.call_fn(fid, &what, Some(recv), args, explicit, None, method.span, span)
    }

    /// A default method `name` that type `t` gets from an interface: one it
    /// says it fits (`struct Circle(Shape):`), the bound of a type parameter
    /// that stands for it in the generic code being compiled, or the
    /// interface of the method table being built.
    pub(super) fn default_for(&mut self, t: &Type, name: &str) -> Option<(FuncId, StructId)> {
        let mut cands: Vec<StructId> = self.extra_default_ifaces.clone();
        if let Some(f) = self.cur_fn
            && let Some((decl, args)) = self.funcs[f].instance.clone()
        {
            let tps = self.funcs[decl].type_params.clone();
            let map: Vec<(ParamId, Type)> = tps.iter().copied().zip(args.iter().cloned()).collect();
            for (p, a) in tps.iter().zip(&args) {
                if a == t {
                    for (b, _) in self.type_params[*p].bounds.clone() {
                        if let Type::Iface(x) = self.subst(&Type::Iface(b), &map) {
                            cands.push(x);
                        }
                    }
                }
            }
        }
        if let Type::Struct(sid) | Type::Enum(sid, _) = t {
            let origin = self.struct_origin[*sid];
            let map: Vec<(ParamId, Type)> = self.struct_params[origin].iter().copied().zip(self.struct_args[*sid].clone()).collect();
            for i in self.declared_ifaces.get(&origin).cloned().unwrap_or_default() {
                if let Type::Iface(x) = self.subst(&Type::Iface(i), &map) {
                    cands.push(x);
                }
            }
        }
        cands.into_iter().find_map(|i| self.iface_default(i, name))
    }
}

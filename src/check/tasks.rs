//! Tasks: `spawn(fn() -> ...)`, and the standard library's intrinsics that
//! carry values between tasks (`__chan_send`, `__mutex_lock`...: see
//! `tast::SyncOp`). The types `Task`, `Channel`, `Mutex` and `Atomic` are
//! written in the prelude around a `__Handle` (`Type::Handle`).
//!
//! Tasks share nothing but those handles: a value given to another task is
//! moved or copied (the generated `tarn.xfer` functions). So a function given
//! to `spawn`, or run while a `Mutex` is held, can't capture a `var`: the
//! variable would be shared between two tasks.

use super::*;

impl Checker {
    /// `spawn(f)`: start a task running `f` (a function without parameters),
    /// giving a `Task[T]` of what it returns (`Unit` when it returns nothing).
    pub(super) fn spawn_call(&mut self, args: &[Arg], span: Span) -> TExpr {
        if !self.arity(args, 1, "`spawn`", span) {
            return Self::error_expr(span);
        }
        let f = self.expr(&args[0].value, None);
        let ret = match &f.ty {
            Type::Func(ps, r) if ps.is_empty() => (**r).clone(),
            Type::Error => return Self::error_expr(span),
            other => {
                let tn = self.tn(other);
                self.err(
                    Diagnostic::error(format!("`spawn` takes a function without parameters, not {}", a_type(&tn)), f.span)
                        .with_primary(format!("this is `{tn}`"))
                        .with_help("give it a lambda: `spawn(fn() -> work(x))`"),
                );
                return Self::error_expr(span);
            }
        };
        self.no_shared_vars(&f, "a task");
        let (Some(task), Some(unit)) = (self.prelude_type("Task"), self.prelude_type("Unit")) else { return Self::error_expr(span) };
        let t = if ret == Type::Unit { self.def_types[unit].clone() } else { ret.clone() };
        let Some(inst) = self.struct_instance(task, vec![t]) else { return Self::error_expr(span) };
        let h = Self::mk(TExprKind::Sync(SyncOp::Spawn, vec![ret], vec![f]), Type::Handle, span);
        Self::mk(TExprKind::StructNew(inst, vec![h]), self.def_types[inst].clone(), span)
    }

    /// A function value that runs in another task, or while a `Mutex` is
    /// held, can't capture a `var` (two tasks would share it).
    pub(super) fn no_shared_vars(&mut self, f: &TExpr, runs_in: &str) {
        let TExprKind::Closure { sid: Some(sid), captures, .. } = &f.kind else { return };
        for (i, c) in captures.iter().enumerate() {
            let Type::Struct(cell) = c.ty else { continue };
            if !self.structs[cell].name.starts_with("<var ") {
                continue;
            }
            let name = self.structs[*sid].fields[i].name.clone();
            self.err(
                Diagnostic::error(format!("{runs_in} can't share `{name}`, a `var`, with the code around it"), f.span)
                    .with_primary(format!("this function uses `{name}`"))
                    .with_note("tasks share no variables: what a task gets is moved or copied to it, so its changes wouldn't show up here")
                    .with_help(format!("keep `{name}` in an `Atomic` or a `Mutex`, or send the result back (`join()`, a channel)")),
            );
        }
    }

    /// The standard library's intrinsics that carry values of any type
    /// between tasks: `__chan_send(h, value)`, `__chan_receive[T](h)`...
    pub(super) fn sync_intrinsic(&mut self, name: &str, explicit: Option<&[TypeExpr]>, args: &[Arg], span: Span) -> TExpr {
        let ex: Vec<Type> = explicit.map(|e| e.iter().map(|t| self.resolve_type(t)).collect()).unwrap_or_default();
        let vals: Vec<TExpr> = args.iter().map(|a| self.expr(&a.value, None)).collect();
        let arg_ty = |i: usize| vals.get(i).map(|v| v.ty.clone()).unwrap_or(Type::Error);
        let given = |i: usize| ex.get(i).cloned().unwrap_or(Type::Error);
        let func = |t: Type| match t {
            Type::Func(ps, r) if ps.len() == 1 => (ps[0].clone(), *r),
            _ => (Type::Error, Type::Error),
        };
        let (op, types, ty) = match name {
            "__task_take" => (SyncOp::TaskTake, vec![given(0)], given(0)),
            "__chan_new" => (SyncOp::ChanNew, vec![given(0)], Type::Handle),
            "__chan_send" => (SyncOp::ChanSend, vec![arg_ty(1)], Type::Unit),
            "__chan_receive" => (SyncOp::ChanReceive, vec![given(0)], given(0).nullable()),
            "__mutex_new" => (SyncOp::MutexNew, vec![arg_ty(0)], Type::Handle),
            "__mutex_lock" => {
                let (t, r) = func(arg_ty(1));
                (SyncOp::MutexLock, vec![t, r.clone()], r)
            }
            "__mutex_update" => (SyncOp::MutexUpdate, vec![func(arg_ty(1)).0], Type::Unit),
            "__mutex_get" => (SyncOp::MutexGet, vec![given(0)], given(0)),
            _ => (SyncOp::MutexSet, vec![arg_ty(1)], Type::Unit),
        };
        Self::mk(TExprKind::Sync(op, types, vals), ty, span)
    }

    /// Is `fid` a method of the prelude's `Mutex` that runs a function while
    /// the Mutex is held (`lock`, `update`)?
    pub(super) fn runs_while_locked(&self, fid: FuncId) -> bool {
        let decl = self.funcs[fid].instance.as_ref().map(|(d, _)| *d).unwrap_or(fid);
        let f = &self.ide.functions[decl];
        f.owner.is_some_and(|o| Some(o) == self.prelude_type("Mutex")) && matches!(f.name.as_str(), "lock" | "update")
    }

    /// Type parameters of generic function `fid` that may stand for "nothing"
    /// (a function returning nothing): those of a standard-library function
    /// that appear only as what it, or a function passed to it, returns.
    /// (`Mutex.lock[R]` with `fn(m) -> m.clear()`.)
    pub(super) fn unit_params(&self, fid: FuncId) -> Vec<ParamId> {
        let decl = self.funcs[fid].instance.as_ref().map(|(d, _)| *d).unwrap_or(fid);
        if self.ns_kinds[self.ide.functions[decl].ns] != FileKind::Std {
            return Vec::new();
        }
        let sig = &self.funcs[fid];
        sig.tparams
            .iter()
            .copied()
            .filter(|&p| {
                // Mentioned only as a return: the result, or a function's result.
                let ok_ret = sig.ret == Type::Param(p) || !self.mentions(&sig.ret, p);
                let ok_params = sig.params.iter().all(|(_, t)| match t {
                    Type::Func(ps, r) => !ps.iter().any(|x| self.mentions(x, p)) && (**r == Type::Param(p) || !self.mentions(r, p)),
                    t => !self.mentions(t, p),
                });
                ok_ret && ok_params
            })
            .collect()
    }
}

//! Typed AST → MIR.
//!
//! Nested expressions are flattened into named temporaries (A-normal form)
//! and structured control flow becomes an explicit CFG. Evaluation order is
//! exactly the one the language guarantees (left to right; for assignments
//! the value first, then the place).

use crate::ast::BinOp;
use crate::mir::*;
use crate::tast::{self, Builtin, TExpr, TExprKind, TFunc, TPat, TPatKind, TProgram, TStmt};
use crate::types::*;

pub fn lower(prog: &TProgram) -> Program {
    let funcs = prog.funcs.iter().map(|f| Lower::func(prog, f)).collect();
    Program { structs: prog.structs.clone(), funcs }
}

struct Lower<'a> {
    prog: &'a TProgram,
    vars: Vec<Var>,
    /// Blocks under construction: statements + terminator (once known).
    blocks: Vec<(Vec<Stmt>, Option<Term>)>,
    cur: BlockId,
    /// (continue target, break target)
    loops: Vec<(BlockId, BlockId)>,
    /// Locals that are assigned after their declaration (`var`s that change).
    reassigned: Vec<bool>,
}

impl<'a> Lower<'a> {
    fn func(prog: &'a TProgram, f: &TFunc) -> Func {
        let vars = f
            .locals
            .iter()
            .enumerate()
            .map(|(id, l)| Var {
                name: l.name.clone(),
                ty: l.ty.clone(),
                kind: if f.params.contains(&id) { VarKind::Param } else { VarKind::Local },
            })
            .collect();
        let mut reassigned = vec![false; f.locals.len()];
        mark_reassigned(&f.body, &mut reassigned);
        let mut l = Lower { prog, vars, blocks: vec![(Vec::new(), None)], cur: 0, loops: Vec::new(), reassigned };
        l.stmts(&f.body);
        if !l.terminated() {
            l.term(if f.ret == Type::Unit { Term::Return(None) } else { Term::Unreachable });
        }
        let blocks = prune(l.blocks);
        Func {
            name: f.name.clone(),
            symbol: f.symbol.clone(),
            params: f.params.clone(),
            param_owned: vec![false; f.params.len()],
            ret: f.ret.clone(),
            vars: l.vars,
            blocks,
            exported: f.exported,
            track_caller: f.track_caller,
        }
    }

    // ---------- building blocks ----------

    fn new_block(&mut self) -> BlockId {
        self.blocks.push((Vec::new(), None));
        self.blocks.len() - 1
    }

    fn terminated(&self) -> bool {
        self.blocks[self.cur].1.is_some()
    }

    /// Code after a `return`/`break`/... goes into a fresh (unreachable) block.
    fn ensure_open(&mut self) {
        if self.terminated() {
            self.cur = self.new_block();
        }
    }

    fn push(&mut self, s: Stmt) {
        self.ensure_open();
        self.blocks[self.cur].0.push(s);
    }

    fn term(&mut self, t: Term) {
        self.ensure_open();
        self.blocks[self.cur].1 = Some(t);
    }

    /// Fall through to `target` unless the current block already ended.
    fn goto(&mut self, target: BlockId) {
        if !self.terminated() {
            self.blocks[self.cur].1 = Some(Term::Goto(target));
        }
    }

    /// Continue building in `b` (falling through into it if needed).
    fn start(&mut self, b: BlockId) {
        self.goto(b);
        self.cur = b;
    }

    fn temp(&mut self, ty: Type) -> VarId {
        self.vars.push(Var { name: String::new(), ty, kind: VarKind::Temp });
        self.vars.len() - 1
    }

    // ---------- statements ----------

    fn stmts(&mut self, stmts: &[TStmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &TStmt) {
        match s {
            TStmt::Let(id, e) => {
                let r = self.rvalue(e);
                self.push(Stmt::Assign(*id, r));
            }
            TStmt::Assign { place, op, value, span } => {
                // The value is evaluated first (it may e.g. grow a list).
                let v = self.operand(value);
                match place {
                    tast::Place::Local(id) => {
                        let r = match op {
                            None => Rvalue::Use(v),
                            Some(op) => {
                                let ty = self.vars[*id].ty.clone();
                                let t = self.temp(ty.clone());
                                self.push(Stmt::Assign(t, Rvalue::Binary(*op, ty, Operand::Var(*id), v, *span)));
                                Rvalue::Use(Operand::Var(t))
                            }
                        };
                        self.push(Stmt::Assign(*id, r));
                    }
                    tast::Place::Field { obj, sid, idx } => {
                        let o = self.operand(obj);
                        let o = self.as_var(o, &obj.ty);
                        let ty = self.prog.structs[*sid].fields[*idx].ty.clone();
                        let v = match op {
                            None => v,
                            Some(op) => {
                                let cur = self.temp(ty.clone());
                                self.push(Stmt::Assign(cur, Rvalue::Field(o, *sid, *idx)));
                                let new = self.temp(ty.clone());
                                self.push(Stmt::Assign(new, Rvalue::Binary(*op, ty.clone(), Operand::Var(cur), v, *span)));
                                Operand::Var(new)
                            }
                        };
                        let old = is_ref(&ty).then(|| self.temp(ty.clone()));
                        self.push(Stmt::Store { place: Place::Field { obj: o, sid: *sid, idx: *idx }, value: v, old });
                    }
                    tast::Place::Index { list, index, span: ispan } => {
                        let ty = match &list.ty {
                            Type::List(t) => (**t).clone(),
                            _ => unreachable!(),
                        };
                        let l = self.operand(list);
                        let l = self.as_var(l, &list.ty);
                        let i = self.operand(index);
                        let v = match op {
                            None => v,
                            Some(op) => {
                                let cur = self.temp(ty.clone());
                                self.push(Stmt::Assign(cur, Rvalue::Index(l, i.clone(), Some(*ispan))));
                                let new = self.temp(ty.clone());
                                self.push(Stmt::Assign(new, Rvalue::Binary(*op, ty.clone(), Operand::Var(cur), v, *span)));
                                Operand::Var(new)
                            }
                        };
                        let old = is_ref(&ty).then(|| self.temp(ty.clone()));
                        self.push(Stmt::Store { place: Place::Index { list: l, index: i, span: *ispan }, value: v, old });
                    }
                }
            }
            TStmt::Expr(e) => self.effect(e),
            TStmt::If { branches, else_block } => {
                let end = self.new_block();
                for (cond, body) in branches {
                    let c = self.operand(cond);
                    let then = self.new_block();
                    let next = self.new_block();
                    self.term(Term::Branch(c, then, next));
                    self.cur = then;
                    self.stmts(body);
                    self.goto(end);
                    self.cur = next;
                }
                if let Some(b) = else_block {
                    self.stmts(b);
                }
                self.start(end);
            }
            TStmt::While { cond, body } => {
                let lc = self.new_block();
                let lb = self.new_block();
                let le = self.new_block();
                self.start(lc);
                let c = self.operand(cond);
                self.term(Term::Branch(c, lb, le));
                self.cur = lb;
                self.loops.push((lc, le));
                self.stmts(body);
                self.loops.pop();
                self.goto(lc);
                self.cur = le;
            }
            TStmt::ForRange { var, start, end, inclusive, body } => {
                let s = self.operand(start);
                // The bounds are evaluated once, even if the body reassigns a variable used in them.
                let e = self.operand(end);
                let e = self.snapshot(e, Type::Int);
                let ctr = self.temp(Type::Int);
                self.push(Stmt::Assign(ctr, Rvalue::Use(s)));
                let lc = self.new_block();
                let lb = self.new_block();
                let ls = self.new_block();
                let le = self.new_block();
                self.start(lc);
                let c = self.temp(Type::Bool);
                let cmp = if *inclusive { BinOp::Le } else { BinOp::Lt };
                self.push(Stmt::Assign(c, Rvalue::Binary(cmp, Type::Int, Operand::Var(ctr), e.clone(), Default::default())));
                self.term(Term::Branch(Operand::Var(c), lb, le));
                self.cur = lb;
                self.push(Stmt::Assign(*var, Rvalue::Use(Operand::Var(ctr))));
                self.loops.push((ls, le));
                self.stmts(body);
                self.loops.pop();
                self.start(ls);
                if *inclusive {
                    // Stop before incrementing past `end` (avoids overflow at Int max).
                    let at_end = self.temp(Type::Bool);
                    self.push(Stmt::Assign(at_end, Rvalue::Binary(BinOp::Eq, Type::Int, Operand::Var(ctr), e, Default::default())));
                    let inc = self.new_block();
                    self.term(Term::Branch(Operand::Var(at_end), le, inc));
                    self.cur = inc;
                }
                self.push(Stmt::Assign(ctr, Rvalue::Builtin(Builtin::Inc, vec![Operand::Var(ctr)], Default::default())));
                self.goto(lc);
                self.cur = le;
            }
            TStmt::ForList { var, list, body } => {
                let l = self.operand(list);
                // Iterate over the list the expression evaluated to, even if the body rebinds the variable.
                let l = match l {
                    Operand::Var(v) if self.vars[v].kind != VarKind::Temp && self.reassigned[v] => {
                        let t = self.temp(list.ty.clone());
                        self.push(Stmt::Assign(t, Rvalue::Use(Operand::Var(v))));
                        t
                    }
                    other => self.as_var(other, &list.ty),
                };
                let ctr = self.temp(Type::Int);
                self.push(Stmt::Assign(ctr, Rvalue::Use(Operand::Int(0))));
                let lc = self.new_block();
                let lb = self.new_block();
                let ls = self.new_block();
                let le = self.new_block();
                self.start(lc);
                let len = self.temp(Type::Int);
                self.push(Stmt::Assign(len, Rvalue::Builtin(Builtin::ListLen, vec![Operand::Var(l)], Default::default())));
                let c = self.temp(Type::Bool);
                self.push(Stmt::Assign(c, Rvalue::Binary(BinOp::Lt, Type::Int, Operand::Var(ctr), Operand::Var(len), Default::default())));
                self.term(Term::Branch(Operand::Var(c), lb, le));
                self.cur = lb;
                self.push(Stmt::Assign(*var, Rvalue::Index(l, Operand::Var(ctr), None)));
                self.loops.push((ls, le));
                self.stmts(body);
                self.loops.pop();
                self.start(ls);
                self.push(Stmt::Assign(ctr, Rvalue::Builtin(Builtin::Inc, vec![Operand::Var(ctr)], Default::default())));
                self.goto(lc);
                self.cur = le;
            }
            TStmt::Match { scrutinee, arms } => {
                // The value is evaluated once; then each case is tried in order.
                let v = self.operand(scrutinee);
                let v = self.as_var(v, &scrutinee.ty);
                let end = self.new_block();
                for arm in arms {
                    let next = self.new_block();
                    self.pattern(&arm.pat, v, next);
                    if let Some(g) = &arm.guard {
                        let c = self.operand(g);
                        let body = self.new_block();
                        self.term(Term::Branch(c, body, next));
                        self.cur = body;
                    }
                    self.stmts(&arm.body);
                    self.goto(end);
                    self.cur = next;
                }
                // The checker proved the cases exhaustive.
                self.term(Term::Unreachable);
                self.cur = end;
            }
            TStmt::Break => {
                let target = self.loops.last().expect("break outside loop").1;
                self.term(Term::Goto(target));
            }
            TStmt::Continue => {
                let target = self.loops.last().expect("continue outside loop").0;
                self.term(Term::Goto(target));
            }
            TStmt::Return(None) => self.term(Term::Return(None)),
            TStmt::Return(Some(e)) => {
                if e.ty == Type::Unit {
                    self.effect(e);
                    self.term(Term::Return(None));
                } else {
                    let o = self.operand(e);
                    self.term(Term::Return(Some(o)));
                }
            }
        }
    }

    /// An expression evaluated for its side effects.
    fn effect(&mut self, e: &TExpr) {
        let noreturn = matches!(e.kind, TExprKind::Builtin(Builtin::Panic | Builtin::Exit, _));
        let r = self.rvalue(e);
        match r {
            Rvalue::Use(_) => {}
            // A discarded non-unit value still needs a home (it may own memory).
            r if e.ty != Type::Unit => {
                let t = self.temp(e.ty.clone());
                self.push(Stmt::Assign(t, r));
            }
            r => self.push(Stmt::Eval(r)),
        }
        if noreturn {
            self.term(Term::Unreachable);
        }
    }

    // ---------- patterns ----------

    /// Test pattern `p` against variable `v`: jump to `fail` if it doesn't
    /// match, otherwise continue in the current block with its names bound.
    fn pattern(&mut self, p: &TPat, v: VarId, fail: BlockId) {
        match &p.kind {
            TPatKind::Wild => {}
            TPatKind::Bind(id, inner) => {
                self.pattern(inner, v, fail);
                self.push(Stmt::Assign(*id, Rvalue::Use(Operand::Var(v))));
            }
            TPatKind::Int(n) => self.test(Rvalue::Binary(BinOp::Eq, Type::Int, Operand::Var(v), Operand::Int(*n), Default::default()), fail),
            TPatKind::Str(s) => self.test(Rvalue::Binary(BinOp::Eq, Type::Str, Operand::Var(v), Operand::Str(s.clone()), Default::default()), fail),
            TPatKind::Bool(b) => {
                let ok = self.new_block();
                let (t, f) = if *b { (ok, fail) } else { (fail, ok) };
                self.term(Term::Branch(Operand::Var(v), t, f));
                self.cur = ok;
            }
            TPatKind::Null => {
                let c = self.is_null(Operand::Var(v), &p.ty);
                let ok = self.new_block();
                self.term(Term::Branch(c, ok, fail));
                self.cur = ok;
            }
            TPatKind::Some(inner) => {
                let c = self.is_null(Operand::Var(v), &p.ty);
                let ok = self.new_block();
                self.term(Term::Branch(c, fail, ok));
                self.cur = ok;
                let u = self.unwrap(Operand::Var(v), &p.ty);
                let t = self.temp(inner.ty.clone());
                self.push(Stmt::Assign(t, u));
                self.pattern(inner, t, fail);
            }
            TPatKind::Variant(sid, k, subs) => {
                if self.prog.structs[*sid].variants().len() > 1 {
                    let tag = self.temp(Type::Int);
                    self.push(Stmt::Assign(tag, Rvalue::Tag(v)));
                    self.test(Rvalue::Binary(BinOp::Eq, Type::Int, Operand::Var(tag), Operand::Int(*k as i64), Default::default()), fail);
                }
                for (i, sub) in subs.iter().enumerate() {
                    if !matches!(sub.kind, TPatKind::Wild) {
                        let f = self.temp(sub.ty.clone());
                        self.push(Stmt::Assign(f, Rvalue::VariantField(v, *sid, *k, i)));
                        self.pattern(sub, f, fail);
                    }
                }
            }
            TPatKind::Struct(sid, subs) => {
                for (i, sub) in subs.iter().enumerate() {
                    if !matches!(sub.kind, TPatKind::Wild) {
                        let f = self.temp(sub.ty.clone());
                        self.push(Stmt::Assign(f, Rvalue::Field(v, *sid, i)));
                        self.pattern(sub, f, fail);
                    }
                }
            }
            TPatKind::Or(alts) => {
                let ok = self.new_block();
                for (j, alt) in alts.iter().enumerate() {
                    let last = j + 1 == alts.len();
                    let next = if last { fail } else { self.new_block() };
                    self.pattern(alt, v, next);
                    self.goto(ok);
                    self.cur = next;
                }
                self.cur = ok;
            }
        }
    }

    /// Continue if `cond` (a `Bool`) holds, else jump to `fail`.
    fn test(&mut self, cond: Rvalue, fail: BlockId) {
        let c = self.temp(Type::Bool);
        self.push(Stmt::Assign(c, cond));
        let ok = self.new_block();
        self.term(Term::Branch(Operand::Var(c), ok, fail));
        self.cur = ok;
    }

    // ---------- expressions ----------

    /// A local variable's current value, copied so later reassignments don't affect it.
    fn snapshot(&mut self, o: Operand, ty: Type) -> Operand {
        match o {
            Operand::Var(v) if self.vars[v].kind != VarKind::Temp && self.reassigned[v] => {
                let t = self.temp(ty);
                self.push(Stmt::Assign(t, Rvalue::Use(Operand::Var(v))));
                Operand::Var(t)
            }
            other => other,
        }
    }

    fn as_var(&mut self, o: Operand, ty: &Type) -> VarId {
        match o {
            Operand::Var(v) => v,
            other => {
                let t = self.temp(ty.clone());
                self.push(Stmt::Assign(t, Rvalue::Use(other)));
                t
            }
        }
    }

    fn operand(&mut self, e: &TExpr) -> Operand {
        match self.rvalue(e) {
            Rvalue::Use(o) => self.retype(o, &e.ty),
            r => {
                let t = self.temp(e.ty.clone());
                self.push(Stmt::Assign(t, r));
                Operand::Var(t)
            }
        }
    }

    /// `o` as a value of type `ty`. Nullable and non-null references are the
    /// same pointer, but each MIR variable has one type, so a variable of
    /// the other type gets an alias (a borrowed copy: no counting).
    fn retype(&mut self, o: Operand, ty: &Type) -> Operand {
        match o {
            Operand::Var(v) if self.vars[v].ty != *ty => {
                let t = self.temp(ty.clone());
                self.push(Stmt::Assign(t, Rvalue::Use(Operand::Var(v))));
                Operand::Var(t)
            }
            other => other,
        }
    }

    /// Is a value of type `ty` (a `T?`) null? Always false for non-null types.
    fn is_null(&mut self, o: Operand, ty: &Type) -> Operand {
        if !ty.is_nullable() {
            return Operand::Bool(false);
        }
        let t = self.temp(Type::Bool);
        self.push(Stmt::Assign(t, Rvalue::Builtin(Builtin::IsNull, vec![o], Default::default())));
        Operand::Var(t)
    }

    /// The `T` inside a non-null `T?` (`ty` is the type of `o`).
    fn unwrap(&mut self, o: Operand, ty: &Type) -> Rvalue {
        if !ty.is_nullable() || is_ref(ty) {
            Rvalue::Use(o)
        } else {
            Rvalue::Builtin(Builtin::Unwrap, vec![o], Default::default())
        }
    }

    fn unwrap_operand(&mut self, o: Operand, ty: &Type) -> Operand {
        match self.unwrap(o, ty) {
            Rvalue::Use(o) => o,
            r => {
                let t = self.temp(ty.non_null().clone());
                self.push(Stmt::Assign(t, r));
                Operand::Var(t)
            }
        }
    }

    /// A `T` as a `T?` (`ty` is the type of `o`; a `T?` stays as it is).
    fn wrap(&mut self, o: Operand, ty: &Type) -> Rvalue {
        if ty.is_nullable() || is_ref(ty) {
            Rvalue::Use(o)
        } else {
            Rvalue::Builtin(Builtin::Wrap, vec![o], Default::default())
        }
    }

    /// Branch on whether `o` (of nullable type `ty`) is null: returns (null block, non-null block).
    fn branch_null(&mut self, o: Operand, ty: &Type) -> (BlockId, BlockId) {
        let c = self.is_null(o, ty);
        let none = self.new_block();
        let some = self.new_block();
        self.term(Term::Branch(c, none, some));
        (none, some)
    }

    fn operands(&mut self, es: &[TExpr]) -> Vec<Operand> {
        es.iter().map(|e| self.operand(e)).collect()
    }

    fn rvalue(&mut self, e: &TExpr) -> Rvalue {
        match &e.kind {
            TExprKind::Int(v) => Rvalue::Use(Operand::Int(*v)),
            TExprKind::Float(v) => Rvalue::Use(Operand::Float(*v)),
            TExprKind::Bool(v) => Rvalue::Use(Operand::Bool(*v)),
            TExprKind::Str(s) => Rvalue::Use(Operand::Str(s.clone())),
            TExprKind::Local(id) => Rvalue::Use(Operand::Var(*id)),
            TExprKind::Call(fid, args) => {
                let a = self.operands(args);
                Rvalue::Call(*fid, a, e.span)
            }
            // `s.to_string()` on a String is the string itself.
            TExprKind::Builtin(Builtin::ToStr, args) if args[0].ty == Type::Str => Rvalue::Use(self.operand(&args[0])),
            TExprKind::Builtin(b, args) => {
                let a = self.operands(args);
                Rvalue::Builtin(*b, a, e.span)
            }
            TExprKind::Binary(op, l, r) if l.ty.is_nullable() => {
                // `==` / `!=` on nullable primitives: equal when both are null,
                // or neither is and the values are equal.
                let a = self.operand(l);
                let b = self.operand(r);
                let res = self.temp(Type::Bool);
                let b_null = self.is_null(b.clone(), &r.ty);
                let (a_none, a_some) = self.branch_null(a.clone(), &l.ty);
                let end = self.new_block();
                self.cur = a_none;
                let same = if *op == BinOp::Eq { Rvalue::Use(b_null.clone()) } else { Rvalue::Not(b_null.clone()) };
                self.push(Stmt::Assign(res, same));
                self.goto(end);
                self.cur = a_some;
                let (b_none, both) = (self.new_block(), self.new_block());
                self.term(Term::Branch(b_null, b_none, both));
                self.cur = b_none;
                self.push(Stmt::Assign(res, Rvalue::Use(Operand::Bool(*op == BinOp::Ne))));
                self.goto(end);
                self.cur = both;
                let x = self.unwrap_operand(a, &l.ty);
                let y = self.unwrap_operand(b, &r.ty);
                self.push(Stmt::Assign(res, Rvalue::Binary(*op, l.ty.non_null().clone(), x, y, e.span)));
                self.start(end);
                Rvalue::Use(Operand::Var(res))
            }
            TExprKind::Binary(op, l, r) => {
                let a = self.operand(l);
                let b = self.operand(r);
                Rvalue::Binary(*op, l.ty.clone(), a, b, e.span)
            }
            TExprKind::Neg(inner) => {
                let v = self.operand(inner);
                Rvalue::Neg(inner.ty.clone(), v, e.span)
            }
            TExprKind::Not(inner) => {
                let v = self.operand(inner);
                Rvalue::Not(v)
            }
            TExprKind::And(l, r) | TExprKind::Or(l, r) => {
                let is_and = matches!(e.kind, TExprKind::And(..));
                let a = self.operand(l);
                let res = self.temp(Type::Bool);
                self.push(Stmt::Assign(res, Rvalue::Use(a.clone())));
                let rhs = self.new_block();
                let end = self.new_block();
                let (t, f) = if is_and { (rhs, end) } else { (end, rhs) };
                self.term(Term::Branch(a, t, f));
                self.cur = rhs;
                let b = self.operand(r);
                self.push(Stmt::Assign(res, Rvalue::Use(b)));
                self.start(end);
                Rvalue::Use(Operand::Var(res))
            }
            TExprKind::Field(obj, sid, idx) => {
                let o = self.operand(obj);
                let o = self.as_var(o, &obj.ty);
                Rvalue::Field(o, *sid, *idx)
            }
            TExprKind::Index(list, index) => {
                let l = self.operand(list);
                let l = self.as_var(l, &list.ty);
                let i = self.operand(index);
                Rvalue::Index(l, i, Some(e.span))
            }
            TExprKind::StructNew(sid, args) => {
                let a = self.operands(args);
                Rvalue::New(Ctor::Struct(*sid), a)
            }
            TExprKind::EnumNew(sid, k, args) => {
                let a = self.operands(args);
                Rvalue::New(Ctor::Variant(*sid, *k), a)
            }
            TExprKind::ListLit(elems) => {
                let elem = match &e.ty {
                    Type::List(t) => (**t).clone(),
                    _ => unreachable!(),
                };
                let a = self.operands(elems);
                Rvalue::ListNew(elem, a)
            }
            TExprKind::FString(parts) => {
                let mut acc: Option<Operand> = None;
                for p in parts {
                    let v = self.operand(p);
                    acc = Some(match acc {
                        None => v,
                        Some(prev) => {
                            let t = self.temp(Type::Str);
                            self.push(Stmt::Assign(t, Rvalue::Binary(BinOp::Add, Type::Str, prev, v, p.span)));
                            Operand::Var(t)
                        }
                    });
                }
                Rvalue::Use(acc.unwrap_or(Operand::Str(String::new())))
            }
            TExprKind::Null => Rvalue::Use(Operand::Null(e.ty.clone())),
            // For references `T` and `T?` are the same pointer: a computed value
            // (a call, a new struct, a field) is simply stored under the new type.
            TExprKind::Wrap(inner) | TExprKind::Unwrap(inner) if is_ref(&inner.ty) => match self.rvalue(inner) {
                Rvalue::Use(o) => Rvalue::Use(o),
                r => r,
            },
            TExprKind::Wrap(inner) => {
                let o = self.operand(inner);
                self.wrap(o, &inner.ty)
            }
            TExprKind::Unwrap(inner) => {
                let o = self.operand(inner);
                self.unwrap(o, &inner.ty)
            }
            TExprKind::IsNull(inner) => {
                let o = self.operand(inner);
                Rvalue::Use(self.is_null(o, &inner.ty))
            }
            TExprKind::NotNull(inner) => {
                let o = self.operand(inner);
                let (none, some) = self.branch_null(o.clone(), &inner.ty);
                self.cur = none;
                let msg = Operand::Str("unwrapped a null value with `!!`".into());
                self.push(Stmt::Eval(Rvalue::Builtin(Builtin::Panic, vec![msg], e.span)));
                self.term(Term::Unreachable);
                self.cur = some;
                self.unwrap(o, &inner.ty)
            }
            TExprKind::SafeCall { recv, binder, body } => {
                let r = self.operand(recv);
                let res = (e.ty != Type::Unit).then(|| self.temp(e.ty.clone()));
                let (none, some) = self.branch_null(r.clone(), &recv.ty);
                let end = self.new_block();
                self.cur = none;
                if let Some(res) = res {
                    self.push(Stmt::Assign(res, Rvalue::Use(Operand::Null(e.ty.clone()))));
                }
                self.goto(end);
                self.cur = some;
                let it = self.unwrap(r, &recv.ty);
                self.push(Stmt::Assign(*binder, it));
                match res {
                    Some(res) => {
                        let v = self.operand(body);
                        let w = self.wrap(v, &body.ty);
                        self.push(Stmt::Assign(res, w));
                    }
                    None => self.effect(body),
                }
                self.start(end);
                match res {
                    Some(res) => Rvalue::Use(Operand::Var(res)),
                    None => Rvalue::Use(Operand::Bool(false)), // (a unit value; never used)
                }
            }
            TExprKind::Elvis(l, r) => {
                let a = self.operand(l);
                let (none, some) = self.branch_null(a.clone(), &l.ty);
                if r.diverges() {
                    self.cur = none;
                    match &r.kind {
                        TExprKind::Jump(s) => self.stmt(s),
                        _ => self.effect(r),
                    }
                    if !self.terminated() {
                        self.term(Term::Unreachable);
                    }
                    self.cur = some;
                    return self.unwrap(a, &l.ty);
                }
                let res = self.temp(e.ty.clone());
                let end = self.new_block();
                self.cur = none;
                let v = self.operand(r);
                self.push(Stmt::Assign(res, Rvalue::Use(v)));
                self.goto(end);
                self.cur = some;
                let u = if e.ty.is_nullable() { Rvalue::Use(a) } else { self.unwrap(a, &l.ty) };
                self.push(Stmt::Assign(res, u));
                self.start(end);
                Rvalue::Use(Operand::Var(res))
            }
            TExprKind::Jump(s) => {
                self.stmt(s);
                Rvalue::Use(Operand::Bool(false))
            }
        }
    }
}

fn mark_reassigned(stmts: &[TStmt], out: &mut [bool]) {
    for s in stmts {
        match s {
            TStmt::Assign { place: tast::Place::Local(id), .. } => out[*id] = true,
            TStmt::If { branches, else_block } => {
                for (_, b) in branches {
                    mark_reassigned(b, out);
                }
                if let Some(b) = else_block {
                    mark_reassigned(b, out);
                }
            }
            TStmt::While { body, .. } | TStmt::ForRange { body, .. } | TStmt::ForList { body, .. } => mark_reassigned(body, out),
            TStmt::Match { arms, .. } => {
                for a in arms {
                    mark_reassigned(&a.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Drop blocks that can't be reached from the entry and renumber the rest.
fn prune(blocks: Vec<(Vec<Stmt>, Option<Term>)>) -> Vec<Block> {
    let n = blocks.len();
    let mut reachable = vec![false; n];
    let mut stack = vec![0];
    while let Some(b) = stack.pop() {
        if reachable[b] {
            continue;
        }
        reachable[b] = true;
        if let Some(t) = &blocks[b].1 {
            stack.extend(t.successors());
        }
    }
    let mut new_id = vec![usize::MAX; n];
    let mut next = 0;
    for b in 0..n {
        if reachable[b] {
            new_id[b] = next;
            next += 1;
        }
    }
    blocks
        .into_iter()
        .enumerate()
        .filter(|(b, _)| reachable[*b])
        .map(|(_, (stmts, term))| {
            let mut term = term.unwrap_or(Term::Unreachable);
            for s in term.successors_mut() {
                *s = new_id[*s];
            }
            Block { stmts, term }
        })
        .collect()
}

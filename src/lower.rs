//! Typed AST → MIR.
//!
//! Nested expressions are flattened into named temporaries (A-normal form)
//! and structured control flow becomes an explicit CFG. Evaluation order is
//! exactly the one the language guarantees (left to right; for assignments
//! the value first, then the place).

use crate::ast::BinOp;
use crate::mir::*;
use crate::span::Span;
use crate::tast::{self, Builtin, TExpr, TExprKind, TFunc, TPat, TPatKind, TProgram, TStmt};
use crate::types::*;

pub fn lower(prog: &TProgram) -> Program {
    let funcs = prog.funcs.iter().map(|f| Lower::func(prog, f)).collect();
    Program { structs: prog.structs.clone(), funcs, map_keys: prog.map_keys.clone() }
}

struct Lower<'a> {
    prog: &'a TProgram,
    vars: Vec<Var>,
    /// Blocks under construction: statements + terminator (once known).
    blocks: Vec<(Vec<Stmt>, Option<Term>)>,
    cur: BlockId,
    /// (continue target, break target, how many `scopes` are outside the loop)
    loops: Vec<(BlockId, BlockId, usize)>,
    /// Per enclosing block: its variables holding values with a `fn drop`,
    /// kept alive to the end of the block (or a jump out of it).
    scopes: Vec<Vec<VarId>>,
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
        let mut l = Lower { prog, vars, blocks: vec![(Vec::new(), None)], cur: 0, loops: Vec::new(), scopes: Vec::new(), reassigned };
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
            generic: f.generic,
            closure: f.closure,
            is_drop: f.is_drop,
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

    /// A block of statements: also a scope for values with a `fn drop`.
    fn stmts(&mut self, stmts: &[TStmt]) {
        self.scoped(Vec::new(), |l| {
            for s in stmts {
                l.stmt(s);
                if let TStmt::Let(id, _) = s
                    && l.scoped_type(&l.vars[*id].ty)
                {
                    l.scopes.last_mut().unwrap().push(*id);
                }
            }
        });
    }

    /// Run `body` in a new scope that starts with variables `vars`; at its end,
    /// they're kept alive (when the end is reached).
    fn scoped(&mut self, vars: Vec<VarId>, body: impl FnOnce(&mut Self)) {
        self.scopes.push(vars);
        body(self);
        let frame = self.scopes.pop().unwrap();
        if !self.terminated() {
            for v in frame.into_iter().rev() {
                self.push(Stmt::KeepAlive(v));
            }
        }
    }

    /// Before jumping out of the scopes from `depth` on: keep their variables alive up to here.
    fn keep_alive_from(&mut self, depth: usize) {
        let vars: Vec<VarId> = self.scopes[depth..].iter().rev().flat_map(|f| f.iter().rev().copied()).collect();
        for v in vars {
            self.push(Stmt::KeepAlive(v));
        }
    }

    /// Values of this type live to the end of their variable's block: structs with a `fn drop`.
    fn scoped_type(&self, t: &Type) -> bool {
        matches!(t.non_null(), Type::Struct(sid) if self.prog.structs[*sid].drop.is_some())
    }

    /// The variables a pattern binds to values with a `fn drop`.
    fn scoped_binds(&self, p: &TPat, out: &mut Vec<VarId>) {
        match &p.kind {
            TPatKind::Bind(id, inner) => {
                if self.scoped_type(&self.vars[*id].ty) {
                    out.push(*id);
                }
                self.scoped_binds(inner, out);
            }
            TPatKind::Variant(_, _, subs) | TPatKind::Struct(_, subs) | TPatKind::Or(subs) => subs.iter().for_each(|s| self.scoped_binds(s, out)),
            TPatKind::Some(inner) => self.scoped_binds(inner, out),
            _ => {}
        }
    }

    fn stmt(&mut self, s: &TStmt) {
        match s {
            TStmt::Let(id, e) => {
                let r = self.rvalue(e);
                self.push(Stmt::Assign(*id, r));
            }
            // `s += f"line {i}\n"`: each part is appended to `s` in turn (in place
            // when `s` has one owner), instead of building the f-string first.
            TStmt::Assign { place: tast::Place::Local(id), op: Some(tast::CompoundOp::Builtin(BinOp::Add)), value, span }
                if self.vars[*id].ty == Type::Str
                    && matches!(&value.kind, TExprKind::FString(parts) if parts.iter().all(|p| independent_of(p, *id))) =>
            {
                let TExprKind::FString(parts) = &value.kind else { unreachable!() };
                for p in parts {
                    let v = self.operand(p);
                    self.push(Stmt::Assign(*id, Rvalue::Binary(BinOp::Add, Type::Str, Operand::Var(*id), v, *span)));
                }
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
                                Rvalue::Use(self.compound(*op, &ty, Operand::Var(*id), v, *span))
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
                                self.compound(*op, &ty, Operand::Var(cur), v, *span)
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
                                self.compound(*op, &ty, Operand::Var(cur), v, *span)
                            }
                        };
                        let old = is_ref(&ty).then(|| self.temp(ty.clone()));
                        self.push(Stmt::Store { place: Place::Index { list: l, index: i, span: *ispan }, value: v, old });
                    }
                    // `grid[i] = v` is `grid.[]=(i, v)`; `grid[i] += v` reads with `[]` first.
                    tast::Place::Custom { obj, index, get, set, ty } => {
                        let o = self.operand(obj);
                        let o = self.as_var(o, &obj.ty);
                        let i = self.operand(index);
                        let v = match (op, get) {
                            (None, _) => v,
                            (Some(op), Some(get)) => {
                                let cur = self.temp(ty.clone());
                                self.push(Stmt::Assign(cur, Rvalue::Call(*get, vec![Operand::Var(o), i.clone()], *span)));
                                self.compound(*op, ty, Operand::Var(cur), v, *span)
                            }
                            (Some(_), None) => unreachable!("checked: `x[i] op= v` needs `fn []`"),
                        };
                        self.push(Stmt::Eval(Rvalue::Call(*set, vec![Operand::Var(o), i, v], *span)));
                    }
                    tast::Place::MapEntry { map, key, span: kspan } => {
                        let Type::Map(_, vt) = &map.ty else { unreachable!() };
                        let vt = (**vt).clone();
                        let m = self.operand(map);
                        let m = self.as_var(m, &map.ty);
                        let k = self.operand(key);
                        let v = match op {
                            None => v,
                            Some(op) => {
                                let cur = self.temp(vt.clone());
                                self.push(Stmt::Assign(cur, Rvalue::Builtin(Builtin::MapGet, vec![Operand::Var(m), k.clone()], *kspan)));
                                self.compound(*op, &vt, Operand::Var(cur), v, *span)
                            }
                        };
                        self.push(Stmt::Eval(Rvalue::Builtin(Builtin::MapSet, vec![Operand::Var(m), k, v], *kspan)));
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
                self.loops.push((lc, le, self.scopes.len()));
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
                self.loops.push((ls, le, self.scopes.len()));
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
            TStmt::ForMap { key, value, map, body, span } => {
                let m = self.operand(map);
                let m = self.iterated(m, &map.ty);
                self.map_loop(m, *span, |l, i| {
                    l.push(Stmt::Assign(*key, Rvalue::Builtin(Builtin::MapKeyAt, vec![Operand::Var(m), Operand::Var(i)], Default::default())));
                    if let Some(v) = value {
                        l.push(Stmt::Assign(*v, Rvalue::Builtin(Builtin::MapValueAt, vec![Operand::Var(m), Operand::Var(i)], Default::default())));
                    }
                    l.stmts(body);
                });
            }
            // `for byte in b`: each byte, as an `Int`.
            TStmt::ForList { var, index, list, body } if list.ty == Type::Bytes => {
                let b = self.operand(list);
                let b = self.iterated(b, &list.ty);
                let ctr = self.temp(Type::Int);
                self.push(Stmt::Assign(ctr, Rvalue::Use(Operand::Int(0))));
                let (lc, lb, ls, le) = (self.new_block(), self.new_block(), self.new_block(), self.new_block());
                self.start(lc);
                let len = self.temp(Type::Int);
                self.push(Stmt::Assign(len, Rvalue::Builtin(Builtin::StrLen, vec![Operand::Var(b)], Default::default())));
                let c = self.temp(Type::Bool);
                self.push(Stmt::Assign(c, Rvalue::Binary(BinOp::Lt, Type::Int, Operand::Var(ctr), Operand::Var(len), Default::default())));
                self.term(Term::Branch(Operand::Var(c), lb, le));
                self.cur = lb;
                if let Some(ix) = index {
                    self.push(Stmt::Assign(*ix, Rvalue::Use(Operand::Var(ctr))));
                }
                self.push(Stmt::Assign(*var, Rvalue::Builtin(crate::stdlib::rt("bytes_at"), vec![Operand::Var(b), Operand::Var(ctr)], Default::default())));
                self.loops.push((ls, le, self.scopes.len()));
                self.stmts(body);
                self.loops.pop();
                self.start(ls);
                self.push(Stmt::Assign(ctr, Rvalue::Builtin(Builtin::Inc, vec![Operand::Var(ctr)], Default::default())));
                self.goto(lc);
                self.cur = le;
            }
            // `for c in s`: `ctr` is the byte index of the next character.
            TStmt::ForList { var, index, list, body } if list.ty == Type::Str => {
                let s = self.operand(list);
                let s = self.iterated(s, &list.ty);
                let ctr = self.temp(Type::Int);
                self.push(Stmt::Assign(ctr, Rvalue::Use(Operand::Int(0))));
                let nth = index.map(|_| {
                    let n = self.temp(Type::Int);
                    self.push(Stmt::Assign(n, Rvalue::Use(Operand::Int(0))));
                    n
                });
                let (lc, lb, ls, le) = (self.new_block(), self.new_block(), self.new_block(), self.new_block());
                self.start(lc);
                let len = self.temp(Type::Int);
                self.push(Stmt::Assign(len, Rvalue::Builtin(Builtin::StrLen, vec![Operand::Var(s)], Default::default())));
                let c = self.temp(Type::Bool);
                self.push(Stmt::Assign(c, Rvalue::Binary(BinOp::Lt, Type::Int, Operand::Var(ctr), Operand::Var(len), Default::default())));
                self.term(Term::Branch(Operand::Var(c), lb, le));
                self.cur = lb;
                if let (Some(ix), Some(n)) = (index, nth) {
                    self.push(Stmt::Assign(*ix, Rvalue::Use(Operand::Var(n))));
                }
                self.push(Stmt::Assign(*var, Rvalue::Builtin(Builtin::StrCharAt, vec![Operand::Var(s), Operand::Var(ctr)], Default::default())));
                let step = self.temp(Type::Int);
                self.push(Stmt::Assign(step, Rvalue::Builtin(Builtin::StrLen, vec![Operand::Var(*var)], Default::default())));
                self.loops.push((ls, le, self.scopes.len()));
                self.stmts(body);
                self.loops.pop();
                self.start(ls);
                self.push(Stmt::Assign(ctr, Rvalue::Binary(BinOp::Add, Type::Int, Operand::Var(ctr), Operand::Var(step), Default::default())));
                if let Some(n) = nth {
                    self.push(Stmt::Assign(n, Rvalue::Builtin(Builtin::Inc, vec![Operand::Var(n)], Default::default())));
                }
                self.goto(lc);
                self.cur = le;
            }
            TStmt::ForList { var, index, list, body } => {
                let l = self.operand(list);
                // Iterate over the list the expression evaluated to, even if the body rebinds the variable.
                let l = self.iterated(l, &list.ty);
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
                if let Some(ix) = index {
                    self.push(Stmt::Assign(*ix, Rvalue::Use(Operand::Var(ctr))));
                }
                self.push(Stmt::Assign(*var, Rvalue::Index(l, Operand::Var(ctr), None)));
                self.loops.push((ls, le, self.scopes.len()));
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
                    let mut binds = Vec::new();
                    self.scoped_binds(&arm.pat, &mut binds);
                    self.scoped(binds, |l| l.stmts(&arm.body));
                    self.goto(end);
                    self.cur = next;
                }
                // The checker proved the cases exhaustive.
                self.term(Term::Unreachable);
                self.cur = end;
            }
            TStmt::Break => {
                let (_, target, depth) = *self.loops.last().expect("break outside loop");
                self.keep_alive_from(depth);
                self.term(Term::Goto(target));
            }
            TStmt::Continue => {
                let (target, _, depth) = *self.loops.last().expect("continue outside loop");
                self.keep_alive_from(depth);
                self.term(Term::Goto(target));
            }
            TStmt::Return(None) => {
                self.keep_alive_from(0);
                self.term(Term::Return(None));
            }
            TStmt::Return(Some(e)) => {
                if e.ty == Type::Unit {
                    self.effect(e);
                    self.keep_alive_from(0);
                    self.term(Term::Return(None));
                } else {
                    let o = self.operand(e);
                    self.keep_alive_from(0);
                    self.term(Term::Return(Some(o)));
                }
            }
        }
    }

    /// `acc = acc + part` on strings (in place when `acc` is the only reference).
    fn append(&mut self, acc: VarId, part: Operand) {
        self.push(Stmt::Assign(acc, Rvalue::Binary(BinOp::Add, Type::Str, Operand::Var(acc), part, Default::default())));
    }

    /// `", "` before every item but the first (`n` counts the items so far).
    fn separator(&mut self, acc: VarId, n: VarId) {
        let c = self.temp(Type::Bool);
        self.push(Stmt::Assign(c, Rvalue::Binary(BinOp::Gt, Type::Int, Operand::Var(n), Operand::Int(0), Default::default())));
        let (sep, done) = (self.new_block(), self.new_block());
        self.term(Term::Branch(Operand::Var(c), sep, done));
        self.cur = sep;
        self.append(acc, Operand::Str(", ".into()));
        self.start(done);
    }

    /// The value a `for` loop goes over, kept in a variable of its own if the
    /// loop body might rebind the variable it came from.
    fn iterated(&mut self, o: Operand, ty: &Type) -> VarId {
        match o {
            Operand::Var(v) if self.vars[v].kind != VarKind::Temp && self.reassigned[v] => {
                let t = self.temp(ty.clone());
                self.push(Stmt::Assign(t, Rvalue::Use(Operand::Var(v))));
                t
            }
            other => self.as_var(other, ty),
        }
    }

    /// Loop over the entries of map `m` in insertion order: `each(self, i)`
    /// builds the body for the entry at index `i` (a `continue` there goes
    /// to the next entry). Adding or removing keys meanwhile panics at `span`.
    fn map_loop(&mut self, m: VarId, span: Span, each: impl FnOnce(&mut Self, VarId)) {
        let len = self.temp(Type::Int);
        self.push(Stmt::Assign(len, Rvalue::Builtin(Builtin::MapLen, vec![Operand::Var(m)], Default::default())));
        let i = self.temp(Type::Int);
        self.push(Stmt::Assign(i, Rvalue::Use(Operand::Int(0))));
        let lc = self.new_block();
        let lb = self.new_block();
        let ls = self.new_block();
        let le = self.new_block();
        self.start(lc);
        self.push(Stmt::Assign(i, Rvalue::Builtin(Builtin::MapNext, vec![Operand::Var(m), Operand::Var(i), Operand::Var(len)], span)));
        let c = self.temp(Type::Bool);
        self.push(Stmt::Assign(c, Rvalue::Binary(BinOp::Ge, Type::Int, Operand::Var(i), Operand::Int(0), Default::default())));
        self.term(Term::Branch(Operand::Var(c), lb, le));
        self.cur = lb;
        self.loops.push((ls, le, self.scopes.len()));
        each(self, i);
        self.loops.pop();
        self.start(ls);
        self.push(Stmt::Assign(i, Rvalue::Builtin(Builtin::Inc, vec![Operand::Var(i)], Default::default())));
        self.goto(lc);
        self.cur = le;
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

    /// The new value of `x op= v`, from the current value `cur`.
    fn compound(&mut self, op: tast::CompoundOp, ty: &Type, cur: Operand, v: Operand, span: Span) -> Operand {
        let new = self.temp(ty.clone());
        let r = match op {
            tast::CompoundOp::Builtin(op) => Rvalue::Binary(op, ty.clone(), cur, v, span),
            tast::CompoundOp::Call(f) => Rvalue::Call(f, vec![cur, v], span),
        };
        self.push(Stmt::Assign(new, r));
        Operand::Var(new)
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
            // `m.keys()` / `m.values()`: a list, filled in a loop over the map.
            TExprKind::Builtin(b @ (Builtin::MapKeys | Builtin::MapValues), args) => {
                let m = self.operand(&args[0]);
                let m = self.as_var(m, &args[0].ty);
                let Type::List(elem) = &e.ty else { unreachable!() };
                let out = self.temp(e.ty.clone());
                self.push(Stmt::Assign(out, Rvalue::ListNew((**elem).clone(), Vec::new())));
                let at = if *b == Builtin::MapKeys { Builtin::MapKeyAt } else { Builtin::MapValueAt };
                let elem = (**elem).clone();
                self.map_loop(m, e.span, |l, i| {
                    let x = l.temp(elem);
                    l.push(Stmt::Assign(x, Rvalue::Builtin(at, vec![Operand::Var(m), Operand::Var(i)], Default::default())));
                    l.push(Stmt::Eval(Rvalue::Builtin(Builtin::ListPush, vec![Operand::Var(out), Operand::Var(x)], Default::default())));
                });
                Rvalue::Use(Operand::Var(out))
            }
            TExprKind::Builtin(b, args) => {
                let a = self.operands(args);
                Rvalue::Builtin(*b, a, e.span)
            }
            TExprKind::Let(id, value, body) => {
                let r = self.rvalue(value);
                self.push(Stmt::Assign(*id, r));
                self.rvalue(body)
            }
            TExprKind::MapLit(entries) => {
                let m = self.temp(e.ty.clone());
                self.push(Stmt::Assign(m, Rvalue::MapNew(e.ty.clone(), entries.len())));
                for (k, v) in entries {
                    let ko = self.operand(k);
                    let vo = self.operand(v);
                    self.push(Stmt::Eval(Rvalue::Builtin(Builtin::MapSet, vec![Operand::Var(m), ko, vo], k.span)));
                }
                Rvalue::Use(Operand::Var(m))
            }
            TExprKind::ListFormat { list, elem, each } => {
                let l = self.operand(list);
                let l = self.as_var(l, &list.ty);
                let acc = self.temp(Type::Str);
                self.push(Stmt::Assign(acc, Rvalue::Use(Operand::Str("[".into()))));
                let ctr = self.temp(Type::Int);
                self.push(Stmt::Assign(ctr, Rvalue::Use(Operand::Int(0))));
                let (lc, lb, ls, le) = (self.new_block(), self.new_block(), self.new_block(), self.new_block());
                self.start(lc);
                let len = self.temp(Type::Int);
                self.push(Stmt::Assign(len, Rvalue::Builtin(Builtin::ListLen, vec![Operand::Var(l)], Default::default())));
                let c = self.temp(Type::Bool);
                self.push(Stmt::Assign(c, Rvalue::Binary(BinOp::Lt, Type::Int, Operand::Var(ctr), Operand::Var(len), Default::default())));
                self.term(Term::Branch(Operand::Var(c), lb, le));
                self.cur = lb;
                self.separator(acc, ctr);
                self.push(Stmt::Assign(*elem, Rvalue::Index(l, Operand::Var(ctr), None)));
                let part = self.operand(each);
                self.append(acc, part);
                self.start(ls);
                self.push(Stmt::Assign(ctr, Rvalue::Builtin(Builtin::Inc, vec![Operand::Var(ctr)], Default::default())));
                self.goto(lc);
                self.cur = le;
                self.append(acc, Operand::Str("]".into()));
                Rvalue::Use(Operand::Var(acc))
            }
            TExprKind::MapFormat { map, key, value, key_str, value_str } => {
                let m = self.operand(map);
                let m = self.as_var(m, &map.ty);
                let acc = self.temp(Type::Str);
                self.push(Stmt::Assign(acc, Rvalue::Use(Operand::Str("{".into()))));
                let n = self.temp(Type::Int);
                self.push(Stmt::Assign(n, Rvalue::Use(Operand::Int(0))));
                self.map_loop(m, e.span, |l, i| {
                    l.separator(acc, n);
                    l.push(Stmt::Assign(n, Rvalue::Builtin(Builtin::Inc, vec![Operand::Var(n)], Default::default())));
                    l.push(Stmt::Assign(*key, Rvalue::Builtin(Builtin::MapKeyAt, vec![Operand::Var(m), Operand::Var(i)], Default::default())));
                    l.push(Stmt::Assign(*value, Rvalue::Builtin(Builtin::MapValueAt, vec![Operand::Var(m), Operand::Var(i)], Default::default())));
                    let ks = l.operand(key_str);
                    l.append(acc, ks);
                    l.append(acc, Operand::Str(": ".into()));
                    let vs = l.operand(value_str);
                    l.append(acc, vs);
                });
                self.append(acc, Operand::Str("}".into()));
                Rvalue::Use(Operand::Var(acc))
            }
            TExprKind::ListFind { list, value, elem, needle, test, index } => {
                let l = self.operand(list);
                let l = self.as_var(l, &list.ty);
                let n = self.rvalue(value);
                self.push(Stmt::Assign(*needle, n));
                let res = self.temp(e.ty.clone());
                let init = if *index { Operand::Null(e.ty.clone()) } else { Operand::Bool(false) };
                self.push(Stmt::Assign(res, Rvalue::Use(init)));
                let ctr = self.temp(Type::Int);
                self.push(Stmt::Assign(ctr, Rvalue::Use(Operand::Int(0))));
                let (lc, lb, found, ls, le) = (self.new_block(), self.new_block(), self.new_block(), self.new_block(), self.new_block());
                self.start(lc);
                let len = self.temp(Type::Int);
                self.push(Stmt::Assign(len, Rvalue::Builtin(Builtin::ListLen, vec![Operand::Var(l)], Default::default())));
                let c = self.temp(Type::Bool);
                self.push(Stmt::Assign(c, Rvalue::Binary(BinOp::Lt, Type::Int, Operand::Var(ctr), Operand::Var(len), Default::default())));
                self.term(Term::Branch(Operand::Var(c), lb, le));
                self.cur = lb;
                self.push(Stmt::Assign(*elem, Rvalue::Index(l, Operand::Var(ctr), None)));
                let t = self.operand(test);
                self.term(Term::Branch(t, found, ls));
                self.cur = found;
                let hit = if *index { self.wrap(Operand::Var(ctr), &Type::Int) } else { Rvalue::Use(Operand::Bool(true)) };
                self.push(Stmt::Assign(res, hit));
                self.goto(le);
                self.cur = ls;
                self.push(Stmt::Assign(ctr, Rvalue::Builtin(Builtin::Inc, vec![Operand::Var(ctr)], Default::default())));
                self.goto(lc);
                self.cur = le;
                Rvalue::Use(Operand::Var(res))
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
            TExprKind::Try { value, binder, on_err } => {
                let o = self.operand(value);
                let v = self.as_var(o, &value.ty);
                let Type::Enum(sid, _) = value.ty else { unreachable!("checked: `?` on a Result") };
                // `Ok` is variant 0 and `Err` variant 1 (see the prelude).
                let tag = self.temp(Type::Int);
                self.push(Stmt::Assign(tag, Rvalue::Tag(v)));
                let is_err = self.temp(Type::Bool);
                self.push(Stmt::Assign(is_err, Rvalue::Binary(BinOp::Eq, Type::Int, Operand::Var(tag), Operand::Int(1), Default::default())));
                let (err, ok) = (self.new_block(), self.new_block());
                self.term(Term::Branch(Operand::Var(is_err), err, ok));
                self.cur = err;
                self.push(Stmt::Assign(*binder, Rvalue::VariantField(v, sid, 1, 0)));
                match &on_err.kind {
                    TExprKind::Jump(s) => self.stmt(s),
                    _ => self.effect(on_err),
                }
                if !self.terminated() {
                    self.term(Term::Unreachable);
                }
                self.cur = ok;
                Rvalue::VariantField(v, sid, 0, 0)
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
            TExprKind::Closure { func, sid, captures } => {
                let caps = self.operands(captures);
                Rvalue::Closure(*func, *sid, caps)
            }
            TExprKind::CallValue(f, args) => {
                let fo = self.operand(f);
                let a = self.operands(args);
                Rvalue::CallIndirect(fo, a, e.span)
            }
            TExprKind::Boxed(sid, x) => {
                let o = self.operand(x);
                Rvalue::Boxed(*sid, o)
            }
            TExprKind::CallMethod(recv, slot, args) => {
                let r = self.operand(recv);
                let a = self.operands(args);
                Rvalue::CallMethod(r, *slot, a, e.ty.clone(), e.span)
            }
            TExprKind::Block(stmts, value) => {
                self.stmts(stmts);
                self.rvalue(value)
            }
            TExprKind::Jump(s) => {
                self.stmt(s);
                Rvalue::Use(Operand::Bool(false))
            }
        }
    }
}

/// Is `e` a simple expression (literals, other locals, fields, calls and
/// built-ins of those) that can't read local `id`?
fn independent_of(e: &TExpr, id: LocalId) -> bool {
    match &e.kind {
        TExprKind::Int(_) | TExprKind::Float(_) | TExprKind::Bool(_) | TExprKind::Str(_) => true,
        TExprKind::Local(x) => *x != id,
        TExprKind::Field(x, ..) => independent_of(x, id),
        TExprKind::Call(_, args) | TExprKind::Builtin(_, args) => args.iter().all(|a| independent_of(a, id)),
        _ => false,
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
            TStmt::While { body, .. } | TStmt::ForRange { body, .. } | TStmt::ForList { body, .. } | TStmt::ForMap { body, .. } => {
                mark_reassigned(body, out)
            }
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

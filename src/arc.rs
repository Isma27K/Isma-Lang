//! Automatic reference counting, Perceus style.
//!
//! Every heap value in a MIR function is either *owned* (it holds one
//! reference, which must eventually be given away or dropped) or *borrowed*
//! (it is kept alive by something else). This pass makes ownership explicit by
//! inserting `Dup` and `Drop` statements:
//!
//! - A value is dropped right after its **last use**, computed by a liveness
//!   analysis over the CFG, not at the end of its scope.
//! - Using a value in a **consuming** position (storing it in a struct or
//!   list, returning it, binding it to a variable, passing it to an owned
//!   parameter) moves it if this was its last use, and dups it otherwise.
//! - **Borrow inference** (as in Lean 4) makes every parameter borrowed unless
//!   the function consumes it (or could reuse its memory), so most calls do no
//!   reference counting at all.
//! - **Borrowed projections**: `p.f`, `xs[i]` and plain aliases are borrowed
//!   for as long as the slot they were read from provably isn't overwritten
//!   (checked against interprocedural write-effect summaries). A loop over a
//!   list of structs typically does no counting at all.
//! - **Reuse** (FBIP): when a struct dies right before a struct of the same
//!   size is built, and nobody else holds it, its memory is reused in place.
//! - `s + t` on an owned string that dies there appends in place when unique.
//!
//! Critical edges are split first, so a value that dies on one branch but
//! not the other can be dropped at the start of the branch where it's dead.

use crate::ast::BinOp;
use crate::mir::*;
use crate::tast::{Builtin, TStruct};
use crate::types::{FuncId, StructId, Type};
use std::collections::HashSet;

pub fn run(prog: &mut Program) {
    for f in &mut prog.funcs {
        split_critical_edges(f);
    }
    let sizes: Vec<u64> = (0..prog.structs.len()).map(|s| struct_alloc_size(&prog.structs, s)).collect();
    let cyclic = crate::cycles::cyclic_structs(&prog.structs);
    let writes = write_effects(prog);
    let sigs = infer_borrows(prog, &sizes, &cyclic);
    for (i, f) in prog.funcs.iter_mut().enumerate() {
        f.param_owned = sigs[i].clone();
        let own = Ownership::analyze(f, &sigs, &writes);
        use_owned_append(f, &sigs, &own);
        insert_rc(f, &sigs, &own);
        reuse(f, &sizes, &cyclic);
        peephole(f);
    }
}

// ---------------------------------------------------------------- sets

#[derive(Clone, PartialEq, Eq, Debug)]
struct VarSet(Vec<u64>);

impl VarSet {
    fn new(n: usize) -> Self {
        VarSet(vec![0; n.div_ceil(64)])
    }
    fn contains(&self, v: VarId) -> bool {
        self.0[v / 64] & (1 << (v % 64)) != 0
    }
    fn insert(&mut self, v: VarId) {
        self.0[v / 64] |= 1 << (v % 64);
    }
    fn remove(&mut self, v: VarId) {
        self.0[v / 64] &= !(1 << (v % 64));
    }
    fn union(&mut self, o: &VarSet) {
        for (a, b) in self.0.iter_mut().zip(&o.0) {
            *a |= b;
        }
    }
    fn iter(&self) -> impl Iterator<Item = VarId> + '_ {
        (0..self.0.len() * 64).filter(|&v| self.contains(v))
    }
}

// ---------------------------------------------------------------- ownership

/// Which variables hold a reference of their own, and which borrow from another.
struct Ownership {
    /// Borrowed from `parent` (a projection or alias that needs no counting).
    borrowed: Vec<bool>,
    parent: Vec<Option<VarId>>,
    /// Holds a reference that must be given away or dropped.
    owned: Vec<bool>,
}

impl Ownership {
    /// No borrowed locals: just parameter conventions.
    fn plain(f: &Func) -> Self {
        let nv = f.vars.len();
        let mut o = Ownership { borrowed: vec![false; nv], parent: vec![None; nv], owned: vec![false; nv] };
        o.finish(f);
        o
    }

    fn finish(&mut self, f: &Func) {
        for v in 0..f.vars.len() {
            self.owned[v] = rc(f, v)
                && match f.vars[v].kind {
                    VarKind::Param => f.params.iter().position(|&p| p == v).is_some_and(|i| f.param_owned[i]),
                    VarKind::Local | VarKind::Temp => !self.borrowed[v],
                };
        }
    }

    /// `v` plus every variable it (transitively) borrows from: using `v`
    /// requires all of them to still be alive.
    fn expand(&self, v: VarId, out: &mut Vec<VarId>) {
        let mut cur = Some(v);
        while let Some(x) = cur {
            if !out.contains(&x) {
                out.push(x);
            }
            cur = if self.borrowed[x] { self.parent[x] } else { None };
        }
    }

    fn expand_all(&self, vs: &[VarId]) -> Vec<VarId> {
        let mut out = Vec::new();
        for &v in vs {
            self.expand(v, &mut out);
        }
        out
    }
}

// ---------------------------------------------------------------- roles

/// How one statement uses the reference-counted variables it mentions.
#[derive(Default, Debug)]
struct Roles {
    /// Ownership is transferred (in evaluation order; may repeat).
    consumed: Vec<VarId>,
    /// Read without taking ownership.
    borrowed: Vec<VarId>,
    /// The variable defined, if reference-counted.
    def: Option<VarId>,
    /// The defined value is borrowed from another object (a projection).
    projection: bool,
}

impl Roles {
    fn uses(&self) -> Vec<VarId> {
        self.consumed.iter().chain(&self.borrowed).copied().collect()
    }
}

fn rc(f: &Func, v: VarId) -> bool {
    is_ref(&f.vars[v].ty)
}

fn rvalue_roles(f: &Func, rv: &Rvalue, sigs: &[Vec<bool>], r: &mut Roles) {
    let add = |o: &Operand, consume: bool, r: &mut Roles| {
        if let Some(v) = o.var()
            && rc(f, v)
        {
            if consume { r.consumed.push(v) } else { r.borrowed.push(v) }
        }
    };
    match rv {
        Rvalue::Use(o) => add(o, true, r),
        Rvalue::Call(fid, args, _) => {
            for (i, a) in args.iter().enumerate() {
                add(a, sigs[*fid][i], r);
            }
        }
        Rvalue::Builtin(Builtin::ListPush, args, _) => {
            add(&args[0], false, r);
            add(&args[1], true, r);
        }
        Rvalue::Builtin(Builtin::StrAppendOwned, args, _) => {
            add(&args[0], true, r);
            add(&args[1], false, r);
        }
        Rvalue::StructNew(_, args) | Rvalue::Reuse(_, _, args) | Rvalue::ListNew(_, args) => {
            for a in args {
                add(a, true, r);
            }
        }
        Rvalue::Field(o, ..) => add(&Operand::Var(*o), false, r),
        Rvalue::Index(l, i, _) => {
            add(&Operand::Var(*l), false, r);
            add(i, false, r);
        }
        _ => {
            for o in rv.operands() {
                add(o, false, r);
            }
        }
    }
}

/// `borrowed`: variables known to be borrowed aliases (their defining
/// `Use(x)` borrows `x` instead of consuming it).
fn stmt_roles(f: &Func, s: &Stmt, sigs: &[Vec<bool>], borrowed: Option<&[bool]>) -> Roles {
    let mut r = Roles::default();
    match s {
        Stmt::Assign(d, rv) => {
            let alias = borrowed.is_some_and(|b| b[*d]);
            match (rv, alias) {
                (Rvalue::Use(Operand::Var(x)), true) => r.borrowed.push(*x),
                _ => rvalue_roles(f, rv, sigs, &mut r),
            }
            if rc(f, *d) {
                r.def = Some(*d);
                r.projection = alias || matches!(rv, Rvalue::Field(..) | Rvalue::Index(..));
            }
        }
        Stmt::Eval(rv) => rvalue_roles(f, rv, sigs, &mut r),
        Stmt::Store { place, value, old } => {
            let base = match place {
                Place::Field { obj, .. } => *obj,
                Place::Index { list, .. } => *list,
            };
            r.borrowed.push(base);
            if let Some(v) = value.var()
                && rc(f, v)
            {
                r.consumed.push(v);
            }
            r.def = *old;
        }
        Stmt::Dup(_) | Stmt::Drop(_) | Stmt::DropReuse(..) => {}
    }
    r
}

/// The variable a `return` gives away, if any.
fn returned(f: &Func, t: &Term) -> Option<VarId> {
    match t {
        Term::Return(Some(Operand::Var(v))) if rc(f, *v) => Some(*v),
        _ => None,
    }
}

// ---------------------------------------------------------------- CFG prep

/// Give every edge from a branching block into a join point its own block,
/// so drops for values that die on just that edge have somewhere to go.
fn split_critical_edges(f: &mut Func) {
    let preds = f.predecessors();
    for b in 0..f.blocks.len() {
        let Term::Branch(_, t, e) = f.blocks[b].term.clone() else { continue };
        for (is_then, target) in [(true, t), (false, e)] {
            if preds[target].len() > 1 {
                let nb = f.blocks.len();
                f.blocks.push(Block { stmts: Vec::new(), term: Term::Goto(target) });
                if let Term::Branch(_, t2, e2) = &mut f.blocks[b].term {
                    if is_then { *t2 = nb } else { *e2 = nb }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- liveness

/// Live reference-counted variables at the start and end of every block.
/// A use of a borrowed variable also keeps everything it borrows from alive.
fn liveness(f: &Func, sigs: &[Vec<bool>], own: &Ownership) -> (Vec<VarSet>, Vec<VarSet>) {
    let n = f.blocks.len();
    let nv = f.vars.len();
    let mut live_in = vec![VarSet::new(nv); n];
    let mut live_out = vec![VarSet::new(nv); n];
    let roles: Vec<Vec<(Option<VarId>, Vec<VarId>)>> = f
        .blocks
        .iter()
        .map(|b| {
            b.stmts
                .iter()
                .map(|s| {
                    let r = stmt_roles(f, s, sigs, Some(&own.borrowed));
                    (r.def, own.expand_all(&r.uses()))
                })
                .collect()
        })
        .collect();
    loop {
        let mut changed = false;
        for b in (0..n).rev() {
            let mut out = VarSet::new(nv);
            for s in f.blocks[b].term.successors() {
                out.union(&live_in[s]);
            }
            let mut live = out.clone();
            if let Some(v) = returned(f, &f.blocks[b].term) {
                for x in own.expand_all(&[v]) {
                    live.insert(x);
                }
            }
            for (def, uses) in roles[b].iter().rev() {
                if let Some(d) = def {
                    live.remove(*d);
                }
                for &v in uses {
                    live.insert(v);
                }
            }
            if live != live_in[b] || out != live_out[b] {
                changed = true;
                live_in[b] = live;
                live_out[b] = out;
            }
        }
        if !changed {
            return (live_in, live_out);
        }
    }
}

/// Walk a block backwards, calling `visit(index, roles, live_after, expanded_uses)`
/// for every statement (live_after has the defined variable removed).
fn walk_block(
    f: &Func,
    b: BlockId,
    live_out: &VarSet,
    sigs: &[Vec<bool>],
    own: &Ownership,
    mut visit: impl FnMut(usize, &Roles, &VarSet, &[VarId]),
) {
    let mut live = live_out.clone();
    if let Some(v) = returned(f, &f.blocks[b].term) {
        for x in own.expand_all(&[v]) {
            live.insert(x);
        }
    }
    for (i, s) in f.blocks[b].stmts.iter().enumerate().rev() {
        let r = stmt_roles(f, s, sigs, Some(&own.borrowed));
        if let Some(d) = r.def {
            live.remove(d);
        }
        let uses = own.expand_all(&r.uses());
        visit(i, &r, &live, &uses);
        for &v in &uses {
            live.insert(v);
        }
    }
}

// ---------------------------------------------------------------- effects

/// A kind of heap slot: a field of a struct, or the elements of lists of a type.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Class {
    Field(StructId, usize),
    Elem(Type),
}

fn elem_of(t: &Type) -> Type {
    match t {
        Type::List(e) => (**e).clone(),
        _ => unreachable!("not a list"),
    }
}

/// The slot classes a statement may overwrite (or empty, for `pop`), not counting calls.
fn direct_writes(f: &Func, s: &Stmt, calls: &mut Vec<FuncId>) -> Vec<Class> {
    let mut out = Vec::new();
    let rv = match s {
        Stmt::Store { place: Place::Field { sid, idx, .. }, .. } => {
            out.push(Class::Field(*sid, *idx));
            None
        }
        Stmt::Store { place: Place::Index { list, .. }, .. } => {
            out.push(Class::Elem(elem_of(&f.vars[*list].ty)));
            None
        }
        Stmt::Assign(_, rv) | Stmt::Eval(rv) => Some(rv),
        _ => None,
    };
    match rv {
        Some(Rvalue::Call(fid, ..)) => calls.push(*fid),
        Some(Rvalue::Builtin(Builtin::ListPop, args, _)) => out.push(Class::Elem(elem_of(&f.operand_ty(&args[0])))),
        _ => {}
    }
    out
}


/// Every slot class each function may overwrite, directly or through calls.
fn write_effects(prog: &Program) -> Vec<HashSet<Class>> {
    let mut writes: Vec<HashSet<Class>> = Vec::new();
    let mut callees: Vec<Vec<FuncId>> = Vec::new();
    for f in &prog.funcs {
        let mut w = HashSet::new();
        let mut calls = Vec::new();
        for b in &f.blocks {
            for s in &b.stmts {
                w.extend(direct_writes(f, s, &mut calls));
            }
        }
        writes.push(w);
        callees.push(calls);
    }
    loop {
        let mut changed = false;
        for fi in 0..writes.len() {
            for &c in &callees[fi] {
                if c == fi {
                    continue;
                }
                let add: Vec<Class> = writes[c].difference(&writes[fi]).cloned().collect();
                if !add.is_empty() {
                    writes[fi].extend(add);
                    changed = true;
                }
            }
        }
        if !changed {
            return writes;
        }
    }
}

fn stmt_writes(f: &Func, s: &Stmt, writes: &[HashSet<Class>]) -> Vec<Class> {
    let mut calls = Vec::new();
    let mut out = direct_writes(f, s, &mut calls);
    for c in calls {
        out.extend(writes[c].iter().cloned());
    }
    out
}

// ---------------------------------------------------------------- borrowed projections

impl Ownership {
    /// Decide which projections (`p.f`, `xs[i]`) and aliases (`val b = a`) can
    /// stay borrowed. Start optimistic, then demote any whose source might
    /// change while they are in use, until nothing changes.
    fn analyze(f: &Func, sigs: &[Vec<bool>], writes: &[HashSet<Class>]) -> Self {
        let nv = f.vars.len();
        let mut own = Ownership { borrowed: vec![false; nv], parent: vec![None; nv], owned: vec![false; nv] };
        let mut class: Vec<Option<Class>> = vec![None; nv];
        let mut defs = vec![0usize; nv];
        for b in &f.blocks {
            for s in &b.stmts {
                for d in s.defs() {
                    defs[d] += 1;
                }
            }
        }
        for b in &f.blocks {
            for s in &b.stmts {
                let Stmt::Assign(d, rv) = s else { continue };
                if !rc(f, *d) || f.vars[*d].kind == VarKind::Param || defs[*d] != 1 {
                    continue;
                }
                let (parent, cls) = match rv {
                    Rvalue::Field(o, sid, idx) => (*o, Some(Class::Field(*sid, *idx))),
                    Rvalue::Index(l, ..) => (*l, Some(Class::Elem(elem_of(&f.vars[*l].ty)))),
                    Rvalue::Use(Operand::Var(x)) if rc(f, *x) => (*x, None),
                    _ => continue,
                };
                own.borrowed[*d] = true;
                own.parent[*d] = Some(parent);
                class[*d] = cls;
            }
        }
        let plain = Ownership::plain(f);
        loop {
            own.finish(f);
            let (_, live_out) = liveness(f, sigs, &own);
            let (_, direct_out) = liveness(f, sigs, &plain);
            let mut viol = vec![false; nv];
            let mut direct_viol = vec![false; nv];
            let candidates: Vec<VarId> = (0..nv).filter(|&v| own.borrowed[v]).collect();
            for b in 0..f.blocks.len() {
                // Statement-level liveness, with and without borrow expansion.
                let mut direct_after: Vec<VarSet> = vec![VarSet::new(nv); f.blocks[b].stmts.len()];
                walk_block(f, b, &direct_out[b], sigs, &plain, |i, _, live, _| direct_after[i] = live.clone());
                walk_block(f, b, &live_out[b], sigs, &own, |i, r, live, uses| {
                    let s = &f.blocks[b].stmts[i];
                    let w = stmt_writes(f, s, writes);
                    let is_store = matches!(s, Stmt::Store { .. });
                    let direct_uses = r.uses();
                    let defs = s.defs();
                    for &v in &candidates {
                        // Redefining anything v (transitively) borrows from invalidates it.
                        let redefines_parent = own.expand_all(&[v]).iter().any(|a| *a != v && defs.contains(a));
                        let overwrites = class[v].as_ref().is_some_and(|c| w.contains(c));
                        if !redefines_parent && !overwrites {
                            continue;
                        }
                        // A store only frees the old value after writing, so a value used
                        // just by the store itself is fine; anything else in use is not.
                        if live.contains(v) || (!is_store && uses.contains(&v)) {
                            viol[v] = true;
                        }
                        if direct_after[i].contains(v) || (!is_store && direct_uses.contains(&v)) {
                            direct_viol[v] = true;
                        }
                    }
                });
            }
            // Demote the variables that conflict on their own first; ones that only
            // conflict through something borrowed from them may be fine afterwards.
            let pick = if direct_viol.iter().any(|&x| x) { &direct_viol } else { &viol };
            if !pick.iter().any(|&x| x) {
                return own;
            }
            for (v, _) in pick.iter().enumerate().filter(|(_, demote)| **demote) {
                own.borrowed[v] = false;
                own.parent[v] = None;
            }
        }
    }
}

// ---------------------------------------------------------------- borrow inference

/// The allocation size class (rounded like the runtime does) of a struct.
fn struct_alloc_size(structs: &[TStruct], sid: StructId) -> u64 {
    let mut off = 8u64; // reference count
    for f in &structs[sid].fields {
        let (size, align) = if f.ty == Type::Bool { (1, 1) } else { (8, 8) };
        off = off.div_ceil(align) * align + size;
    }
    let size = off.div_ceil(8) * 8; // struct alignment
    size.div_ceil(16) * 16
}

/// A parameter is owned when its function consumes it, or when it could
/// donate its memory to a struct the function builds (reuse); everything
/// else is borrowed. Consuming includes passing it to another function's
/// owned parameter, so this is a fixpoint over the call graph.
fn infer_borrows(prog: &Program, sizes: &[u64], cyclic: &[bool]) -> Vec<Vec<bool>> {
    let mut sigs: Vec<Vec<bool>> = prog.funcs.iter().map(|f| vec![false; f.params.len()]).collect();
    // Reuse candidates don't depend on other functions' signatures.
    for (fi, f) in prog.funcs.iter().enumerate() {
        sigs[fi] = reuse_params(f, sizes, cyclic, &sigs);
    }
    loop {
        let mut changed = false;
        for (fi, f) in prog.funcs.iter().enumerate() {
            for (i, &p) in f.params.iter().enumerate() {
                if sigs[fi][i] || !rc(f, p) {
                    continue;
                }
                let consumed = f.blocks.iter().any(|b| {
                    returned(f, &b.term) == Some(p)
                        || b.stmts.iter().any(|s| stmt_roles(f, s, &sigs, None).consumed.contains(&p))
                });
                if consumed {
                    sigs[fi][i] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            return sigs;
        }
    }
}

/// Parameters that could donate their memory to a struct the function
/// builds after their last use (at most one parameter per construction,
/// preferring earlier parameters such as `self`).
fn reuse_params(f: &Func, sizes: &[u64], cyclic: &[bool], sigs: &[Vec<bool>]) -> Vec<bool> {
    let mut chosen = vec![false; f.params.len()];
    let plain = Ownership::plain(f);
    let (_, live_out) = liveness(f, sigs, &plain);
    for (b, out) in live_out.iter().enumerate() {
        // (statement index, size) of each construction, and for each parameter
        // the constructions that come after its last use in this block.
        let mut news: Vec<(usize, u64)> = Vec::new();
        let mut dead_at: Vec<Vec<usize>> = vec![Vec::new(); f.params.len()];
        let mut used_before: Vec<Vec<usize>> = vec![Vec::new(); f.params.len()];
        walk_block(f, b, out, sigs, &plain, |i, _, live, uses| {
            if let Stmt::Assign(_, Rvalue::StructNew(sid, _)) = &f.blocks[b].stmts[i] {
                news.push((i, sizes[*sid]));
                for (k, &p) in f.params.iter().enumerate() {
                    if !live.contains(p) && !uses.contains(&p) {
                        dead_at[k].push(i);
                    }
                }
            }
            for (k, &p) in f.params.iter().enumerate() {
                if uses.contains(&p) {
                    // Constructions after this use, where p is already dead.
                    let later: Vec<usize> = dead_at[k].iter().copied().filter(|&n| n > i).collect();
                    used_before[k].extend(later);
                }
            }
        });
        for (n, size) in news {
            let pick = f.params.iter().enumerate().position(|(k, &p)| {
                !chosen[k]
                    && used_before[k].contains(&n)
                    && matches!(f.vars[p].ty, Type::Struct(sid) if !cyclic[sid] && sizes[sid] == size)
            });
            if let Some(k) = pick {
                chosen[k] = true;
            }
        }
    }
    chosen
}

// ---------------------------------------------------------------- dup/drop insertion

fn insert_rc(f: &mut Func, sigs: &[Vec<bool>], own: &Ownership) {
    let (live_in, live_out) = liveness(f, sigs, own);
    let owned = |v: VarId| own.owned[v];
    let mut new_stmts: Vec<Vec<Stmt>> = Vec::with_capacity(f.blocks.len());

    for (b, block) in f.blocks.iter().enumerate() {
        let mut live = live_out[b].clone();
        // Built back to front, then reversed.
        let mut rev: Vec<Stmt> = Vec::new();
        if let Some(v) = returned(f, &block.term) {
            // `return v` gives v away: dup it unless it's ours to move, then drop
            // whatever it was borrowed from (nothing is live after a return).
            let moved = owned(v) && !live.contains(v);
            let uses = own.expand_all(&[v]);
            let mut after: Vec<Stmt> = Vec::new();
            for &x in &uses {
                if owned(x) && !live.contains(x) && !(x == v && moved) {
                    after.push(Stmt::Drop(x));
                }
            }
            rev.extend(after.into_iter().rev());
            if !moved {
                rev.push(Stmt::Dup(v));
            }
            for x in uses {
                live.insert(x);
            }
        }
        for s in block.stmts.iter().rev() {
            let r = stmt_roles(f, s, sigs, Some(&own.borrowed));
            // Live after this statement, with the old value of the defined variable gone.
            let mut after_live = live.clone();
            let mut before: Vec<Stmt> = Vec::new();
            let mut after: Vec<Stmt> = Vec::new();
            if let Some(d) = r.def {
                after_live.remove(d);
                if live.contains(d) {
                    // A borrowed projection stored in an owned variable takes a reference.
                    if r.projection && owned(d) {
                        after.push(Stmt::Dup(d));
                    }
                } else if owned(d) && !r.projection {
                    after.push(Stmt::Drop(d)); // defined but never used
                }
            }
            let borrowed_here = own.expand_all(&r.borrowed);
            let mut moved: Vec<VarId> = Vec::new();
            for &v in &r.consumed {
                // Keep our reference when the value is still needed afterwards, is
                // also borrowed by this same statement, or isn't ours to give.
                if !owned(v) || after_live.contains(v) || moved.contains(&v) || borrowed_here.contains(&v) {
                    before.push(Stmt::Dup(v));
                } else {
                    moved.push(v);
                }
            }
            let uses = own.expand_all(&r.uses());
            for &v in &uses {
                if owned(v) && !after_live.contains(v) && !moved.contains(&v) {
                    debug_assert!(r.def != Some(v), "statement both uses and redefines {v} in {}", f.name);
                    after.push(Stmt::Drop(v));
                }
            }
            rev.extend(after.into_iter().rev());
            rev.push(s.clone());
            rev.extend(before.into_iter().rev());
            live = after_live;
            for &v in &uses {
                live.insert(v);
            }
        }
        debug_assert_eq!(live, live_in[b], "liveness mismatch in {} bb{b}", f.name);
        rev.reverse();
        new_stmts.push(rev);
    }

    // Values that die on an edge out of a branch are dropped where that edge lands.
    let preds = f.predecessors();
    for (b, out) in live_out.iter().enumerate() {
        for s in f.blocks[b].term.successors() {
            if preds[s].len() != 1 {
                continue; // (then b has a single successor, so nothing dies on the edge)
            }
            let dying: Vec<VarId> = out.iter().filter(|&v| !live_in[s].contains(v) && owned(v)).collect();
            for v in dying.into_iter().rev() {
                new_stmts[s].insert(0, Stmt::Drop(v));
            }
        }
    }
    // Owned parameters that are never used.
    for &p in &f.params {
        if owned(p) && !live_in[0].contains(p) {
            new_stmts[0].insert(0, Stmt::Drop(p));
        }
    }
    for (block, stmts) in f.blocks.iter_mut().zip(new_stmts) {
        block.stmts = stmts;
    }
}

// ---------------------------------------------------------------- in-place append

/// `s + t` where `s` is an owned string that dies right there becomes an
/// in-place append (`isma_str_append_owned`), which reuses `s`'s buffer when
/// nobody else holds it. This is what makes `out += x` loops linear.
fn use_owned_append(f: &mut Func, sigs: &[Vec<bool>], own: &Ownership) {
    let (_, live_out) = liveness(f, sigs, own);
    for (b, out) in live_out.iter().enumerate() {
        let mut rewrite: Vec<usize> = Vec::new();
        walk_block(f, b, out, sigs, own, |i, r, live, _| {
            if let Stmt::Assign(_, Rvalue::Binary(BinOp::Add, Type::Str, Operand::Var(a), _, _)) = &f.blocks[b].stmts[i] {
                let a = *a;
                // `a` must be ours, dead afterwards, and not also read (directly or
                // through something borrowed from it) by the right-hand side.
                let rhs_uses = own.expand_all(&r.borrowed.iter().copied().filter(|&v| v != a).collect::<Vec<_>>());
                if own.owned[a] && !live.contains(a) && !rhs_uses.contains(&a) {
                    rewrite.push(i);
                }
            }
        });
        for i in rewrite {
            if let Stmt::Assign(d, Rvalue::Binary(_, _, a, rhs, span)) = &f.blocks[b].stmts[i] {
                let new = Stmt::Assign(*d, Rvalue::Builtin(Builtin::StrAppendOwned, vec![a.clone(), rhs.clone()], *span));
                f.blocks[b].stmts[i] = new;
            }
        }
    }
}

// ---------------------------------------------------------------- reuse

/// `drop x` followed (in the same block) by building a struct of the same
/// allocation size becomes `tok = drop-reuse x` + `new S(...) reusing tok`:
/// when `x` was the last reference, its memory is recycled in place.
fn reuse(f: &mut Func, sizes: &[u64], cyclic: &[bool]) {
    for b in 0..f.blocks.len() {
        let mut pending: Vec<(usize, VarId, u64)> = Vec::new(); // (stmt index, var, size)
        let mut pairs: Vec<(usize, usize, VarId)> = Vec::new(); // (drop index, new index, var)
        for (i, s) in f.blocks[b].stmts.iter().enumerate() {
            match s {
                Stmt::Drop(v) => {
                    if let Type::Struct(sid) = f.vars[*v].ty
                        && !cyclic[sid]
                    {
                        pending.push((i, *v, sizes[sid]));
                    }
                }
                Stmt::Assign(_, Rvalue::StructNew(sid, _)) => {
                    if let Some(k) = pending.iter().position(|(_, _, size)| *size == sizes[*sid]) {
                        let (di, v, _) = pending.remove(k);
                        pairs.push((di, i, v));
                    }
                }
                _ => {}
            }
        }
        for (di, ni, v) in pairs {
            let ty = f.vars[v].ty.clone();
            f.vars.push(Var { name: "reuse".into(), ty, kind: VarKind::Temp });
            let tok = f.vars.len() - 1;
            f.blocks[b].stmts[di] = Stmt::DropReuse(v, tok);
            if let Stmt::Assign(d, Rvalue::StructNew(sid, args)) = &f.blocks[b].stmts[ni] {
                f.blocks[b].stmts[ni] = Stmt::Assign(*d, Rvalue::Reuse(tok, *sid, args.clone()));
            }
        }
    }
}

// ---------------------------------------------------------------- peephole

/// `dup x` immediately followed by `drop x` cancels out.
fn peephole(f: &mut Func) {
    for b in &mut f.blocks {
        let mut out: Vec<Stmt> = Vec::with_capacity(b.stmts.len());
        for s in b.stmts.drain(..) {
            if let (Stmt::Drop(v), Some(Stmt::Dup(u))) = (&s, out.last())
                && u == v
            {
                out.pop();
                continue;
            }
            out.push(s);
        }
        b.stmts = out;
    }
}

#[cfg(test)]
mod tests {
    /// The MIR (after these passes) of one function, as `--emit-mir` prints it.
    fn mir_of(src: &str, func: &str) -> String {
        let (prog, _) = crate::compile_to_mir(src).unwrap_or_else(|d| panic!("{:?}", d.iter().map(|d| &d.message).collect::<Vec<_>>()));
        let text = crate::mir::print_program(&prog);
        text.split("\n\n").find(|f| f.starts_with(&format!("fn {func}("))).unwrap_or_else(|| panic!("no fn {func} in\n{text}")).to_string()
    }

    fn count(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    #[test]
    fn parameters_are_borrowed_unless_consumed() {
        let src = "struct Box:\n    s: String\n\nfn size(s: String) -> Int:\n    return s.len()\n\nfn wrap(s: String) -> Box:\n    return Box(s)\n\nfn main():\n    val s = \"a\" + \"b\"\n    print(size(s))\n    val b = wrap(s)\n    print(b.s)\n";
        let size = mir_of(src, "size");
        assert!(size.contains("fn size(s%0: String)"), "{size}");
        assert_eq!(count(&size, "dup") + count(&size, "drop"), 0, "{size}");
        let wrap = mir_of(src, "wrap");
        assert!(wrap.contains("owned s%0"), "{wrap}");
        assert_eq!(count(&wrap, "dup"), 0, "{wrap}");
        // main moves `s` into `wrap` on its last use: no dup anywhere.
        let main = mir_of(src, "main");
        assert_eq!(count(&main, "dup"), 0, "{main}");
    }

    #[test]
    fn values_are_dropped_right_after_their_last_use() {
        let src = "fn main():\n    val s = \"a\" + \"b\"\n    print(s)\n    var i = 0\n    while i < 3:\n        i += 1\n    print(i)\n";
        let main = mir_of(src, "main");
        let lines: Vec<&str> = main.lines().map(str::trim).collect();
        let print = lines.iter().position(|l| l.starts_with("@Print(s%")).unwrap();
        assert!(lines[print + 1].starts_with("drop s%"), "{main}");
        assert_eq!(count(&main, "drop"), 1, "{main}");
    }

    #[test]
    fn loops_over_structs_do_no_counting() {
        let src = "struct Task:\n    name: String\n    var done: Bool\n\n    fn finish(self):\n        self.done = true\n\nfn main():\n    val todo = [Task(\"a\", false), Task(\"b\", false)]\n    for task in todo:\n        if task.name != \"b\":\n            task.finish()\n    print(todo.len())\n";
        let main = mir_of(src, "main");
        assert_eq!(count(&main, "dup"), 0, "{main}");
        assert_eq!(count(&main, "drop"), 1, "only the list itself is dropped:\n{main}");
    }

    #[test]
    fn overwriting_a_slot_forces_a_reference() {
        let src = "struct H:\n    var name: String\n\nfn main():\n    val h = H(\"a\" + \"b\")\n    val n = h.name\n    h.name = \"c\"\n    print(n)\n";
        let main = mir_of(src, "main");
        assert!(main.contains("dup n%"), "{main}");
    }

    #[test]
    fn unique_structs_are_reused_in_place() {
        let src = "struct V:\n    var x: Float\n\n    fn add(self, o: V) -> V:\n        return V(self.x + o.x)\n\nfn main():\n    var a = V(0.0)\n    val b = V(1.0)\n    for _ in 0..10:\n        a = a.add(b)\n    print(a.x)\n";
        let add = mir_of(src, "V.add");
        assert!(add.contains("owned self%0") && !add.contains("owned o%"), "{add}");
        assert!(add.contains("drop-reuse self%0") && add.contains("reusing"), "{add}");
        let main = mir_of(src, "main");
        assert_eq!(count(&main, "dup"), 0, "{main}");
    }

    #[test]
    fn string_building_appends_in_place() {
        let src = "fn main():\n    var s = \"\"\n    for _ in 0..10:\n        s += \"x\"\n    print(s)\n";
        let main = mir_of(src, "main");
        assert!(main.contains("@StrAppendOwned(s%"), "{main}");
    }
}

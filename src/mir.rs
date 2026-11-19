//! Mid-level IR: a control-flow graph in A-normal form.
//!
//! Every intermediate value has a name (a `Temp` var), every operand is a
//! variable or a constant, and control flow is explicit. This is the form the
//! reference-counting passes (`arc.rs`) work on: liveness is computed over
//! the CFG, and `Dup`/`Drop` statements are inserted between instructions.

use crate::ast::BinOp;
use crate::span::Span;
use crate::tast::{Builtin, SyncOp, TStruct};
use crate::types::*;
use std::fmt::Write as _;

pub type VarId = usize;
pub type BlockId = usize;

#[derive(Debug)]
pub struct Program {
    pub structs: Vec<TStruct>,
    pub funcs: Vec<Func>,
    pub map_keys: Vec<crate::tast::MapKey>,
}

#[derive(Debug)]
pub struct Func {
    pub name: String,
    pub symbol: String,
    pub params: Vec<VarId>,
    /// Per parameter: does the callee take ownership (+1) of the argument?
    /// Filled in by borrow inference; `false` (borrowed) until then.
    pub param_owned: Vec<bool>,
    pub ret: Type,
    pub vars: Vec<Var>,
    /// `blocks[0]` is the entry block.
    pub blocks: Vec<Block>,
    pub exported: bool,
    pub track_caller: bool,
    /// A placeholder for a generic function (see `TFunc::generic`): never emitted.
    pub generic: bool,
    /// Called through function values (see `TFunc::closure`): its parameters
    /// stay borrowed.
    pub closure: bool,
    /// A `fn drop(self)`: its parameter stays borrowed too.
    pub is_drop: bool,
    /// Per block, per statement: the references (variable, how many) to
    /// release if the statement panics and the task unwinds (see `arc.rs`).
    pub cleanups: Vec<Vec<Vec<(VarId, u32)>>>,
}

#[derive(Debug, Clone)]
pub struct Var {
    pub name: String,
    pub ty: Type,
    pub kind: VarKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Param,
    /// A `val`/`var`/loop variable from the source.
    Local,
    /// A compiler-introduced intermediate value.
    Temp,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub term: Term,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Assign(VarId, Rvalue),
    /// Evaluate for side effects only (unit calls, `print`, ...).
    Eval(Rvalue),
    /// Write `value` into a heap slot. For reference-typed slots, `old`
    /// receives the value previously stored there (now owned by `old`).
    Store { place: Place, value: Operand, old: Option<VarId> },
    /// Reference counting, inserted by `arc.rs`.
    Dup(VarId),
    Drop(VarId),
    /// Drop `.0`, but if this was the last reference keep its memory in the
    /// reuse token `.1` (null otherwise) for a following `Reuse` (Perceus FBIP).
    DropReuse(VarId, VarId),
    /// Keep the variable's value alive until here (a use that does nothing):
    /// a value with a `fn drop` lives to the end of its variable's block.
    KeepAlive(VarId),
}

#[derive(Debug, Clone)]
pub enum Place {
    Field { obj: VarId, sid: StructId, idx: usize },
    Index { list: VarId, index: Operand, span: Span },
}

#[derive(Debug, Clone)]
pub enum Rvalue {
    Use(Operand),
    /// Arithmetic / comparison; the operand type is given explicitly.
    Binary(BinOp, Type, Operand, Operand, Span),
    Neg(Type, Operand, Span),
    Not(Operand),
    Call(FuncId, Vec<Operand>, Span),
    Builtin(Builtin, Vec<Operand>, Span),
    /// Projection `obj.field`.
    Field(VarId, StructId, usize),
    /// Projection `list[index]`; bounds-checked when a span is given.
    Index(VarId, Operand, Option<Span>),
    /// A new struct or enum value (one operand per field).
    New(Ctor, Vec<Operand>),
    /// `New` into the memory held by a reuse token (allocating if it's null).
    Reuse(VarId, Ctor, Vec<Operand>),
    /// Which variant an enum value is (`Int`).
    Tag(VarId),
    /// Projection: field `.3` of variant `.2` of the enum value `.0` (enum `.1`).
    /// Only valid when the value is known to be that variant.
    VariantField(VarId, StructId, usize, usize),
    /// A list literal with the given element type.
    ListNew(Type, Vec<Operand>),
    /// An empty map of the given (`Map`) type, with room for this many entries.
    MapNew(Type, usize),
    /// A function value: closure code `.0` with the captured values (consumed),
    /// stored in a new closure of struct `.1`. Without captures it's a constant.
    Closure(FuncId, Option<StructId>, Vec<Operand>),
    /// Call the function value `.0` (borrowing it and every argument).
    CallIndirect(Operand, Vec<Operand>, Span),
    /// A new interface value: box `.0` (see `TStruct::closure`) holding `.1`
    /// (consumed), with the method table of its type.
    Boxed(StructId, Operand),
    /// Call the method in slot `.1` of the method table of interface value
    /// `.0` (borrowing it and every argument); it returns a `.3`.
    CallMethod(Operand, usize, Vec<Operand>, Type, Span),
    /// Does interface value `.0` hold a value of type `.1` (`Bool`)? (Looks
    /// through boxes holding other interface values.)
    IsType(VarId, Type),
    /// Projection: the `.1` inside interface value `.0` (only valid when it
    /// holds one). Box contents never change, so it can stay borrowed.
    Unbox(VarId, Type),
    /// An operation of tasks, channels or mutexes, on values of the types
    /// given (see `SyncOp`).
    Sync(SyncOp, Vec<Type>, Vec<Operand>, Span),
}

/// What a `New` builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctor {
    Struct(StructId),
    /// Enum, variant.
    Variant(StructId, usize),
}

#[derive(Debug, Clone)]
pub enum Operand {
    Var(VarId),
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// `null` of the given (nullable) type.
    Null(Type),
}

#[derive(Debug, Clone)]
pub enum Term {
    Goto(BlockId),
    Branch(Operand, BlockId, BlockId),
    Return(Option<Operand>),
    Unreachable,
}

impl Term {
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Term::Goto(b) => vec![*b],
            Term::Branch(_, t, f) => vec![*t, *f],
            Term::Return(_) | Term::Unreachable => vec![],
        }
    }

    pub fn successors_mut(&mut self) -> Vec<&mut BlockId> {
        match self {
            Term::Goto(b) => vec![b],
            Term::Branch(_, t, f) => vec![t, f],
            Term::Return(_) | Term::Unreachable => vec![],
        }
    }
}

impl Func {
    pub fn operand_ty(&self, o: &Operand) -> Type {
        match o {
            Operand::Var(v) => self.vars[*v].ty.clone(),
            Operand::Int(_) => Type::Int,
            Operand::Float(_) => Type::Float,
            Operand::Bool(_) => Type::Bool,
            Operand::Str(_) => Type::Str,
            Operand::Null(t) => t.clone(),
        }
    }

    pub fn predecessors(&self) -> Vec<Vec<BlockId>> {
        let mut preds = vec![Vec::new(); self.blocks.len()];
        for (b, block) in self.blocks.iter().enumerate() {
            for s in block.term.successors() {
                preds[s].push(b);
            }
        }
        preds
    }
}

/// Does a value of this type live on the heap (and so need reference counting)?
/// A nullable reference is a possibly-null pointer, so it counts too. An enum
/// does when some variant carries data.
pub fn is_ref(t: &Type) -> bool {
    matches!(t.non_null(), Type::Str | Type::Bytes | Type::List(_) | Type::Map(..) | Type::Struct(_) | Type::Enum(_, true) | Type::Func(..) | Type::Iface(_) | Type::Handle)
}

impl Operand {
    pub fn var(&self) -> Option<VarId> {
        match self {
            Operand::Var(v) => Some(*v),
            _ => None,
        }
    }
}

impl Rvalue {
    /// Every operand this rvalue reads, in evaluation order.
    pub fn operands(&self) -> Vec<&Operand> {
        match self {
            Rvalue::Use(o) | Rvalue::Neg(_, o, _) | Rvalue::Not(o) => vec![o],
            Rvalue::Binary(_, _, a, b, _) => vec![a, b],
            Rvalue::Call(_, args, _)
            | Rvalue::Builtin(_, args, _)
            | Rvalue::New(_, args)
            | Rvalue::Reuse(_, _, args)
            | Rvalue::ListNew(_, args)
            | Rvalue::Closure(_, _, args) => args.iter().collect(),
            Rvalue::CallIndirect(f, args, _) | Rvalue::CallMethod(f, _, args, ..) => std::iter::once(f).chain(args).collect(),
            Rvalue::Sync(_, _, args, _) => args.iter().collect(),
            Rvalue::Boxed(_, o) => vec![o],
            Rvalue::Field(..) | Rvalue::Index(..) | Rvalue::Tag(_) | Rvalue::VariantField(..) | Rvalue::MapNew(..) | Rvalue::IsType(..) | Rvalue::Unbox(..) => vec![],
            // (the list / object of a projection is a VarId, see `base`)
        }
    }

    /// The object a projection reads from.
    pub fn base(&self) -> Option<VarId> {
        match self {
            Rvalue::Field(o, ..) | Rvalue::VariantField(o, ..) | Rvalue::Tag(o) | Rvalue::IsType(o, _) | Rvalue::Unbox(o, _) => Some(*o),
            Rvalue::Index(l, ..) => Some(*l),
            _ => None,
        }
    }

    /// Every variable this rvalue reads (operands, projection bases, indices).
    pub fn uses(&self) -> Vec<VarId> {
        let mut out: Vec<VarId> = self.operands().iter().filter_map(|o| o.var()).collect();
        match self {
            Rvalue::Field(o, ..) | Rvalue::VariantField(o, ..) | Rvalue::Tag(o) | Rvalue::IsType(o, _) | Rvalue::Unbox(o, _) => out.push(*o),
            Rvalue::Index(l, i, _) => {
                out.push(*l);
                out.extend(i.var());
            }
            Rvalue::Reuse(tok, ..) => out.push(*tok),
            _ => {}
        }
        out
    }
}

impl Stmt {
    /// Variables read by this statement.
    pub fn uses(&self) -> Vec<VarId> {
        match self {
            Stmt::Assign(_, r) | Stmt::Eval(r) => r.uses(),
            Stmt::Store { place, value, .. } => {
                let mut out = Vec::new();
                match place {
                    Place::Field { obj, .. } => out.push(*obj),
                    Place::Index { list, index, .. } => {
                        out.push(*list);
                        out.extend(index.var());
                    }
                }
                out.extend(value.var());
                out
            }
            Stmt::Dup(v) | Stmt::Drop(v) | Stmt::DropReuse(v, _) | Stmt::KeepAlive(v) => vec![*v],
        }
    }

    /// Variables written by this statement.
    pub fn defs(&self) -> Vec<VarId> {
        match self {
            Stmt::Assign(v, _) => vec![*v],
            Stmt::Store { old: Some(o), .. } => vec![*o],
            Stmt::DropReuse(_, tok) => vec![*tok],
            _ => vec![],
        }
    }
}

// ---------------------------------------------------------------- printing

pub fn print_program(p: &Program) -> String {
    let names: Vec<String> = p.structs.iter().map(|s| s.name.clone()).collect();
    let mut out = String::new();
    for f in p.funcs.iter().filter(|f| !f.generic) {
        print_func(&mut out, f, p, &names);
        out.push('\n');
    }
    out
}

fn print_func(out: &mut String, f: &Func, p: &Program, names: &[String]) {
    let var = |v: VarId| format!("{}%{v}", f.vars[v].name);
    let params: Vec<String> = f
        .params
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let own = if f.param_owned.get(i).copied().unwrap_or(false) { "owned " } else { "" };
            format!("{own}{}: {}", var(v), f.vars[v].ty.display(names, &[]))
        })
        .collect();
    let _ = writeln!(out, "fn {}({}) -> {}:", f.name, params.join(", "), f.ret.display(names, &[]));
    let op = |o: &Operand| match o {
        Operand::Var(v) => var(*v),
        Operand::Int(i) => i.to_string(),
        Operand::Float(x) => format!("{x:?}"),
        Operand::Bool(b) => b.to_string(),
        Operand::Str(s) => format!("{s:?}"),
        Operand::Null(_) => "null".to_string(),
    };
    let ops = |os: &[Operand]| os.iter().map(op).collect::<Vec<_>>().join(", ");
    let ctor = |c: &Ctor, args: &[Operand]| match c {
        Ctor::Struct(sid) => format!("{}({})", p.structs[*sid].name, ops(args)),
        Ctor::Variant(sid, k) => {
            let v = &p.structs[*sid].variants()[*k];
            if args.is_empty() { format!("{}.{}", p.structs[*sid].name, v.name) } else { format!("{}.{}({})", p.structs[*sid].name, v.name, ops(args)) }
        }
    };
    let rv = |r: &Rvalue| match r {
        Rvalue::Use(o) => op(o),
        Rvalue::Binary(b, _, x, y, _) => format!("{} {} {}", op(x), binop_str(*b), op(y)),
        Rvalue::Neg(_, x, _) => format!("-{}", op(x)),
        Rvalue::Not(x) => format!("not {}", op(x)),
        Rvalue::Call(fid, args, _) => format!("{}({})", p.funcs[*fid].name, ops(args)),
        Rvalue::Builtin(b, args, _) => format!("@{b:?}({})", ops(args)),
        Rvalue::Field(o, sid, idx) => format!("{}.{}", var(*o), p.structs[*sid].fields[*idx].name),
        Rvalue::Index(l, i, checked) => format!("{}[{}]{}", var(*l), op(i), if checked.is_some() { "" } else { "!" }),
        Rvalue::New(c, args) => format!("new {}", ctor(c, args)),
        Rvalue::Reuse(tok, c, args) => format!("new {} reusing {}", ctor(c, args), var(*tok)),
        Rvalue::Tag(o) => format!("tag {}", var(*o)),
        Rvalue::VariantField(o, sid, k, i) => {
            let v = &p.structs[*sid].variants()[*k];
            format!("{} as {}.{}", var(*o), v.name, v.fields[*i].name)
        }
        Rvalue::ListNew(_, args) => format!("[{}]", ops(args)),
        Rvalue::MapNew(..) => "{}".to_string(),
        Rvalue::Closure(fid, _, caps) if caps.is_empty() => format!("fn {}", p.funcs[*fid].name),
        Rvalue::Closure(fid, _, caps) => format!("closure {}[{}]", p.funcs[*fid].name, ops(caps)),
        Rvalue::CallIndirect(f, args, _) => format!("{}({})", op(f), ops(args)),
        Rvalue::Boxed(sid, o) => format!("box {} as {}", op(o), p.structs[*sid].closure.as_ref().map(|t| t.display(names, &[])).unwrap_or_default()),
        Rvalue::CallMethod(o, slot, args, ..) => format!("{}.<method {slot}>({})", op(o), ops(args)),
        Rvalue::IsType(o, t) => format!("{} is {}", var(*o), t.display(names, &[])),
        Rvalue::Sync(op, _, args, _) => format!("@{op:?}({})", ops(args)),
        Rvalue::Unbox(o, t) => format!("{} as {}", var(*o), t.display(names, &[])),
    };
    for (b, block) in f.blocks.iter().enumerate() {
        let _ = writeln!(out, "  bb{b}:");
        for s in &block.stmts {
            let line = match s {
                Stmt::Assign(v, r) => format!("{} = {}", var(*v), rv(r)),
                Stmt::Eval(r) => rv(r),
                Stmt::Store { place, value, old } => {
                    let pl = match place {
                        Place::Field { obj, sid, idx } => format!("{}.{}", var(*obj), p.structs[*sid].fields[*idx].name),
                        Place::Index { list, index, .. } => format!("{}[{}]", var(*list), op(index)),
                    };
                    match old {
                        Some(o) => format!("{} = swap {pl} <- {}", var(*o), op(value)),
                        None => format!("{pl} <- {}", op(value)),
                    }
                }
                Stmt::Dup(v) => format!("dup {}", var(*v)),
                Stmt::Drop(v) => format!("drop {}", var(*v)),
                Stmt::DropReuse(v, tok) => format!("{} = drop-reuse {}", var(*tok), var(*v)),
                Stmt::KeepAlive(v) => format!("keep-alive {}", var(*v)),
            };
            let _ = writeln!(out, "    {line}");
        }
        let t = match &block.term {
            Term::Goto(t) => format!("goto bb{t}"),
            Term::Branch(c, t, e) => format!("if {} goto bb{t} else bb{e}", op(c)),
            Term::Return(None) => "return".to_string(),
            Term::Return(Some(o)) => format!("return {}", op(o)),
            Term::Unreachable => "unreachable".to_string(),
        };
        let _ = writeln!(out, "    {t}");
    }
}

fn binop_str(b: BinOp) -> &'static str {
    match b {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::Cmp => "<=>",
        _ => "?",
    }
}

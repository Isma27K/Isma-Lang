//! Typed AST produced by the checker and consumed by codegen.
//! Every name is resolved and every expression carries its type.

use crate::ast::BinOp;
use crate::span::Span;
use crate::types::*;

#[derive(Debug)]
pub struct TProgram {
    pub structs: Vec<TStruct>,
    pub funcs: Vec<TFunc>,
}

/// A struct, or an enum (`variants` is `Some`): both live in one table, so
/// a `StructId` names either.
#[derive(Debug, Clone)]
pub struct TStruct {
    pub name: String,
    pub fields: Vec<TField>,
    pub variants: Option<Vec<TVariant>>,
}

#[derive(Debug, Clone)]
pub struct TVariant {
    pub name: String,
    pub fields: Vec<TField>,
}

impl TStruct {
    pub fn variants(&self) -> &[TVariant] {
        self.variants.as_deref().unwrap_or(&[])
    }
}

#[derive(Debug, Clone)]
pub struct TField {
    pub name: String,
    pub ty: Type,
    pub mutable: bool,
}

#[derive(Debug)]
pub struct TFunc {
    pub name: String,
    pub symbol: String,
    pub params: Vec<LocalId>,
    pub ret: Type,
    pub locals: Vec<TLocal>,
    pub body: Vec<TStmt>,
    pub exported: bool,
    /// Standard-library functions report panics at their caller's location
    /// (like Rust's `#[track_caller]`): they take it as hidden arguments.
    pub track_caller: bool,
}

#[derive(Debug, Clone)]
pub struct TLocal {
    pub name: String,
    pub ty: Type,
}

#[derive(Debug)]
pub enum TStmt {
    Let(LocalId, TExpr),
    Assign { place: Place, op: Option<CompoundOp>, value: TExpr, span: Span },
    Expr(TExpr),
    If { branches: Vec<(TExpr, Vec<TStmt>)>, else_block: Option<Vec<TStmt>> },
    While { cond: TExpr, body: Vec<TStmt> },
    ForRange { var: LocalId, start: TExpr, end: TExpr, inclusive: bool, body: Vec<TStmt> },
    ForList { var: LocalId, list: TExpr, body: Vec<TStmt> },
    /// Exhaustive (checked): exactly one arm runs.
    Match { scrutinee: TExpr, arms: Vec<TArm> },
    Break,
    Continue,
    Return(Option<TExpr>),
}

#[derive(Debug)]
pub struct TArm {
    pub pat: TPat,
    pub guard: Option<TExpr>,
    pub body: Vec<TStmt>,
}

/// A checked pattern; `ty` is the type of the value it's matched against.
#[derive(Debug, Clone)]
pub struct TPat {
    pub kind: TPatKind,
    pub ty: Type,
}

#[derive(Debug, Clone)]
pub enum TPatKind {
    Wild,
    /// Matches when the inner pattern does, and binds the value to the local.
    Bind(LocalId, Box<TPat>),
    /// A variant of an enum, with one pattern per field.
    Variant(StructId, usize, Vec<TPat>),
    /// A struct, with one pattern per field.
    Struct(StructId, Vec<TPat>),
    Int(i64),
    Str(String),
    Bool(bool),
    Null,
    /// A non-null value of a `T?` whose `T` matches the inner pattern.
    Some(Box<TPat>),
    Or(Vec<TPat>),
}

#[derive(Debug)]
pub enum Place {
    Local(LocalId),
    Field { obj: TExpr, sid: StructId, idx: usize },
    Index { list: TExpr, index: TExpr, span: Span },
    /// `grid[i]` on a type with operator functions: `set` is its `fn []=`,
    /// `get` its `fn []` (needed by `grid[i] += x`). `ty` is the element type.
    Custom { obj: TExpr, index: TExpr, get: Option<FuncId>, set: FuncId, ty: Type },
}

/// What `x op= v` computes from the current value of `x` and `v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompoundOp {
    /// On numbers and strings.
    Builtin(BinOp),
    /// An operator function: `x = f(x, v)`.
    Call(FuncId),
}

#[derive(Debug)]
pub struct TExpr {
    pub kind: TExprKind,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Print,
    Input,
    Args,
    Panic,
    Assert,
    Sqrt,
    Exit,
    IntToFloat,
    FloatToInt,
    StrToInt,
    StrToFloat,
    StrChars,
    StrSubstring,
    StrContains,
    StrStartsWith,
    StrEndsWith,
    RealtimeNs,
    MonotonicNs,
    SleepNs,
    ToStr,
    StrLen,
    ListLen,
    ListPush,
    ListPop,
    /// Internal (loop counters): `x + 1` that is known not to overflow.
    Inc,
    /// Internal: `a + b` on strings, consuming `a` (appends in place when unique).
    StrAppendOwned,
    /// `s.to_int_or_null()` / `s.to_float_or_null()`
    StrToIntOrNull,
    StrToFloatOrNull,
    /// Internal (nullable primitives): is it `null`? / `T` -> `T?` / `T?` -> `T` (unchecked).
    IsNull,
    Wrap,
    Unwrap,
}

#[derive(Debug)]
pub enum TExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Local(LocalId),
    Call(FuncId, Vec<TExpr>),
    Builtin(Builtin, Vec<TExpr>),
    /// Arithmetic / comparison; operand type is `lhs.ty`.
    Binary(BinOp, Box<TExpr>, Box<TExpr>),
    Neg(Box<TExpr>),
    Not(Box<TExpr>),
    And(Box<TExpr>, Box<TExpr>),
    Or(Box<TExpr>, Box<TExpr>),
    Field(Box<TExpr>, StructId, usize),
    Index(Box<TExpr>, Box<TExpr>),
    StructNew(StructId, Vec<TExpr>),
    /// `Shape.Circle(r)`: enum, variant, one value per field.
    EnumNew(StructId, usize, Vec<TExpr>),
    ListLit(Vec<TExpr>),
    /// Concatenation of String-typed parts.
    FString(Vec<TExpr>),
    /// `null`, of the (nullable) type of the expression.
    Null,
    /// `e == null` (`e` may also be of a non-null type: then it's just `false`).
    IsNull(Box<TExpr>),
    /// A `T` used where a `T?` is expected.
    Wrap(Box<TExpr>),
    /// A `T?` known to be non-null here (a smart cast).
    Unwrap(Box<TExpr>),
    /// `e!!`: panics if `e` is null.
    NotNull(Box<TExpr>),
    /// `recv?.member`: if `recv` is null the result is null; otherwise `binder`
    /// is set to the (non-null) receiver and `body` is evaluated.
    SafeCall { recv: Box<TExpr>, binder: LocalId, body: Box<TExpr> },
    /// `a ?: b`: `b` is evaluated only when `a` is null.
    Elvis(Box<TExpr>, Box<TExpr>),
    /// `return`/`break`/`continue` on the right of `?:`.
    Jump(Box<TStmt>),
}

impl TExpr {
    /// Does evaluating this expression never finish normally (`panic`, `exit`, a jump)?
    pub fn diverges(&self) -> bool {
        matches!(self.kind, TExprKind::Jump(_) | TExprKind::Builtin(Builtin::Panic | Builtin::Exit, _))
    }
}

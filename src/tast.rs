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

#[derive(Debug, Clone)]
pub struct TStruct {
    pub name: String,
    pub fields: Vec<TField>,
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
}

#[derive(Debug, Clone)]
pub struct TLocal {
    pub name: String,
    pub ty: Type,
}

#[derive(Debug)]
pub enum TStmt {
    Let(LocalId, TExpr),
    Assign { place: Place, op: Option<BinOp>, value: TExpr, span: Span },
    Expr(TExpr),
    If { branches: Vec<(TExpr, Vec<TStmt>)>, else_block: Option<Vec<TStmt>> },
    While { cond: TExpr, body: Vec<TStmt> },
    ForRange { var: LocalId, start: TExpr, end: TExpr, inclusive: bool, body: Vec<TStmt> },
    ForList { var: LocalId, list: TExpr, body: Vec<TStmt> },
    Break,
    Continue,
    Return(Option<TExpr>),
}

#[derive(Debug)]
pub enum Place {
    Local(LocalId),
    Field { obj: TExpr, sid: StructId, idx: usize },
    Index { list: TExpr, index: TExpr, span: Span },
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
    ToStr,
    StrLen,
    ListLen,
    ListPush,
    ListPop,
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
    ListLit(Vec<TExpr>),
    /// Concatenation of String-typed parts.
    FString(Vec<TExpr>),
}

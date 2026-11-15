//! Untyped syntax tree produced by the parser.

use crate::span::Span;

#[derive(Debug, Clone)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Debug)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Debug)]
pub enum Item {
    Fn(FnDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    /// `import time`, `import shapes.circle as c`, `from ..geometry import Point`
    Import(Import),
    /// `val pi = 3.14159`: a constant, worked out when compiling.
    Const(ConstDecl),
}

#[derive(Debug)]
pub struct ConstDecl {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Import {
    /// Leading dots of a `from` import: `from .x` is 1, `from ..x` is 2.
    pub level: usize,
    /// `shapes.circle`: a file path relative to the importing file. Empty in `from .. import x`.
    pub path: Vec<Ident>,
    /// `import geometry as geo`
    pub alias: Option<Ident>,
    /// `Some` for `from path import a, b as c`.
    pub names: Option<Vec<ImportName>>,
    pub span: Span,
}

impl Import {
    /// `..shapes.circle`, as written.
    pub fn path_text(&self) -> String {
        let dots = ".".repeat(self.level);
        let segs: Vec<&str> = self.path.iter().map(|i| i.name.as_str()).collect();
        format!("{dots}{}", segs.join("."))
    }

    /// The span of the module path (or of the dots alone).
    pub fn path_span(&self) -> Span {
        match (self.path.first(), self.path.last()) {
            (Some(a), Some(b)) => a.span.merge(b.span),
            _ => self.span,
        }
    }

    /// The name a plain `import` binds: the alias, else the last segment.
    pub fn binding(&self) -> Option<&Ident> {
        self.alias.as_ref().or(self.path.last())
    }
}

#[derive(Debug, Clone)]
pub struct ImportName {
    pub name: Ident,
    pub alias: Option<Ident>,
}

impl ImportName {
    pub fn binding(&self) -> &Ident {
        self.alias.as_ref().unwrap_or(&self.name)
    }
}

#[derive(Debug)]
pub struct FnDecl {
    pub name: Ident,
    /// `[T, U]` in `fn pair[T, U](a: T, b: U)`.
    pub type_params: Vec<Ident>,
    /// Span of `self` if the first parameter is `self`.
    pub self_param: Option<Span>,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Block,
    /// Span of the signature (`fn name(...) -> T`).
    pub sig_span: Span,
}

#[derive(Debug)]
pub struct Param {
    pub name: Ident,
    pub ty: TypeExpr,
}

#[derive(Debug)]
pub struct StructDecl {
    pub name: Ident,
    /// `[T]` in `struct Box[T]:`.
    pub type_params: Vec<Ident>,
    pub fields: Vec<FieldDecl>,
    pub methods: Vec<FnDecl>,
}

/// `enum Shape:` with its variants (`Circle(radius: Float)`, `Empty`) and methods.
#[derive(Debug)]
pub struct EnumDecl {
    pub name: Ident,
    /// `[T]` in `enum Tree[T]:`.
    pub type_params: Vec<Ident>,
    pub variants: Vec<VariantDecl>,
    pub methods: Vec<FnDecl>,
}

#[derive(Debug)]
pub struct VariantDecl {
    pub name: Ident,
    /// Always immutable.
    pub fields: Vec<FieldDecl>,
    pub span: Span,
}

#[derive(Debug)]
pub struct FieldDecl {
    pub name: Ident,
    pub ty: TypeExpr,
    pub mutable: bool,
}

#[derive(Debug, Clone)]
pub struct TypeExpr {
    /// `time` in `time.DateTime`.
    pub module: Option<Ident>,
    pub name: Ident,
    pub args: Vec<TypeExpr>,
    /// `T?`
    pub nullable: bool,
    pub span: Span,
}

pub type Block = Vec<Stmt>;

#[derive(Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum StmtKind {
    Let { mutable: bool, name: Ident, ty: Option<TypeExpr>, value: Expr },
    Assign { target: Expr, op: Option<BinOp>, value: Expr },
    Expr(Expr),
    If { branches: Vec<(Expr, Block)>, else_block: Option<Block> },
    While { cond: Expr, body: Block },
    /// `for x in xs:`, or with two names: `for i, x in xs:` (index and
    /// element) and `for k, v in m:` (key and value).
    For { var: Ident, second: Option<Ident>, iter: ForIter, body: Block },
    Match { scrutinee: Expr, arms: Vec<MatchArm> },
    Break,
    Continue,
    Return(Option<Expr>),
    Pass,
}

#[derive(Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    /// `case p if cond:`
    pub guard: Option<Expr>,
    pub body: Block,
    /// From `case` to the end of the pattern (or guard).
    pub span: Span,
}

#[derive(Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum PatternKind {
    /// `_`
    Wild,
    /// A lowercase name: matches anything and binds it.
    Bind(Ident),
    /// `Circle`, `Circle(r)`, `Shape.Circle(radius=r)`, `Point(x, _)`:
    /// a (possibly qualified) variant or struct name, and its field patterns
    /// if parenthesized.
    Ctor { path: Vec<Ident>, args: Option<Vec<PatArg>> },
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
    /// `a | b`
    Or(Vec<Pattern>),
    /// `p as name`
    As(Box<Pattern>, Ident),
}

#[derive(Debug)]
pub struct PatArg {
    pub name: Option<Ident>,
    pub pat: Pattern,
}

#[derive(Debug)]
pub enum ForIter {
    Range { start: Expr, end: Expr, inclusive: bool },
    Expr(Expr),
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    FStr(Vec<FStrPart>),
    Name(String),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    Call { callee: Box<Expr>, args: Vec<Arg> },
    Field(Box<Expr>, Ident),
    /// `obj?.field` (and, as a callee, `obj?.method(...)`).
    SafeField(Box<Expr>, Ident),
    Index(Box<Expr>, Box<Expr>),
    /// `max[Int]` in `max[Int](a, b)`, `Box[String]` in `Box[String](s)`,
    /// `Tree[Int]` in `Tree[Int].Leaf`: a generic function or type given its
    /// type arguments explicitly.
    TypeApply(Box<Expr>, Vec<TypeExpr>),
    List(Vec<Expr>),
    /// `{"a": 1, "b": 2}`, or `{}`.
    Map(Vec<(Expr, Expr)>),
    /// `x in xs` / `x not in xs`.
    In { item: Box<Expr>, container: Box<Expr>, negated: bool },
    Null,
    /// `x!!`
    NotNull(Box<Expr>),
    /// `a ?: b`
    Elvis(Box<Expr>, Box<Expr>),
    /// `return`, `break` or `continue` on the right of `?:`.
    Jump(Jump),
    /// Placeholder produced after a parse error.
    Error,
}

#[derive(Debug)]
pub enum Jump {
    Return(Option<Box<Expr>>),
    Break,
    Continue,
}

#[derive(Debug)]
pub enum FStrPart {
    Lit(String),
    Expr(Expr),
}

#[derive(Debug)]
pub struct Arg {
    pub name: Option<Ident>,
    pub value: Expr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `a <=> b`: -1, 0 or 1.
    Cmp,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
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
            BinOp::And => "and",
            BinOp::Or => "or",
        }
    }

    /// Does it produce a `Bool` from two values? (`<=>` produces an `Int`.)
    pub fn is_comparison(self) -> bool {
        matches!(self, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
    }

    /// `<`, `<=`, `>`, `>=`: the orderings, which user types get from `<=>`.
    pub fn is_ordering(self) -> bool {
        matches!(self, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
    }
}

/// The names of operator functions, as written after `fn`.
pub const OPERATOR_NAMES: &[&str] = &["+", "-", "*", "/", "%", "==", "<=>", "[]", "[]="];

pub fn is_operator_name(name: &str) -> bool {
    OPERATOR_NAMES.contains(&name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

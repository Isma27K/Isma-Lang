//! Typed AST produced by the checker and consumed by codegen.
//! Every name is resolved and every expression carries its type.

use crate::ast::BinOp;
use crate::span::Span;
use crate::types::*;

#[derive(Debug)]
pub struct TProgram {
    pub structs: Vec<TStruct>,
    pub funcs: Vec<TFunc>,
    /// Struct or enum types used as map keys: their `fn hash` and `fn ==`.
    pub map_keys: Vec<MapKey>,
}

/// How a map hashes and compares keys of a user type.
#[derive(Debug, Clone, Copy)]
pub struct MapKey {
    pub sid: StructId,
    pub hash: FuncId,
    pub eq: FuncId,
}

/// A struct, or an enum (`variants` is `Some`): both live in one table, so
/// a `StructId` names either.
#[derive(Debug, Clone)]
pub struct TStruct {
    pub name: String,
    pub fields: Vec<TField>,
    pub variants: Option<Vec<TVariant>>,
    /// A generic type, or an instance of one whose type arguments still
    /// contain type parameters (`Box[T]`): only generic code uses it, so the
    /// back end skips it.
    pub generic: bool,
    /// The values one lambda captured (its fields, all immutable), and the
    /// lambda's function type. Its closures are values of that type.
    ///
    /// Or a *box*: one value (its only field) of a type that fits an
    /// interface, and the interface type (`Type::Iface`). Boxes are the
    /// values of that interface type, laid out like closures, with the
    /// method table where a closure has its code.
    pub closure: Option<Type>,
    /// A box's method table: for each method of its interface, in order, the
    /// closure code that calls it on the boxed value. Empty otherwise.
    pub vtable: Vec<FuncId>,
    /// A struct's `fn drop(self)`: run when its last reference goes away,
    /// before its fields are released.
    pub drop: Option<FuncId>,
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
    /// A generic function (or method of a generic type), as written: an empty
    /// placeholder. Its copies for concrete types (`max[Int]`) are the ones
    /// that run.
    pub generic: bool,
    /// A `fn drop(self)`: its parameter stays borrowed (it runs while the
    /// value is being freed).
    pub is_drop: bool,
    /// The code of a function value: a lambda's body, or a named function
    /// wrapped to be called through a value. Its first parameter is the
    /// closure itself (holding the captured values), and every parameter is
    /// borrowed, since callers can't know which function they call.
    pub closure: bool,
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
    /// `for x in xs:` / `for i, x in xs:` (`index` is `i`).
    ForList { var: LocalId, index: Option<LocalId>, list: TExpr, body: Vec<TStmt> },
    /// `for k in m:` / `for k, v in m:`, in insertion order. The span is for
    /// the panic when the body adds or removes keys.
    ForMap { key: LocalId, value: Option<LocalId>, map: TExpr, body: Vec<TStmt>, span: Span },
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
    /// `m[k] = v` on a `Map` (`m[k] += v` reads it first, panicking if missing).
    MapEntry { map: TExpr, key: TExpr, span: Span },
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
    /// `s.char_count()`: the number of characters (not bytes).
    StrCharCount,
    /// Internal (`for c in s`): the character starting at byte index `i`, as a string.
    StrCharAt,
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
    StrSplit,
    StrSplitWhitespace,
    StrLines,
    StrTrim,
    StrTrimStart,
    StrTrimEnd,
    StrUpper,
    StrLower,
    StrReplace,
    /// `s.find(t) -> Int?`: byte index of the first match.
    StrFind,
    /// `s.rfind(t) -> Int?`: byte index of the last match.
    StrRfind,
    StrRepeat,
    StrCount,
    StrPadStart,
    StrPadEnd,
    /// `", ".join(...)`-style, as `parts.join(sep)` on a `List[String]`.
    ListJoin,
    ListInsert,
    ListRemoveAt,
    ListClear,
    ListReverse,
    /// `xs.sort()` on a list of `Int`, `Float` or `String`.
    ListSort,
    /// `ListSortBy(xs, keys)`: sort `xs` in place, stably, by the matching
    /// `keys` (a list of `Int`, `Float` or `String` as long as `xs`).
    ListSortBy,
    /// `m[k]`: panics if `k` is missing.
    MapGet,
    /// `m.get(k) -> V?`
    MapGetOrNull,
    /// `m.get(k, default) -> V`
    MapGetOr,
    /// `m[k] = v` (consumes the key and value).
    MapSet,
    MapContains,
    /// `m.remove(k) -> V?`
    MapRemove,
    MapLen,
    MapClear,
    /// `m.keys()` / `m.values()`: new lists, in insertion order.
    MapKeys,
    MapValues,
    /// Internal (`for` loops, `keys()`, `values()`): `MapNext(m, i, len)` is the
    /// index of the next entry at or after `i`, or -1 at the end; panics if the
    /// map's size isn't `len` any more.
    MapNext,
    /// Internal: the key / value of the entry at an index from `MapNext`.
    MapKeyAt,
    MapValueAt,
    /// A string as it's written in code: `"a\"b"` (for printing lists and maps).
    StrRepr,
    /// `hash(x)` of one value of a type with a built-in hash.
    HashOf,
    /// `hash(a, b, ...)`: the hashes of the parts, mixed into one `Int`.
    HashCombine,
    /// Math (`math` module intrinsics), on `Float`s.
    MathUnary(MathFn),
    MathPow,
    MathAtan2,
    /// File system (`fs` module intrinsics).
    Fs(FsOp),
    /// A runtime function of `stdlib::RT` (its index there), called directly.
    Rt(u16),
    /// Internal (nullable primitives): is it `null`? / `T` -> `T?` / `T?` -> `T` (unchecked).
    IsNull,
    Wrap,
    Unwrap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathFn {
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Sinh,
    Cosh,
    Tanh,
    Exp,
    Ln,
    Log2,
    Log10,
    Floor,
    Ceil,
    Round,
    Trunc,
    Abs,
    Cbrt,
}

impl MathFn {
    pub const ALL: &[(&str, MathFn)] = &[
        ("__sin", MathFn::Sin),
        ("__cos", MathFn::Cos),
        ("__tan", MathFn::Tan),
        ("__asin", MathFn::Asin),
        ("__acos", MathFn::Acos),
        ("__atan", MathFn::Atan),
        ("__sinh", MathFn::Sinh),
        ("__cosh", MathFn::Cosh),
        ("__tanh", MathFn::Tanh),
        ("__exp", MathFn::Exp),
        ("__ln", MathFn::Ln),
        ("__log2", MathFn::Log2),
        ("__log10", MathFn::Log10),
        ("__floor", MathFn::Floor),
        ("__ceil", MathFn::Ceil),
        ("__round", MathFn::Round),
        ("__trunc", MathFn::Trunc),
        ("__fabs", MathFn::Abs),
        ("__cbrt", MathFn::Cbrt),
    ];
}

/// The file-system operations behind the `fs` module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsOp {
    /// `(path) -> String?`
    Read,
    /// `(path, text, append) -> Bool`
    Write,
    /// `(path) -> Int`: 0 missing, 1 file, 2 directory, 3 other.
    Kind,
    /// `(path) -> List[String]?`: sorted names.
    ListDir,
    /// `(path) -> Bool`
    MakeDir,
    Remove,
    /// `(from, to) -> Bool`
    Rename,
    /// `() -> String`: why the last failed operation failed.
    LastError,
    /// `() -> String`
    Cwd,
    /// `() -> String`: everything left on stdin.
    ReadStdin,
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
    /// Evaluate the first expression into the local, then the second.
    Let(LocalId, Box<TExpr>, Box<TExpr>),
    /// `{k: v, ...}` of the expression's (`Map`) type.
    MapLit(Vec<(TExpr, TExpr)>),
    /// `x in xs`, `xs.contains(x)` (Bool) or `xs.index_of(x)` (`Int?`): the
    /// list's elements are put in `elem` in turn and `test` (which reads
    /// `elem` and `needle`, set to the value looked for) says if one matches.
    ListFind { list: Box<TExpr>, value: Box<TExpr>, elem: LocalId, needle: LocalId, test: Box<TExpr>, index: bool },
    /// `[1, 2, 3]`: the list's elements are put in `elem` in turn, and `each`
    /// (which reads it) formats one.
    ListFormat { list: Box<TExpr>, elem: LocalId, each: Box<TExpr> },
    /// `{"a": 1}`: like `ListFormat`, for each key and value.
    MapFormat { map: Box<TExpr>, key: LocalId, value: LocalId, key_str: Box<TExpr>, value_str: Box<TExpr> },
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
    /// `e?` (and `e!!`) on a `Result`: the `Ok`'s value; for an `Err`,
    /// `binder` is set to its error and `on_err` runs (a `return`, or a panic).
    Try { value: Box<TExpr>, binder: LocalId, on_err: Box<TExpr> },
    /// `recv?.member`: if `recv` is null the result is null; otherwise `binder`
    /// is set to the (non-null) receiver and `body` is evaluated.
    SafeCall { recv: Box<TExpr>, binder: LocalId, body: Box<TExpr> },
    /// `a ?: b`: `b` is evaluated only when `a` is null.
    Elvis(Box<TExpr>, Box<TExpr>),
    /// `return`/`break`/`continue` on the right of `?:`.
    Jump(Box<TStmt>),
    /// A function value: `func` (closure code, see `TFunc::closure`) with the
    /// captured values, which become the fields of closure struct `sid`.
    /// Without captures, it's a constant (nothing is allocated).
    Closure { func: FuncId, sid: Option<StructId>, captures: Vec<TExpr> },
    /// Calling a function value: `f(x)`.
    CallValue(Box<TExpr>, Vec<TExpr>),
    /// A value used as an interface value: put in a new box (struct `.0`,
    /// see `TStruct::closure`) with the method table of its type.
    Boxed(StructId, Box<TExpr>),
    /// `s.area()` on an interface value: the method in slot `.1` of its
    /// table (the interface's `.1`-th method), with the arguments.
    CallMethod(Box<TExpr>, usize, Vec<TExpr>),
    /// Run the statements, then evaluate the expression (the code that the
    /// list methods taking functions, like `map` and `filter`, become).
    Block(Vec<TStmt>, Box<TExpr>),
}

impl TExpr {
    /// Does evaluating this expression never finish normally (`panic`, `exit`, a jump)?
    pub fn diverges(&self) -> bool {
        matches!(self.kind, TExprKind::Jump(_) | TExprKind::Builtin(Builtin::Panic | Builtin::Exit, _))
    }
}

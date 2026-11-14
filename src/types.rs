pub type StructId = usize;
pub type FuncId = usize;
pub type LocalId = usize;
/// A type parameter (`T` in `fn max[T]`): an index into the checker's table of them.
pub type ParamId = usize;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    Unit,
    List(Box<Type>),
    Struct(StructId),
    /// A user enum (its id indexes the same table as structs). The flag is
    /// true when some variant carries data: values are then reference-counted
    /// heap objects. Otherwise a value is just its variant's number.
    Enum(StructId, bool),
    /// `T?`: a `T` or `null`. Never nested, and never `Unit?`.
    Nullable(Box<Type>),
    /// The type of a bare `null` literal before it meets an expected type.
    Null,
    /// A type parameter inside generic code (`T` in `fn max[T](a: T, b: T)`).
    /// Generic code is checked once with its parameters as they are, then
    /// copied for every set of types it's used with: the back end never sees one.
    Param(ParamId),
    /// Produced after a type error; compatible with everything so one
    /// mistake doesn't cascade into a pile of follow-up errors.
    Error,
}

impl Type {
    pub fn is_error(&self) -> bool {
        match self {
            Type::Error => true,
            Type::List(t) | Type::Nullable(t) => t.is_error(),
            _ => false,
        }
    }

    /// `T?` (flattening `T??` and leaving `Unit`, `null` and errors alone).
    pub fn nullable(self) -> Type {
        match self {
            Type::Nullable(_) | Type::Null | Type::Unit | Type::Error => self,
            t => Type::Nullable(Box::new(t)),
        }
    }

    pub fn is_nullable(&self) -> bool {
        matches!(self, Type::Nullable(_) | Type::Null)
    }

    /// The type without its `?`.
    pub fn non_null(&self) -> &Type {
        match self {
            Type::Nullable(t) => t,
            t => t,
        }
    }

    /// Can a value of type `self` be used where `to` is expected? Exactly
    /// when the types are compatible, or `to` is `T?` and `self` is `T` or `null`.
    pub fn assignable(&self, to: &Type) -> bool {
        self.compatible(to) || matches!(to, Type::Nullable(inner) if *self == Type::Null || self.compatible(inner))
    }

    /// Compared with `==`, printed and formatted directly. An enum without
    /// data counts: it's just a number, printed as its variant's name.
    pub fn is_primitive(&self) -> bool {
        matches!(self, Type::Int | Type::Float | Type::Bool | Type::Str | Type::Enum(_, false))
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Type::Int | Type::Float)
    }

    /// Two types are compatible if equal, or either contains an error.
    pub fn compatible(&self, other: &Type) -> bool {
        match (self, other) {
            (Type::Error, _) | (_, Type::Error) => true,
            (Type::List(a), Type::List(b)) | (Type::Nullable(a), Type::Nullable(b)) => a.compatible(b),
            (a, b) => a == b,
        }
    }

    /// Does a type parameter occur in it? (Struct and enum instances count
    /// through `generic`: per struct id, whether its type arguments have one.)
    pub fn has_param(&self, generic: &[bool]) -> bool {
        match self {
            Type::Param(_) => true,
            Type::List(t) | Type::Nullable(t) => t.has_param(generic),
            Type::Struct(id) | Type::Enum(id, _) => generic.get(*id).copied().unwrap_or(false),
            _ => false,
        }
    }

    /// `struct_names` per struct id, `param_names` per type parameter.
    pub fn display(&self, struct_names: &[String], param_names: &[String]) -> String {
        match self {
            Type::Int => "Int".into(),
            Type::Float => "Float".into(),
            Type::Bool => "Bool".into(),
            Type::Str => "String".into(),
            Type::Unit => "Unit".into(),
            Type::List(t) => format!("List[{}]", t.display(struct_names, param_names)),
            Type::Struct(id) | Type::Enum(id, _) => struct_names[*id].clone(),
            Type::Nullable(t) => format!("{}?", t.display(struct_names, param_names)),
            Type::Param(p) => param_names.get(*p).cloned().unwrap_or_else(|| "T".into()),
            Type::Null => "null".into(),
            Type::Error => "{unknown}".into(),
        }
    }
}

/// A type's name without its module and type arguments: `Box` for `geo.Box[geo.Point]`.
pub fn short_name(name: &str) -> &str {
    let base = name.split('[').next().unwrap_or(name);
    base.rsplit('.').next().unwrap_or(base)
}

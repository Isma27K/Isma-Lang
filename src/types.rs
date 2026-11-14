pub type StructId = usize;
pub type FuncId = usize;
pub type LocalId = usize;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    Unit,
    List(Box<Type>),
    Struct(StructId),
    /// `T?`: a `T` or `null`. Never nested, and never `Unit?`.
    Nullable(Box<Type>),
    /// The type of a bare `null` literal before it meets an expected type.
    Null,
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

    pub fn is_primitive(&self) -> bool {
        matches!(self, Type::Int | Type::Float | Type::Bool | Type::Str)
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

    pub fn display(&self, struct_names: &[String]) -> String {
        match self {
            Type::Int => "Int".into(),
            Type::Float => "Float".into(),
            Type::Bool => "Bool".into(),
            Type::Str => "String".into(),
            Type::Unit => "Unit".into(),
            Type::List(t) => format!("List[{}]", t.display(struct_names)),
            Type::Struct(id) => struct_names[*id].clone(),
            Type::Nullable(t) => format!("{}?", t.display(struct_names)),
            Type::Null => "null".into(),
            Type::Error => "{unknown}".into(),
        }
    }
}

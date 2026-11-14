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
    /// Produced after a type error; compatible with everything so one
    /// mistake doesn't cascade into a pile of follow-up errors.
    Error,
}

impl Type {
    pub fn is_error(&self) -> bool {
        match self {
            Type::Error => true,
            Type::List(t) => t.is_error(),
            _ => false,
        }
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
            (Type::List(a), Type::List(b)) => a.compatible(b),
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
            Type::Error => "{unknown}".into(),
        }
    }
}

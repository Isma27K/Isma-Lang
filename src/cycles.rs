//! Which types can be part of a reference cycle.
//!
//! Reference counting alone can't free a cycle (`n.children.push(n)`), so
//! values of such types are tracked by the cycle collector. The analysis is
//! on the type graph: a type is *cyclic* when, following the types of its
//! fields and list elements, you can get back to it. Strings, `List[Int]`
//! and most structs are acyclic and never pay for cycle collection.

use crate::mir::is_ref;
use crate::tast::TStruct;
use crate::types::Type;

/// The reference-counted types directly inside a value of type `t`.
fn children(t: &Type, structs: &[TStruct]) -> Vec<Type> {
    match t {
        Type::Struct(sid) => structs[*sid].fields.iter().map(|f| f.ty.clone()).filter(|t| is_ref(t) && *t != Type::Str).collect(),
        Type::List(e) if is_ref(e) && **e != Type::Str => vec![(**e).clone()],
        _ => Vec::new(),
    }
}

/// Can a value of type `t` (transitively) contain another value of type `t`?
pub fn is_cyclic(t: &Type, structs: &[TStruct]) -> bool {
    let mut seen: Vec<Type> = Vec::new();
    let mut stack = children(t, structs);
    while let Some(u) = stack.pop() {
        if u == *t {
            return true;
        }
        if seen.contains(&u) {
            continue;
        }
        stack.extend(children(&u, structs));
        seen.push(u);
    }
    false
}

pub fn cyclic_structs(structs: &[TStruct]) -> Vec<bool> {
    (0..structs.len()).map(|sid| is_cyclic(&Type::Struct(sid), structs)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tast::TField;

    fn st(name: &str, fields: &[Type]) -> TStruct {
        TStruct {
            name: name.into(),
            fields: fields.iter().enumerate().map(|(i, t)| TField { name: format!("f{i}"), ty: t.clone(), mutable: true }).collect(),
        }
    }

    #[test]
    fn finds_cycles_through_lists_and_other_structs() {
        let list = |t: Type| Type::List(Box::new(t));
        let structs = vec![
            st("Node", &[Type::Int, list(Type::Struct(0))]),         // Node -> List[Node] -> Node
            st("Point", &[Type::Float, Type::Str]),                  // no references to structs
            st("A", &[list(Type::Struct(3))]),                       // A -> B -> A
            st("B", &[Type::Struct(2)]),
            st("Wrapper", &[Type::Struct(0), list(list(Type::Int))]), // contains a cyclic type, but isn't on a cycle
        ];
        assert_eq!(cyclic_structs(&structs), vec![true, false, true, true, false]);
        assert!(is_cyclic(&list(Type::Struct(0)), &structs));
        assert!(!is_cyclic(&list(Type::Struct(4)), &structs));
        assert!(!is_cyclic(&list(list(Type::Int)), &structs));
    }
}

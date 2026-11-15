//! Which types can be part of a reference cycle.
//!
//! Reference counting alone can't free a cycle (`n.children.push(n)`), so
//! values of such types are tracked by the cycle collector. The analysis is
//! on the type graph, whose edges go from a type to the types of its fields,
//! list elements and map keys and values.
//!
//! A cycle of values needs a reference that was stored *after* its target
//! existed. An immutable field (a `val` field, any enum variant field) can
//! only point at a value built before its owner, so a cycle made only of
//! immutable fields is impossible. A type is therefore *cyclic* when it lies
//! on a cycle of the type graph that goes through at least one mutable edge:
//! a `var` field or a list element. Strings, `List[Int]`, immutable trees
//! (`enum Expr: Add(a: Expr, b: Expr)`) and most structs never pay for cycle
//! collection.
//!
//! A function value can hold anything its lambda captured, so a function
//! type's children are the closure structs of every lambda of that type (in
//! the whole program), and those hold the captured values. Captures never
//! change, so these edges are immutable: `n.on_change = fn() -> n.value`
//! closes a cycle only because `on_change` is a `var` field. Likewise, an
//! interface type's children are its boxes (one per type used as it),
//! which each hold one value.

use crate::mir::is_ref;
use crate::tast::TStruct;
use crate::types::Type;

/// The reference-counted types directly inside a value of type `t`, and
/// whether each reference can be replaced after the value is built.
fn children(t: &Type, structs: &[TStruct]) -> Vec<(Type, bool)> {
    let refs = |ty: &Type| is_ref(ty) && *ty.non_null() != Type::Str;
    match t {
        Type::Struct(sid) => structs[*sid].fields.iter().filter(|f| refs(&f.ty)).map(|f| (f.ty.non_null().clone(), f.mutable)).collect(),
        Type::Enum(sid, true) => {
            structs[*sid].variants().iter().flat_map(|v| &v.fields).filter(|f| refs(&f.ty)).map(|f| (f.ty.non_null().clone(), false)).collect()
        }
        Type::List(e) if refs(e) => vec![(e.non_null().clone(), true)],
        // Entries are added to a map that already exists, like list elements.
        Type::Map(k, v) => [k, v].into_iter().filter(|t| refs(t)).map(|t| (t.non_null().clone(), true)).collect(),
        Type::Func(..) | Type::Iface(_) => {
            (0..structs.len()).filter(|&s| !structs[s].generic && structs[s].closure.as_ref() == Some(t)).map(|s| (Type::Struct(s), false)).collect()
        }
        _ => Vec::new(),
    }
}

/// Every type reachable from `t` (including `t`).
fn reachable(t: &Type, structs: &[TStruct]) -> Vec<Type> {
    let mut seen = vec![t.clone()];
    let mut i = 0;
    while i < seen.len() {
        for (c, _) in children(&seen[i].clone(), structs) {
            if !seen.contains(&c) {
                seen.push(c);
            }
        }
        i += 1;
    }
    seen
}

/// Can a value of type `t` end up (transitively) referring to itself?
pub fn is_cyclic(t: &Type, structs: &[TStruct]) -> bool {
    let t = t.non_null();
    // A mutable edge u -> v with t ->* u and v ->* t.
    reachable(t, structs).iter().any(|u| {
        children(u, structs).into_iter().any(|(v, mutable)| mutable && reachable(&v, structs).contains(t))
    })
}

pub fn cyclic_structs(structs: &[TStruct]) -> Vec<bool> {
    (0..structs.len())
        .map(|sid| {
            let t = match &structs[sid].variants {
                Some(vs) => Type::Enum(sid, vs.iter().any(|v| !v.fields.is_empty())),
                // A closure is counted as a value of its function type.
                None if structs[sid].closure.is_some() => structs[sid].closure.clone().unwrap(),
                None => Type::Struct(sid),
            };
            is_cyclic(&t, structs)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tast::TField;

    fn st(name: &str, fields: &[Type]) -> TStruct {
        TStruct {
            name: name.into(),
            fields: fields.iter().enumerate().map(|(i, t)| TField { name: format!("f{i}"), ty: t.clone(), mutable: true }).collect(),
            variants: None,
            generic: false,
            closure: None,
            vtable: Vec::new(),
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

    #[test]
    fn nullable_links_make_cycles() {
        let structs = vec![st("Node", &[Type::Int, Type::Struct(0).nullable()]), st("Opt", &[Type::Int.nullable(), Type::Str.nullable()])];
        assert_eq!(cyclic_structs(&structs), vec![true, false]);
        assert!(is_cyclic(&Type::Struct(0).nullable(), &structs));
    }

    #[test]
    fn immutable_links_never_form_cycles() {
        let fixed = |name: &str, fields: &[Type]| {
            let mut s = st(name, fields);
            for f in &mut s.fields {
                f.mutable = false;
            }
            s
        };
        let variant = |n: &str, fs: &[Type]| crate::tast::TVariant {
            name: n.into(),
            fields: fs.iter().map(|t| TField { name: "f".into(), ty: t.clone(), mutable: false }).collect(),
        };
        let expr = Type::Enum(1, true);
        let structs = vec![
            fixed("Cons", &[Type::Int, Type::Struct(0).nullable()]), // a `val next`: no cycle possible
            TStruct { name: "Expr".into(), fields: vec![], variants: Some(vec![variant("Num", &[Type::Int]), variant("Add", &[expr.clone(), expr.clone()])]), generic: false, closure: None, vtable: Vec::new() },
            TStruct { name: "Json".into(), fields: vec![], variants: Some(vec![variant("Arr", &[Type::List(Box::new(Type::Enum(2, true)))])]), generic: false, closure: None, vtable: Vec::new() },
        ];
        assert_eq!(cyclic_structs(&structs), vec![false, false, true]); // `Arr(items)`: `items.push(json)` can close a cycle
    }
}

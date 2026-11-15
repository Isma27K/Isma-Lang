//! `tarn doc --json`: everything the standard library and the built-in types
//! offer, with signatures and documentation, as JSON for tools (the website's
//! reference pages are generated from it).
//!
//! It comes from the same place as the editor's hovers and completions: the
//! standard library is checked like any program, and its doc comments and
//! signatures are read back from the checker's editor information.

use crate::ide::{self, Member, MemberKind};
use crate::types::Type;
use serde_json::{Value, json};

fn member(m: &Member) -> Value {
    let kind = match m.kind {
        MemberKind::Field => "field",
        MemberKind::Method => "method",
        MemberKind::Function => "function",
        MemberKind::Struct => "type",
        MemberKind::Constant => "constant",
    };
    json!({ "name": m.name, "kind": kind, "signature": m.signature, "doc": m.doc })
}

fn short(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// The whole reference.
pub fn reference() -> Value {
    let names: Vec<&str> = crate::stdlib::MODULES.iter().map(|m| m.name).collect();
    let probe: String = names.iter().map(|m| format!("import {m}\n")).collect();
    let a = crate::analyze(&probe);
    let info = &a.ide;

    let mut modules = Vec::new();
    for name in &names {
        let Some(ns) = info.imported_module(name) else { continue };
        let doc = info.modules.iter().find(|m| m.ns == ns).and_then(|m| m.doc.clone());
        let members = info.module_members(ns);
        let of = |k: MemberKind| members.iter().filter(|m| m.kind == k).map(member).collect::<Vec<_>>();
        let types: Vec<Value> = info
            .structs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.ns == ns && !s.instance && !short(&s.name).starts_with('_'))
            .map(|(sid, s)| {
                let ty = if s.variants.is_some() { Type::Enum(sid, true) } else { Type::Struct(sid) };
                let variants: Vec<Value> =
                    s.variants.iter().flatten().map(|v| json!({ "name": v.name, "signature": v.signature, "doc": v.doc })).collect();
                json!({
                    "name": short(&s.name),
                    "kind": if s.variants.is_some() { "enum" } else { "struct" },
                    "doc": s.doc,
                    "variants": variants,
                    "members": info.members(&ty).iter().map(member).collect::<Vec<_>>(),
                })
            })
            .collect();
        modules.push(json!({
            "name": name,
            "doc": doc,
            "functions": of(MemberKind::Function),
            "constants": of(MemberKind::Constant),
            "types": types,
        }));
    }

    // Built-in types: `List[T]` and `Map[K, V]` are shown with their type parameters.
    let param_name = |map: bool| {
        move |t: &Type| match t {
            Type::Param(0) if map => "K".to_string(),
            Type::Param(0) => "T".to_string(),
            Type::Param(_) => "V".to_string(),
            other => info.type_name(other),
        }
    };
    let builtin_types: Vec<Value> = [
        ("Int", "Int", Type::Int, false),
        ("Float", "Float", Type::Float, false),
        ("Bool", "Bool", Type::Bool, false),
        ("String", "String", Type::Str, false),
        ("List", "List[T]", Type::List(Box::new(Type::Param(0))), false),
        ("Map", "Map[K, V]", Type::Map(Box::new(Type::Param(0)), Box::new(Type::Param(1))), true),
    ]
    .into_iter()
    .map(|(name, shown, ty, is_map)| {
        let doc = ide::BUILTIN_TYPES.iter().find(|(n, _)| *n == name).map(|(_, d)| *d).unwrap_or("");
        let methods: Vec<Value> = ide::builtin_methods(&ty, shown, &param_name(is_map)).iter().map(member).collect();
        json!({ "name": name, "shown": shown, "doc": doc, "methods": methods })
    })
    .collect();

    let functions: Vec<Value> = ide::BUILTIN_FUNCTIONS
        .iter()
        .filter_map(|n| ide::builtin_function(n).map(|(sig, doc)| json!({ "name": n, "signature": sig, "doc": doc })))
        .collect();

    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "functions": functions,
        "types": builtin_types,
        "modules": modules,
    })
}

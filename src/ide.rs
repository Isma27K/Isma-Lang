//! Editor-facing information collected while type checking: hover text,
//! go-to-definition links, expression types (for `.` completion), inlay
//! hints and symbols. Consumed by the language server (`isma lsp`).

use crate::span::Span;
use crate::types::Type;

#[derive(Debug, Clone, Default)]
pub struct IdeInfo {
    pub hovers: Vec<HoverInfo>,
    /// (use site, definition site)
    pub definitions: Vec<(Span, Span)>,
    /// The type of every expression that was checked.
    pub expr_types: Vec<(Span, Type)>,
    /// Inferred types to show inline: (byte offset, label such as ": Int").
    pub inlay_hints: Vec<(u32, String)>,
    pub locals: Vec<LocalSymbol>,
    /// Indexed by function id (methods included), in declaration order.
    pub functions: Vec<FnSymbol>,
    /// Indexed by struct id.
    pub structs: Vec<StructSymbol>,
}

#[derive(Debug, Clone)]
pub struct HoverInfo {
    pub span: Span,
    /// Isma code shown in a code block, e.g. `fn fib(n: Int) -> Int`.
    pub code: String,
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalKind {
    Param,
    Val,
    Var,
    LoopVar,
}

#[derive(Debug, Clone)]
pub struct LocalSymbol {
    pub name: String,
    pub ty: String,
    pub decl: Span,
    pub kind: LocalKind,
}

#[derive(Debug, Clone)]
pub struct FnSymbol {
    pub name: String,
    pub signature: String,
    pub name_span: Span,
    /// From `fn` to the end of the body.
    pub full: Span,
    pub owner: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct StructSymbol {
    pub name: String,
    pub name_span: Span,
    pub full: Span,
    pub fields: Vec<FieldSymbol>,
    /// Function ids of the methods.
    pub methods: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct FieldSymbol {
    pub name: String,
    pub ty: String,
    pub mutable: bool,
    pub span: Span,
}

impl IdeInfo {
    pub fn type_name(&self, t: &Type) -> String {
        let names: Vec<String> = self.structs.iter().map(|s| s.name.clone()).collect();
        t.display(&names)
    }

    /// Fields + methods available on a value of type `t`.
    pub fn members(&self, t: &Type) -> Vec<Member> {
        match t {
            Type::Struct(sid) => {
                let Some(s) = self.structs.get(*sid) else { return Vec::new() };
                let mut out: Vec<Member> = s
                    .fields
                    .iter()
                    .map(|f| Member {
                        name: f.name.clone(),
                        signature: format!("{} {}: {}", if f.mutable { "var" } else { "val" }, f.name, f.ty),
                        doc: format!("field of `{}`", s.name),
                        is_method: false,
                        takes_args: false,
                    })
                    .collect();
                for &fid in &s.methods {
                    let f = &self.functions[fid];
                    out.push(Member {
                        name: f.name.clone(),
                        signature: f.signature.clone(),
                        doc: format!("method of `{}`", s.name),
                        is_method: true,
                        takes_args: !f.signature.contains("(self)"),
                    });
                }
                out
            }
            _ => builtin_methods(t, &self.type_name(t), &|t| self.type_name(t)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub signature: String,
    pub doc: String,
    pub is_method: bool,
    pub takes_args: bool,
}

fn method(name: &str, signature: String, doc: &str, takes_args: bool) -> Member {
    Member { name: name.into(), signature, doc: doc.into(), is_method: true, takes_args }
}

/// The built-in methods of primitive and list types.
pub fn builtin_methods(t: &Type, tn: &str, type_name: &dyn Fn(&Type) -> String) -> Vec<Member> {
    let to_string = method("to_string", "fn to_string() -> String".into(), "Convert to a `String`.", false);
    match t {
        Type::Int => vec![
            to_string,
            method("to_float", "fn to_float() -> Float".into(), "Convert to a `Float`.", false),
        ],
        Type::Float => vec![
            to_string,
            method("to_int", "fn to_int() -> Int".into(), "Truncate toward zero (saturating at the `Int` range; NaN becomes 0).", false),
        ],
        Type::Bool => vec![to_string],
        Type::Str => vec![
            method("len", "fn len() -> Int".into(), "Length in bytes.", false),
            method("to_int", "fn to_int() -> Int".into(), "Parse as an `Int`. Panics if the string is not a valid integer.", false),
            method("to_float", "fn to_float() -> Float".into(), "Parse as a `Float`. Panics if the string is not a valid number.", false),
            to_string,
        ],
        Type::List(elem) => {
            let e = type_name(elem);
            vec![
                method("len", "fn len() -> Int".into(), "Number of elements.", false),
                method("push", format!("fn push(value: {e})"), "Append an element to the end of the list.", true),
                method("pop", format!("fn pop() -> {e}"), "Remove and return the last element. Panics if the list is empty.", false),
            ]
        }
        _ => {
            let _ = tn;
            Vec::new()
        }
    }
}

/// Signature + documentation of the built-in functions.
pub fn builtin_function(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "print" => ("fn print(value: Int | Float | Bool | String)", "Print a value followed by a newline."),
        "input" => ("fn input() -> String", "Read one line from stdin, without the trailing newline. Returns `\"\"` at end of input."),
        "args" => ("fn args() -> List[String]", "The command-line arguments passed to the program (not including the program name)."),
        "panic" => ("fn panic(message: String)", "Stop the program with an error message and exit code 101."),
        "assert" => ("fn assert(condition: Bool)", "Panic if `condition` is false."),
        "sqrt" => ("fn sqrt(x: Float) -> Float", "Square root."),
        "exit" => ("fn exit(code: Int)", "Exit the program immediately with the given status code."),
        _ => return None,
    })
}

pub const BUILTIN_FUNCTIONS: &[&str] = &["print", "input", "args", "panic", "assert", "sqrt", "exit"];

pub const KEYWORDS: &[&str] = &[
    "fn", "val", "var", "if", "elif", "else", "while", "for", "in", "break", "continue", "return", "pass", "struct", "true",
    "false", "and", "or", "not",
];

pub const BUILTIN_TYPES: &[(&str, &str)] = &[
    ("Int", "64-bit signed integer. Overflow panics."),
    ("Float", "64-bit IEEE 754 floating point number."),
    ("Bool", "`true` or `false`."),
    ("String", "Immutable UTF-8 text."),
    ("List", "`List[T]`: growable, bounds-checked list (reference type)."),
];

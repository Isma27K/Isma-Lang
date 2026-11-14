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
    /// Every module loaded for this file (imported directly or not).
    pub modules: Vec<ModuleSymbol>,
    /// Modules the entry file imports: (the name it binds, its span, the module's namespace).
    pub imports: Vec<(String, Span, usize)>,
    /// Names the entry file brings in with `from m import name`: (the name it binds, what it is).
    pub imported: Vec<(String, ImportedSymbol)>,
}

#[derive(Debug, Clone)]
pub struct ModuleSymbol {
    pub name: String,
    /// Namespace index (functions and structs carry the same `ns`).
    pub ns: usize,
    pub doc: Option<String>,
    /// The file, for the user's own modules.
    pub path: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportedSymbol {
    /// A struct or enum (index into `structs`).
    Struct(usize),
    /// A function (index into `functions`).
    Func(usize),
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
    /// 0 for the user's file, otherwise the module's namespace.
    pub ns: usize,
    /// The `#` comment block above the declaration.
    pub doc: Option<String>,
}

/// A struct, or an enum when `variants` is `Some` (they share one table).
#[derive(Debug, Clone)]
pub struct StructSymbol {
    pub name: String,
    pub name_span: Span,
    pub full: Span,
    pub fields: Vec<FieldSymbol>,
    pub variants: Option<Vec<VariantSymbol>>,
    /// Function ids of the methods.
    pub methods: Vec<usize>,
    pub ns: usize,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VariantSymbol {
    pub name: String,
    /// `Circle(radius: Float)`
    pub signature: String,
    pub span: Span,
    pub doc: Option<String>,
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

    /// Fields + methods available on a value of type `t` (as seen from the user's file).
    pub fn members(&self, t: &Type) -> Vec<Member> {
        match t {
            Type::Struct(sid) | Type::Enum(sid, _) => {
                let Some(s) = self.structs.get(*sid) else { return Vec::new() };
                let hide_private = s.ns != 0;
                let mut out: Vec<Member> = s
                    .fields
                    .iter()
                    .filter(|f| !(hide_private && f.name.starts_with('_')))
                    .map(|f| Member {
                        name: f.name.clone(),
                        signature: format!("{} {}: {}", if f.mutable { "var" } else { "val" }, f.name, f.ty),
                        doc: format!("field of `{}`", s.name),
                        kind: MemberKind::Field,
                        takes_args: false,
                    })
                    .collect();
                for &fid in &s.methods {
                    let f = &self.functions[fid];
                    // Operator functions (`fn +`) are used through their operator, not `.`.
                    if hide_private && f.name.starts_with('_') || crate::ast::is_operator_name(&f.name) {
                        continue;
                    }
                    let doc = match &f.doc {
                        Some(d) => format!("method of `{}`\n\n{d}", s.name),
                        None => format!("method of `{}`", s.name),
                    };
                    out.push(Member {
                        name: f.name.clone(),
                        signature: f.signature.clone(),
                        doc,
                        kind: MemberKind::Method,
                        takes_args: !f.signature.contains("(self)"),
                    });
                }
                if matches!(t, Type::Enum(_, false)) && !out.iter().any(|m| m.name == "to_string") {
                    out.push(method("to_string", "fn to_string() -> String".into(), "The variant's name.", false));
                }
                out
            }
            // (offered after `?.`, or after `.` with an edit that turns it into `?.`)
            Type::Nullable(inner) => self.members(inner),
            _ => builtin_methods(t, &self.type_name(t), &|t| self.type_name(t)),
        }
    }

    /// The module the entry file imports as `binding` (`time`, or `geo` in `import geometry as geo`).
    pub fn imported_module(&self, binding: &str) -> Option<usize> {
        self.imports.iter().find(|(b, _, _)| b == binding).map(|(_, _, ns)| *ns)
    }

    /// Public functions and types of a module, for `time.` completion.
    pub fn module_members(&self, ns: usize) -> Vec<Member> {
        let mut out: Vec<Member> = self
            .functions
            .iter()
            .filter(|f| f.ns == ns && f.owner.is_none() && !f.name.starts_with('_'))
            .map(|f| Member {
                name: f.name.clone(),
                signature: f.signature.clone(),
                doc: f.doc.clone().unwrap_or_default(),
                kind: MemberKind::Function,
                takes_args: !f.signature.contains("()"),
            })
            .collect();
        for s in self.structs.iter().filter(|s| s.ns == ns) {
            let short = s.name.rsplit('.').next().unwrap_or(&s.name).to_string();
            if short.starts_with('_') {
                continue;
            }
            out.push(Member {
                name: short,
                signature: format!("{} {}", if s.variants.is_some() { "enum" } else { "struct" }, s.name),
                doc: s.doc.clone().unwrap_or_default(),
                kind: MemberKind::Struct,
                takes_args: false,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Field,
    Method,
    Function,
    Struct,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub signature: String,
    pub doc: String,
    pub kind: MemberKind,
    pub takes_args: bool,
}

fn method(name: &str, signature: String, doc: &str, takes_args: bool) -> Member {
    Member { name: name.into(), signature, doc: doc.into(), kind: MemberKind::Method, takes_args }
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
            method("chars", "fn chars() -> List[String]".into(), "The characters of the string, each as a one-character `String`.", false),
            method(
                "substring",
                "fn substring(start: Int, end: Int) -> String".into(),
                "The bytes from `start` up to (not including) `end`. Panics if out of range or not on a character boundary.",
                true,
            ),
            method("contains", "fn contains(text: String) -> Bool".into(), "Does the string contain `text`?", true),
            method("starts_with", "fn starts_with(prefix: String) -> Bool".into(), "Does the string start with `prefix`?", true),
            method("ends_with", "fn ends_with(suffix: String) -> Bool".into(), "Does the string end with `suffix`?", true),
            method("to_int", "fn to_int() -> Int".into(), "Parse as an `Int`. Panics if the string is not a valid integer.", false),
            method("to_float", "fn to_float() -> Float".into(), "Parse as a `Float`. Panics if the string is not a valid number.", false),
            method("to_int_or_null", "fn to_int_or_null() -> Int?".into(), "Parse as an `Int`, or `null` if the string is not a valid integer.", false),
            method("to_float_or_null", "fn to_float_or_null() -> Float?".into(), "Parse as a `Float`, or `null` if the string is not a valid number.", false),
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
        "print" => ("fn print(value: Int | Float | Bool | String)", "Print a value followed by a newline. A nullable value prints as `null` when it is null."),
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
    "fn", "val", "var", "if", "elif", "else", "while", "for", "in", "break", "continue", "return", "pass", "struct", "enum",
    "match", "case", "import", "true", "false", "and", "or", "not", "null",
];

pub const BUILTIN_TYPES: &[(&str, &str)] = &[
    ("Int", "64-bit signed integer. Overflow panics."),
    ("Float", "64-bit IEEE 754 floating point number."),
    ("Bool", "`true` or `false`."),
    ("String", "Immutable UTF-8 text."),
    ("List", "`List[T]`: growable, bounds-checked list (reference type)."),
];

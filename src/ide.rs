//! Editor-facing information collected while type checking: hover text,
//! go-to-definition links, expression types (for `.` completion), inlay
//! hints and symbols. Consumed by the language server (`tarn lsp`).

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
    /// The names of type parameters, by `ParamId`.
    pub params: Vec<String>,
    /// Top-level constants (`val pi = 3.14159`), indexed by constant id.
    pub consts: Vec<ConstSymbol>,
}

#[derive(Debug, Clone)]
pub struct ConstSymbol {
    pub name: String,
    pub ns: usize,
    pub span: Span,
    /// `val pi: Float = 3.141592653589793`
    pub signature: String,
    pub doc: Option<String>,
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
    /// A constant (index into `consts`).
    Const(usize),
}

#[derive(Debug, Clone)]
pub struct HoverInfo {
    pub span: Span,
    /// Tarn code shown in a code block, e.g. `fn fib(n: Int) -> Int`.
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
    /// A copy of a generic function for some type arguments (`max[Int]`):
    /// not a declaration of its own, so outlines and completion skip it.
    pub instance: bool,
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
    /// `["T"]` for `struct Box[T]` (and for its instances).
    pub type_params: Vec<String>,
    /// `["Int"]` for the instance `Box[Int]`.
    pub type_args: Vec<String>,
    /// A generic type with type arguments (`Box[Int]`): not a declaration of
    /// its own, so outlines and completion skip it.
    pub instance: bool,
    /// An interface (its `methods` are the ones it asks for).
    pub iface: bool,
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
        t.display(&names, &self.params)
    }

    /// Fields + methods available on a value of type `t` (as seen from the user's file).
    pub fn members(&self, t: &Type) -> Vec<Member> {
        match t {
            Type::Struct(sid) | Type::Enum(sid, _) | Type::Iface(sid) => {
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
                    // Operator functions (`fn +`) are used through their operator, not `.`,
                    // and `fn drop` runs by itself: it can't be called.
                    if hide_private && f.name.starts_with('_') || crate::ast::is_operator_name(&f.name) || f.name == "drop" && !s.iface {
                        continue;
                    }
                    let of = if s.iface { "method of interface" } else { "method of" };
                    let doc = match &f.doc {
                        Some(d) if s.iface => d.clone(),
                        Some(d) => format!("{of} `{}`\n\n{d}", s.name),
                        None => format!("{of} `{}`", s.name),
                    };
                    out.push(Member {
                        name: f.name.clone(),
                        signature: replace_words(&f.signature, &s.type_params, &s.type_args),
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
            .filter(|f| f.ns == ns && f.owner.is_none() && !f.instance && !f.name.starts_with('_'))
            .map(|f| Member {
                name: f.name.clone(),
                signature: f.signature.clone(),
                doc: f.doc.clone().unwrap_or_default(),
                kind: MemberKind::Function,
                takes_args: !f.signature.contains("()"),
            })
            .collect();
        for s in self.structs.iter().filter(|s| s.ns == ns && !s.instance) {
            let short = s.name.rsplit('.').next().unwrap_or(&s.name).to_string();
            if short.starts_with('_') {
                continue;
            }
            out.push(Member {
                name: short,
                signature: format!("{} {}", if s.iface { "interface" } else if s.variants.is_some() { "enum" } else { "struct" }, s.name),
                doc: s.doc.clone().unwrap_or_default(),
                kind: MemberKind::Struct,
                takes_args: false,
            });
        }
        for c in self.consts.iter().filter(|c| c.ns == ns && !c.name.starts_with('_')) {
            out.push(Member {
                name: c.name.clone(),
                signature: c.signature.clone(),
                doc: c.doc.clone().unwrap_or_default(),
                kind: MemberKind::Constant,
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
    Constant,
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
        Type::Bytes => vec![
            method("len", "fn len() -> Int".into(), "Number of bytes.", false),
            method("is_empty", "fn is_empty() -> Bool".into(), "Are there no bytes?", false),
            method("slice", "fn slice(start: Int, end: Int) -> Bytes".into(), "The bytes from index `start` up to (not including) `end`, copied. Panics if the range is outside the bytes.", true),
            method("find", "fn find(b: Bytes) -> Int?".into(), "The index of the first `b`, or `null`.", true),
            method("rfind", "fn rfind(b: Bytes) -> Int?".into(), "The index of the last `b`, or `null`.", true),
            method("contains", "fn contains(b: Bytes) -> Bool".into(), "Does `b` occur in them?", true),
            method("starts_with", "fn starts_with(b: Bytes) -> Bool".into(), "Do they start with `b`?", true),
            method("ends_with", "fn ends_with(b: Bytes) -> Bool".into(), "Do they end with `b`?", true),
            method("split", "fn split(sep: Bytes) -> List[Bytes]".into(), "The parts between each `sep`.", true),
            method("count", "fn count(b: Bytes) -> Int".into(), "How many times `b` occurs (without overlapping).", true),
            method("replace", "fn replace(old: Bytes, new: Bytes) -> Bytes".into(), "Every `old` replaced by `new`.", true),
            method("trim", "fn trim() -> Bytes".into(), "Without ASCII whitespace at either end.", false),
            method("decode", "fn decode() -> String".into(), "The bytes as text. They're shared, not copied. Panics if they aren't valid UTF-8.", false),
            method("decode_or_null", "fn decode_or_null() -> String?".into(), "The bytes as text, or `null` if they aren't valid UTF-8.", false),
            method("decode_lossy", "fn decode_lossy() -> String".into(), "The bytes as text, with `\u{FFFD}` in place of each byte that isn't valid UTF-8.", false),
            method("hex", "fn hex() -> String".into(), "Two lowercase hex digits per byte: `\"48690a\"`.", false),
            method("to_list", "fn to_list() -> List[Int]".into(), "The bytes as numbers from 0 to 255.", false),
            method("to_string", "fn to_string() -> String".into(), "As written in code: `b\"GET /\\r\\n\"`, other bytes as `\\xff`.", false),
        ],
        Type::Str => vec![
            method("len", "fn len() -> Int".into(), "Length in bytes.", false),
            method("chars", "fn chars() -> List[String]".into(), "The characters of the string, each as a one-character `String`.", false),
            method("encode", "fn encode() -> Bytes".into(), "The text's UTF-8 bytes. They're shared, not copied.", false),
            method("char_count", "fn char_count() -> Int".into(), "The number of characters. (`len()` counts bytes: `\"héllo\".len()` is 6, `char_count()` is 5.)", false),
            method(
                "substring",
                "fn substring(start: Int, end: Int) -> String".into(),
                "The bytes from `start` up to (not including) `end`. Panics if out of range or not on a character boundary.",
                true,
            ),
            method("contains", "fn contains(text: String) -> Bool".into(), "Does the string contain `text`?", true),
            method("starts_with", "fn starts_with(prefix: String) -> Bool".into(), "Does the string start with `prefix`?", true),
            method("ends_with", "fn ends_with(suffix: String) -> Bool".into(), "Does the string end with `suffix`?", true),
            method("is_empty", "fn is_empty() -> Bool".into(), "Is it `\"\"`?", false),
            method("lines", "fn lines() -> List[String]".into(), "The lines, without their `\\n` or `\\r\\n`.", false),
            method(
                "split",
                "fn split(separator: String) -> List[String]".into(),
                "The parts between each `separator`: `\"a,b,,c\".split(\",\")` is `[\"a\", \"b\", \"\", \"c\"]`. Without a separator, `split()` splits on runs of whitespace and drops empty parts.",
                true,
            ),
            method("trim", "fn trim() -> String".into(), "Without whitespace at either end.", false),
            method("trim_start", "fn trim_start() -> String".into(), "Without whitespace at the start.", false),
            method("trim_end", "fn trim_end() -> String".into(), "Without whitespace at the end.", false),
            method("upper", "fn upper() -> String".into(), "In upper case (ASCII letters).", false),
            method("lower", "fn lower() -> String".into(), "In lower case (ASCII letters).", false),
            method("replace", "fn replace(old: String, new: String) -> String".into(), "Every `old` replaced by `new`.", true),
            method("find", "fn find(text: String) -> Int?".into(), "The byte index of the first `text`, or `null`.", true),
            method("rfind", "fn rfind(text: String) -> Int?".into(), "The byte index of the last `text`, or `null`.", true),
            method("count", "fn count(text: String) -> Int".into(), "How many times `text` occurs (without overlapping).", true),
            method("repeat", "fn repeat(n: Int) -> String".into(), "`n` copies, one after another.", true),
            method(
                "pad_start",
                "fn pad_start(width: Int, fill: String = \" \") -> String".into(),
                "At least `width` characters, adding `fill` at the start: `\"7\".pad_start(3, \"0\")` is `\"007\"`.",
                true,
            ),
            method("pad_end", "fn pad_end(width: Int, fill: String = \" \") -> String".into(), "At least `width` characters, adding `fill` at the end.", true),
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
                method("is_empty", "fn is_empty() -> Bool".into(), "Does it have no elements?", false),
                method("insert", format!("fn insert(index: Int, value: {e})"), "Insert at `index` (0 to `len()`), moving the rest up.", true),
                method("remove_at", format!("fn remove_at(index: Int) -> {e}"), "Remove and return the element at `index`, moving the rest down.", true),
                method("clear", "fn clear()".into(), "Remove every element.", false),
                method("contains", format!("fn contains(value: {e}) -> Bool"), "Is `value` in the list (compared with `==`)? Same as `value in xs`.", true),
                method("index_of", format!("fn index_of(value: {e}) -> Int?"), "The index of the first element equal to `value`, or `null`.", true),
                method("reverse", "fn reverse()".into(), "Reverse the order of the elements, in place.", false),
                method("sort", "fn sort()".into(), "Sort in place, smallest first (lists of `Int`, `Float` or `String`).", false),
                method(
                    "sort_by",
                    format!("fn sort_by(key: fn({e}) -> K)"),
                    "Sort in place by each element's key (an `Int`, `Float` or `String`), smallest first. Elements with equal keys keep their order: `people.sort_by(fn(p) -> p.age)`.",
                    true,
                ),
                method("map", format!("fn map(transform: fn({e}) -> U) -> List[U]"), "A new list of `transform` applied to each element, in order: `xs.map(fn(x) -> x * 2)`.", true),
                method("filter", format!("fn filter(keep: fn({e}) -> Bool) -> List[{e}]"), "A new list of the elements for which `keep` returns `true`, in order.", true),
                method("any", format!("fn any(test: fn({e}) -> Bool) -> Bool"), "Is `test` true for some element? Stops at the first one.", true),
                method("all", format!("fn all(test: fn({e}) -> Bool) -> Bool"), "Is `test` true for every element (or the list empty)? Stops at the first that fails.", true),
                method("find", format!("fn find(test: fn({e}) -> Bool) -> {e}?"), "The first element for which `test` is true, or `null`.", true),
                method("count", format!("fn count(test: fn({e}) -> Bool) -> Int"), "How many elements `test` is true for.", true),
                method(
                    "fold",
                    format!("fn fold(initial: A, combine: fn(A, {e}) -> A) -> A"),
                    "Combine the elements into one value, starting from `initial`: `xs.fold(0, fn(sum, x) -> sum + x)`.",
                    true,
                ),
                method("min_by", format!("fn min_by(key: fn({e}) -> K) -> {e}?"), "The element with the smallest key (the first, on a tie), or `null` for an empty list.", true),
                method("max_by", format!("fn max_by(key: fn({e}) -> K) -> {e}?"), "The element with the largest key (the first, on a tie), or `null` for an empty list.", true),
                method("join", "fn join(separator: String) -> String".into(), "The elements (strings) with `separator` between them.", true),
                method("to_string", "fn to_string() -> String".into(), "As written in code: `[1, 2, 3]`.", false),
            ]
        }
        Type::Map(k, v) => {
            let (k, v) = (type_name(k), type_name(v));
            vec![
                method("len", "fn len() -> Int".into(), "Number of entries.", false),
                method("is_empty", "fn is_empty() -> Bool".into(), "Does it have no entries?", false),
                method("get", format!("fn get(key: {k}) -> {v}?"), "The value for `key`, or `null` if it isn't there. `get(key, default)` gives `default` instead.", true),
                method("contains", format!("fn contains(key: {k}) -> Bool"), "Is `key` in the map? Same as `key in m`.", true),
                method("remove", format!("fn remove(key: {k}) -> {v}?"), "Remove `key`, returning its value (or `null` if it wasn't there).", true),
                method("keys", format!("fn keys() -> List[{k}]"), "The keys, in insertion order.", false),
                method("values", format!("fn values() -> List[{v}]"), "The values, in insertion order of their keys.", false),
                method("clear", "fn clear()".into(), "Remove every entry.", false),
                method("to_string", "fn to_string() -> String".into(), "As written in code: `{\"a\": 1}`.", false),
            ]
        }
        // A type parameter: generic code can only call `to_string()` on it.
        Type::Param(_) => vec![method("to_string", "fn to_string() -> String".into(), "Convert to a `String` (the type used for it must support that).", false)],
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
        "hash" => (
            "fn hash(values...) -> Int",
            "A hash of one or more values, mixed into one `Int`: `return hash(self.x, self.y)` in a `fn hash(self) -> Int` lets a type be a map key.",
        ),
        _ => return None,
    })
}

pub const BUILTIN_FUNCTIONS: &[&str] = &["print", "input", "args", "panic", "assert", "sqrt", "exit", "hash"];

pub const KEYWORDS: &[&str] = &[
    "fn", "val", "var", "if", "elif", "else", "while", "for", "in", "break", "continue", "return", "pass", "struct", "enum",
    "match", "case", "import", "true", "false", "and", "or", "not", "null",
];

pub const BUILTIN_TYPES: &[(&str, &str)] = &[
    ("Int", "64-bit signed integer. Overflow panics."),
    ("Float", "64-bit IEEE 754 floating point number."),
    ("Bool", "`true` or `false`."),
    ("String", "Immutable UTF-8 text."),
    ("Bytes", "Raw bytes: what files and sockets carry. `s.encode()` makes one from text, `b.decode()` turns valid UTF-8 back into a `String`."),
    ("List", "`List[T]`: growable, bounds-checked list (reference type)."),
    ("Map", "`Map[K, V]`: hash map from keys to values, in insertion order (reference type)."),
];

/// `fn get(self) -> T` with `T` replaced by `Int` (whole words only), for the
/// members of `Box[Int]`.
pub fn replace_words(text: &str, from: &[String], to: &[String]) -> String {
    if from.is_empty() || from.len() != to.len() {
        return text.to_string();
    }
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        match from.iter().position(|f| f == word) {
            Some(i) => out.push_str(&to[i]),
            None => out.push_str(word),
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

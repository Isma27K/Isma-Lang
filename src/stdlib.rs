//! The standard library: modules written in Isma itself, embedded in the
//! compiler and loaded on `import`.

pub struct StdModule {
    pub name: &'static str,
    pub source: &'static str,
}

pub const MODULES: &[StdModule] = &[
    StdModule { name: "time", source: include_str!("../stdlib/time.isma") },
    StdModule { name: "math", source: include_str!("../stdlib/math.isma") },
    StdModule { name: "fs", source: include_str!("../stdlib/fs.isma") },
];

pub fn find(name: &str) -> Option<&'static StdModule> {
    MODULES.iter().find(|m| m.name == name)
}

/// Built-in hooks that only standard-library modules may call:
/// (name, parameter types, return type), using Isma type names.
pub const INTRINSICS: &[(&str, &[&str], &str)] = &[
    ("__realtime_ns", &[], "Int"),
    ("__monotonic_ns", &[], "Int"),
    ("__sleep_ns", &["Int"], "Unit"),
    // math: one `Float` in, one out (see `MathFn::ALL`), plus these.
    ("__sin", &["Float"], "Float"),
    ("__cos", &["Float"], "Float"),
    ("__tan", &["Float"], "Float"),
    ("__asin", &["Float"], "Float"),
    ("__acos", &["Float"], "Float"),
    ("__atan", &["Float"], "Float"),
    ("__sinh", &["Float"], "Float"),
    ("__cosh", &["Float"], "Float"),
    ("__tanh", &["Float"], "Float"),
    ("__exp", &["Float"], "Float"),
    ("__ln", &["Float"], "Float"),
    ("__log2", &["Float"], "Float"),
    ("__log10", &["Float"], "Float"),
    ("__floor", &["Float"], "Float"),
    ("__ceil", &["Float"], "Float"),
    ("__round", &["Float"], "Float"),
    ("__trunc", &["Float"], "Float"),
    ("__fabs", &["Float"], "Float"),
    ("__cbrt", &["Float"], "Float"),
    ("__sqrt", &["Float"], "Float"),
    ("__pow", &["Float", "Float"], "Float"),
    ("__atan2", &["Float", "Float"], "Float"),
    // fs
    ("__fs_read", &["String"], "String?"),
    ("__fs_write", &["String", "String", "Bool"], "Bool"),
    ("__fs_kind", &["String"], "Int"),
    ("__fs_list_dir", &["String"], "List[String]?"),
    ("__fs_make_dir", &["String"], "Bool"),
    ("__fs_remove", &["String"], "Bool"),
    ("__fs_rename", &["String", "String"], "Bool"),
    ("__fs_last_error", &[], "String"),
    ("__fs_cwd", &[], "String"),
    ("__fs_read_stdin", &[], "String"),
];

/// Display path for a module in panic messages.
pub fn display_path(name: &str) -> String {
    format!("<std>/{name}.isma")
}

//! The standard library: modules written in Tarn itself, embedded in the
//! compiler and loaded on `import`.

pub struct StdModule {
    pub name: &'static str,
    pub source: &'static str,
}

pub const MODULES: &[StdModule] = &[
    StdModule { name: PRELUDE, source: include_str!("../stdlib/prelude.tr") },
    StdModule { name: "time", source: include_str!("../stdlib/time.tr") },
    StdModule { name: "math", source: include_str!("../stdlib/math.tr") },
    StdModule { name: "fs", source: include_str!("../stdlib/fs.tr") },
    StdModule { name: "url", source: include_str!("../stdlib/url.tr") },
    StdModule { name: "env", source: include_str!("../stdlib/env.tr") },
    StdModule { name: "signal", source: include_str!("../stdlib/signal.tr") },
    StdModule { name: "net", source: include_str!("../stdlib/net.tr") },
];

/// The module every program gets without importing it: `Result`, `Ok`, `Err`, `Unit`.
pub const PRELUDE: &str = "prelude";

/// A module that can be imported by name (the prelude can't: it's always there).
pub fn find(name: &str) -> Option<&'static StdModule> {
    MODULES.iter().find(|m| m.name == name && m.name != PRELUDE)
}

/// The modules an `import` can name (all but the prelude).
pub fn importable() -> impl Iterator<Item = &'static StdModule> {
    MODULES.iter().filter(|m| m.name != PRELUDE)
}

pub fn prelude() -> &'static StdModule {
    MODULES.iter().find(|m| m.name == PRELUDE).unwrap()
}

/// Built-in hooks that only standard-library modules may call:
/// (name, parameter types, return type), using Tarn type names.
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

/// Runtime functions called directly, by standard-library modules
/// (`__url_encode(...)` calls the C function `tarn_rt_url_encode`) and by
/// built-in methods: (name, parameter types, return type, whether the C
/// function also takes the caller's location, for panics).
///
/// The C side borrows its arguments and returns new references. `Bool`s
/// cross as `int64_t` (0 or 1); a nullable result must be a reference (a
/// null pointer), never a nullable `Int`, `Float` or `Bool`.
pub const RT: &[(&str, &[&str], &str, bool)] = &[
    // Bytes (the methods of the built-in type)
    ("bytes_new", &[], "Bytes", false),
    ("bytes_from_list", &["List[Int]"], "Bytes", true),
    ("bytes_get", &["Bytes", "Int"], "Int", true),
    ("bytes_at", &["Bytes", "Int"], "Int", false),
    ("bytes_slice", &["Bytes", "Int", "Int"], "Bytes", true),
    ("bytes_decode", &["Bytes"], "String", true),
    ("bytes_decode_or_null", &["Bytes"], "String?", false),
    ("bytes_decode_lossy", &["Bytes"], "String", false),
    ("bytes_hex", &["Bytes"], "String", false),
    ("bytes_to_list", &["Bytes"], "List[Int]", false),
    ("bytes_repr", &["Bytes"], "String", false),
    ("str_encode", &["String"], "Bytes", false),
    ("str_to_bool", &["String"], "Bool", true),
    // env
    ("env_get", &["String"], "String?", false),
    ("env_set", &["String", "String"], "Bool", false),
    ("env_remove", &["String"], "Bool", false),
    ("env_all", &[], "List[String]", false),
    ("env_root_dir", &[], "String", false),
    ("env_claim_default", &[], "Bool", false),
    // signal
    ("signal_catch", &["Int"], "Unit", false),
    ("signal_reset", &["Int"], "Unit", false),
    ("signal_next", &[], "Int", false),
    ("signal_wait", &[], "Int", false),
    ("signal_send", &["Int"], "Unit", false),
    // net
    ("net_listen", &["String", "Int"], "Int", false),
    ("net_accept", &["Int"], "Int", false),
    ("net_connect", &["String", "Int"], "Int", false),
    ("net_read", &["Int", "Int"], "Bytes?", false),
    ("net_write", &["Int", "Bytes"], "Int", false),
    ("net_set_timeout", &["Int", "Int"], "Int", false),
    ("net_close", &["Int"], "Unit", false),
    ("net_peer_address", &["Int"], "String?", false),
    ("net_local_address", &["Int"], "String?", false),
    ("net_last_error", &[], "Int", false),
    ("net_error_kind", &["Int"], "Int", false),
    ("net_error_message", &["Int"], "String", false),
    // tasks, channels, atomics (their handles: see `Type::Handle`)
    ("task_wait", &["Handle"], "Bool", false),
    ("task_is_done", &["Handle"], "Bool", false),
    ("task_panic_message", &["Handle"], "String", false),
    ("task_panic_location", &["Handle"], "String", false),
    ("chan_close", &["Handle"], "Unit", false),
    ("chan_is_closed", &["Handle"], "Bool", false),
    ("chan_len", &["Handle"], "Int", false),
    ("atomic_new", &["Int"], "Handle", false),
    ("atomic_get", &["Handle"], "Int", false),
    ("atomic_set", &["Handle", "Int"], "Unit", false),
    ("atomic_add", &["Handle", "Int"], "Int", false),
    ("atomic_swap", &["Handle", "Int"], "Int", false),
    ("atomic_compare_and_set", &["Handle", "Int", "Int"], "Bool", false),
    ("cpu_count", &[], "Int", false),
    // url
    ("url_encode", &["String", "String"], "String", false),
    ("url_decode", &["String", "Bool"], "String?", false),
];

/// `Builtin::Rt` of the runtime function `name`.
pub fn rt(name: &str) -> crate::tast::Builtin {
    crate::tast::Builtin::Rt(rt_index(name).unwrap_or_else(|| panic!("no runtime function `{name}`")) as u16)
}

/// The index in `RT` of the runtime function `name` (without the `__`).
pub fn rt_index(name: &str) -> Option<usize> {
    RT.iter().position(|(n, ..)| *n == name)
}

/// A standard-library module may call `name` (`__fs_read`, `__url_encode`...).
pub fn is_intrinsic(name: &str) -> bool {
    name == "__repr" || SYNC_INTRINSICS.contains(&name) || INTRINSICS.iter().any(|(n, _, _)| *n == name) || name.strip_prefix("__").is_some_and(|n| rt_index(n).is_some())
}

/// The type written `s` in the tables above.
pub fn parse_type(s: &str) -> crate::types::Type {
    use crate::types::Type;
    if let Some(inner) = s.strip_suffix('?') {
        return parse_type(inner).nullable();
    }
    if let Some(inner) = s.strip_prefix("List[").and_then(|r| r.strip_suffix(']')) {
        return Type::List(Box::new(parse_type(inner)));
    }
    match s {
        "Int" => Type::Int,
        "Float" => Type::Float,
        "Bool" => Type::Bool,
        "String" => Type::Str,
        "Bytes" => Type::Bytes,
        "Unit" => Type::Unit,
        "Handle" => Type::Handle,
        _ => panic!("unknown type `{s}` in a runtime function table"),
    }
}

/// Intrinsics of tasks, channels and mutexes that carry values of any type
/// (`__chan_send(h, value)`, `__chan_receive[T](h)`): see `SyncOp`.
pub const SYNC_INTRINSICS: &[&str] =
    &["__task_take", "__chan_new", "__chan_send", "__chan_receive", "__mutex_new", "__mutex_lock", "__mutex_update", "__mutex_get", "__mutex_set"];

/// Display path for a module in panic messages.
pub fn display_path(name: &str) -> String {
    format!("<std>/{name}.tr")
}

//! The standard library: modules written in Isma itself, embedded in the
//! compiler and loaded on `import`.

pub struct StdModule {
    pub name: &'static str,
    pub source: &'static str,
}

pub const MODULES: &[StdModule] = &[StdModule { name: "time", source: include_str!("../stdlib/time.isma") }];

pub fn find(name: &str) -> Option<&'static StdModule> {
    MODULES.iter().find(|m| m.name == name)
}

/// Built-in hooks that only standard-library modules may call:
/// (name, parameter types, return type), using Isma type names.
pub const INTRINSICS: &[(&str, &[&str], &str)] = &[
    ("__realtime_ns", &[], "Int"),
    ("__monotonic_ns", &[], "Int"),
    ("__sleep_ns", &["Int"], "Unit"),
];

/// Display path for a module in panic messages.
pub fn display_path(name: &str) -> String {
    format!("<std>/{name}.isma")
}

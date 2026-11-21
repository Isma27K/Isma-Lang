//! Loading a program's files: the entry file, the `.tr` files it imports
//! (found relative to the importing file, like Python's package-relative
//! imports), the packages Thorn installed, and the standard-library modules.
//! Every file becomes one namespace of the type checker; file `k` is
//! namespace `k`, the entry file 0.
//!
//! Packages come from a table Thorn writes, `.thorn/packages.json`, next to
//! the project's `thorn.toml` (the nearest one above the entry file), or from
//! the file `TARN_PACKAGES` names:
//!
//! ```json
//! {"version": 1, "packages": {"colors": {"version": "1.0.0", "path": "/home/me/.cache/thorn/packages/colors/1.0.0"}}}
//! ```
//!
//! A relative `path` is relative to the folder holding the table. A package's
//! folder holds `src/<name>.tr`, its root module (`import colors`), and
//! optionally `src/<name>/*.tr` (`import colors.codes`).

use crate::ast::{Import, Item, Program};
use crate::diagnostics::Diagnostic;
use crate::span::Span;
use crate::stdlib;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// The file being compiled (namespace 0).
    Root,
    /// One of the user's own `.tr` files, imported.
    User,
    /// A file of a package installed by Thorn (its warnings aren't shown).
    Package,
    /// A standard-library module (its problems are compiler bugs).
    Std,
}

/// A package Thorn installed for the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// The package's folder, holding `thorn.toml` and `src/` (canonical).
    pub root: PathBuf,
}

/// The packages a program can import, and what's needed to explain a missing one.
#[derive(Debug, Clone, Default)]
pub struct Packages {
    /// Sorted by name.
    pub list: Vec<Package>,
    /// The folder holding the project's `thorn.toml`, if there is one.
    pub project_dir: Option<PathBuf>,
    /// The package table that was read (or that failed to read).
    pub table: Option<PathBuf>,
    /// Why the table couldn't be used.
    pub error: Option<String>,
    /// Packages named in `thorn.lock`: one that's locked but missing from the
    /// table just needs `thorn sync`.
    pub locked: Vec<String>,
    /// Packages in the table whose folder is gone (the cache was cleaned).
    pub gone: Vec<String>,
}

impl Packages {
    pub fn get(&self, name: &str) -> Option<&Package> {
        self.list.iter().find(|p| p.name == name)
    }

    /// The package whose folder contains `path` (canonical).
    pub fn containing(&self, path: &Path) -> Option<&Package> {
        self.list.iter().filter(|p| path.starts_with(&p.root)).max_by_key(|p| p.root.components().count())
    }

    /// Why `import name` finds nothing, when the reason is on Thorn's side.
    pub fn missing_help(&self, name: &str) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(format!("{e}; run `thorn sync` to install the project's packages again"));
        }
        if self.gone.iter().any(|g| g == name) {
            return Some(format!("the package `{name}` was removed from Thorn's cache: run `thorn sync` to install it again"));
        }
        if self.locked.iter().any(|l| l == name) {
            return Some(format!("`{name}` is in `thorn.lock` but isn't installed: run `thorn sync`"));
        }
        None
    }
}

/// The packages of the program whose entry file is in `entry_dir`.
pub fn find_packages(entry_dir: Option<&Path>) -> Packages {
    let table = std::env::var_os("TARN_PACKAGES").filter(|v| !v.is_empty()).map(PathBuf::from);
    discover_packages(entry_dir, table.as_deref())
}

/// [`find_packages`], with the `TARN_PACKAGES` override passed in.
fn discover_packages(entry_dir: Option<&Path>, table: Option<&Path>) -> Packages {
    let mut out = Packages::default();
    out.project_dir = entry_dir.and_then(|d| d.ancestors().find(|a| a.join("thorn.toml").is_file()).map(Path::to_path_buf));
    if let Some(dir) = &out.project_dir {
        out.locked = locked_names(&dir.join("thorn.lock"));
    }
    let table = match (table, &out.project_dir) {
        (Some(t), _) => t.to_path_buf(),
        (None, Some(dir)) => dir.join(".thorn").join("packages.json"),
        (None, None) => return out,
    };
    let shown_table = match &out.project_dir {
        Some(dir) if table.starts_with(dir) => table.strip_prefix(dir).unwrap_or(&table).display().to_string(),
        _ => table.display().to_string(),
    };
    out.table = Some(table.clone());
    let text = match std::fs::read_to_string(&table) {
        Ok(t) => t,
        // A project that has no dependencies yet, or hasn't been synced.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !out.locked.is_empty() => {
            out.error = Some(format!("`{shown_table}` doesn't exist"));
            return out;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
        Err(e) => {
            out.error = Some(format!("`{shown_table}` can't be read ({e})"));
            return out;
        }
    };
    let base = table.parent().map(Path::to_path_buf).unwrap_or_default();
    match parse_table(&text, &base) {
        Ok(list) => {
            for p in list {
                match std::fs::canonicalize(&p.root) {
                    Ok(root) if root.is_dir() => out.list.push(Package { root, ..p }),
                    _ => out.gone.push(p.name),
                }
            }
            out.list.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Err(e) => out.error = Some(format!("`{shown_table}` is malformed ({e})")),
    }
    out
}

/// The entries of a `packages.json` (paths not yet checked).
fn parse_table(text: &str, base: &Path) -> Result<Vec<Package>, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    match v.get("version").and_then(|v| v.as_u64()) {
        Some(1) => {}
        Some(n) => return Err(format!("version {n} is newer than this compiler understands")),
        None => return Err("no `version`".into()),
    }
    let packages = v.get("packages").and_then(|p| p.as_object()).ok_or("no `packages` object")?;
    let mut out = Vec::new();
    for (name, entry) in packages {
        let version = entry.get("version").and_then(|v| v.as_str()).ok_or_else(|| format!("`{name}` has no `version`"))?;
        let path = entry.get("path").and_then(|v| v.as_str()).ok_or_else(|| format!("`{name}` has no `path`"))?;
        let path = Path::new(path);
        let root = if path.is_absolute() { path.to_path_buf() } else { base.join(path) };
        out.push(Package { name: name.clone(), version: version.to_string(), root });
    }
    Ok(out)
}

/// The `name = "..."` entries of a `thorn.lock` (a light read: Thorn owns the format).
fn locked_names(lock: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(lock) else { return Vec::new() };
    text.lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("name")?.trim_start().strip_prefix('=')?.trim();
            Some(rest.strip_prefix('"')?.strip_suffix('"')?.to_string())
        })
        .collect()
}

pub struct SourceFile {
    pub kind: FileKind,
    /// The canonical path on disk (`None` for the standard library and for
    /// programs that only exist in memory).
    pub path: Option<PathBuf>,
    /// How diagnostics and panics name the file: `app/geometry.tr`, `<std>/time.tr`.
    pub display: String,
    /// The module's name, its file stem (`geometry`); empty for the root.
    pub name: String,
    /// Unique among the files, for symbol names in the generated code.
    pub symbol: String,
    pub text: String,
    /// Where this file starts in the shared offset space of spans.
    pub base: u32,
    pub program: Program,
    /// The resolution of each `import` item of the file, in order.
    pub imports: Vec<Resolved>,
    /// The file that first imported this one, and the span of that `import`.
    pub imported_from: Option<(usize, Span)>,
}

impl SourceFile {
    /// Does this global offset fall inside the file?
    pub fn contains(&self, offset: u32) -> bool {
        offset >= self.base && offset <= self.base + self.text.len() as u32
    }
}

/// What the module path of an `import` refers to.
#[derive(Debug, Clone)]
pub enum Target {
    /// A loaded file (its namespace).
    File(usize),
    /// A folder (`from shapes import circle`, `from .. import util`).
    Dir(PathBuf),
    /// Nothing: the paths that were looked at, nearby module names, and
    /// what to do when the package exists but isn't installed.
    Missing { tried: Vec<String>, nearby: Vec<String>, help: Option<String> },
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub target: Target,
    /// For `from` imports: the module file each imported name refers to,
    /// when it names a file of a folder (`from shapes import circle`) rather
    /// than something defined in the module.
    pub submodules: Vec<Option<usize>>,
}

pub struct Project {
    pub files: Vec<SourceFile>,
    /// Syntax errors (of every file).
    pub diags: Vec<Diagnostic>,
    /// The packages Thorn installed for the program.
    pub packages: Packages,
}

impl Project {
    /// The file containing a global offset.
    pub fn file_of(&self, offset: u32) -> usize {
        self.files.iter().rposition(|f| f.base <= offset).unwrap_or(0)
    }

    /// Identifies the exact sources of the program, for the build cache.
    pub fn fingerprint(&self) -> String {
        let mut h = crate::driver::Fnv::new();
        for f in &self.files {
            h.write(f.display.as_bytes()).write(f.text.as_bytes());
        }
        h.hex()
    }
}

/// Reads a module from disk (the language server substitutes unsaved editor buffers).
pub type Reader<'a> = &'a dyn Fn(&Path) -> std::io::Result<String>;

pub fn read_from_disk(p: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(p)
}

/// Load `src` (the file at `path`, if it is on disk) and everything it imports.
pub fn load(path: Option<&Path>, display: &str, src: &str, read: Reader) -> Project {
    let canonical = path.and_then(|p| std::fs::canonicalize(p).ok().or_else(|| Some(p.to_path_buf())));
    let root_dir = canonical.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf);
    let packages = if path.is_some() { find_packages(root_dir.as_deref()) } else { Packages::default() };
    let mut l = Loader { files: Vec::new(), by_path: HashMap::new(), by_std: HashMap::new(), diags: Vec::new(), read, next_base: 0, packages };
    let display_dir = Path::new(display).parent().map(Path::to_path_buf).unwrap_or_default();
    l.add(FileKind::Root, canonical, display.to_string(), String::new(), src.to_string(), None);
    let mut next = 0;
    while next < l.files.len() {
        l.resolve_imports(next, root_dir.as_deref(), &display_dir);
        next += 1;
    }
    // The prelude (`Result`, `Ok`, `Err`, `Unit`) is part of every program, as its last file.
    let prelude = stdlib::prelude();
    l.add(FileKind::Std, None, stdlib::display_path(prelude.name), prelude.name.to_string(), prelude.source.to_string(), None);
    Project { files: l.files, diags: l.diags, packages: l.packages }
}

struct Loader<'a> {
    files: Vec<SourceFile>,
    by_path: HashMap<PathBuf, usize>,
    by_std: HashMap<&'static str, usize>,
    diags: Vec<Diagnostic>,
    read: Reader<'a>,
    next_base: u32,
    packages: Packages,
}

impl Loader<'_> {
    fn add(&mut self, kind: FileKind, path: Option<PathBuf>, display: String, name: String, text: String, from: Option<(usize, Span)>) -> usize {
        let base = self.next_base;
        // One byte of gap, so an end-of-file span never touches the next file.
        self.next_base = base.saturating_add(text.len() as u32 + 1);
        let (program, diags) = crate::parser::parse_at(&text, base as usize);
        match kind {
            FileKind::Std => self.diags.extend(diags.iter().map(|d| crate::internal_error(d, &name))),
            _ => self.diags.extend(diags),
        }
        let mut symbol: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
        if self.files.iter().any(|f| f.symbol == symbol) {
            symbol = format!("{symbol}_{}", self.files.len());
        }
        let id = self.files.len();
        if let Some(p) = &path {
            self.by_path.insert(p.clone(), id);
        }
        self.files.push(SourceFile { kind, path, display, name, symbol, text, base, program, imports: Vec::new(), imported_from: from });
        id
    }

    fn resolve_imports(&mut self, file: usize, root_dir: Option<&Path>, display_dir: &Path) {
        let imports: Vec<Import> =
            self.files[file].program.items.iter().filter_map(|i| if let Item::Import(im) = i { Some(im.clone()) } else { None }).collect();
        let dir = match self.files[file].kind {
            FileKind::Std => None,
            _ => self.files[file].path.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf),
        };
        let mut out = Vec::new();
        for im in &imports {
            let target = self.module(file, im, dir.as_deref(), root_dir, display_dir);
            let submodules = match (&im.names, &target) {
                (Some(names), Target::File(t)) => {
                    let t = *t;
                    // A file next to the module, in the folder of the same name.
                    let folder = self.files[t].path.as_ref().map(|p| p.with_extension(""));
                    names
                        .iter()
                        .map(|n| {
                            if defines(&self.files[t].program, &n.name.name) {
                                return None;
                            }
                            let folder = folder.as_ref()?;
                            self.open(&folder.join(format!("{}.tr", n.name.name)), file, im.span, root_dir, display_dir)
                        })
                        .collect()
                }
                (Some(names), Target::Dir(d)) => {
                    let d = d.clone();
                    names.iter().map(|n| self.open(&d.join(format!("{}.tr", n.name.name)), file, im.span, root_dir, display_dir)).collect()
                }
                _ => Vec::new(),
            };
            out.push(Resolved { target, submodules });
        }
        self.files[file].imports = out;
    }

    /// The module (or folder) an import's path names.
    fn module(&mut self, file: usize, im: &Import, dir: Option<&Path>, root_dir: Option<&Path>, display_dir: &Path) -> Target {
        let segs: Vec<&str> = im.path.iter().map(|i| i.name.as_str()).collect();
        let std_name = (im.level == 0 && segs.len() == 1).then(|| stdlib::find(segs[0])).flatten();
        let Some(dir) = dir else {
            // The standard library, or a program that only exists in memory.
            if let Some(m) = std_name {
                return Target::File(self.std(m, file, im.span));
            }
            return Target::Missing { tried: Vec::new(), nearby: stdlib::importable().map(|m| m.name.to_string()).collect(), help: None };
        };
        let mut base = dir.to_path_buf();
        for _ in 1..im.level.max(1) {
            base = base.parent().map(Path::to_path_buf).unwrap_or(base);
        }
        if segs.is_empty() {
            return Target::Dir(base);
        }
        let mut stem = base.clone();
        for s in &segs {
            stem.push(s);
        }
        let file_path = stem.with_extension("tr");
        // A file of yours wins over a package or a standard-library module of the same name.
        if let Some(id) = self.open(&file_path, file, im.span, root_dir, display_dir) {
            return Target::File(id);
        }
        // `import colors`, `import colors.codes`: the package's `src/` folder.
        if im.level == 0
            && let Some(pkg) = self.packages.get(segs[0])
        {
            let mut stem = pkg.root.join("src");
            for s in &segs {
                stem.push(s);
            }
            let in_pkg = stem.with_extension("tr");
            if let Some(id) = self.open(&in_pkg, file, im.span, root_dir, display_dir) {
                return Target::File(id);
            }
            if stem.is_dir() {
                return Target::Dir(stem);
            }
            let parent = in_pkg.parent().map(Path::to_path_buf).unwrap_or_default();
            return Target::Missing { tried: vec![self.package_display(&in_pkg).unwrap_or_default()], nearby: module_names(&parent), help: None };
        }
        if let Some(m) = std_name {
            return Target::File(self.std(m, file, im.span));
        }
        if stem.is_dir() {
            return Target::Dir(stem);
        }
        let parent = file_path.parent().unwrap_or(&base).to_path_buf();
        let mut nearby: Vec<String> = module_names(&parent);
        if im.level == 0 && segs.len() == 1 {
            nearby.extend(stdlib::importable().map(|m| m.name.to_string()));
            nearby.extend(self.packages.list.iter().map(|p| p.name.clone()));
        }
        let help = if im.level == 0 { self.packages.missing_help(segs[0]) } else { None };
        Target::Missing { tried: vec![shown(&file_path, root_dir, display_dir)], nearby, help }
    }

    /// How a file inside a package is shown: `colors@1.0.0/src/colors.tr`.
    fn package_display(&self, path: &Path) -> Option<String> {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path));
        let pkg = self.packages.containing(&canonical)?;
        let rel = canonical.strip_prefix(&pkg.root).ok()?;
        Some(format!("{}@{}/{}", pkg.name, pkg.version, rel.display()))
    }

    /// Load the file at `path` if it exists (once per file).
    fn open(&mut self, path: &Path, from: usize, span: Span, root_dir: Option<&Path>, display_dir: &Path) -> Option<usize> {
        if !path.is_file() {
            return None;
        }
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(&id) = self.by_path.get(&canonical) {
            return Some(id);
        }
        let text = match (self.read)(&canonical) {
            Ok(t) => t,
            Err(e) => {
                self.diags.push(Diagnostic::error(format!("cannot read `{}`: {e}", shown(path, root_dir, display_dir)), span));
                return None;
            }
        };
        let name = canonical.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // A package's files, however they're reached (an `import colors`, or
        // a relative import inside the package), are the package's.
        let (kind, display) = match self.package_display(&canonical) {
            Some(d) => (FileKind::Package, d),
            None => (FileKind::User, shown(&canonical, root_dir, display_dir)),
        };
        Some(self.add(kind, Some(canonical), display, name, text, Some((from, span))))
    }

    fn std(&mut self, m: &'static stdlib::StdModule, from: usize, span: Span) -> usize {
        if let Some(&id) = self.by_std.get(m.name) {
            return id;
        }
        let id = self.add(FileKind::Std, None, stdlib::display_path(m.name), m.name.to_string(), m.source.to_string(), Some((from, span)));
        self.by_std.insert(m.name, id);
        id
    }
}

/// Does the module define a function, struct or enum called `name`?
fn defines(p: &Program, name: &str) -> bool {
    p.items.iter().any(|i| match i {
        Item::Fn(f) => f.name.name == name,
        Item::Struct(s) => s.name.name == name,
        Item::Enum(e) => e.name.name == name,
        Item::Interface(i) => i.name.name == name,
        Item::Const(c) => c.name.name == name,
        Item::Attribute(a) => a.name.name == name,
        Item::Import(_) => false,
    })
}

/// The `.tr` files of a folder, by module name (for suggestions and completion).
pub fn module_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension().is_some_and(|x| x == "tr")).then(|| p.file_stem()?.to_str().map(String::from)).flatten()
        })
        .collect();
    names.sort();
    names
}

/// How to show a path to the user: relative to the entry file's folder, as
/// the entry file itself was named on the command line.
fn shown(path: &Path, root_dir: Option<&Path>, display_dir: &Path) -> String {
    let Some(root) = root_dir else { return path.display().to_string() };
    let abs = if path.is_absolute() { path.to_path_buf() } else { root.join(path) };
    let rel = relative(root, &normalize(&abs));
    display_dir.join(rel).display().to_string()
}

/// `a/b/../c` -> `a/c`, without touching the file system.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// The path from folder `from` to `to` (both absolute).
fn relative(from: &Path, to: &Path) -> PathBuf {
    let a: Vec<Component> = from.components().collect();
    let b: Vec<Component> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..a.len() {
        out.push("..");
    }
    for c in &b[common..] {
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tarn-project-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (p, text) in files {
            let p = dir.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
    }

    fn load_at(dir: &Path, main: &str) -> Project {
        let path = dir.join(main);
        let src = std::fs::read_to_string(&path).unwrap();
        load(Some(&path), main, &src, &read_from_disk)
    }

    #[test]
    fn resolves_files_folders_and_the_standard_library() {
        let dir = tree(
            "resolve",
            &[
                ("main.tr", "import geometry\nimport time\nimport shapes.circle as c\nfrom shapes import square\nfrom geometry import Point\nimport nope\n"),
                ("geometry.tr", "struct Point:\n    x: Int\n"),
                ("shapes/circle.tr", "from .. import geometry\n"),
                ("shapes/square.tr", "from ..geometry import Point\n"),
            ],
        );
        let p = load_at(&dir, "main.tr");
        assert!(p.diags.is_empty());
        let names: Vec<&str> = p.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["", "geometry", "time", "circle", "square", "prelude"]);
        let displays: Vec<&str> = p.files.iter().map(|f| f.display.as_str()).collect();
        assert_eq!(displays, ["main.tr", "geometry.tr", "<std>/time.tr", "shapes/circle.tr", "shapes/square.tr", "<std>/prelude.tr"]);
        let root = &p.files[0].imports;
        assert!(matches!(root[0].target, Target::File(1)));
        assert!(matches!(root[1].target, Target::File(2)));
        assert!(matches!(root[2].target, Target::File(3)));
        assert!(matches!(root[3].target, Target::Dir(_)) && root[3].submodules == [Some(4)]);
        assert!(matches!(root[4].target, Target::File(1)) && root[4].submodules == [None]);
        assert!(matches!(&root[5].target, Target::Missing { tried, .. } if tried == &["nope.tr"]));
        // `..` climbs to the folder above, and both reach the same file.
        assert!(matches!(p.files[3].imports[0].target, Target::Dir(_)) && p.files[3].imports[0].submodules == [Some(1)]);
        assert!(matches!(p.files[4].imports[0].target, Target::File(1)));
        // Spans of different files never overlap.
        for w in p.files.windows(2) {
            assert!(w[0].base + w[0].text.len() as u32 <= w[1].base);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn your_files_shadow_the_standard_library() {
        let dir = tree("shadow", &[("main.tr", "import time\n"), ("time.tr", "fn now() -> Int:\n    return 1\n")]);
        let p = load_at(&dir, "main.tr");
        assert_eq!(p.files[1].kind, FileKind::User);
        assert_eq!(p.files[1].display, "time.tr");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cycles_load_each_file_once() {
        let dir = tree("cycle", &[("main.tr", "import a\n"), ("a.tr", "import b\n"), ("b.tr", "import a\nimport main\n")]);
        let p = load_at(&dir, "main.tr");
        assert_eq!(p.files.len(), 4); // and the prelude
        assert!(matches!(p.files[2].imports[0].target, Target::File(1)));
        assert!(matches!(p.files[2].imports[1].target, Target::File(0)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_tables_and_what_goes_wrong_with_them() {
        let table = |json: &str| {
            let dir = tree("pkgtable", &[("app/thorn.toml", "[package]\n"), ("app/.thorn/packages.json", json), ("app/lib/x.tr", ""), ("pkgs/a/src/a.tr", "")]);
            (discover_packages(Some(&dir.join("app/lib")), None), dir)
        };
        // Relative paths are relative to the table's folder; the project is the nearest thorn.toml above.
        let (p, dir) = table(r#"{"version": 1, "packages": {"a": {"version": "1.2.3", "path": "../../pkgs/a"}, "b": {"version": "1.0.0", "path": "/nowhere"}}}"#);
        assert_eq!(p.project_dir.as_deref(), Some(dir.join("app").as_path()));
        assert_eq!(p.list.len(), 1);
        assert_eq!((p.list[0].name.as_str(), p.list[0].version.as_str()), ("a", "1.2.3"));
        assert_eq!(p.list[0].root, std::fs::canonicalize(dir.join("pkgs/a")).unwrap());
        assert_eq!(p.gone, ["b"]);
        assert!(p.missing_help("b").unwrap().contains("removed from Thorn's cache"));
        assert!(p.missing_help("c").is_none());
        let (p, _) = table(r#"{"version": 2, "packages": {}}"#);
        assert!(p.error.as_deref().unwrap().contains("version 2 is newer"));
        assert!(p.missing_help("a").unwrap().contains("run `thorn sync`"));
        // No thorn.toml above: no packages, and no complaints.
        let dir = tree("pkgnone", &[("x.tr", "")]);
        let p = discover_packages(Some(&dir), None);
        assert!(p.project_dir.is_none() && p.list.is_empty() && p.error.is_none());
        // Locked but never synced.
        let dir = tree("pkglock", &[("thorn.toml", ""), ("thorn.lock", "version = 1\n\n[[package]]\nname = \"a\"\nversion = \"1.0.0\"\n")]);
        let p = discover_packages(Some(&dir), None);
        assert_eq!(p.locked, ["a"]);
        assert!(p.error.as_deref().unwrap().contains("doesn't exist"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_are_shown_like_the_entry_file() {
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/c/d.tr")), PathBuf::from("../c/d.tr"));
        assert_eq!(shown(Path::new("/p/app/lib/x.tr"), Some(Path::new("/p/app")), Path::new("app")), "app/lib/x.tr");
        assert_eq!(shown(Path::new("/p/app/../x.tr"), Some(Path::new("/p/app")), Path::new("")), "../x.tr");
    }
}

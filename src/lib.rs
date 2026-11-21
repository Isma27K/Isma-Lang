//! The Tarn compiler: Python-like syntax, Kotlin-strict types, native speed.

pub mod arc;
pub mod ast;
pub mod check;
pub mod codegen;
pub mod cycles;
pub mod diagnostics;
pub mod doc;
pub mod driver;
pub mod ide;
pub mod lexer;
pub mod lower;
pub mod lsp;
pub mod mir;
pub mod parser;
pub mod patterns;
pub mod project;
pub mod span;
pub mod stdlib;
pub mod tast;
pub mod token;
pub mod types;

use diagnostics::Diagnostic;
use project::{FileKind, Project};
use span::{LineIndex, SourceMap, Span};
use std::path::Path;

pub(crate) fn internal_error(d: &Diagnostic, module: &str) -> Diagnostic {
    Diagnostic::error(format!("internal error in the standard library module `{module}`: {}", d.message), Span::default())
        .with_note("this is a bug in the Tarn compiler, not in your code")
}

/// Load the program in the file at `path` (with contents `src`) and the files it imports.
pub fn load_file(path: &Path, display: &str, src: &str) -> Project {
    project::load(Some(path), display, src, &project::read_from_disk)
}

/// A program that only exists in memory: it can import the standard library only.
pub fn load_source(src: &str) -> Project {
    project::load(None, "<input>", src, &project::read_from_disk)
}

/// Problems inside standard-library code are compiler bugs: never show them
/// as if they were in the user's code. Warnings in packages are hidden too.
fn visible_diagnostics(project: &Project, diags: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diags
        .into_iter()
        .filter_map(|d| {
            let f = &project.files[project.file_of(d.span.start)];
            match f.kind {
                FileKind::Std if d.is_error() => Some(internal_error(&d, &f.name)),
                FileKind::Std => None,
                // A package's warnings are its authors' business, not yours.
                FileKind::Package if !d.is_error() => None,
                _ => Some(d),
            }
        })
        .collect()
}

/// The result of a successful type check.
pub struct Checked {
    pub program: tast::TProgram,
    pub warnings: Vec<Diagnostic>,
}

/// Front end only: parse + type-check. On failure, every diagnostic (errors
/// and warnings) of the user's files.
pub fn check_project(project: &Project) -> Result<Checked, Vec<Diagnostic>> {
    if !project.diags.is_empty() {
        return Err(project.diags.clone());
    }
    match check::check(&project.files) {
        Ok((program, warnings)) => Ok(Checked { program, warnings: visible_diagnostics(project, warnings) }),
        Err(d) => Err(visible_diagnostics(project, d)),
    }
}

/// [`check_project`] for a program in memory.
pub fn check_source(src: &str) -> Result<Checked, Vec<Diagnostic>> {
    check_project(&load_source(src))
}

/// Editor analysis of the entry file of `project`: always returns
/// hover/definition/completion data, even for broken code. With syntax errors
/// only those are reported (type errors on a half-parsed file would just be
/// noise). Problems in imported files show up on the `import` that leads to them.
pub fn analyze_project(project: &Project) -> check::Analysis {
    let mut a = check::analyze(&project.files);
    let root = &project.files[0];
    let in_root = |s: Span| root.contains(s.start);
    let diags = if project.diags.iter().any(|d| in_root(d.span)) {
        a.program = None;
        project.diags.clone()
    } else {
        // Syntax errors in imported files are reported on their `import` lines;
        // the entry file itself is still fully checked.
        let mut all = project.diags.clone();
        all.extend(visible_diagnostics(project, std::mem::take(&mut a.diagnostics)));
        if !project.diags.is_empty() {
            a.program = None;
        }
        all
    };
    let (mut own, elsewhere): (Vec<Diagnostic>, Vec<Diagnostic>) = diags.into_iter().partition(|d| in_root(d.span));
    own.extend(import_errors(project, elsewhere));
    a.diagnostics = own;
    // Editor positions only make sense inside the entry file (definitions may
    // point into the user's other files).
    a.ide.hovers.retain(|h| in_root(h.span));
    a.ide.definitions.retain(|(used, def)| in_root(*used) && project.files[project.file_of(def.start)].kind != FileKind::Std);
    a.ide.expr_types.retain(|(s, _)| in_root(*s));
    a.ide.inlay_hints.retain(|(o, _)| root.contains(*o));
    a.ide.locals.retain(|l| in_root(l.decl));
    a
}

/// Errors in imported files, reported on the entry file's `import` lines that lead to them.
fn import_errors(project: &Project, diags: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut by_import: Vec<(Span, Vec<String>)> = Vec::new();
    for d in diags.iter().filter(|d| d.is_error()) {
        let mut f = project.file_of(d.span.start);
        let file = &project.files[f];
        let (line, col) = LineIndex::new(&file.text).line_col(d.span.start - file.base);
        let shown = format!("{}:{line}:{col}: {}", file.display, d.message);
        // Walk up the chain of importers to the entry file.
        let mut guard = 0;
        while let Some((parent, span)) = project.files[f].imported_from {
            if parent == 0 {
                match by_import.iter_mut().find(|(s, _)| *s == span) {
                    Some((_, list)) => list.push(shown),
                    None => by_import.push((span, vec![shown])),
                }
                break;
            }
            f = parent;
            guard += 1;
            if guard > project.files.len() {
                break;
            }
        }
    }
    by_import
        .into_iter()
        .map(|(span, list)| {
            let n = list.len();
            let mut d = Diagnostic::error(
                if n == 1 { "an imported file has an error".to_string() } else { format!("imported files have {n} errors") },
                span,
            )
            .with_primary("errors in this module");
            for e in list.iter().take(5) {
                d = d.with_note(e.clone());
            }
            if n > 5 {
                d = d.with_note(format!("and {} more", n - 5));
            }
            d
        })
        .collect()
}

/// [`analyze_project`] for a program in memory.
pub fn analyze(src: &str) -> check::Analysis {
    analyze_project(&load_source(src))
}

/// Building or running needs an entry point. Checking doesn't: a file
/// without `main` is a module (and the editor checks it like any other file).
pub fn missing_main(project: &Project) -> Option<Diagnostic> {
    let root = &project.files[0];
    let has_main = root.program.items.iter().any(|i| matches!(i, ast::Item::Fn(f) if f.name.name == "main"));
    if has_main {
        return None;
    }
    let file = Path::new(&root.display).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display.clone());
    Some(
        Diagnostic::error("no `main` function found", Span::default())
            .with_note(format!("`{file}` has no `main`, so it can only be imported as a module: run the file that imports it"))
            .with_help("or give it an entry point:\n\nfn main():\n    print(\"hello\")"),
    )
}

/// The back end's view of a program: MIR after the memory-management passes.
pub fn compile_project_to_mir(project: &Project) -> Result<(mir::Program, Checked), Vec<Diagnostic>> {
    let checked = match check_project(project) {
        Ok(c) => c,
        Err(mut d) => {
            d.extend(missing_main(project));
            return Err(d);
        }
    };
    if let Some(d) = missing_main(project) {
        let mut all = checked.warnings;
        all.push(d);
        return Err(all);
    }
    let mut program = lower::lower(&checked.program);
    arc::run(&mut program);
    Ok((program, checked))
}

/// [`compile_project_to_mir`] for a program in memory.
pub fn compile_to_mir(src: &str) -> Result<(mir::Program, Checked), Vec<Diagnostic>> {
    compile_project_to_mir(&load_source(src))
}

/// The output of the full pipeline.
pub struct Compiled {
    pub ir: String,
    /// The MIR, pretty-printed (for `--emit-mir`).
    pub mir: String,
    pub warnings: Vec<Diagnostic>,
}

/// Full pipeline up to LLVM IR text.
pub fn compile_project(project: &Project, target_triple: Option<&str>) -> Result<Compiled, Vec<Diagnostic>> {
    let (program, checked) = compile_project_to_mir(project)?;
    let root = &project.files[0];
    let mut sources = SourceMap::new(&root.display, &root.text);
    for f in &project.files[1..] {
        sources.add(&f.display, f.base, &f.text);
    }
    // Where the program's `.env` is by default (see the env module).
    if let Some(dir) = root.path.as_deref().and_then(std::path::Path::parent) {
        sources.set_root_dir(&dir.to_string_lossy());
    }
    let ir = codegen::generate(&program, &sources, target_triple);
    Ok(Compiled { ir, mir: mir::print_program(&program), warnings: checked.warnings })
}

/// Full pipeline for a program in memory, shown as `path` in panic messages.
/// `Ok((ir, warnings))`.
pub fn compile_to_ir(src: &str, path: &str, target_triple: Option<&str>) -> Result<(String, Vec<Diagnostic>), Vec<Diagnostic>> {
    let project = project::load(None, path, src, &project::read_from_disk);
    compile_project(&project, target_triple).map(|c| (c.ir, c.warnings))
}

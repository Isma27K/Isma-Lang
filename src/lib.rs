//! The Isma compiler: Python-like syntax, Kotlin-strict types, native speed.

pub mod arc;
pub mod ast;
pub mod check;
pub mod codegen;
pub mod cycles;
pub mod diagnostics;
pub mod driver;
pub mod ide;
pub mod lexer;
pub mod lower;
pub mod lsp;
pub mod mir;
pub mod parser;
pub mod patterns;
pub mod span;
pub mod stdlib;
pub mod tast;
pub mod token;
pub mod types;

use diagnostics::Diagnostic;
use span::{MODULE_BASE, SourceMap, Span, is_user_span};

/// A standard-library module parsed for this compilation.
pub struct LoadedModule {
    pub name: &'static str,
    pub source: &'static str,
    pub base: u32,
    pub program: ast::Program,
}

/// Parse the user's file and every standard-library module it imports.
fn load(src: &str) -> (ast::Program, Vec<LoadedModule>, Vec<Diagnostic>) {
    let (program, mut diags) = parser::parse(src);
    let mut modules: Vec<LoadedModule> = Vec::new();
    for item in &program.items {
        let ast::Item::Import(name) = item else { continue };
        let Some(m) = stdlib::find(&name.name) else { continue };
        if modules.iter().any(|l| l.name == m.name) {
            continue;
        }
        let base = (modules.len() as u32 + 1) * MODULE_BASE;
        let (module_program, module_diags) = parser::parse_at(m.source, base as usize);
        diags.extend(module_diags.into_iter().map(|d| internal_error(&d, m.name)));
        modules.push(LoadedModule { name: m.name, source: m.source, base, program: module_program });
    }
    (program, modules, diags)
}

fn inputs(modules: &[LoadedModule]) -> Vec<check::ModuleInput<'_>> {
    modules
        .iter()
        .map(|m| check::ModuleInput { name: m.name, program: &m.program, source: m.source, base: m.base })
        .collect()
}

fn internal_error(d: &Diagnostic, module: &str) -> Diagnostic {
    Diagnostic::error(format!("internal error in the standard library module `{module}`: {}", d.message), Span::default())
        .with_note("this is a bug in the Isma compiler, not in your code")
}

/// Problems inside standard-library code are compiler bugs; never show
/// them as if they were in the user's file.
fn user_diagnostics(diags: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diags
        .into_iter()
        .filter_map(|d| {
            if is_user_span(d.span) {
                Some(d)
            } else if d.is_error() {
                Some(internal_error(&d, "?"))
            } else {
                None
            }
        })
        .collect()
}

/// The result of a successful type check.
pub struct Checked {
    pub program: tast::TProgram,
    pub warnings: Vec<Diagnostic>,
    pub modules: Vec<LoadedModule>,
}

/// Front end only: parse + type-check. On failure, every diagnostic
/// (errors and warnings) of the user's file.
pub fn check_source(src: &str) -> Result<Checked, Vec<Diagnostic>> {
    let (program, modules, diags) = load(src);
    if !diags.is_empty() {
        return Err(diags);
    }
    match check::check(&program, src, &inputs(&modules)) {
        Ok((program, warnings)) => Ok(Checked { program, warnings: user_diagnostics(warnings), modules }),
        Err(d) => Err(user_diagnostics(d)),
    }
}

/// Editor analysis: always returns hover/definition/completion data, even for
/// broken code. With syntax errors only those are reported (type errors on a
/// half-parsed file would just be noise).
pub fn analyze(src: &str) -> check::Analysis {
    let (program, modules, parse_diags) = load(src);
    let mut a = check::analyze(&program, src, &inputs(&modules));
    if parse_diags.is_empty() {
        a.diagnostics = user_diagnostics(a.diagnostics);
    } else {
        a.diagnostics = parse_diags;
        a.program = None;
    }
    // Editor positions only make sense inside the user's own file.
    a.ide.hovers.retain(|h| is_user_span(h.span));
    a.ide.definitions.retain(|(used, def)| is_user_span(*used) && is_user_span(*def));
    a.ide.expr_types.retain(|(s, _)| is_user_span(*s));
    a.ide.inlay_hints.retain(|(o, _)| *o < MODULE_BASE);
    a.ide.locals.retain(|l| is_user_span(l.decl));
    a
}

/// The back end's view of a program: MIR after the memory-management passes.
pub fn compile_to_mir(src: &str) -> Result<(mir::Program, Checked), Vec<Diagnostic>> {
    let checked = check_source(src)?;
    let mut program = lower::lower(&checked.program);
    arc::run(&mut program);
    Ok((program, checked))
}

/// The output of the full pipeline.
pub struct Compiled {
    pub ir: String,
    /// The MIR, pretty-printed (for `--emit-mir`).
    pub mir: String,
    pub warnings: Vec<Diagnostic>,
}

/// Full pipeline up to LLVM IR text.
pub fn compile(src: &str, path: &str, target_triple: Option<&str>) -> Result<Compiled, Vec<Diagnostic>> {
    let (program, checked) = compile_to_mir(src)?;
    let mut sources = SourceMap::new(path, src);
    for m in &checked.modules {
        sources.add(&stdlib::display_path(m.name), m.base, m.source);
    }
    let ir = codegen::generate(&program, &sources, target_triple);
    Ok(Compiled { ir, mir: mir::print_program(&program), warnings: checked.warnings })
}

/// Full pipeline up to LLVM IR text. `Ok((ir, warnings))`.
pub fn compile_to_ir(src: &str, path: &str, target_triple: Option<&str>) -> Result<(String, Vec<Diagnostic>), Vec<Diagnostic>> {
    compile(src, path, target_triple).map(|c| (c.ir, c.warnings))
}

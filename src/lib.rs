//! The Isma compiler: Python-like syntax, Kotlin-strict types, native speed.

pub mod ast;
pub mod check;
pub mod codegen;
pub mod diagnostics;
pub mod driver;
pub mod ide;
pub mod lexer;
pub mod lsp;
pub mod parser;
pub mod span;
pub mod tast;
pub mod token;
pub mod types;

use diagnostics::Diagnostic;

/// Front end only: parse + type-check. `Ok((program, warnings))`, or every
/// diagnostic (errors and warnings) if there is at least one error.
pub fn check_source(src: &str) -> Result<(tast::TProgram, Vec<Diagnostic>), Vec<Diagnostic>> {
    let (program, diags) = parser::parse(src);
    if !diags.is_empty() {
        return Err(diags);
    }
    check::check(&program)
}

/// Editor analysis: always returns hover/definition/completion data, even for
/// broken code. With syntax errors only those are reported (type errors on a
/// half-parsed file would just be noise).
pub fn analyze(src: &str) -> check::Analysis {
    let (program, parse_diags) = parser::parse(src);
    let mut analysis = check::analyze(&program);
    if !parse_diags.is_empty() {
        analysis.diagnostics = parse_diags;
        analysis.program = None;
    }
    analysis
}

/// Full pipeline up to LLVM IR text. `Ok((ir, warnings))`.
pub fn compile_to_ir(src: &str, path: &str, target_triple: Option<&str>) -> Result<(String, Vec<Diagnostic>), Vec<Diagnostic>> {
    let (tprog, warnings) = check_source(src)?;
    Ok((codegen::generate(&tprog, src, path, target_triple), warnings))
}

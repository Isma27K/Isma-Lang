//! The Isma compiler: Python-like syntax, Kotlin-strict types, native speed.

pub mod ast;
pub mod check;
pub mod codegen;
pub mod diagnostics;
pub mod driver;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod tast;
pub mod token;
pub mod types;

use diagnostics::Diagnostic;

/// Front end only: parse + type-check.
pub fn check_source(src: &str) -> Result<tast::TProgram, Vec<Diagnostic>> {
    let (program, diags) = parser::parse(src);
    if !diags.is_empty() {
        return Err(diags);
    }
    check::check(&program)
}

/// Full pipeline up to LLVM IR text.
pub fn compile_to_ir(src: &str, path: &str, target_triple: Option<&str>) -> Result<String, Vec<Diagnostic>> {
    let tprog = check_source(src)?;
    Ok(codegen::generate(&tprog, src, path, target_triple))
}

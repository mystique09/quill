pub mod ast;
pub mod compile;
pub mod error;
pub mod eval;
pub mod infer;
pub mod lexer;
pub mod modules;
pub mod ownership;
pub mod parser;
pub mod types;

use std::path::Path;

pub use error::{Diagnostic, Severity, Span};

/// Compile and run a Quill program given its source text, resolving `use`
/// imports against the embedded standard library and the current directory.
pub fn run(source: &str) -> Result<String, Vec<Diagnostic>> {
    let program = modules::Loader::new().load_source(source)?;
    infer::infer_program(&program)?;
    let ownership_errors = ownership::check(&program);
    if !ownership_errors.is_empty() {
        return Err(ownership_errors);
    }
    eval::evaluate_program(&program)
}

/// Compile and run a Quill source file, resolving `use` imports relative to
/// the file's directory.
pub fn run_file(path: &Path) -> Result<String, Vec<Diagnostic>> {
    let program = modules::Loader::new().load_file(path)?;
    infer::infer_program(&program)?;
    let ownership_errors = ownership::check(&program);
    if !ownership_errors.is_empty() {
        return Err(ownership_errors);
    }
    eval::evaluate_program(&program)
}

/// Type-check a source file (resolving imports) without executing it.
pub fn check(source: &str) -> Result<infer::TypeInfo, Vec<Diagnostic>> {
    let program = modules::Loader::new().load_source(source)?;
    infer::infer_program(&program)
}

/// Compile a source file to a relocatable object file.
pub fn compile_file(path: &Path) -> Result<Vec<u8>, Vec<Diagnostic>> {
    let program = modules::Loader::new().load_file(path)?;
    let types = infer::infer_program(&program)?;
    compile::compile_object(&program, &types)
        .map_err(|e| vec![Diagnostic::error(7000, e, None)])
}

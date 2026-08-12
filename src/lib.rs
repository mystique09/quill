pub mod ast;
pub mod error;
pub mod eval;
pub mod infer;
pub mod lexer;
pub mod ownership;
pub mod parser;
pub mod types;

pub use error::{Diagnostic, Severity, Span};

/// Compile and run a Quill source file, returning the printed output.
pub fn run(source: &str) -> Result<String, Vec<Diagnostic>> {
    let program = parser::parse(source)?;
    let types = infer::infer_program(&program)?;
    let ownership_errors = ownership::check(&program, &types);
    if !ownership_errors.is_empty() {
        return Err(ownership_errors);
    }
    eval::evaluate_program(&program)
}

/// Type-check a source file without executing it.
pub fn check(source: &str) -> Result<infer::TypeInfo, Vec<Diagnostic>> {
    let program = parser::parse(source)?;
    infer::infer_program(&program)
}

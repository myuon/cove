//! Lexer, AST, and parser for the Cove surface language.

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod token;

use cove_diag::{Diagnostic, FileId, SourceMap};

/// Lexes and parses one `.cove` file.
pub fn parse_file(sources: &SourceMap, file: FileId) -> Result<ast::SourceUnit, Vec<Diagnostic>> {
    let tokens = lexer::lex(sources, file)?;
    parser::parse(sources, file, tokens)
}

//! Lexer, AST, and parser for the Cove surface language.

pub mod ast;
pub mod format;
pub mod lexer;
pub mod number;
pub mod parser;
pub mod token;

use cove_diag::{Diagnostic, FileId, SourceMap};

/// Lexes, parses, and numbers one `.cove` file.
///
/// Numbering happens here rather than in the parser so that every caller gets
/// it: a unit this function returns has an [`ast::ExprId`] on every
/// expression, and none of them is [`ast::ExprId::UNSET`].
pub fn parse_file(sources: &SourceMap, file: FileId) -> Result<ast::SourceUnit, Vec<Diagnostic>> {
    let tokens = lexer::lex(sources, file)?;
    let mut unit = parser::parse(sources, file, tokens)?;
    number::number_unit(&mut unit);
    Ok(unit)
}

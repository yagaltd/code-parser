/// Language extractor trait — implemented by each language.
use code_parser_ir::FileParseIR;
use tree_sitter::Tree;

/// A language-specific extractor that walks a tree-sitter tree and
/// populates a `FileParseIR` with symbols, calls, imports, and diagnostics.
pub trait LanguageExtractor {
    fn extract(&self, tree: &Tree, source: &[u8], file_path: &str) -> FileParseIR;
}

#[cfg(feature = "rust")]
pub mod rust;

#[cfg(feature = "typescript")]
pub mod typescript;

#[cfg(feature = "python")]
pub mod python;

#[cfg(feature = "javascript")]
pub mod javascript;

pub mod golden;

pub mod utils;

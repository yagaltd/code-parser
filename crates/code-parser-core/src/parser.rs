/// Thread-local tree-sitter parser pool.
///
/// Each thread gets its own parser instance per language.
/// Parsers are not `Sync`, so we keep them thread-local.
/// Each language parser is gated behind its feature flag.

use tree_sitter::Tree;

use crate::language::Language;

// ── Rust parser ──────────────────────────────────────────────────────────

#[cfg(feature = "rust")]
mod rust_parser {
    use std::cell::RefCell;
    use tree_sitter::{Parser, Tree};

    thread_local! {
        static PARSER: RefCell<Option<Parser>> = RefCell::new(None);
    }

    pub fn parse(source: &[u8]) -> Result<Tree, anyhow::Error> {
        PARSER.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                let mut parser = Parser::new();
                parser
                    .set_language(&tree_sitter_rust::LANGUAGE.into())
                    .map_err(|e| anyhow::anyhow!("Failed to set Rust language: {e}"))?;
                *opt = Some(parser);
            }
            opt.as_mut()
                .unwrap()
                .parse(source, None)
                .ok_or_else(|| anyhow::anyhow!("tree-sitter parse returned None"))
        })
    }
}

// ── TypeScript parser ────────────────────────────────────────────────────

#[cfg(feature = "typescript")]
mod typescript_parser {
    use std::cell::RefCell;
    use tree_sitter::{Parser, Tree};

    thread_local! {
        static TS_PARSER: RefCell<Option<Parser>> = RefCell::new(None);
        static TSX_PARSER: RefCell<Option<Parser>> = RefCell::new(None);
    }

    pub fn parse(source: &[u8], is_tsx: bool) -> Result<Tree, anyhow::Error> {
        if is_tsx {
            TSX_PARSER.with(|cell| {
                let mut opt = cell.borrow_mut();
                if opt.is_none() {
                    let mut parser = Parser::new();
                    parser
                        .set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
                        .map_err(|e| anyhow::anyhow!("Failed to set TSX language: {e}"))?;
                    *opt = Some(parser);
                }
                opt.as_mut()
                    .unwrap()
                    .parse(source, None)
                    .ok_or_else(|| anyhow::anyhow!("tree-sitter parse returned None"))
            })
        } else {
            TS_PARSER.with(|cell| {
                let mut opt = cell.borrow_mut();
                if opt.is_none() {
                    let mut parser = Parser::new();
                    parser
                        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
                        .map_err(|e| anyhow::anyhow!("Failed to set TypeScript language: {e}"))?;
                    *opt = Some(parser);
                }
                opt.as_mut()
                    .unwrap()
                    .parse(source, None)
                    .ok_or_else(|| anyhow::anyhow!("tree-sitter parse returned None"))
            })
        }
    }
}

// ── Python parser ────────────────────────────────────────────────────────

#[cfg(feature = "python")]
mod python_parser {
    use std::cell::RefCell;
    use tree_sitter::{Parser, Tree};

    thread_local! {
        static PARSER: RefCell<Option<Parser>> = RefCell::new(None);
    }

    pub fn parse(source: &[u8]) -> Result<Tree, anyhow::Error> {
        PARSER.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                let mut parser = Parser::new();
                parser
                    .set_language(&tree_sitter_python::LANGUAGE.into())
                    .map_err(|e| anyhow::anyhow!("Failed to set Python language: {e}"))?;
                *opt = Some(parser);
            }
            opt.as_mut()
                .unwrap()
                .parse(source, None)
                .ok_or_else(|| anyhow::anyhow!("tree-sitter parse returned None"))
        })
    }
}

// ── JavaScript parser ────────────────────────────────────────────────────

#[cfg(feature = "javascript")]
mod javascript_parser {
    use std::cell::RefCell;
    use tree_sitter::{Parser, Tree};

    thread_local! {
        static PARSER: RefCell<Option<Parser>> = RefCell::new(None);
    }

    pub fn parse(source: &[u8]) -> Result<Tree, anyhow::Error> {
        PARSER.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                let mut parser = Parser::new();
                parser
                    .set_language(&tree_sitter_javascript::LANGUAGE.into())
                    .map_err(|e| anyhow::anyhow!("Failed to set JavaScript language: {e}"))?;
                *opt = Some(parser);
            }
            opt.as_mut()
                .unwrap()
                .parse(source, None)
                .ok_or_else(|| anyhow::anyhow!("tree-sitter parse returned None"))
        })
    }
}

// ── Public dispatch ──────────────────────────────────────────────────────

/// Parse source code for a given language.
/// Returns an error if the language feature is not enabled.
pub fn parse(language: Language, source: &[u8]) -> Result<Tree, anyhow::Error> {
    match language {
        #[cfg(feature = "rust")]
        Language::Rust => rust_parser::parse(source),
        #[cfg(not(feature = "rust"))]
        Language::Rust => anyhow::bail!("Rust parser not enabled. Build with --features rust"),

        Language::TypeScript => {
            #[cfg(feature = "typescript")]
            {
                // Use LANGUAGE_TYPESCRIPT for both .ts and .tsx.
                // Callers use separate parser for TSX dialect when needed.
                typescript_parser::parse(source, false)
            }
            #[cfg(not(feature = "typescript"))]
            anyhow::bail!("TypeScript parser not enabled. Build with --features typescript")
        },

        Language::Python => {
            #[cfg(feature = "python")]
            {
                python_parser::parse(source)
            }
            #[cfg(not(feature = "python"))]
            anyhow::bail!("Python parser not enabled. Build with --features python")
        },
        Language::JavaScript => {
            #[cfg(feature = "javascript")]
            {
                javascript_parser::parse(source)
            }
            #[cfg(not(feature = "javascript"))]
            anyhow::bail!("JavaScript parser not enabled. Build with --features javascript")
        },
    }
}

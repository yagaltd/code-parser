//! code-parser-core — tree-sitter engine.
//!
//! Parses source files into `FileParseIR` using tree-sitter grammars.
//! Provides per-file parsing, repository-wide parsing with cross-file
//! resolution, and optional file watching.

use std::path::Path;

use anyhow::Context;
use code_parser_ir::FileParseIR;

pub mod cache;
pub mod extractors;
pub mod file_collect;
pub mod hash;
pub mod language;
pub mod metrics;
pub mod parser;
pub mod resolve;
pub mod watcher;

#[cfg(feature = "javascript")]
use extractors::javascript::JavaScriptExtractor;
#[cfg(feature = "python")]
use extractors::python::PythonExtractor;
#[cfg(feature = "rust")]
use extractors::rust::RustExtractor;
#[cfg(feature = "typescript")]
use extractors::typescript::TypeScriptExtractor;
use extractors::LanguageExtractor;
use language::Language;

pub use cache::HashCache;

// ── Public API ───────────────────────────────────────────────────────────

/// Result of parsing a single file.
#[derive(Debug)]
pub struct ParseResult {
    pub ir: FileParseIR,
    pub errors: Vec<String>,
}

/// Parse a single file into `FileParseIR`.
///
/// Language is detected from the file extension. Returns an error
/// if the language is unsupported or parsing fails.
#[allow(unreachable_patterns)]
pub fn parse_file(path: &Path) -> Result<ParseResult, anyhow::Error> {
    let path_str = path.to_string_lossy().to_string();
    let source = std::fs::read(path).with_context(|| format!("Failed to read {path_str}"))?;
    let ir = parse_file_bytes(&path_str, &source)?;
    Ok(ParseResult {
        ir,
        errors: Vec::new(),
    })
}

/// Parse all source files in a directory, with cross-file resolution.
///
/// Collects source files via gitignore-aware walk, parses each,
/// then applies the cross-file resolution pass.
pub fn parse_repo(
    root: &Path,
    languages: Option<Vec<Language>>,
) -> Result<Vec<ParseResult>, anyhow::Error> {
    let full_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root_str = full_root.to_string_lossy().to_string();
    let languages = languages.unwrap_or_else(|| {
        vec![
            Language::Rust,
            Language::TypeScript,
            Language::JavaScript,
            Language::Python,
        ]
    });

    let paths = file_collect::collect_source_files(&root_str, &languages)
        .context("Failed to collect source files")?;

    let mut results: Vec<ParseResult> = Vec::new();

    for rel_path in &paths {
        let full_path = full_root.join(rel_path);
        match parse_file(&full_path) {
            Ok(mut result) => {
                // Normalize path in IR to be relative.
                result.ir.path = rel_path.clone();
                results.push(result);
            }
            Err(e) => {
                // Emit a diagnostic-only IR for unparseable files.
                let ir = FileParseIR::empty(rel_path.as_str(), "unknown");
                results.push(ParseResult {
                    ir,
                    errors: vec![format!("{rel_path}: {e}")],
                });
            }
        }
    }

    // Cross-file resolve pass.
    let mut irs: Vec<FileParseIR> = results.iter().map(|r| r.ir.clone()).collect();
    resolve::resolve_cross_file(&mut irs);
    for (i, ir) in irs.into_iter().enumerate() {
        // Rebuild cards after resolve so they reflect resolved calls.
        let (file_card, symbol_cards) = code_parser_ir::build_all_cards(&ir, &Default::default());
        let mut ir = ir;
        ir.retrieval_card = file_card;
        ir.symbol_cards = symbol_cards;
        results[i].ir = ir;
    }

    Ok(results)
}

/// Return just the IR for a given file path (used by domain_code's
/// `ingest-via-parser` feature bridge).
#[allow(unreachable_patterns)]
pub fn parse_file_bytes(path: &str, source: &[u8]) -> Result<FileParseIR, anyhow::Error> {
    let language =
        Language::from_path(path).with_context(|| format!("Unsupported file extension: {path}"))?;

    let tree =
        parser::parse(language, source).with_context(|| format!("Failed to parse {path}"))?;

    let mut ir = match language {
        #[cfg(feature = "rust")]
        Language::Rust => {
            let extractor = RustExtractor;
            extractor.extract(&tree, source, path)
        }
        #[cfg(not(feature = "rust"))]
        Language::Rust => anyhow::bail!("Rust extractor not enabled. Build with --features rust"),

        #[cfg(feature = "typescript")]
        Language::TypeScript => {
            let extractor = TypeScriptExtractor;
            extractor.extract(&tree, source, path)
        }
        #[cfg(not(feature = "typescript"))]
        Language::TypeScript => {
            anyhow::bail!("TypeScript extractor not enabled. Build with --features typescript")
        }

        Language::JavaScript => {
            #[cfg(feature = "javascript")]
            {
                let extractor = JavaScriptExtractor;
                extractor.extract(&tree, source, path)
            }
            #[cfg(not(feature = "javascript"))]
            anyhow::bail!("JavaScript extractor not enabled. Build with --features javascript")
        }

        #[cfg(feature = "python")]
        Language::Python => {
            let extractor = PythonExtractor;
            extractor.extract(&tree, source, path)
        }
        #[cfg(not(feature = "python"))]
        Language::Python => {
            anyhow::bail!("Python extractor not enabled. Build with --features python")
        }

        _ => anyhow::bail!("Extractor for {language:?} not yet implemented"),
    };

    // A0: per-file line metrics (code/comment/blank) from the tree.
    ir.metrics = metrics::compute(&tree, source, metrics::comment_kinds(&ir.language));

    // A1: always build retrieval cards after extract+resolve.
    ir.ir_version = 4;
    let (file_card, symbol_cards) = code_parser_ir::build_all_cards(&ir, &Default::default());
    ir.retrieval_card = file_card;
    ir.symbol_cards = symbol_cards;

    Ok(ir)
}

/// Parse a file, skipping if the blake3 hash matches the cache.
///
/// Returns `None` when the file hasn't changed (cache hit).
/// Returns `Some(ParseResult)` when the file is new or changed.
/// On parse error, emits a diagnostic-only IR (never returns `Err`).
#[allow(unreachable_patterns)]
pub fn parse_file_cached(
    path: &Path,
    cache: &mut HashCache,
) -> Result<Option<ParseResult>, anyhow::Error> {
    let path_str = path.to_string_lossy().to_string();

    // Read bytes for hash check.
    let source = std::fs::read(path).with_context(|| format!("Failed to read {path_str}"))?;
    let hash = hash::hash_bytes(&source);

    // Short-circuit: same hash → skip.
    if cache.is_unchanged(&path_str, &hash) {
        return Ok(None);
    }

    cache.update(&path_str, &hash);
    let ir = parse_file_bytes(&path_str, &source)?;
    Ok(Some(ParseResult {
        ir,
        errors: Vec::new(),
    }))
}

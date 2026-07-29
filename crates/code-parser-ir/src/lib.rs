//! code-parser IR types — zero heavy dependencies.
//!
//! These types represent the output of a tree-sitter parse run:
//! one `FileParseIR` per file, containing symbols, calls, imports, and diagnostics.
//! domain_code maps IR onto its node model (Phase 2).

use serde::{Deserialize, Serialize};

// ── FileParseIR ──────────────────────────────────────────────────────────

/// Top-level parse result for a single source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileParseIR {
    pub ir_version: u32,
    /// Repository-relative path, e.g. `src/main.rs`.
    pub path: String,
    /// Language identifier: `"Rust"`, `"TypeScript"`, `"Python"`, etc.
    pub language: String,
    /// blake3 lowercase hex digest of the file content.
    pub content_hash: String,
    /// Byte length of the source file (fixture/metadata only; not stored on domain payload).
    pub byte_len: u64,
    /// Line count (1-indexed).
    pub line_count: u32,
    pub symbols: Vec<SymbolIR>,
    pub calls: Vec<CallIR>,
    pub imports: Vec<ImportIR>,
    pub diagnostics: Vec<DiagnosticIR>,
}

// ── SymbolIR ─────────────────────────────────────────────────────────────

/// One top-level or member symbol extracted from a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolIR {
    /// Stable key within this file, e.g. `"Cache::get"` or `"main"`.
    pub local_key: String,
    /// Unqualified short name, e.g. `"get"`.
    pub name: String,
    /// Qualified name, e.g. `"Cache::get"`.
    pub qualified_name: String,
    pub kind: SymbolKind,

    // Location (1-indexed lines)
    pub start_line: u32,
    pub end_line: u32,
    /// Byte offsets within the file (0-indexed). Optional — extractor may omit.
    pub start_byte: Option<u32>,
    pub end_byte: Option<u32>,

    /// Human-readable signature, e.g. `"fn get(&self, key: &str) -> Option<&User>"`.
    pub signature: Option<String>,
    pub parameters: Vec<ParameterIR>,
    pub return_type: Option<String>,
    /// Doc comment text (leading `///` or `/** */` stripped).
    pub docstring: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParameterIR {
    pub name: String,
    pub type_annotation: Option<String>,
    /// Maps to domain `ParameterInfo.default_value` — often `None`.
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Interface,
    Enum,
    Trait,
    TypeAlias,
    Module,
    Namespace,
    Constant,
    Variable,
    Macro,
}

// ── CallIR ───────────────────────────────────────────────────────────────

/// A call-site extracted from a function/method body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallIR {
    /// References `SymbolIR.local_key` of the caller (function/method containing this call).
    pub caller_local_key: String,
    /// Best-effort text from the AST, e.g. `"self.data.get"`, `"Cache::new"`, `"c.get"`.
    pub callee_name: String,
    /// Set after in-file resolve pass when the callee is a symbol in the same file.
    pub callee_local_key: Option<String>,
    /// Set after cross-file resolve pass — the relative path of the file declaring the callee.
    pub callee_file: Option<String>,
    /// `true` when callee_name contains `::` AND was not found in the cross-file index.
    pub callee_external: bool,
    /// 1-indexed line of the call site.
    pub line: u32,
    /// 0-based column. Optional — extractor may omit.
    pub column: Option<u32>,
}

// ── ImportIR ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportIR {
    pub import_name: String,
    pub target_module: String,
    pub kind: ImportKind,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ImportKind {
    Named,
    Default,
    Star,
}

// ── DiagnosticIR ─────────────────────────────────────────────────────────

/// Non-fatal parse or resolution diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticIR {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub line: Option<u32>,
    pub byte_span: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}

impl DiagnosticSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

impl FileParseIR {
    /// Create a minimal IR for a file that failed to parse (diagnostics only).
    pub fn empty(path: impl Into<String>, language: impl Into<String>) -> Self {
        Self {
            ir_version: 1,
            path: path.into(),
            language: language.into(),
            content_hash: String::new(),
            byte_len: 0,
            line_count: 0,
            symbols: Vec::new(),
            calls: Vec::new(),
            imports: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

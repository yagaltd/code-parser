//! code-parser IR types — zero heavy dependencies.
//!
//! These types represent the output of a tree-sitter parse run:
//! one `FileParseIR` per file, containing symbols, calls, imports,
//! diagnostics, and deterministic retrieval cards.
//! domain_code maps IR onto its node model and decides how to use cards.

use serde::{Deserialize, Serialize};

// ── File metrics ─────────────────────────────────────────────────────────

/// Per-file line metrics (code/comment/blank) — tokei-style pass, computed
/// from the tree-sitter tree at parse time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetrics {
    pub code_lines: u32,
    pub comment_lines: u32,
    pub blank_lines: u32,
}

// ── Retrieval card ───────────────────────────────────────────────────────

/// Deterministic retrieval projection of a file or symbol.
/// Not a Cos node — domain_code decides whether to store/index/embed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalCard {
    /// Format version for embed/lex cache invalidation (e.g. 1).
    pub card_version: u32,
    /// Estimated token count (heuristic ~4 chars/token).
    pub est_tokens: u32,
    /// Bound ≤ max_chars; ready for Lex + optional later embed.
    pub text: String,
}

impl Default for RetrievalCard {
    fn default() -> Self {
        Self {
            card_version: 1,
            est_tokens: 0,
            text: String::new(),
        }
    }
}

/// Format knobs for card text generation.
/// Cards are always built — these only control text caps.
#[derive(Debug, Clone)]
pub struct CardFormat {
    pub max_file_chars: usize,     // default 2048
    pub max_symbol_chars: usize,   // default 512
    pub max_symbol_lines: usize,   // default 40
    pub max_imports: usize,        // default 24
    pub max_call_histogram: usize, // default 15
}

impl Default for CardFormat {
    fn default() -> Self {
        Self {
            max_file_chars: 2048,
            max_symbol_chars: 512,
            max_symbol_lines: 40,
            max_imports: 24,
            max_call_histogram: 15,
        }
    }
}

/// Noise call names excluded from call histograms.
/// Lang-tunable later; shared constant for now.
pub const CALL_NOISE_DENYLIST: &[&str] = &[
    "unwrap",
    "clone",
    "into",
    "to_string",
    "ok",
    "some",
    "none",
    "push",
    "map",
    "and_then",
    "or_else",
    "unwrap_or",
    "unwrap_or_else",
    "expect",
    "as_ref",
    "as_mut",
    "iter",
    "collect",
    "len",
    "is_empty",
    "new",
    "default",
    "from",
    "try_from",
    "try_into",
];

/// Symbol kind sort order for deterministic card output.
fn kind_order(kind: &SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Method => 1,
        SymbolKind::Struct => 2,
        SymbolKind::Enum => 3,
        SymbolKind::Trait => 4,
        SymbolKind::Class => 5,
        SymbolKind::Interface => 6,
        SymbolKind::Module => 7,
        SymbolKind::Namespace => 8,
        SymbolKind::TypeAlias => 9,
        SymbolKind::Constant => 10,
        SymbolKind::Variable => 11,
        SymbolKind::Macro => 12,
    }
}

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
    /// Per-file line metrics (code/comment/blank) — tokei-style pass.
    pub metrics: FileMetrics,
    pub symbols: Vec<SymbolIR>,
    pub calls: Vec<CallIR>,
    pub imports: Vec<ImportIR>,
    pub diagnostics: Vec<DiagnosticIR>,
    /// File-level retrieval card — always populated by `parse_file` / `parse_repo`.
    pub retrieval_card: RetrievalCard,
    /// Per-symbol cards — always same length as `symbols` (index-aligned).
    pub symbol_cards: Vec<RetrievalCard>,
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
    /// True when the symbol is test code: `#[test]`/`#[cfg(test)]` in Rust,
    /// `test_*` names / test files in Python, `.test.`/`.spec.` files in
    /// TS/JS. Emitted by the extractors (code-v3-improv idea 2).
    #[serde(default)]
    pub is_test: bool,
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

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "Function",
            Self::Method => "Method",
            Self::Class => "Class",
            Self::Struct => "Struct",
            Self::Interface => "Interface",
            Self::Enum => "Enum",
            Self::Trait => "Trait",
            Self::TypeAlias => "TypeAlias",
            Self::Module => "Module",
            Self::Namespace => "Namespace",
            Self::Constant => "Constant",
            Self::Variable => "Variable",
            Self::Macro => "Macro",
        }
    }
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
            ir_version: 4,
            path: path.into(),
            language: language.into(),
            content_hash: String::new(),
            byte_len: 0,
            line_count: 0,
            metrics: FileMetrics::default(),
            symbols: Vec::new(),
            calls: Vec::new(),
            imports: Vec::new(),
            diagnostics: Vec::new(),
            retrieval_card: RetrievalCard {
                card_version: 1,
                est_tokens: 0,
                text: String::new(),
            },
            symbol_cards: Vec::new(),
        }
    }
}

// ── Card builders (pure, no tree-sitter) ─────────────────────────────────

/// Sort a call histogram: count desc, then name asc.
fn sort_histogram(hist: &std::collections::HashMap<String, usize>) -> Vec<(&String, &usize)> {
    let mut v: Vec<_> = hist.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    v
}

/// Build a file-level retrieval card from IR fields.
pub fn build_file_card(ir: &FileParseIR, fmt: &CardFormat) -> RetrievalCard {
    let mut lines: Vec<String> = Vec::new();

    // FILE line — always present
    lines.push(format!(
        "FILE path={} lang={} lines={} hash={}",
        ir.path,
        ir.language,
        ir.line_count,
        &ir.content_hash[..8.min(ir.content_hash.len())]
    ));

    // IMPORTS line
    if !ir.imports.is_empty() {
        let mut import_mods: Vec<&str> = ir
            .imports
            .iter()
            .map(|i| i.target_module.as_str())
            .collect();
        import_mods.sort();
        import_mods.dedup();
        let total = import_mods.len();
        let shown = import_mods
            .iter()
            .take(fmt.max_imports)
            .copied()
            .collect::<Vec<_>>();
        let mut line = format!("IMPORTS {}", shown.join(", "));
        if total > fmt.max_imports {
            line.push_str(&format!(" (+{} more)", total - fmt.max_imports));
        }
        lines.push(line);
    }

    // SYMBOLS section
    if !ir.symbols.is_empty() {
        lines.push("SYMBOLS".to_string());
        // Sort by (kind_order, qualified_name)
        let mut sorted: Vec<&SymbolIR> = ir.symbols.iter().collect();
        sorted.sort_by(|a, b| {
            kind_order(&a.kind)
                .cmp(&kind_order(&b.kind))
                .then_with(|| a.qualified_name.cmp(&b.qualified_name))
        });

        for sym in sorted.iter().take(fmt.max_symbol_lines) {
            let mut line = format!(
                "  {} {} L{}-{}",
                sym.kind.as_str(),
                sym.qualified_name,
                sym.start_line,
                sym.end_line
            );
            if let Some(ref sig) = sym.signature {
                line.push_str(&format!(" sig={}", sig));
            }
            lines.push(line);
        }
        if sorted.len() > fmt.max_symbol_lines {
            lines.push(format!("  (+{} more)", sorted.len() - fmt.max_symbol_lines));
        }
    }

    // CALLS_OUT histogram
    if !ir.calls.is_empty() {
        let mut hist: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for call in &ir.calls {
            let name = call
                .callee_name
                .split("::")
                .last()
                .unwrap_or(&call.callee_name);
            if !CALL_NOISE_DENYLIST.contains(&name) {
                *hist.entry(name.to_string()).or_insert(0) += 1;
            }
        }
        if !hist.is_empty() {
            let sorted = sort_histogram(&hist);
            let parts: Vec<String> = sorted
                .iter()
                .take(fmt.max_call_histogram)
                .map(|(name, count)| format!("{}×{}", name, count))
                .collect();
            lines.push(format!("CALLS_OUT {}", parts.join(" ")));
            if sorted.len() > fmt.max_call_histogram {
                lines.push(format!(
                    "  (+{} more)",
                    sorted.len() - fmt.max_call_histogram
                ));
            }
        }
    }

    // PUB line — top-level public names (heuristic: not starting with lowercase)
    let pubs: Vec<&str> = ir
        .symbols
        .iter()
        .filter(|s| s.name.chars().next().is_some_and(|c| c.is_uppercase()))
        .map(|s| s.name.as_str())
        .collect();
    if !pubs.is_empty() {
        lines.push(format!("PUB {}", pubs.join(", ")));
    }

    let text = lines.join("\n");
    let est_tokens = (text.len() as u32).div_ceil(4);

    // Truncate from bottom if over max_chars
    let text = if text.len() > fmt.max_file_chars {
        truncate_lines(&lines, fmt.max_file_chars)
    } else {
        text
    };

    RetrievalCard {
        card_version: 1,
        est_tokens,
        text,
    }
}

/// Build a per-symbol retrieval card.
pub fn build_symbol_card(
    ir_path: &str,
    sym: &SymbolIR,
    calls: &[&CallIR], // calls where caller_local_key == sym.local_key
    fmt: &CardFormat,
) -> RetrievalCard {
    let mut lines: Vec<String> = Vec::new();

    lines.push(format!(
        "SYM path={} qname={} kind={} L{}-{}",
        ir_path,
        sym.qualified_name,
        sym.kind.as_str(),
        sym.start_line,
        sym.end_line
    ));

    if let Some(ref sig) = sym.signature {
        let sig_line = format!("SIG {}", sig);
        lines.push(sig_line);
    }

    if let Some(ref doc) = sym.docstring {
        let short: String = doc.chars().take(240).collect();
        if !short.is_empty() {
            lines.push(format!("DOC {}", short));
        }
    }

    if !calls.is_empty() {
        let mut names: Vec<&str> = calls
            .iter()
            .map(|c| c.callee_name.as_str())
            .filter(|n| {
                let short = n.split("::").last().unwrap_or(n);
                !CALL_NOISE_DENYLIST.contains(&short)
            })
            .collect();
        names.sort();
        names.dedup();
        if !names.is_empty() {
            let capped: Vec<&str> = names.iter().take(10).copied().collect();
            let mut line = format!("CALLS {}", capped.join(", "));
            if names.len() > 10 {
                line.push_str(&format!(" (+{})", names.len() - 10));
            }
            lines.push(line);
        }
    }

    let text = if lines.len() > fmt.max_symbol_lines {
        truncate_lines(&lines[..fmt.max_symbol_lines], fmt.max_symbol_chars)
    } else {
        let joined = lines.join("\n");
        if joined.len() > fmt.max_symbol_chars {
            truncate_lines(&lines, fmt.max_symbol_chars)
        } else {
            joined
        }
    };

    let est_tokens = (text.len() as u32).div_ceil(4);

    RetrievalCard {
        card_version: 1,
        est_tokens,
        text,
    }
}

/// Build all retrieval cards for an IR.
/// Returns (file_card, symbol_cards) where symbol_cards.len() == ir.symbols.len().
pub fn build_all_cards(ir: &FileParseIR, fmt: &CardFormat) -> (RetrievalCard, Vec<RetrievalCard>) {
    let file_card = build_file_card(ir, fmt);

    // Index calls by caller_local_key for efficient lookup
    let mut calls_by_caller: std::collections::HashMap<&str, Vec<&CallIR>> =
        std::collections::HashMap::new();
    for call in &ir.calls {
        calls_by_caller
            .entry(call.caller_local_key.as_str())
            .or_default()
            .push(call);
    }

    let symbol_cards: Vec<RetrievalCard> = ir
        .symbols
        .iter()
        .map(|sym| {
            let calls = calls_by_caller
                .get(sym.local_key.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            build_symbol_card(&ir.path, sym, calls, fmt)
        })
        .collect();

    (file_card, symbol_cards)
}

/// Truncate lines from the bottom, keeping the FILE/SYM header line.
fn truncate_lines(lines: &[String], max_chars: usize) -> String {
    let mut result = String::new();
    for line in lines {
        if result.len() + line.len() + 1 > max_chars {
            if !result.is_empty() {
                break;
            }
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(line);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_ir() -> FileParseIR {
        FileParseIR {
            ir_version: 3,
            path: "src/main.rs".into(),
            language: "Rust".into(),
            content_hash: "abc123def4567890".into(),
            byte_len: 500,
            line_count: 42,
            symbols: vec![
                SymbolIR {
                    local_key: "main".into(),
                    name: "main".into(),
                    qualified_name: "main".into(),
                    kind: SymbolKind::Function,
                    start_line: 10,
                    end_line: 20,
                    start_byte: None,
                    end_byte: None,
                    signature: Some("fn main()".into()),
                    parameters: vec![],
                    return_type: None,
                    docstring: Some("Entry point.".into()),
                },
                SymbolIR {
                    local_key: "Cache".into(),
                    name: "Cache".into(),
                    qualified_name: "Cache".into(),
                    kind: SymbolKind::Struct,
                    start_line: 1,
                    end_line: 8,
                    start_byte: None,
                    end_byte: None,
                    signature: None,
                    parameters: vec![],
                    return_type: None,
                    docstring: None,
                },
            ],
            calls: vec![
                CallIR {
                    caller_local_key: "main".into(),
                    callee_name: "Cache::new".into(),
                    callee_local_key: None,
                    callee_file: None,
                    callee_external: false,
                    line: 15,
                    column: None,
                },
                CallIR {
                    caller_local_key: "main".into(),
                    callee_name: "println".into(),
                    callee_local_key: None,
                    callee_file: None,
                    callee_external: true,
                    line: 18,
                    column: None,
                },
            ],
            imports: vec![ImportIR {
                import_name: "HashMap".into(),
                target_module: "std::collections".into(),
                kind: ImportKind::Named,
                line: Some(1),
                column: None,
            }],
            diagnostics: vec![],
            retrieval_card: RetrievalCard::default(),
            symbol_cards: vec![],
        }
    }

    #[test]
    fn file_card_starts_with_file_line() {
        let ir = make_test_ir();
        let card = build_file_card(&ir, &CardFormat::default());
        assert!(card.text.starts_with("FILE path=src/main.rs lang=Rust"));
        assert!(card.text.contains("hash=abc123de"));
    }

    #[test]
    fn file_card_includes_symbols_sorted_by_kind() {
        let ir = make_test_ir();
        let card = build_file_card(&ir, &CardFormat::default());
        // Function (kind_order=0) comes before Struct (kind_order=2)
        let func_pos = card.text.find("Function main").unwrap();
        let struct_pos = card.text.find("Struct Cache").unwrap();
        assert!(func_pos < struct_pos, "Function should sort before Struct");
    }

    #[test]
    fn file_card_includes_imports() {
        let ir = make_test_ir();
        let card = build_file_card(&ir, &CardFormat::default());
        assert!(card.text.contains("IMPORTS std::collections"));
    }

    #[test]
    fn file_card_noise_denylist_filters_calls() {
        let ir = make_test_ir(); // has Cache::new and println
        let card = build_file_card(&ir, &CardFormat::default());
        // "new" is in denylist, so Cache::new should not appear as CALLS_OUT
        assert!(
            !card.text.contains("new×"),
            "noise call 'new' should be filtered"
        );
        // println is not in denylist
        assert!(card.text.contains("println×"));
    }

    #[test]
    fn file_card_truncates_at_max_chars() {
        let ir = make_test_ir();
        let tiny = CardFormat {
            max_file_chars: 80,
            ..Default::default()
        };
        let card = build_file_card(&ir, &tiny);
        assert!(card.text.len() <= 80);
        assert!(card.text.starts_with("FILE"));
    }

    #[test]
    fn symbol_card_includes_sym_line_and_sig() {
        let ir = make_test_ir();
        let sym = &ir.symbols[0]; // main
        let calls: Vec<&CallIR> = ir
            .calls
            .iter()
            .filter(|c| c.caller_local_key == "main")
            .collect();
        let card = build_symbol_card("src/main.rs", sym, &calls, &CardFormat::default());
        assert!(card
            .text
            .contains("SYM path=src/main.rs qname=main kind=Function L10-20"));
        assert!(card.text.contains("SIG fn main()"));
        assert!(card.text.contains("DOC Entry point."));
    }

    #[test]
    fn symbol_card_includes_calls() {
        let ir = make_test_ir();
        let sym = &ir.symbols[0]; // main
        let calls: Vec<&CallIR> = ir
            .calls
            .iter()
            .filter(|c| c.caller_local_key == "main")
            .collect();
        let card = build_symbol_card("src/main.rs", sym, &calls, &CardFormat::default());
        // println should appear (not in denylist), Cache::new split to "new" → denylisted
        assert!(card.text.contains("println"));
    }

    #[test]
    fn build_all_cards_invariant() {
        let ir = make_test_ir();
        let (file_card, symbol_cards) = build_all_cards(&ir, &CardFormat::default());
        assert_eq!(symbol_cards.len(), ir.symbols.len());
        assert!(!file_card.text.is_empty());
        assert!(file_card.text.starts_with("FILE"));
    }

    #[test]
    fn empty_ir_produces_minimal_card() {
        let ir = FileParseIR::empty("empty.rs", "Rust");
        let card = build_file_card(&ir, &CardFormat::default());
        assert!(card.text.starts_with("FILE path=empty.rs lang=Rust"));
        assert!(!card.text.contains("SYMBOLS"));
    }
}

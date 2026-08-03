/// Test utilities for golden IR comparison.
///
/// Shared across all language extractor test modules.
/// Conditionally compiled (only in test builds).

use code_parser_ir::FileParseIR;

/// Normalize volatile fields before comparing golden IR.
///
/// Fields excluded from comparison:
/// - `content_hash` — zeroed (hash still tested in dedicated unit tests)
/// - `start_byte` / `end_byte` — tree-sitter offsets
/// - `column` — grammar-unstable
///
/// Retrieval cards **are** compared. After zeroing `content_hash`, cards are
/// rebuilt so the FILE line `hash=` token matches on both actual and golden
/// (extractors leave placeholder cards; fixtures may hold full cards).
///
/// Asserted exactly after normalize:
/// - `byte_len`, `line_count`, symbols/calls/imports structure
/// - `retrieval_card` / `symbol_cards` text (hash-stripped rebuild)
pub fn normalize_for_compare(ir: &mut FileParseIR) {
    ir.content_hash = String::new();
    for sym in &mut ir.symbols {
        sym.start_byte = None;
        sym.end_byte = None;
    }
    for call in &mut ir.calls {
        call.column = None;
    }
    for imp in &mut ir.imports {
        imp.column = None;
    }
    let (file_card, symbol_cards) =
        code_parser_ir::build_all_cards(ir, &code_parser_ir::CardFormat::default());
    ir.retrieval_card = file_card;
    ir.symbol_cards = symbol_cards;
}

/// Validate IR against the schema contract (structural checks only — no JSON Schema dep).
///
/// Prefer calling **after** [`normalize_for_compare`] so v2 card invariants hold
/// (extractors may leave empty placeholder cards until rebuild).
pub fn validate_schema(ir: &FileParseIR) -> Result<(), String> {
    if ir.ir_version != 1 && ir.ir_version != 2 && ir.ir_version != 3 {
        return Err(format!("ir_version must be 1, 2 or 3, got {}", ir.ir_version));
    }
    if ir.path.is_empty() {
        return Err("path is empty".into());
    }
    if ir.language.is_empty() {
        return Err("language is empty".into());
    }

    let keys: std::collections::HashSet<&str> =
        ir.symbols.iter().map(|s| s.local_key.as_str()).collect();

    for sym in &ir.symbols {
        if sym.local_key.is_empty() {
            return Err("symbol with empty local_key".into());
        }
        if sym.name.is_empty() {
            return Err(format!("symbol {} has empty name", sym.local_key));
        }
        if sym.qualified_name.is_empty() {
            return Err(format!("symbol {} has empty qualified_name", sym.local_key));
        }
        if sym.start_line > sym.end_line {
            return Err(format!(
                "symbol {} has inverted span: {} > {}",
                sym.local_key, sym.start_line, sym.end_line
            ));
        }
    }

    for call in &ir.calls {
        // Top-level calls (empty caller) allowed for module-level expressions.
        if call.caller_local_key.is_empty() {
            continue;
        }
        if !keys.contains(call.caller_local_key.as_str()) {
            return Err(format!(
                "call from unknown caller '{}' (callee: '{}')",
                call.caller_local_key, call.callee_name
            ));
        }
    }

    // After normalize_for_compare, cards are always rebuilt.
    if ir.symbol_cards.len() != ir.symbols.len() {
        return Err(format!(
            "symbol_cards.len() {} != symbols.len() {}",
            ir.symbol_cards.len(),
            ir.symbols.len()
        ));
    }
    if ir.retrieval_card.text.is_empty() || !ir.retrieval_card.text.starts_with("FILE ") {
        return Err("retrieval_card.text must be non-empty and start with 'FILE '".into());
    }

    Ok(())
}


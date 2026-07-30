/// Test utilities for golden IR comparison.
///
/// Shared across all language extractor test modules.
/// Conditionally compiled (only in test builds).

use code_parser_ir::FileParseIR;

/// Normalize volatile fields before comparing golden IR.
///
/// Fields excluded from comparison:
/// - `content_hash` — changes with any byte edit (hash asserted separately)
/// - `start_byte` / `end_byte` — tree-sitter byte offsets, unstable across grammar versions
/// - `column` — unstable across grammar versions
/// - `retrieval_card.text`, `symbol_cards[*].text` — built in `parse_file_bytes`, not by extractors
///
/// Fields asserted exactly:
/// - `byte_len`, `line_count` — structural, must match golden
/// - All symbol names, kinds, lines, params, signatures
/// - All call callee names, caller keys, lines, external flags
/// - All import names, modules, kinds, lines
pub fn normalize_for_compare(ir: &mut FileParseIR) {
    ir.content_hash = String::new();
    ir.retrieval_card.text = String::new();
    ir.retrieval_card.est_tokens = 0;
    ir.symbol_cards.clear();
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
}

/// Validate IR against the schema contract (structural checks only — no JSON Schema dep).
///
/// Verifies:
/// - `ir_version == 1`
/// - `path` is non-empty
/// - `language` is non-empty
/// - All symbols have non-empty `local_key`, `name`, `qualified_name`
/// - `start_line <= end_line` for every symbol
/// - `caller_local_key` of every call references a known symbol `local_key`
pub fn validate_schema(ir: &FileParseIR) -> Result<(), String> {
    if ir.ir_version != 1 && ir.ir_version != 2 {
        return Err(format!("ir_version must be 1 or 2, got {}", ir.ir_version));
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
        // Top-level calls (empty caller) are allowed — emitted by some extractors
        // for module-level expressions. Not counted as a schema violation.
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

    Ok(())
}

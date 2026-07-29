/// Resolution passes — in-file (per-file) and cross-file (across a repo).
///
/// The in-file pass is applied automatically by each extractor.
/// The cross-file pass is applied after parsing a whole repo.

use std::collections::HashMap;

use code_parser_ir::FileParseIR;

/// Cross-file call resolution using exact qualified_name lookup.
///
/// Operates on a slice of already-parsed IRs. After this pass:
/// - `CallIR.callee_file` is set when the callee's qualified_name is found in another file.
/// - `CallIR.callee_external` is set when callee_name contains `::` but was not found.
///
/// V1 limitations: no method binding, no trait resolution, no re-exports.
pub fn resolve_cross_file(irs: &mut [FileParseIR]) {
    // Build index: qualified_name → file path.
    let mut qualified_to_file: HashMap<String, String> = HashMap::new();
    for ir in irs.iter() {
        for sym in &ir.symbols {
            qualified_to_file
                .entry(sym.qualified_name.clone())
                .or_insert_with(|| ir.path.clone());
        }
    }

    // Apply to each IR's calls.
    for ir in irs.iter_mut() {
        for call in &mut ir.calls {
            // Already resolved in-file.
            if call.callee_local_key.is_some() {
                continue;
            }
            // Already cross-file resolved.
            if call.callee_file.is_some() {
                continue;
            }
            // Only try to resolve names containing '::'.
            if call.callee_name.contains("::") {
                if let Some(path) = qualified_to_file.get(&call.callee_name) {
                    // Don't self-resolve to the same file.
                    if *path != ir.path {
                        call.callee_file = Some(path.clone());
                        call.callee_external = false;
                    } else {
                        // Same file but not resolved in-file — leave unresolved.
                    }
                } else {
                    // Qualified path not found anywhere → external.
                    call.callee_external = true;
                }
            }
            // Bare names / method chains stay unresolved (not external).
        }
    }
}

/// Rust language extractor — walks a tree-sitter Rust AST and collects
/// symbols, calls, imports, and diagnostics.
///
/// Single-pass recursive walk. Tracks current function context for call attribution.
use code_parser_ir::{
    CallIR, DiagnosticIR, FileParseIR, ImportIR, ImportKind, ParameterIR, SymbolIR, SymbolKind,
};
use tree_sitter::Node;

use crate::hash;
use crate::language::Language;

use super::utils;
use super::LanguageExtractor;

pub struct RustExtractor;

impl LanguageExtractor for RustExtractor {
    fn extract(&self, tree: &tree_sitter::Tree, source: &[u8], file_path: &str) -> FileParseIR {
        let content_hash = hash::hash_bytes(source);
        let line_count = source.iter().filter(|&&b| b == b'\n').count() as u32 + 1;
        let byte_len = source.len() as u64;

        let mut ctx = ExtractCtx::new(source);
        ctx.file_path = file_path;
        ctx.visit_node(&tree.root_node());

        // In-file resolve: map same-file calls to local_keys.
        resolve_in_file(&mut ctx);

        // Diagnostics: top-level ERROR regions + missing markers (bounded).
        ctx.diagnostics = utils::collect_error_diagnostics(tree, source);

        FileParseIR {
            ir_version: 3,
            path: file_path.to_string(),
            language: Language::Rust.as_str().to_string(),
            content_hash,
            byte_len,
            line_count,
            metrics: code_parser_ir::FileMetrics::default(),
            symbols: ctx.symbols,
            calls: ctx.calls,
            imports: ctx.imports,
            retrieval_card: Default::default(),
            symbol_cards: Vec::new(),
            diagnostics: ctx.diagnostics,
        }
    }
}

// ── Extraction context ───────────────────────────────────────────────────

struct ExtractCtx<'a> {
    source: &'a [u8],
    symbols: Vec<SymbolIR>,
    calls: Vec<CallIR>,
    imports: Vec<ImportIR>,
    diagnostics: Vec<DiagnosticIR>,
    /// When inside a function/method body, this holds the caller's local_key.
    current_caller: Option<String>,
    /// Current impl type name stack (e.g. "Cache") — used for Method qualified names.
    impl_stack: Vec<String>,
    /// Test context stack: pushed when visiting `mod tests` or a
    /// `#[cfg(test)]` module, so every symbol inside is flagged is_test.
    test_ctx: Vec<bool>,
    /// Source file path (for file-level test detection).
    file_path: &'a str,
}

impl<'a> ExtractCtx<'a> {
    fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            symbols: Vec::new(),
            calls: Vec::new(),
            imports: Vec::new(),
            diagnostics: Vec::new(),
            current_caller: None,
            impl_stack: Vec::new(),
            test_ctx: Vec::new(),
            file_path: "",
        }
    }

    /// True when the current node is inside test code (mod tests /
    /// #[cfg(test)] ancestor, #[test]-style attribute, or a tests/ file).
    fn is_test(&self, node: &Node) -> bool {
        if self.test_ctx.last() == Some(&true) {
            return true;
        }
        if has_test_attribute(node, self.source) {
            return true;
        }
        is_test_file_path(self.file_path)
    }

    fn text(&self, node: &Node) -> &str {
        utils::node_text(node, self.source)
    }

    // ── recursive walk ───────────────────────────────────────────────

    /// Walk a node and its children, dispatching by kind.
    fn visit_node(&mut self, node: &Node<'a>) {
        match node.kind() {
            "source_file"
            | "block"
            | "declaration_list"
            | "field_declaration_list"
            | "enum_variant_list"
            | "use_list"
            | "scoped_use_list"
            | "parameters"
            | "formal_parameters"
            | "attribute_item"
            | "inner_attribute_item"
            | "line_comment"
            | "block_comment" => {
                // Container nodes: recurse into children.
                self.visit_children(node);
            }

            "function_item" => {
                let impl_type = self.impl_stack.last().cloned();
                self.visit_function(node, impl_type.as_deref());
            }
            "mod_item" => {
                let is_test_mod = field_text(node, "name", self.source)
                    .map(|n| n == "tests")
                    .unwrap_or(false)
                    || has_test_attribute(node, self.source);
                if is_test_mod {
                    self.test_ctx.push(true);
                }
                self.visit_children(node);
                if is_test_mod {
                    self.test_ctx.pop();
                }
            }
            "struct_item" => self.visit_struct(node),
            "enum_item" => self.visit_enum(node),
            "trait_item" => self.visit_trait(node),
            "impl_item" => self.visit_impl(node),
            "use_declaration" => self.visit_use(node),
            "call_expression" => self.visit_call(node, false),
            "method_call_expression" => self.visit_call(node, true),
            "macro_invocation" => {
                // V1: macros off by default.
                // Could emit SymbolIR { kind: Macro } here if enabled.
            }

            // Skip: lifetime annotations, type args, etc.
            _ => {
                self.visit_children(node);
            }
        }
    }

    fn visit_children(&mut self, node: &Node<'a>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit_node(&child);
        }
    }

    // ── symbol visitors ──────────────────────────────────────────────

    fn visit_function(&mut self, node: &Node<'a>, impl_type: Option<&str>) {
        let name_node = node.child_by_field_name("name");
        let name = name_node
            .as_ref()
            .map(|n| self.text(n).to_string())
            .unwrap_or_default();

        let params = extract_parameters(node, self.source);
        let return_type = extract_return_type(node, self.source);
        let sig = build_signature(node, self.source);
        let doc = extract_docstring(node, self.source);

        let (qualified_name, local_key, kind) = if let Some(impl_ty) = impl_type {
            (
                format!("{impl_ty}::{name}"),
                format!("{impl_ty}::{name}"),
                SymbolKind::Method,
            )
        } else {
            (name.clone(), name.clone(), SymbolKind::Function)
        };

        self.symbols.push(SymbolIR {
            local_key: local_key.clone(),
            name,
            qualified_name,
            kind,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: sig,
            parameters: params,
            return_type,
            is_test: self.is_test(node),
            docstring: doc,
        });

        // Visit body with caller context.
        let prev = self.current_caller.replace(local_key);
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        }
        self.current_caller = prev;
    }

    fn visit_struct(&mut self, node: &Node<'a>) {
        let name = field_text(node, "name", self.source).unwrap_or_default();
        let doc = extract_docstring(node, self.source);

        self.symbols.push(SymbolIR {
            local_key: name.clone(),
            name: name.clone(),
            qualified_name: name.clone(),
            kind: SymbolKind::Struct,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: None,
            parameters: Vec::new(),
            return_type: None,
            is_test: self.is_test(node),
            docstring: doc,
        });

        // Visit fields (for nested calls, if any).
        self.visit_children(node);
    }

    fn visit_enum(&mut self, node: &Node<'a>) {
        let name = field_text(node, "name", self.source).unwrap_or_default();
        let doc = extract_docstring(node, self.source);

        self.symbols.push(SymbolIR {
            local_key: name.clone(),
            name: name.clone(),
            qualified_name: name.clone(),
            kind: SymbolKind::Enum,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: None,
            parameters: Vec::new(),
            return_type: None,
            is_test: self.is_test(node),
            docstring: doc,
        });
        self.visit_children(node);
    }

    fn visit_trait(&mut self, node: &Node<'a>) {
        let name = field_text(node, "name", self.source).unwrap_or_default();
        let doc = extract_docstring(node, self.source);

        self.symbols.push(SymbolIR {
            local_key: name.clone(),
            name: name.clone(),
            qualified_name: name.clone(),
            kind: SymbolKind::Trait,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: None,
            parameters: Vec::new(),
            return_type: None,
            is_test: self.is_test(node),
            docstring: doc,
        });
        self.visit_children(node);
    }

    fn visit_impl(&mut self, node: &Node<'a>) {
        // Extract the impl type name.
        let impl_type = node
            .child_by_field_name("type")
            .or_else(|| utils::child_by_kind(node, "type_identifier"))
            .as_ref()
            .map(|n| self.text(n).to_string());

        // Push impl context so child functions get the Method kind.
        if let Some(ref ty) = impl_type {
            self.impl_stack.push(ty.clone());
        }

        // Recurse into body normally. visit_function picks up impl_stack.
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        } else {
            self.visit_children(node);
        }

        if impl_type.is_some() {
            self.impl_stack.pop();
        }
    }

    // ── call visitor ─────────────────────────────────────────────────

    fn visit_call(&mut self, node: &Node<'a>, _is_method: bool) {
        let line = utils::point_line(node);
        let callee_name = extract_call_callee(node, self.source);

        if let Some(ref caller) = self.current_caller {
            self.calls.push(CallIR {
                caller_local_key: caller.clone(),
                callee_name,
                callee_local_key: None,
                callee_file: None,
                callee_external: false,
                line,
                column: Some(utils::point_column(node)),
            });
        }
    }

    // ── import visitor ───────────────────────────────────────────────

    fn visit_use(&mut self, node: &Node<'a>) {
        let line = Some(utils::point_line(node));
        extract_use_decl(node, self.source, line, &mut self.imports);
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Get text of a named field child.
fn field_text<'a>(node: &Node<'a>, field: &str, source: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| utils::node_text(&n, source).to_string())
}

/// Extract callee expression text from a call_expression or method_call_expression.
fn extract_call_callee(node: &Node, source: &[u8]) -> String {
    let func = node.child_by_field_name("function");
    match func {
        Some(f) => {
            let mut parts: Vec<String> = Vec::new();
            collect_call_name_parts(&f, source, &mut parts);
            if parts.is_empty() {
                utils::node_text(&f, source).to_string()
            } else {
                parts.join(".")
            }
        }
        None => utils::node_text(node, source).to_string(),
    }
}

/// Recursively collect dotted name parts (e.g. self.data.get → ["self","data","get"]).
fn collect_call_name_parts(node: &Node, source: &[u8], parts: &mut Vec<String>) {
    match node.kind() {
        "field_expression" => {
            // Recurse on the value/object, then push the field name.
            if let Some(value) = node.child_by_field_name("value") {
                collect_call_name_parts(&value, source, parts);
            }
            if let Some(field) = node.child_by_field_name("field") {
                parts.push(utils::node_text(&field, source).to_string());
            }
        }
        "scoped_identifier" => {
            // e.g. Cache::new — push the full qualified name.
            parts.push(utils::node_text(node, source).to_string());
        }
        "identifier" | "self" | "super" | "crate" => {
            parts.push(utils::node_text(node, source).to_string());
        }
        _ => {
            // Fallback: use source text.
            let text = utils::node_text(node, source);
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
    }
}

/// Build human-readable signature from a function_item node.
fn build_signature(node: &Node, source: &[u8]) -> Option<String> {
    // Use source text from the start of the function_item up to the body.
    let body = node.child_by_field_name("body")?;
    let start = node.start_byte() as usize;
    let body_start = body.start_byte() as usize;
    let sig_bytes = &source[start..body_start];
    let sig = String::from_utf8_lossy(sig_bytes).into_owned();
    // Trim trailing whitespace and any trailing '{' that may have been included.
    let sig = sig.trim();
    // Remove trailing newlines / spaces.
    Some(sig.lines().map(|l| l.trim()).collect::<Vec<_>>().join(" "))
}

/// Extract parameters from a function_item.
fn extract_parameters(node: &Node, source: &[u8]) -> Vec<ParameterIR> {
    let params_node = node.child_by_field_name("parameters");
    let params_node = match params_node {
        Some(p) => p,
        None => return Vec::new(),
    };

    let mut params = Vec::new();
    let mut cursor = params_node.walk();
    for child in params_node.named_children(&mut cursor) {
        if child.kind() == "parameter" {
            let name = child
                .child_by_field_name("pattern")
                .map(|n| utils::node_text(&n, source).to_string())
                .unwrap_or_default();
            let type_annotation = child
                .child_by_field_name("type")
                .map(|n| utils::node_text(&n, source).to_string());
            let default_value = child
                .child_by_field_name("default_value")
                .map(|n| utils::node_text(&n, source).to_string());

            params.push(ParameterIR {
                name,
                type_annotation,
                default_value,
            });
        } else if child.kind() == "self_parameter" {
            params.push(ParameterIR {
                name: "self".to_string(),
                type_annotation: None,
                default_value: None,
            });
        }
    }
    params
}

/// Extract return type annotation text.
fn extract_return_type(node: &Node, source: &[u8]) -> Option<String> {
    node.child_by_field_name("return_type")
        .map(|n| utils::node_text(&n, source).to_string())
}

/// Extract docstring from preceding comment nodes.
fn extract_docstring(node: &Node, source: &[u8]) -> Option<String> {
    // tree-sitter: doc comments are attribute_items or line_comment/block_comment siblings.
    // For now, look at previous sibling nodes.
    let mut doc_lines: Vec<String> = Vec::new();
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "line_comment" => {
                let text = utils::node_text(&p, source);
                if let Some(doc) = text.strip_prefix("///") {
                    doc_lines.push(doc.trim().to_string());
                } else if let Some(doc) = text.strip_prefix("//!") {
                    doc_lines.push(doc.trim().to_string());
                } else {
                    break; // Not a doc comment — stop.
                }
            }
            "block_comment" => {
                let text = utils::node_text(&p, source);
                if text.starts_with("/**") {
                    let inner = text
                        .strip_prefix("/**")
                        .and_then(|t| t.strip_suffix("*/"))
                        .unwrap_or(text);
                    for line in inner.lines() {
                        let trimmed = line.trim().strip_prefix('*').unwrap_or(line).trim();
                        doc_lines.push(trimmed.to_string());
                    }
                } else {
                    break;
                }
            }
            "attribute_item" => {
                let text = utils::node_text(&p, source);
                if text.starts_with("#[doc") {
                    // Extract doc string from attribute: #[doc = "..."] or #[doc = r#"..."]
                    if let Some(start) = text.find('"') {
                        let inner = &text[start + 1..];
                        if let Some(end) = inner.rfind('"') {
                            doc_lines.push(inner[..end].to_string());
                        }
                    }
                } else {
                    break; // Non-doc attribute.
                }
            }
            _ => break, // Not a comment / attribute — stop.
        }
        prev = p.prev_sibling();
    }
    doc_lines.reverse();
    if doc_lines.is_empty() {
        None
    } else {
        Some(doc_lines.join("\n"))
    }
}

/// Extract import entries from a use_declaration.
fn extract_use_decl(node: &Node, source: &[u8], line: Option<u32>, imports: &mut Vec<ImportIR>) {
    // The use_declaration has the form: use path::to::Item;
    // We find the argument (the path) and extract the last segment as the import name.
    let arg = node.child_by_field_name("argument");
    let arg = match arg {
        Some(a) => a,
        None => {
            // Fallback: use the whole node text minus "use " prefix.
            let full = utils::node_text(node, source);
            if let Some(rest) = full.strip_prefix("use ") {
                let rest = rest.trim_end_matches(';');
                imports.push(ImportIR {
                    import_name: rest.to_string(),
                    target_module: String::new(),
                    kind: ImportKind::Named,
                    line,
                    column: Some(utils::point_column(node)),
                });
            }
            return;
        }
    };

    match arg.kind() {
        "identifier" => {
            // Single item: use Foo;
            let name = utils::node_text(&arg, source).to_string();
            imports.push(ImportIR {
                import_name: name,
                target_module: String::new(),
                kind: ImportKind::Named,
                line,
                column: Some(utils::point_column(node)),
            });
        }
        "scoped_identifier" => {
            // use std::collections::HashMap;
            let text = utils::node_text(&arg, source);
            let (module, name) = split_scoped(text);
            imports.push(ImportIR {
                import_name: name,
                target_module: module,
                kind: ImportKind::Named,
                line,
                column: Some(utils::point_column(node)),
            });
        }
        "scoped_use_list" => {
            // use std::collections::{HashMap, HashSet};
            let path_node = arg.child_by_field_name("path");
            let prefix = path_node
                .as_ref()
                .map(|p| utils::node_text(p, source).to_string())
                .unwrap_or_default();

            let list_node = arg.child_by_field_name("list");
            if let Some(list) = list_node {
                let mut cursor = list.walk();
                for child in list.named_children(&mut cursor) {
                    match child.kind() {
                        "identifier" => {
                            let name = utils::node_text(&child, source).to_string();
                            imports.push(ImportIR {
                                import_name: name.clone(),
                                target_module: if prefix.is_empty() {
                                    String::new()
                                } else {
                                    format!("{prefix}::{name}")
                                },
                                kind: ImportKind::Named,
                                line,
                                column: Some(utils::point_column(node)),
                            });
                        }
                        "scoped_identifier" => {
                            let text = utils::node_text(&child, source);
                            let (sub_module, name) = split_scoped(text);
                            let full_module = if prefix.is_empty() {
                                sub_module
                            } else {
                                format!("{prefix}::{sub_module}")
                            };
                            imports.push(ImportIR {
                                import_name: name,
                                target_module: full_module,
                                kind: ImportKind::Named,
                                line,
                                column: Some(utils::point_column(node)),
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        "use_list" => {
            // use {Foo, Bar}; (legacy)
            let mut cursor = arg.walk();
            for child in arg.named_children(&mut cursor) {
                if child.kind() == "identifier" {
                    let name = utils::node_text(&child, source).to_string();
                    imports.push(ImportIR {
                        import_name: name,
                        target_module: String::new(),
                        kind: ImportKind::Named,
                        line,
                        column: Some(utils::point_column(node)),
                    });
                }
            }
        }
        _ => {
            // Fallback.
            let full = utils::node_text(&arg, source);
            imports.push(ImportIR {
                import_name: full.to_string(),
                target_module: String::new(),
                kind: ImportKind::Named,
                line,
                column: Some(utils::point_column(node)),
            });
        }
    }
}

/// Split "std::collections::HashMap" into ("std::collections", "HashMap").
fn split_scoped(text: &str) -> (String, String) {
    if let Some(pos) = text.rfind("::") {
        let module = &text[..pos];
        let name = &text[pos + 2..];
        (module.to_string(), name.to_string())
    } else {
        (String::new(), text.to_string())
    }
}

// ── In-file resolution ───────────────────────────────────────────────────

fn resolve_in_file(ctx: &mut ExtractCtx) {
    // Build lookup: name → local_key and qualified_name → local_key.
    let mut by_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut by_qualified: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for sym in &ctx.symbols {
        by_name
            .entry(sym.name.clone())
            .or_insert_with(|| sym.local_key.clone());
        by_qualified
            .entry(sym.qualified_name.clone())
            .or_insert_with(|| sym.local_key.clone());
    }

    for call in &mut ctx.calls {
        if call.callee_local_key.is_some() {
            continue;
        }
        // 1. Exact bare name match.
        if let Some(key) = by_name.get(&call.callee_name) {
            call.callee_local_key = Some(key.clone());
            call.callee_external = false;
            continue;
        }
        // 2. Exact qualified_name match.
        if let Some(key) = by_qualified.get(&call.callee_name) {
            call.callee_local_key = Some(key.clone());
            call.callee_external = false;
            continue;
        }
        // 3. Contains '::' but not found — external.
        if call.callee_name.contains("::") {
            call.callee_external = true;
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

/// True when the node carries a test-related attribute: `#[test]`,
/// `#[tokio::test]` (any attribute path ending in `test`), or `#[cfg(test)]`.
///
/// tree-sitter-rust places `#[test]` as a *preceding sibling* of the item it
/// annotates (child of the enclosing list), so both the node's own inner
/// attributes and the up-to-two attribute items immediately before it are
/// checked.
fn has_test_attribute(node: &Node, source: &[u8]) -> bool {
    if let Some(parent) = node.parent() {
        let mut cursor = parent.walk();
        let mut prev_attrs: Vec<Node> = Vec::new();
        for child in parent.named_children(&mut cursor) {
            if child.id() == node.id() {
                break;
            }
            if child.kind() == "attribute_item" || child.kind() == "inner_attribute_item" {
                prev_attrs.push(child);
            }
        }
        for a in prev_attrs.iter().rev().take(2) {
            if utils::node_text(a, source).contains("test") {
                return true;
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "attribute_item" || child.kind() == "inner_attribute_item" {
            if utils::node_text(&child, source).contains("test") {
                return true;
            }
        }
    }
    false
}

/// File-level test detection: `tests/` directory or `tests.rs` module file.
fn is_test_file_path(path: &str) -> bool {
    path.contains("/tests/")
        || path.starts_with("tests/")
        || path == "tests.rs"
        || path.ends_with("/tests.rs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_parser_ir::DiagnosticSeverity;
    use code_parser_ir::SymbolKind as SK;

    fn parse_rust(source: &[u8], path: &str) -> FileParseIR {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let ir = RustExtractor.extract(&tree, source, path);
        let mut ir = ir;
        ir.metrics =
            crate::metrics::compute(&tree, source, crate::metrics::comment_kinds(&ir.language));
        ir.ir_version = 3;
        ir
    }

    // ── Golden fixture: simple.rs ────────────────────────────────────

    #[test]
    fn simple_fixture_symbols() {
        let source = include_str!("../../../../fixtures/rust/simple.rs");
        let ir = parse_rust(source.as_bytes(), "fixtures/rust/simple.rs");

        assert_eq!(ir.language, "Rust");

        let sym_keys: Vec<&str> = ir.symbols.iter().map(|s| s.local_key.as_str()).collect();
        assert!(
            sym_keys.contains(&"Cache"),
            "missing struct Cache: {sym_keys:?}"
        );
        assert!(
            sym_keys.contains(&"Cache::get"),
            "missing method Cache::get"
        );
        assert!(sym_keys.contains(&"main"), "missing fn main");

        let cache = ir.symbols.iter().find(|s| s.local_key == "Cache").unwrap();
        assert_eq!(cache.kind, SK::Struct);
        assert_eq!(cache.start_line, 3);

        let get = ir
            .symbols
            .iter()
            .find(|s| s.local_key == "Cache::get")
            .unwrap();
        assert_eq!(get.kind, SK::Method);
        assert!(get.signature.as_ref().map_or(false, |s| s.contains("get")));
        assert_eq!(get.parameters.len(), 2);
        assert_eq!(get.parameters[0].name, "self");
        assert_eq!(get.parameters[1].name, "key");

        let main_sym = ir.symbols.iter().find(|s| s.local_key == "main").unwrap();
        assert_eq!(main_sym.kind, SK::Function);
    }

    #[test]
    fn simple_fixture_calls() {
        let source = include_str!("../../../../fixtures/rust/simple.rs");
        let ir = parse_rust(source.as_bytes(), "fixtures/rust/simple.rs");

        let main_calls: Vec<&CallIR> = ir
            .calls
            .iter()
            .filter(|c| c.caller_local_key == "main")
            .collect();
        assert!(main_calls.len() >= 2, "expected >=2 calls from main");

        // Cache::new → external (has ::, not in-file)
        let new_call = main_calls
            .iter()
            .find(|c| c.callee_name == "Cache::new")
            .unwrap();
        assert!(new_call.callee_external, "Cache::new should be external");
        assert_eq!(new_call.line, 14);

        // c.get → unresolved, not external (no ::)
        let cget = main_calls
            .iter()
            .find(|c| c.callee_name == "c.get")
            .unwrap();
        assert!(!cget.callee_external, "c.get should NOT be external");
        assert!(cget.callee_local_key.is_none());

        // self.data.get from Cache::get → unresolved, not external
        let get_calls: Vec<&CallIR> = ir
            .calls
            .iter()
            .filter(|c| c.caller_local_key == "Cache::get")
            .collect();
        assert!(get_calls.len() >= 1, "expected >=1 call from Cache::get");
        let sdata = get_calls
            .iter()
            .find(|c| c.callee_name == "self.data.get")
            .unwrap();
        assert!(!sdata.callee_external);
        assert!(sdata.callee_local_key.is_none());
    }

    #[test]
    fn simple_fixture_imports() {
        let source = include_str!("../../../../fixtures/rust/simple.rs");
        let ir = parse_rust(source.as_bytes(), "fixtures/rust/simple.rs");

        let hashmap = ir
            .imports
            .iter()
            .find(|i| i.import_name == "HashMap")
            .expect("HashMap import");
        assert_eq!(hashmap.target_module, "std::collections");
        assert_eq!(hashmap.kind, ImportKind::Named);
        assert_eq!(hashmap.line, Some(1));
    }

    // ── Domain fixture: mod_a.rs (foo→bar resolved) ─────────────────

    const MOD_A: &str = r#"use std::collections::HashMap;

fn bar() -> usize {
    1
}

fn foo() -> usize {
    let _type_anchor: Option<HashMap<(), ()>> = None;
    bar()
}
"#;

    #[test]
    fn mod_a_symbols() {
        let ir = parse_rust(MOD_A.as_bytes(), "rust/mod_a.rs");

        assert_eq!(ir.symbols.len(), 2, "expected bar + foo");

        let bar = ir.symbols.iter().find(|s| s.name == "bar").expect("bar");
        assert_eq!(bar.kind, SK::Function);
        assert_eq!(bar.qualified_name, "bar");
        assert_eq!(bar.start_line, 3);
        assert_eq!(bar.end_line, 5);

        let foo = ir.symbols.iter().find(|s| s.name == "foo").expect("foo");
        assert_eq!(foo.kind, SK::Function);
        assert_eq!(foo.qualified_name, "foo");
        assert_eq!(foo.start_line, 7);
        assert_eq!(foo.end_line, 10);
    }

    #[test]
    fn mod_a_foo_calls_bar_in_file_resolved() {
        let ir = parse_rust(MOD_A.as_bytes(), "rust/mod_a.rs");

        // The call from foo → bar should be in-file resolved.
        let call = ir
            .calls
            .iter()
            .find(|c| c.callee_name == "bar" && c.caller_local_key == "foo")
            .expect("foo → bar call");

        assert_eq!(call.callee_local_key.as_deref(), Some("bar"));
        assert_eq!(call.line, 9);
        assert!(!call.callee_external, "bar is in-file, not external");
    }

    #[test]
    fn mod_a_import_hashmap() {
        let ir = parse_rust(MOD_A.as_bytes(), "rust/mod_a.rs");

        let imp = ir
            .imports
            .iter()
            .find(|i| i.import_name == "HashMap")
            .expect("HashMap import");
        assert_eq!(imp.target_module, "std::collections");
        assert_eq!(imp.kind, ImportKind::Named);
        assert_eq!(imp.line, Some(1));
    }

    #[test]
    fn mod_a_content_hash_matches_domain_expected() {
        let bytes = MOD_A.as_bytes();
        let ir = parse_rust(bytes, "rust/mod_a.rs");

        // Expected hash from domain's expected.json.
        assert_eq!(
            ir.content_hash,
            "961aea6de2f2dcb6a32a3d898608d187d26be01d6989cfff8fcaf1b1423f674b"
        );
        assert_eq!(ir.byte_len, 147);
    }

    // ── Domain fixture: mod_b.rs ─────────────────────────────────────

    const MOD_B: &str = "fn unrelated() -> &'static str {\n    \"isolation\"\n}\n";

    #[test]
    fn mod_b_symbols() {
        let ir = parse_rust(MOD_B.as_bytes(), "rust/mod_b.rs");

        assert_eq!(ir.symbols.len(), 1);
        assert_eq!(ir.symbols[0].name, "unrelated");
        assert_eq!(ir.symbols[0].kind, SK::Function);
        assert_eq!(ir.symbols[0].start_line, 1);
        assert_eq!(ir.symbols[0].end_line, 3);
        assert!(ir.calls.is_empty());
        assert!(ir.imports.is_empty());
    }

    #[test]
    fn mod_b_content_hash_matches_domain_expected() {
        let bytes = MOD_B.as_bytes();
        let ir = parse_rust(bytes, "rust/mod_b.rs");

        assert_eq!(
            ir.content_hash,
            "33cebd63a6b40c2692782a2bea54aac997aad9de121ccd2ae8cf5f5b0a4cbe0e"
        );
        assert_eq!(ir.byte_len, 51);
    }

    // ── Span correctness ─────────────────────────────────────────────

    #[test]
    fn spans_are_within_source() {
        let source = include_str!("../../../../fixtures/rust/simple.rs");
        let source_len = source.len() as u32;
        let ir = parse_rust(source.as_bytes(), "t.rs");

        for sym in &ir.symbols {
            if let Some(sb) = sym.start_byte {
                assert!(sb < source_len, "start_byte OOB for {}", sym.local_key);
            }
            if let Some(eb) = sym.end_byte {
                assert!(eb <= source_len, "end_byte OOB for {}", sym.local_key);
            }
            assert!(
                sym.start_line <= sym.end_line,
                "inverted span for {}",
                sym.local_key
            );
        }
    }

    #[test]
    fn call_lines_fall_within_caller() {
        let source = include_str!("../../../../fixtures/rust/simple.rs");
        let ir = parse_rust(source.as_bytes(), "t.rs");

        for call in &ir.calls {
            if let Some(caller) = ir
                .symbols
                .iter()
                .find(|s| s.local_key == call.caller_local_key)
            {
                assert!(
                    call.line >= caller.start_line && call.line <= caller.end_line,
                    "call line {} outside caller {} span {}-{}",
                    call.line,
                    call.caller_local_key,
                    caller.start_line,
                    caller.end_line
                );
            }
        }
    }

    #[test]
    fn parameters_have_non_empty_names() {
        let source = include_str!("../../../../fixtures/rust/simple.rs");
        let ir = parse_rust(source.as_bytes(), "t.rs");

        for sym in &ir.symbols {
            for (i, param) in sym.parameters.iter().enumerate() {
                assert!(
                    !param.name.is_empty(),
                    "param {i} of {} has empty name",
                    sym.local_key
                );
            }
        }
    }

    // ── Error resilience ─────────────────────────────────────────────

    #[test]
    fn parse_error_non_fatal() {
        let broken = b"fn broken( {\n  let x =\n}\nfn fine() {}\n";
        let ir = parse_rust(broken, "broken.rs");

        // Should still find `fine` symbol.
        assert!(
            ir.symbols.iter().any(|s| s.name == "fine"),
            "partial parse should extract fine"
        );
        // Diagnostics must be non-empty (previously dead code — never filled).
        assert!(
            !ir.diagnostics.is_empty(),
            "broken file must emit diagnostics"
        );
        let err = ir
            .diagnostics
            .iter()
            .find(|d| d.severity == DiagnosticSeverity::Error)
            .unwrap_or_else(|| panic!("expected an Error diagnostic, got {:?}", ir.diagnostics));
        // The `let x =` region is the first maximal ERROR region (line 2).
        assert_eq!(err.line, Some(2), "Error diagnostic must carry the right line");
        assert!(
            err.byte_span.is_some(),
            "Error diagnostic must carry a byte span"
        );
        let (s, e) = err.byte_span.unwrap();
        assert!(s < e, "byte span must be non-empty");
    }

    #[test]
    fn clean_sources_produce_zero_diagnostics() {
        let sources: [&[u8]; 3] = [
            include_str!("../../../../fixtures/rust/simple.rs").as_bytes(),
            MOD_A.as_bytes(),
            MOD_B.as_bytes(),
        ];
        for src in sources {
            let ir = parse_rust(src, "t.rs");
            assert!(
                ir.diagnostics.is_empty(),
                "clean source must have zero diagnostics: {:?}",
                ir.diagnostics
            );
        }
    }

    #[test]
    fn diagnostics_bounded_with_overflow_warning() {
        // 100 broken function items → ≤ MAX_DIAGNOSTICS_PER_FILE entries,
        // the last one being the overflow Warning.
        let mut src = String::new();
        for _ in 0..100 {
            src.push_str("fn broken() {\n    let x = ;\n}\n");
        }
        let ir = parse_rust(src.as_bytes(), "broken_many.rs");
        assert!(
            ir.diagnostics.len() <= utils::MAX_DIAGNOSTICS_PER_FILE,
            "diagnostics must be bounded, got {}",
            ir.diagnostics.len()
        );
        let last = ir.diagnostics.last().expect("at least one diagnostic");
        assert_eq!(
            last.severity,
            DiagnosticSeverity::Warning,
            "last diagnostic must be the overflow Warning: {:?}",
            ir.diagnostics.last()
        );
        assert!(
            last.message.contains("omitted"),
            "overflow Warning must mention the count: {}",
            last.message
        );
    }

    #[test]
    fn empty_file() {
        let ir = parse_rust(b"", "empty.rs");
        assert!(ir.symbols.is_empty());
        assert!(ir.calls.is_empty());
        assert_eq!(ir.line_count, 1); // empty file = 1 line
        assert_eq!(ir.byte_len, 0);
    }

    #[test]
    fn only_comments() {
        let ir = parse_rust(b"// just a comment\n/* block */\n", "c.rs");
        assert!(ir.symbols.is_empty());
        assert!(ir.calls.is_empty());
    }

    // ── Method + impl tests ──────────────────────────────────────────

    #[test]
    fn method_inside_impl_has_qualified_name() {
        let src = br#"
struct S;
impl S {
    fn method(&self) -> u8 {
        1
    }
}
fn free() {}
"#;
        let ir = parse_rust(src, "impl.rs");

        let method = ir
            .symbols
            .iter()
            .find(|s| s.name == "method")
            .expect("method");
        assert_eq!(method.kind, SK::Method);
        assert_eq!(method.qualified_name, "S::method");
        assert_eq!(method.local_key, "S::method");

        let free = ir.symbols.iter().find(|s| s.name == "free").expect("free");
        assert_eq!(free.kind, SK::Function);
        assert_eq!(free.qualified_name, "free");
    }

    #[test]
    fn content_hash_stable_across_parses() {
        let src = MOD_A.as_bytes();
        let a = parse_rust(src, "rust/mod_a.rs");
        let b = parse_rust(src, "rust/mod_a.rs");
        assert_eq!(a.content_hash, b.content_hash);
    }

    // ── Cross-file resolve ───────────────────────────────────────────

    #[test]
    fn cross_file_resolve_sets_callee_file() {
        // mod_a has bar() called by foo(). mod_b has unrelated().
        let mut irs = vec![
            parse_rust(MOD_A.as_bytes(), "rust/mod_a.rs"),
            parse_rust(MOD_B.as_bytes(), "rust/mod_b.rs"),
        ];

        // Before cross-file: foo→bar should be in-file resolved.
        {
            let call = irs[0]
                .calls
                .iter()
                .find(|c| c.callee_name == "bar")
                .unwrap();
            assert_eq!(call.callee_local_key.as_deref(), Some("bar"));
            assert!(call.callee_file.is_none()); // same file, no cross-file needed
        }

        crate::resolve::resolve_cross_file(&mut irs);

        // After: in-file resolved call unchanged.
        {
            let call = irs[0]
                .calls
                .iter()
                .find(|c| c.callee_name == "bar")
                .unwrap();
            assert_eq!(call.callee_local_key.as_deref(), Some("bar"));
        }

        // Cross-file: add a call from mod_b to foo (qualified).
        let mod_b_with_call = "fn unrelated() -> &'static str {\n    foo::bar();\n    \"iso\"\n}\n";
        let mut irs2 = vec![
            parse_rust(MOD_A.as_bytes(), "rust/mod_a.rs"),
            parse_rust(mod_b_with_call.as_bytes(), "rust/mod_b.rs"),
        ];
        crate::resolve::resolve_cross_file(&mut irs2);

        // foo::bar is qualified with :: but `foo` is not a symbol in mod_a (foo IS a symbol).
        // foo::bar won't match because the qualified name in the index is "foo", not "foo::bar".
        // So it should become external.
        let call = irs2[1]
            .calls
            .iter()
            .find(|c| c.callee_name == "foo::bar")
            .expect("foo::bar call from mod_b");
        // "foo::bar" contains ::, not found → external.
        assert!(call.callee_external);
    }

    // ── Golden IR compare ───────────────────────────────────────────

    #[test]
    fn golden_ir_matches_simple_fixture() {
        use crate::extractors::golden;

        let source = include_str!("../../../../fixtures/rust/simple.rs");
        let actual_ir = parse_rust(source.as_bytes(), "fixtures/rust/simple.rs");
        let expected_json = include_str!("../../../../fixtures/rust/simple.ir.json");
        let mut expected_ir: FileParseIR =
            serde_json::from_str(expected_json).expect("valid golden JSON");

        // Rebuild cards with cleared hash, then validate card invariants.
        golden::normalize_for_compare(&mut expected_ir);
        let mut actual_norm = actual_ir.clone();
        golden::normalize_for_compare(&mut actual_norm);

        golden::validate_schema(&actual_norm).expect("actual IR violates schema");
        golden::validate_schema(&expected_ir).expect("golden IR violates schema");

        assert_eq!(
            serde_json::to_string_pretty(&actual_norm).unwrap(),
            serde_json::to_string_pretty(&expected_ir).unwrap(),
            "Golden IR mismatch for Rust"
        );
    }

    #[test]
    fn is_test_flag_detects_attributes_mods_and_test_dirs() {
        let src = r#"
pub fn prod() -> u32 { 1 }
#[test]
fn unit_test() {}
mod tests {
    pub fn helper_in_tests() {}
}
#[cfg(test)]
mod cfg_tests {
    pub fn also_test() {}
}
"#;
        let ir = parse_rust(src.as_bytes(), "src/lib.rs");
        let flag = |k: &str| {
            ir.symbols
                .iter()
                .find(|s| s.local_key == k)
                .map(|s| s.is_test)
                .unwrap_or_else(|| panic!("symbol {k}"))
        };
        assert!(!flag("prod"), "prod must not be test");
        assert!(flag("unit_test"), "#[test] fn must be test");
        assert!(flag("helper_in_tests"), "mod tests content must be test");
        assert!(flag("also_test"), "#[cfg(test)] mod content must be test");

        let file_ir = parse_rust(b"pub fn t() {}", "tests/integration.rs");
        assert!(file_ir.symbols[0].is_test, "tests/ dir file must be test");
    }
}

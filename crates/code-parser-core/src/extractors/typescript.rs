/// TypeScript/TSX language extractor — walks a tree-sitter TypeScript AST
/// and collects symbols, calls, imports.
///
/// Single-pass recursive walk. Tracks current function context for call attribution.
/// Covers `.ts` and `.tsx` via `tree-sitter-typescript` grammar.

use code_parser_ir::{
    CallIR, DiagnosticIR, FileParseIR, ImportIR, ImportKind, ParameterIR,
    SymbolIR, SymbolKind,
};
use tree_sitter::Node;

use crate::hash;
use crate::language::Language;

use super::utils;
use super::LanguageExtractor;

pub struct TypeScriptExtractor;

impl LanguageExtractor for TypeScriptExtractor {
    fn extract(&self, tree: &tree_sitter::Tree, source: &[u8], file_path: &str) -> FileParseIR {
        let content_hash = hash::hash_bytes(source);
        let line_count = source.iter().filter(|&&b| b == b'\n').count() as u32 + 1;
        let byte_len = source.len() as u64;

        let mut ctx = ExtractCtx::new(source);
        ctx.file_path = file_path;

        // Collect imports directly from root children.
        let root = tree.root_node();
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() == "import_statement" {
                ctx.visit_import(&child);
            } else if child.kind() == "export_statement" {
                // export { ... } from '...' can also contain imports.
                // Recurse into export to find import children.
                let mut inner = child.walk();
                for c in child.named_children(&mut inner) {
                    if c.kind() == "import_statement" {
                        ctx.visit_import(&c);
                    }
                }
            }
        }

        // Now walk the full tree for symbols and calls.
        ctx.visit_node(&root);

        resolve_in_file(&mut ctx);

        FileParseIR {
            ir_version: 3,
            path: file_path.to_string(),
            language: Language::TypeScript.as_str().to_string(),
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
    /// When inside a function/arrow/method body, holds the caller's local_key.
    current_caller: Option<String>,
    /// Current class name stack — used for Method qualified names.
    class_stack: Vec<String>,
    /// Source file path (file-level test detection).
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
            class_stack: Vec::new(),
            file_path: "",
        }
    }

    /// Test detection (file-level: .test./.spec. names, __tests__ dirs).
    fn is_test_symbol(&self, _node: &Node) -> bool {
        is_test_file_path(self.file_path)
    }

    fn text(&self, node: &Node) -> &str {
        utils::node_text(node, self.source)
    }

    // ── recursive walk ───────────────────────────────────────────────

    fn visit_node(&mut self, node: &Node<'a>) {
        match node.kind() {
            // Container nodes — recurse.
            "program" | "statement_block" | "class_body" | "object" | "array"
            | "formal_parameters" | "arguments" | "parenthesized_expression"
            | "template_substitution" | "jsx_element" | "jsx_self_closing_element"
            | "jsx_fragment" | "jsx_expression" | "jsx_opening_element"
            | "jsx_closing_element" | "return_statement"
            | "if_statement" | "for_statement" | "while_statement"
            | "switch_statement" | "switch_body" | "try_statement"
            | "catch_clause" | "finally_clause" | "do_statement"
            | "with_statement" | "labeled_statement"
            | "expression_statement" | "comment" | "html_comment"
            | "ternary_expression" | "binary_expression" | "unary_expression"
            | "update_expression" | "assignment_expression" | "sequence_expression"
            | "subscript_expression" | "await_expression" | "yield_expression"
            | "spread_element" | "template_string" | "string" | "regex"
            | "number" | "true" | "false" | "null" | "undefined" | "identifier"
            | "this" | "super" | "type_annotation" | "type_arguments"
            | "type_parameters" | "union_type" | "intersection_type"
            | "index_type_query" | "typeof_expression" | "as_expression"
            | "satisfies_expression" | "non_null_expression" | "optional_type"
            | "readonly_type" | "tuple_type" | "array_type" | "function_type"
            | "constructor_type" | "literal_type" | "object_type" | "generic_type"
            | "indexed_access_type" | "conditional_type" | "mapped_type"
            | "template_literal_type" | "infer_type" | "predicate_type"
            | "qualified_name" | "computed_property_name" | "rest_pattern"
            | "object_pattern" | "array_pattern" | "optional_parameter"
            | "public_field_definition" | "property_signature"
            | "method_signature" | "call_signature" | "construct_signature"
            | "index_signature" | "decorator" | "ambient_declaration"
            | "abstract_class_declaration" | "internal_module" | "module"
            | "external_module_declaration" | "declare_statement"
            | "heritage_clause" | "extends_clause" | "implements_clause"
            | "enum_body" | "enum_member" | "required_parameter"
            | "optional_chain" | "named_imports" | "import_specifier"
            | "namespace_import" | "import_clause" | "export_clause"
            | "export_specifier" | "named_exports" | "pair" | "shorthand_property_identifier"
            | "void_expression" => {
                self.visit_children(node);
            }

            // Symbol declarations
            "function_declaration" => self.visit_function(node),
            "generator_function_declaration" => self.visit_function(node),
            "arrow_function" => self.visit_arrow_function(node),
            "class_declaration" => self.visit_class(node),
            "method_definition" => self.visit_method(node),
            "interface_declaration" => self.visit_interface(node),
            "enum_declaration" => self.visit_ts_enum(node),
            "type_alias_declaration" => self.visit_type_alias(node),

            // Top-level const/let/var
            "lexical_declaration" => self.visit_top_variable(node),
            "variable_declaration" => self.visit_top_variable(node),

            // Calls
            "call_expression" => self.visit_call(node),
            "new_expression" => self.visit_new_expr(node),

            // Imports
            "import_statement" => self.visit_import(node),

            // Skip exports — they wrap other declarations; recurse.
            "export_statement" => self.visit_children(node),

            // Recurse by default for unknown node types.
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

    fn visit_function(&mut self, node: &Node<'a>) {
        let name = field_text(node, "name", self.source).unwrap_or_default();
        if name.is_empty() || name == "function" {
            // Anonymous function — skip as symbol, but still visit body for calls.
            if let Some(body) = node.child_by_field_name("body") {
                self.visit_node(&body);
            }
            return;
        }

        let params = extract_params(node, self.source);
        let return_type = field_text(node, "return_type", self.source);
        let sig = build_sig(node, self.source);
        let doc = extract_docstring(node, self.source);

        self.symbols.push(SymbolIR {
            local_key: name.clone(),
            name: name.clone(),
            qualified_name: name.clone(),
            kind: SymbolKind::Function,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: sig,
            parameters: params,
            return_type,
            is_test: self.is_test_symbol(node),
            docstring: doc,        });

        let prev = self.current_caller.replace(name);
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        }
        self.current_caller = prev;
    }

    fn visit_arrow_function(&mut self, node: &Node<'a>) {
        // Arrow functions are anonymous. Visit body for calls but don't emit a symbol.
        // Exception: `const foo = () => {}` — handled by visit_top_variable.
        let prev_caller = self.current_caller.clone();
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        } else {
            self.visit_children(node);
        }
        self.current_caller = prev_caller;
    }

    fn visit_class(&mut self, node: &Node<'a>) {
        let name = field_text(node, "name", self.source).unwrap_or_default();
        if name.is_empty() {
            self.visit_children(node);
            return;
        }
        let doc = extract_docstring(node, self.source);

        self.symbols.push(SymbolIR {
            local_key: name.clone(),
            name: name.clone(),
            qualified_name: name.clone(),
            kind: SymbolKind::Class,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: None,
            parameters: Vec::new(),
            return_type: None,
            is_test: self.is_test_symbol(node),
            docstring: doc,        });

        // Push class context for method qualified names.
        self.class_stack.push(name);
        // Visit body — methods inside get qualified names.
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        } else {
            self.visit_children(node);
        }
        self.class_stack.pop();
    }

    fn visit_method(&mut self, node: &Node<'a>) {
        let name = field_text(node, "name", self.source).unwrap_or_default();
        if name.is_empty() {
            self.visit_children(node);
            return;
        }
        let params = extract_params(node, self.source);
        let return_type = field_text(node, "return_type", self.source);
        let sig = build_sig(node, self.source);

        let (qualified_name, local_key) = if let Some(cls) = self.class_stack.last() {
            (format!("{cls}.{name}"), format!("{cls}.{name}"))
        } else {
            (name.clone(), name.clone())
        };

        self.symbols.push(SymbolIR {
            local_key: local_key.clone(),
            name,
            qualified_name,
            kind: SymbolKind::Method,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: sig,
            parameters: params,
            return_type,
            is_test: self.is_test_symbol(node),
            docstring: None,        });

        let prev = self.current_caller.replace(local_key);
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        }
        self.current_caller = prev;
    }

    fn visit_interface(&mut self, node: &Node<'a>) {
        let name = field_text(node, "name", self.source).unwrap_or_default();
        if name.is_empty() {
            self.visit_children(node);
            return;
        }
        let doc = extract_docstring(node, self.source);

        self.symbols.push(SymbolIR {
            local_key: name.clone(),
            name: name.clone(),
            qualified_name: name.clone(),
            kind: SymbolKind::Interface,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: None,
            parameters: Vec::new(),
            return_type: None,
            is_test: self.is_test_symbol(node),
            docstring: doc,        });
        self.visit_children(node);
    }

    fn visit_ts_enum(&mut self, node: &Node<'a>) {
        let name = field_text(node, "name", self.source).unwrap_or_default();
        if name.is_empty() {
            self.visit_children(node);
            return;
        }
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
            is_test: self.is_test_symbol(node),
            docstring: None,        });
        self.visit_children(node);
    }

    fn visit_type_alias(&mut self, node: &Node<'a>) {
        let name = field_text(node, "name", self.source).unwrap_or_default();
        if name.is_empty() {
            self.visit_children(node);
            return;
        }
        self.symbols.push(SymbolIR {
            local_key: name.clone(),
            name: name.clone(),
            qualified_name: name.clone(),
            kind: SymbolKind::TypeAlias,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: None,
            parameters: Vec::new(),
            return_type: None,
            is_test: self.is_test_symbol(node),
            docstring: None,        });
        self.visit_children(node);
    }

    /// Top-level `const name = arrow_function` or `let x = ...`
    fn visit_top_variable(&mut self, node: &Node<'a>) {
        // Only emit symbols for top-level declarations (no current_caller, no class_stack).
        // Find the variable_declarator children.
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                if child.kind() == "variable_declarator" {
                    let name = field_text(&child, "name", self.source).unwrap_or_default();
                    if name.is_empty() {
                        self.visit_node(&child);
                        continue;
                    }
                    // Check if it's initialized with a function or arrow.
                    let value = child.child_by_field_name("value");
                    let is_function = value.as_ref().map_or(false, |v| {
                        matches!(v.kind(), "function_declaration" | "arrow_function" | "generator_function_declaration")
                    });
                    let kind = if is_function { SymbolKind::Function } else { SymbolKind::Constant };

                    let sig = is_function.then(|| {
                        value.as_ref().and_then(|v| build_sig(v, self.source))
                    }).flatten();

                    self.symbols.push(SymbolIR {
                        local_key: name.clone(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind,
                        start_line: utils::point_line(&child),
                        end_line: utils::point_end_line(&child),
                        start_byte: Some(utils::start_byte(node)),
                        end_byte: Some(utils::end_byte(node)),
                        signature: sig,
                        parameters: value.as_ref().map_or(Vec::new(), |v| extract_params(v, self.source)),
                        return_type: value.and_then(|v| field_text(&v, "return_type", self.source)),
            is_test: self.is_test_symbol(node),
                        docstring: None,        });

                    // If it's a function, track calls inside it.
                    if is_function {
                        let prev = self.current_caller.replace(name);
                        if let Some(v) = value {
                            if let Some(body) = v.child_by_field_name("body") {
                                self.visit_node(&body);
                            } else {
                                self.visit_node(&v);
                            }
                        }
                        self.current_caller = prev;
                    } else {
                        // Visit for nested calls.
                        self.visit_node(&child);
                    }
                } else {
                    self.visit_node(&child);
                }
            }
        }
    }

    // ── call visitors ────────────────────────────────────────────────

    fn visit_call(&mut self, node: &Node<'a>) {
        if self.current_caller.is_none() {
            self.visit_children(node);
            return;
        }
        let line = utils::point_line(node);
        let callee_name = extract_call_callee(node, self.source);
        let caller = self.current_caller.clone().unwrap();

        self.calls.push(CallIR {
            caller_local_key: caller,
            callee_name,
            callee_local_key: None,
            callee_file: None,
            callee_external: false,
            line,
            column: Some(utils::point_column(node)),
        });

        // Visit arguments for nested calls.
        self.visit_children(node);
    }

    fn visit_new_expr(&mut self, node: &Node<'a>) {
        if self.current_caller.is_none() {
            self.visit_children(node);
            return;
        }
        let line = utils::point_line(node);
        let ctor = node
            .child_by_field_name("constructor")
            .as_ref()
            .map(|n| self.text(n).to_string())
            .unwrap_or_else(|| "new".to_string());
        let callee_name = format!("new {ctor}");
        let caller = self.current_caller.clone().unwrap();

        self.calls.push(CallIR {
            caller_local_key: caller,
            callee_name,
            callee_local_key: None,
            callee_file: None,
            callee_external: false,
            line,
            column: Some(utils::point_column(node)),
        });
        self.visit_children(node);
    }

    // ── import visitor ───────────────────────────────────────────────

    fn visit_import(&mut self, node: &Node<'a>) {
        let line = Some(utils::point_line(node));
        let source_text = node
            .child_by_field_name("source")
            .as_ref()
            .map(|n| self.text(n).to_string())
            .unwrap_or_default();
        let target_module = source_text.trim_matches(|c| c == '\'' || c == '"' || c == '`').to_string();

        // import_clause is a named child, not a field child.
        let clause = utils::child_by_kind(node, "import_clause");
        let clause = match clause {
            Some(c) => c,
            None => return,
        };

        // Default import: the clause may have a direct "identifier" named child.
        if let Some(ident) = utils::child_by_kind(&clause, "identifier") {
            let name = self.text(&ident).to_string();
            if !name.is_empty() && name != "import" {
                self.imports.push(ImportIR {
                    import_name: name,
                    target_module: target_module.clone(),
                    kind: ImportKind::Default,
                    line,
                    column: Some(utils::point_column(node)),
                });
            }
        }

        // Named imports: child_by_kind "named_imports".
        if let Some(named) = utils::child_by_kind(&clause, "named_imports") {
            let mut cursor = named.walk();
            for child in named.named_children(&mut cursor) {
                if child.kind() == "import_specifier" {
                    let name = field_text(&child, "name", self.source).unwrap_or_default();
                    if !name.is_empty() {
                        self.imports.push(ImportIR {
                            import_name: name,
                            target_module: target_module.clone(),
                            kind: ImportKind::Named,
                            line,
                            column: Some(utils::point_column(node)),
                        });
                    }
                }
            }
        }

        // Namespace import: child_by_kind "namespace_import".
        if let Some(ns) = utils::child_by_kind(&clause, "namespace_import") {
            let name = field_text(&ns, "name", self.source).unwrap_or_default();
            if !name.is_empty() {
                self.imports.push(ImportIR {
                    import_name: name,
                    target_module: target_module.clone(),
                    kind: ImportKind::Star,
                    line,
                    column: Some(utils::point_column(node)),
                });
            }
        }

        // Type-only import: child_by_kind "type" — skip for now (handled by named_imports above).
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn field_text<'a>(node: &Node<'a>, field: &str, source: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| utils::node_text(&n, source).to_string())
}

/// Extract callee name from a call_expression or new_expression.
fn extract_call_callee(node: &Node, source: &[u8]) -> String {
    let func = node.child_by_field_name("function");
    match func {
        Some(f) => collect_chain_name(&f, source),
        None => utils::node_text(node, source).to_string(),
    }
}

/// Collect a member expression chain: `obj.method()` → "obj.method".
fn collect_chain_name(node: &Node, source: &[u8]) -> String {
    match node.kind() {
        "member_expression" => {
            let object = node.child_by_field_name("object");
            let property = node.child_by_field_name("property");
            let obj_part = object
                .as_ref()
                .map(|o| collect_chain_name(o, source))
                .unwrap_or_default();
            let prop_part = property
                .as_ref()
                .map(|p| utils::node_text(p, source))
                .unwrap_or("?");
            if obj_part.is_empty() {
                prop_part.to_string()
            } else {
                format!("{obj_part}.{prop_part}")
            }
        }
        "identifier" | "this" | "super" => utils::node_text(node, source).to_string(),
        "call_expression" => {
            // Nested call like `getFoo()()` — use callee text.
            extract_call_callee(node, source)
        }
        _ => utils::node_text(node, source).to_string(),
    }
}

/// Build a human-readable signature from a function node.
fn build_sig(node: &Node, source: &[u8]) -> Option<String> {
    // Use source text from function start to body start.
    let body = node.child_by_field_name("body")?;
    let start = node.start_byte() as usize;
    let body_start = body.start_byte() as usize;
    if body_start <= start || body_start - start > 2000 {
        return None;
    }
    let sig_bytes = &source[start..body_start];
    let sig = String::from_utf8_lossy(sig_bytes).into_owned();
    let sig = sig.trim();
    if sig.is_empty() {
        return None;
    }
    Some(
        sig.lines()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Extract parameters from a function or method.
fn extract_params(node: &Node, source: &[u8]) -> Vec<ParameterIR> {
    let params_node = node.child_by_field_name("parameters");
    let params_node = match params_node {
        Some(p) => p,
        None => return Vec::new(),
    };

    let mut params = Vec::new();
    let mut cursor = params_node.walk();
    for child in params_node.named_children(&mut cursor) {
        match child.kind() {
            "required_parameter" | "optional_parameter" => {
                let pattern = child
                    .child_by_field_name("pattern")
                    .or_else(|| child.child_by_field_name("name"));
                let name = pattern
                    .as_ref()
                    .map(|n| utils::node_text(n, source).to_string())
                    .unwrap_or_default();
                let type_annotation = child
                    .child_by_field_name("type")
                    .map(|n| utils::node_text(&n, source).to_string());
                let default_value = child
                    .child_by_field_name("default_value")
                    .or_else(|| child.child_by_field_name("value"))
                    .map(|n| utils::node_text(&n, source).to_string());

                params.push(ParameterIR {
                    name,
                    type_annotation,
                    default_value,
                });
            }
            "rest_pattern" => {
                // `...args`
                let name = child
                    .child_by_field_name("pattern")
                    .or_else(|| child.child_by_field_name("name"))
                    .map(|n| utils::node_text(&n, source).to_string())
                    .unwrap_or_default();
                params.push(ParameterIR {
                    name: format!("...{name}"),
                    type_annotation: None,
                    default_value: None,
                });
            }
            _ => {}
        }
    }
    params
}

/// Extract JSDoc-style comments preceding a node.
fn extract_docstring(node: &Node, source: &[u8]) -> Option<String> {
    let mut doc_lines: Vec<String> = Vec::new();
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "comment" => {
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
                } else if text.starts_with("//") {
                    // Single-line comment — not a docstring, stop.
                    break;
                }
            }
            _ => break,
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

// ── In-file resolution ───────────────────────────────────────────────────

fn resolve_in_file(ctx: &mut ExtractCtx) {
    let mut by_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut by_qualified: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for sym in &ctx.symbols {
        by_name.entry(sym.name.clone()).or_insert_with(|| sym.local_key.clone());
        by_qualified
            .entry(sym.qualified_name.clone())
            .or_insert_with(|| sym.local_key.clone());
    }

    for call in &mut ctx.calls {
        if call.callee_local_key.is_some() {
            continue;
        }
        if let Some(key) = by_name.get(&call.callee_name) {
            call.callee_local_key = Some(key.clone());
            call.callee_external = false;
            continue;
        }
        if let Some(key) = by_qualified.get(&call.callee_name) {
            call.callee_local_key = Some(key.clone());
            call.callee_external = false;
            continue;
        }
        if call.callee_name.contains("::") {
            call.callee_external = true;
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────


/// File-level test detection (ts/js): .test. / .spec. files, __tests__ dirs.
fn is_test_file_path(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.contains(".test.") || base.contains(".spec.")
        || path.contains("/__tests__/") || path.contains("/test/") || path.contains("/tests/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_parser_ir::SymbolKind as SK;

    fn parse_ts(source: &[u8], path: &str) -> FileParseIR {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let ir = TypeScriptExtractor.extract(&tree, source, path);
        let mut ir = ir;
        ir.metrics = crate::metrics::compute(&tree, source, crate::metrics::comment_kinds(&ir.language));
        ir.ir_version = 3;
        ir
    }

    // ── Simple fixture ───────────────────────────────────────────────

    const SIMPLE_TS: &str = r#"import { useState } from "react";

interface User {
  id: number;
  name: string;
}

class Greeter {
  greet(name: string): string {
    return this.format(name);
  }

  private format(name: string): string {
    return `Hello, ${name}`;
  }
}

function sayHello(user: User): string {
  const g = new Greeter();
  return g.greet(user.name);
}

const double = (x: number): number => x * 2;
"#;

    #[test]
    fn extracts_symbols() {
        let ir = parse_ts(SIMPLE_TS.as_bytes(), "test.ts");

        let keys: Vec<&str> = ir.symbols.iter().map(|s| s.local_key.as_str()).collect();
        assert!(keys.contains(&"User"), "missing interface User, got {keys:?}");
        assert!(keys.contains(&"Greeter"), "missing class Greeter");
        assert!(keys.contains(&"Greeter.greet"), "missing method Greeter.greet");
        assert!(keys.contains(&"Greeter.format"), "missing method Greeter.format");
        assert!(keys.contains(&"sayHello"), "missing fn sayHello");
        assert!(keys.contains(&"double"), "missing const double");

        let user = ir.symbols.iter().find(|s| s.name == "User").unwrap();
        assert_eq!(user.kind, SK::Interface);

        let greeter = ir.symbols.iter().find(|s| s.name == "Greeter").unwrap();
        assert_eq!(greeter.kind, SK::Class);

        let greet = ir.symbols.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(greet.kind, SK::Method);
        assert_eq!(greet.qualified_name, "Greeter.greet");

        let say_hello = ir.symbols.iter().find(|s| s.name == "sayHello").unwrap();
        assert_eq!(say_hello.kind, SK::Function);
    }

    #[test]
    fn extracts_calls() {
        let ir = parse_ts(SIMPLE_TS.as_bytes(), "test.ts");

        // Greeter.greet calls this.format
        let greet_calls: Vec<&CallIR> = ir
            .calls.iter()
            .filter(|c| c.caller_local_key == "Greeter.greet")
            .collect();
        assert!(!greet_calls.is_empty(), "Greeter.greet should have calls");
        let format_call = greet_calls.iter().find(|c| c.callee_name.contains("format")).unwrap();
        assert!(format_call.callee_name.contains("format"));

        // sayHello calls new Greeter and g.greet
        let say_hello_calls: Vec<&CallIR> = ir
            .calls.iter()
            .filter(|c| c.caller_local_key == "sayHello")
            .collect();
        assert!(
            say_hello_calls.iter().any(|c| c.callee_name.contains("Greeter")),
            "sayHello should call new Greeter"
        );
        assert!(
            say_hello_calls.iter().any(|c| c.callee_name.contains("greet")),
            "sayHello should call g.greet"
        );
    }

    #[test]
    fn extracts_imports() {
        let ir = parse_ts(SIMPLE_TS.as_bytes(), "test.ts");

        assert!(!ir.imports.is_empty(), "should have imports");
        let use_state = ir
            .imports.iter()
            .find(|i| i.import_name == "useState")
            .expect("useState import");
        assert_eq!(use_state.target_module, "react");
        assert_eq!(use_state.kind, ImportKind::Named);
    }

    #[test]
    fn empty_file() {
        let ir = parse_ts(b"", "empty.ts");
        assert!(ir.symbols.is_empty());
        assert!(ir.calls.is_empty());
    }

    #[test]
    fn detects_language() {
        let ir = parse_ts(b"const x = 1;", "t.ts");
        assert_eq!(ir.language, "TypeScript");
    }

    #[test]
    fn golden_ir_matches_fixture() {
        use crate::extractors::golden;

        let source = include_str!("../../../../fixtures/typescript/simple.ts");
        let actual_ir = parse_ts(source.as_bytes(), "fixtures/typescript/simple.ts");
        let expected_json = include_str!("../../../../fixtures/typescript/simple.ir.json");
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
            "Golden IR mismatch for TypeScript"
        );
    }


    #[test]
    fn is_test_flag_detects_test_filenames() {
        let src = "export function add(a: number, b: number): number { return a + b; }";
        let normal = parse_ts(src.as_bytes(), "src/math.ts");
        assert!(!normal.symbols[0].is_test);
        let test_file = parse_ts(src.as_bytes(), "src/math.test.ts");
        assert!(test_file.symbols[0].is_test, ".test.ts file must be test");
        let spec_file = parse_ts(src.as_bytes(), "src/math.spec.ts");
        assert!(spec_file.symbols[0].is_test, ".spec.ts file must be test");
    }
}

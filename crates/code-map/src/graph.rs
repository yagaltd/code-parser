//! Call-graph over a loaded map — callers, callees, shortest path (BFS).
//!
//! Edges come only from what the IR already resolved (v1 semantics):
//! - in-file: `CallIR.callee_local_key`
//! - cross-file: `CallIR.callee_file` + exact `qualified_name` match, with a
//!   unique-bare-name fallback inside the target file
//! Ambiguity is surfaced, never guessed: a name lookup matching several
//! nodes returns all of them and callers print one group per match.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use code_parser_ir::{FileParseIR, SymbolKind};

/// One symbol node in the map.
#[derive(Debug, Clone)]
pub struct Node {
    pub path: String,
    pub qualified_name: String,
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
}

/// One resolved call edge: `nodes[caller]` calls `nodes[callee]` at `call_line`.
/// `inferred` = resolved by the globally-unique-bare-name tier (not IR-resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub caller: usize,
    pub callee: usize,
    pub call_line: u32,
    pub inferred: bool,
}

pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// name or qualified_name → node indices (sorted, deduped).
    by_name: HashMap<String, Vec<usize>>,
    forward: HashMap<usize, BTreeSet<usize>>,
    backward: HashMap<usize, BTreeSet<usize>>,
}

impl Graph {
    /// Load a JSONL map (one `FileParseIR` per line, blank lines skipped).
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read map {}", path.display()))?;
        Self::from_jsonl(&text)
    }

    /// Build from JSONL text (testable without a file).
    pub fn from_jsonl(text: &str) -> Result<Self> {
        let mut irs = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let ir: FileParseIR = serde_json::from_str(line)
                .with_context(|| format!("map line {} is not a FileParseIR", i + 1))?;
            irs.push(ir);
        }
        Ok(Self::build(irs))
    }

    pub fn build(irs: Vec<FileParseIR>) -> Self {
        let mut nodes = Vec::new();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        // Build-only lookups: local keys and per-file qualified names.
        let mut by_local: HashMap<(String, String), usize> = HashMap::new();
        let mut by_qn_path: HashMap<(String, String), usize> = HashMap::new();

        for ir in &irs {
            for sym in &ir.symbols {
                let idx = nodes.len();
                nodes.push(Node {
                    path: ir.path.clone(),
                    qualified_name: sym.qualified_name.clone(),
                    name: sym.name.clone(),
                    kind: sym.kind.clone(),
                    start_line: sym.start_line,
                    end_line: sym.end_line,
                });
                by_name.entry(sym.name.clone()).or_default().push(idx);
                by_name
                    .entry(sym.qualified_name.clone())
                    .or_default()
                    .push(idx);
                by_local
                    .entry((ir.path.clone(), sym.local_key.clone()))
                    .or_insert(idx);
                by_qn_path
                    .entry((ir.path.clone(), sym.qualified_name.clone()))
                    .or_insert(idx);
            }
        }
        for v in by_name.values_mut() {
            v.sort_unstable();
            v.dedup();
        }

        let mut edges: Vec<Edge> = Vec::new();
        // Nodes by exact bare name — the global-unique inference tier.
        let mut by_bare: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            by_bare.entry(n.name.as_str()).or_default().push(i);
        }
        for ir in &irs {
            for call in &ir.calls {
                let Some(&caller) = by_local.get(&(ir.path.clone(), call.caller_local_key.clone()))
                else {
                    continue;
                };
                if let Some(callee) = self_callee(&by_local, &by_qn_path, &nodes, ir, call) {
                    edges.push(Edge {
                        caller,
                        callee,
                        call_line: call.line,
                        inferred: false,
                    });
                } else if call.callee_local_key.is_none() && call.callee_file.is_none() {
                    // Inference tier: `code_parser_core::parse_repo` → the
                    // unique symbol named `parse_repo`, wherever it lives.
                    // The extractor records the whole call expression
                    // (`Foo::bar(args).chain`) — clean it first. Field chains
                    // (`x.foo()`) yield None — genuinely ambiguous; ambiguity
                    // emits no edge (codedb's rule).
                    if let Some(bare) = bare_name(&call.callee_name) {
                        let cands = by_bare.get(bare).cloned().unwrap_or_default();
                        if cands.len() == 1 {
                            edges.push(Edge {
                                caller,
                                callee: cands[0],
                                call_line: call.line,
                                inferred: true,
                            });
                        }
                    }
                }
                // Everything else stays unresolved — v1 semantics.
            }
        }

        let mut forward: HashMap<usize, BTreeSet<usize>> = HashMap::new();
        let mut backward: HashMap<usize, BTreeSet<usize>> = HashMap::new();
        for e in &edges {
            forward.entry(e.caller).or_default().insert(e.callee);
            backward.entry(e.callee).or_default().insert(e.caller);
        }

        Self {
            nodes,
            edges,
            by_name,
            forward,
            backward,
        }
    }

    /// All nodes whose name OR qualified_name equals `name` (deduped, sorted).
    /// Empty = unknown; len > 1 = ambiguous (callers print one group each).
    pub fn resolve(&self, name: &str) -> Vec<usize> {
        self.by_name.get(name).cloned().unwrap_or_default()
    }

    /// Who calls this node. Returns (caller node, call-site line).
    pub fn callers(&self, node: usize) -> Vec<(usize, u32)> {
        self.backward
            .get(&node)
            .map(|set| set.iter().map(|&c| (c, self.call_line(c, node))).collect())
            .unwrap_or_default()
    }

    /// What this node calls. Returns (callee node, call-site line).
    pub fn callees(&self, node: usize) -> Vec<(usize, u32)> {
        self.forward
            .get(&node)
            .map(|set| set.iter().map(|&c| (c, self.call_line(node, c))).collect())
            .unwrap_or_default()
    }

    /// Call-site line of the `from → to` edge (0 if absent).
    pub fn call_line(&self, from: usize, to: usize) -> u32 {
        self.edges
            .iter()
            .find(|e| e.caller == from && e.callee == to)
            .map(|e| e.call_line)
            .unwrap_or(0)
    }

    /// Is the `from → to` edge inferred (globally-unique tier, not IR-resolved)?
    pub fn is_inferred(&self, from: usize, to: usize) -> bool {
        self.edges
            .iter()
            .find(|e| e.caller == from && e.callee == to)
            .map(|e| e.inferred)
            .unwrap_or(false)
    }

    /// Shortest call path `from → … → to` by hops (BFS). Returns the walked
    /// nodes in order; pair with [`Graph::call_line`] for display.
    /// `None` = unreachable within `max_hops`.
    pub fn shortest_path(&self, from: usize, to: usize, max_hops: usize) -> Option<Vec<usize>> {
        if from == to {
            return Some(vec![from]);
        }
        let mut parent: HashMap<usize, (usize, usize)> = HashMap::new(); // node → (prev, steps)
        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(from);
        parent.insert(from, (from, 0));
        while let Some(cur) = queue.pop_front() {
            let (_, steps) = parent[&cur];
            if steps >= max_hops {
                continue;
            }
            for &next in self.forward.get(&cur).into_iter().flatten() {
                if parent.contains_key(&next) {
                    continue;
                }
                parent.insert(next, (cur, steps + 1));
                if next == to {
                    // Reconstruct.
                    let mut path = vec![next];
                    let mut cur = next;
                    while cur != from {
                        cur = parent[&cur].0;
                        path.push(cur);
                    }
                    path.reverse();
                    return Some(path);
                }
                queue.push_back(next);
            }
        }
        None
    }
}

/// Clean a raw call expression into a candidate bare name.
/// `"code_parser_core::parse_repo(&dir, x).with_context"` → `"parse_repo"`
/// (the `::` path before the first arg group wins, chaining ignored).
/// Plain field chains (`"results.iter().map"`) → `None` — genuinely
/// ambiguous. `"alpha()"` → `"alpha"`.
fn bare_name(callee: &str) -> Option<&str> {
    let head = callee.split('(').next().unwrap_or(callee);
    if head.contains("::") {
        let bare = head.rsplit("::").next().unwrap_or(head);
        return (!bare.is_empty()).then_some(bare);
    }
    if head.contains('.') || head.is_empty() {
        return None;
    }
    (!head.is_empty()).then_some(head)
}

/// Resolve the callee node for one call. Preference order:
/// 1. in-file `callee_local_key`
/// 2. cross-file exact `qualified_name` inside `callee_file`
/// 3. cross-file unique bare-name fallback inside `callee_file`
fn self_callee(
    by_local: &HashMap<(String, String), usize>,
    by_qn_path: &HashMap<(String, String), usize>,
    nodes: &[Node],
    ir: &FileParseIR,
    call: &code_parser_ir::CallIR,
) -> Option<usize> {
    if let Some(lk) = &call.callee_local_key {
        return by_local.get(&(ir.path.clone(), lk.clone())).copied();
    }
    let Some(file) = &call.callee_file else {
        return None;
    };
    if let Some(&idx) = by_qn_path.get(&(file.clone(), call.callee_name.clone())) {
        return Some(idx);
    }
    // Unique bare-name fallback: `util::helper` not found as qualified_name in
    // the target file, but exactly one symbol named `helper` lives there.
    let bare = call.callee_name.rsplit("::").next().unwrap_or(&call.callee_name);
    let matches: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.path == *file && n.name == bare)
        .map(|(i, _)| i)
        .collect();
    (matches.len() == 1).then(|| matches[0])
}

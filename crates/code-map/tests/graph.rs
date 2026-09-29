//! Graph tests over the committed hand-checked edge fixture
//! (`fixtures/codemap/edges.jsonl`): in-file, cross-file, bare-name
//! fallback, external skip, ambiguity, BFS path/hops.

use code_map::graph::Graph;

fn fixture() -> Graph {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/codemap/edges.jsonl"
    ))
    .expect("fixture edges.jsonl");
    Graph::from_jsonl(&text).expect("fixture deserializes")
}

fn one(g: &Graph, qn: &str) -> usize {
    let m = g.resolve(qn);
    assert_eq!(m.len(), 1, "expected unique '{qn}', got {}", m.len());
    m[0]
}

fn caller_set(g: &Graph, node: usize) -> Vec<String> {
    let mut v: Vec<String> = g
        .callers(node)
        .into_iter()
        .map(|(c, _)| g.nodes[c].qualified_name.clone())
        .collect();
    v.sort();
    v
}

fn callee_set(g: &Graph, node: usize) -> Vec<String> {
    let mut v: Vec<String> = g
        .callees(node)
        .into_iter()
        .map(|(c, _)| g.nodes[c].qualified_name.clone())
        .collect();
    v.sort();
    v
}

#[test]
fn in_file_and_cross_file_edges() {
    let g = fixture();
    let helper = one(&g, "util::helper");
    // run_helper (in-file) + main (cross-file) — and nothing else.
    assert_eq!(caller_set(&g, helper), vec!["main", "util::run_helper"]);
}

#[test]
fn callees_follow_local_key() {
    let g = fixture();
    let setup = one(&g, "setup");
    assert_eq!(callee_set(&g, setup), vec!["main"]);
    let main = one(&g, "main");
    // IR-resolved + inferred (globally-unique `parse_repo`).
    assert_eq!(callee_set(&g, main), vec!["other::parse_repo", "util::helper"]);
}

#[test]
fn external_call_produces_no_edge() {
    let g = fixture();
    // `missing::thing` has callee_external=true and no file — unknown symbol.
    assert!(g.resolve("missing::thing").is_empty());
    let broken = one(&g, "broken");
    assert!(callee_set(&g, broken).is_empty());
}

#[test]
fn bare_name_lookup_is_ambiguous() {
    let g = fixture();
    let mut m = g.resolve("helper");
    m.sort();
    assert_eq!(m.len(), 2);
    let mut names: Vec<&str> = m.iter().map(|&i| g.nodes[i].qualified_name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["other::helper", "util::helper"]);
}

#[test]
fn cross_file_bare_name_fallback() {
    let g = fixture();
    // `indirect` calls `util::helper` with callee_file=src/other.rs — no such
    // qualified_name there, but exactly one symbol named `helper` → fallback.
    let other_helper = one(&g, "other::helper");
    assert_eq!(caller_set(&g, other_helper), vec!["indirect"]);
}

#[test]
fn globally_unique_bare_name_inferred() {
    let g = fixture();
    // `code_parser_core::parse_repo` → the map's single `parse_repo` symbol.
    let target = one(&g, "other::parse_repo");
    assert_eq!(caller_set(&g, target), vec!["main"]);
    let main = one(&g, "main");
    assert!(g.is_inferred(main, target), "this edge is inference-tier");
    // IR-resolved edges are never marked inferred.
    let helper = one(&g, "util::helper");
    assert!(!g.is_inferred(main, helper));
}

#[test]
fn shortest_path_two_hops() {
    let g = fixture();
    let setup = one(&g, "setup");
    let helper = one(&g, "util::helper");
    let path = g.shortest_path(setup, helper, 8).expect("reachable");
    let qn: Vec<&str> = path.iter().map(|&i| g.nodes[i].qualified_name.as_str()).collect();
    assert_eq!(qn, vec!["setup", "main", "util::helper"]);
}

#[test]
fn unreachable_within_max_hops() {
    let g = fixture();
    let main = one(&g, "main");
    let run_helper = one(&g, "util::run_helper");
    assert!(g.shortest_path(main, run_helper, 8).is_none());
}

#[test]
fn hop_cap_cuts_path() {
    let g = fixture();
    let setup = one(&g, "setup");
    let helper = one(&g, "util::helper");
    // 2 hops needed; cap at 1 → none.
    assert!(g.shortest_path(setup, helper, 1).is_none());
    assert!(g.shortest_path(setup, helper, 2).is_some());
}

#[test]
fn self_path_is_zero_hops() {
    let g = fixture();
    let main = one(&g, "main");
    let path = g.shortest_path(main, main, 8).expect("self");
    assert_eq!(path.len(), 1);
}

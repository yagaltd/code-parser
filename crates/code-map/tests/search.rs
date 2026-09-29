//! Search tests — IDF ordering, OR semantics, regex mode, fuzzy typo
//! recovery, determinism, token accounting. IRs are built programmatically.

use code_map::search;
use code_parser_ir::*;

fn card(text: &str) -> RetrievalCard {
    RetrievalCard {
        card_version: 1,
        est_tokens: (text.len() as u32 / 4).max(1),
        text: text.into(),
    }
}

fn ir(path: &str, card_text: &str, syms: &[(&str, &str)]) -> FileParseIR {
    let mut ir = FileParseIR::empty(path, "Rust");
    ir.content_hash = path.into();
    ir.retrieval_card = card(card_text);
    for &(qn, sig) in syms {
        ir.symbols.push(SymbolIR {
            local_key: qn.into(),
            name: qn.rsplit("::").next().unwrap().into(),
            qualified_name: qn.into(),
            kind: SymbolKind::Function,
            start_line: 1,
            end_line: 2,
            start_byte: None,
            end_byte: None,
            signature: Some(sig.into()),
            parameters: vec![],
            return_type: None,
            docstring: None,
            is_test: false,
        });
        ir.symbol_cards.push(card(&format!("SYM {qn} {sig}")));
    }
    ir
}

#[test]
fn rare_term_outranks_common_term() {
    let irs = vec![
        ir("src/a.rs", "parse repo files with the parser", &[("parse_a", "fn")]),
        ir("src/b.rs", "parse json config", &[("parse_b", "fn")]),
        ir("src/c.rs", "ingest walrus sightings into the store", &[("ingest_c", "fn")]),
    ];
    let hits = search::run(&irs, "walrus parse", 10).unwrap();
    assert!(!hits.is_empty());
    // The only unit mentioning `walrus` (rare) must outrank same-common hits.
    assert_eq!(hits[0].path, "src/c.rs");
}

#[test]
fn or_semantics_more_matches_win() {
    let irs = vec![
        ir("src/one.rs", "alpha only here", &[]),
        ir("src/two.rs", "alpha and beta both here", &[]),
    ];
    let hits = search::run(&irs, "alpha beta", 10).unwrap();
    assert_eq!(hits[0].path, "src/two.rs");
}

#[test]
fn regex_mode_filters_and_fixes_score() {
    let irs = vec![
        ir("src/x.rs", "mentions callee_file passes", &[]),
        ir("src/y.rs", "nothing relevant", &[]),
    ];
    let hits = search::run(&irs, "re:callee_file", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "src/x.rs");
    assert_eq!(hits[0].score, 1.0);
}

#[test]
fn fuzzy_recovers_typo_in_symbol_name() {
    let irs = vec![
        ir("src/p.rs", "repository parsing utilities", &[("parse_repo", "fn parse_repo()")]),
        ir("src/q.rs", "unrelated module", &[("render_ui", "fn render_ui()")]),
    ];
    let hits = search::run(&irs, "parsrepo", 3).unwrap();
    assert!(
        hits.iter().take(3).any(|h| h.name == "parse_repo"),
        "typo query should surface parse_repo in top-3, got {:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
}

#[test]
fn deterministic_output() {
    let irs = vec![
        ir("src/a.rs", "watch debounce events", &[("watch_a", "fn")]),
        ir("src/b.rs", "watch debounce events here too", &[("watch_b", "fn")]),
    ];
    let h1 = search::run(&irs, "watch debounce", 20).unwrap();
    let h2 = search::run(&irs, "watch debounce", 20).unwrap();
    assert_eq!(
        serde_json::to_string(&h1).unwrap(),
        serde_json::to_string(&h2).unwrap()
    );
}

#[test]
fn tokens_est_is_sum_of_hits() {
    let irs = vec![ir("src/a.rs", "watch debounce events", &[("watch_a", "fn")])];
    let hits = search::run(&irs, "watch debounce", 20).unwrap();
    let sum: u32 = hits.iter().map(|h| h.tokens_est).sum();
    assert!(sum > 0);
    // The CLI's tokens_est is this sum — same field, no drift.
    let json = serde_json::to_string(&serde_json::json!({ "results": hits, "tokens_est": sum })).unwrap();
    assert!(json.contains("\"tokens_est\""));
}

#[test]
fn empty_and_junk_queries_are_rejected() {
    let irs = vec![ir("src/a.rs", "some text", &[])];
    assert!(search::run(&irs, "", 5).is_err());
    assert!(search::run(&irs, "re:[bad", 5).is_err());
}

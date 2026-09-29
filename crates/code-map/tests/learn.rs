//! Learning-loop tests: fold math (promote/demote/neutral), holdout
//! flip-drop, lexicon round-trip, search reordering via boosts, mark/join
//! normalization, and trail writing from a mock gate. All filesystem
//! effects go through temp paths.

use std::path::PathBuf;

use code_map::lexicon::{self, Lexicon};
use serde_json::json;

fn temp(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("code-map-learn-{}-{}", std::process::id(), label))
}

/// Write `groups` trail rows (query qi, one candidate with `term`) and usage
/// rows marking `used_of` of them as used.
fn write_corpus(label: &str, groups: usize, term: &str, used_of: usize) -> (PathBuf, PathBuf) {
    let trails = temp(label).join("trails.jsonl");
    let usage = temp(label).join("usage.jsonl");
    std::fs::create_dir_all(temp(label)).unwrap();
    let mut t = String::new();
    let mut u = String::new();
    for i in 0..groups {
        let path = format!("src/f{i}.rs");
        t += &format!(
            "{{\"query\":\"q{i}\",\"candidates\":[{{\"path\":\"{path}\",\"terms\":[\"{term}\",\"shared\"]}}],\"values\":[0.9]}}\n"
        );
        // Every query gets a usage row — an empty "used" list is explicit
        // negative feedback, whereas a missing row means "unknown".
        let used: Vec<String> = if i < used_of {
            vec![path.clone()]
        } else {
            vec![]
        };
        u += &format!(
            "{{\"query\":\"q{i}\",\"used\":{}}}\n",
            serde_json::to_string(&used).unwrap()
        );
    }
    std::fs::write(&trails, t).unwrap();
    std::fs::write(&usage, u).unwrap();
    (trails, usage)
}

#[test]
fn promote_demote_and_neutral_terms() {
    // "hot": 12 groups (5 held out), 10 used → learn precision 1.0 on 7
    // samples (≥ min_samples), holdout 3/5 ≥ 0.5 → promote.
    let (trails, usage) = write_corpus("promote", 12, "hot", 10);
    let lex = lexicon::learn(&trails, &usage, 0.4, 5).unwrap();
    assert!(
        lex.promote.contains_key("hot"),
        "promote: {:?}",
        lex.promote
    );

    // "cold": 12 groups, 0 used → precision 0.0 ≤ 0.3 → demote.
    let (trails, usage) = write_corpus("demote", 12, "cold", 0);
    let lex = lexicon::learn(&trails, &usage, 0.4, 5).unwrap();
    assert!(lex.demote.contains_key("cold"), "demote: {:?}", lex.demote);

    // "thin": 3 groups only → below min_samples → neutral (absent).
    let (trails, usage) = write_corpus("thin", 3, "thin", 3);
    let lex = lexicon::learn(&trails, &usage, 0.4, 5).unwrap();
    assert!(!lex.promote.contains_key("thin"));
    assert!(!lex.demote.contains_key("thin"));
    assert_eq!(lex.stats.get("groups"), Some(&3));
}

#[test]
fn holdout_flip_drops_rule() {
    // 10 sorted groups: q00..q05 used (learn split), q06..q09 not used
    // (holdout). Learn precision 1.0, holdout precision 0.0 → dropped.
    let trails = temp("flip").join("trails.jsonl");
    let usage = temp("flip").join("usage.jsonl");
    std::fs::create_dir_all(temp("flip")).unwrap();
    let mut t = String::new();
    let mut u = String::new();
    for i in 0..10 {
        let path = format!("src/g{i}.rs");
        t += &format!(
            "{{\"query\":\"q{i:02}\",\"candidates\":[{{\"path\":\"{path}\",\"terms\":[\"shifty\"]}}],\"values\":[0.9]}}\n"
        );
        // Explicit feedback on every query: used for q00–q05, empty after.
        let used: Vec<String> = if i < 6 { vec![path.clone()] } else { vec![] };
        u += &format!(
            "{{\"query\":\"q{i:02}\",\"used\":{}}}\n",
            serde_json::to_string(&used).unwrap()
        );
    }
    std::fs::write(&trails, t).unwrap();
    std::fs::write(&usage, u).unwrap();

    let lex = lexicon::learn(&trails, &usage, 0.4, 5).unwrap();
    assert!(
        !lex.promote.contains_key("shifty"),
        "rule that flips on holdout must be dropped, got {:?}",
        lex.promote
    );
}

#[test]
fn lexicon_json_roundtrip_and_boost() {
    let mut lex = Lexicon::default();
    lex.version = lexicon::LEXICON_VERSION;
    lex.promote.insert("good".into(), 1.0);
    lex.demote.insert("bad".into(), 1.0);
    let path = temp("roundtrip").join("gate-lexicon.json");
    lex.save(&path).unwrap();
    let loaded = Lexicon::load(&path).unwrap().unwrap();
    assert_eq!(loaded.version, lexicon::LEXICON_VERSION);
    assert!(loaded.boost_for(&["good".into()]) > 0.0);
    assert!(loaded.boost_for(&["bad".into()]) < 0.0);
    assert_eq!(loaded.boost_for(&["unmeasured".into()]), 0.0);
    assert!(loaded.boost_for(&["good".into(), "bad".into()]) == 0.0); // clamps to 0
    assert!(Lexicon::load(&temp("roundtrip").join("missing.json"))
        .unwrap()
        .is_none());
}

#[test]
fn boost_reorders_search_results() {
    use code_map::search;
    use code_parser_ir::*;
    fn ir(path: &str, card: &str, syms: &[(&str, &str)]) -> FileParseIR {
        let mut ir = FileParseIR::empty(path, "Rust");
        ir.content_hash = path.into();
        ir.retrieval_card = RetrievalCard {
            card_version: 1,
            est_tokens: 8,
            text: card.into(),
        };
        for &(qn, sig) in syms {
            ir.symbols.push(SymbolIR {
                local_key: qn.into(),
                name: qn.into(),
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
            ir.symbol_cards.push(RetrievalCard {
                card_version: 1,
                est_tokens: 4,
                text: format!("SYM {qn}"),
            });
        }
        ir
    }
    let irs = vec![
        ir(
            "src/a_module.rs",
            "alpha handling module",
            // Symbol names deliberately avoid the query term — this test
            // isolates card-term ranking + the lexicon flip. (Since code-aware
            // tokenization, a symbol named `alpha_*` would IDF-match `alpha`
            // directly and outrank any card boost.)
            &[("weak_handler", "fn")],
        ),
        ir(
            "src/z_module.rs",
            "alpha handling module ingest pipeline",
            &[("strong_pipeline", "fn")],
        ),
    ];
    let mut lex = Lexicon::default();
    lex.promote.insert("ingest".into(), 1.0);

    let plain = search::run(&irs, "alpha", 10).unwrap();
    let boosted = search::run_with_lexicon(&irs, "alpha", 10, Some(&lex)).unwrap();
    // Tied by IDF/fuzzy — the path tie-break favors a_module; the lexicon
    // boost must flip the order toward the promoted unit.
    assert_eq!(plain[0].path, "src/a_module.rs");
    assert_eq!(boosted[0].path, "src/z_module.rs");
}

#[test]
fn mark_and_join_normalize_query_text() {
    let dir = temp("join");
    let trails = dir.join("trails.jsonl");
    let usage = dir.join("usage.jsonl");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        &trails,
        "{\"query\":\"Watch  Debounce!\",\"candidates\":[{\"path\":\"src/w.rs\",\"terms\":[\"watch\",\"debounce\"]}],\"values\":[0.9]}\n",
    )
    .unwrap();
    // Messy but equivalent query text — normalization must join it.
    std::fs::write(
        &usage,
        "{\"query\":\"  watch   debounce \",\"used\":[\"src/w.rs\"]}\n",
    )
    .unwrap();

    let lex = lexicon::learn(&trails, &usage, 0.4, 1).unwrap();
    // min_samples=1, precision 1.0, holdout empty → promoted.
    assert!(lex.promote.contains_key("watch"));
    assert!(lex.promote.contains_key("debounce"));
}

#[test]
#[cfg(feature = "typesafe")]
fn gate_writes_trail_row_with_terms() {
    // HOME pointed at a temp dir so trail paths land in a sandbox
    // (and the developer's real key is never consulted).
    let home = temp("gate-home");
    std::fs::create_dir_all(&home).unwrap();
    let prev_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", &home);

    fn mock(_payload: &serde_json::Value, _key: &str) -> anyhow::Result<serde_json::Value> {
        Ok(json!({ "answers": { "q0": { "noul": 0.9 }, "scope0": { "noul": 0.8 } } }))
    }
    let input = code_map::typesafe::GateInput {
        query: "telemetry export".into(),
        candidates: vec![code_map::typesafe::Candidate {
            path: "src/telemetry.rs".into(),
            line: 1,
            kind: "file".into(),
            name: "src/telemetry.rs".into(),
            content_hash: "h1".into(),
            card: "FILE path=src/telemetry.rs sends metrics abroad".into(),
        }],
    };
    let out = code_map::typesafe::gate(input, "k", mock, true).unwrap();
    assert_eq!(out.len(), 1);

    let trails = std::fs::read_to_string(home.join(".cache/code-parser/trails.jsonl")).unwrap();
    let row: serde_json::Value = serde_json::from_str(trails.lines().next().unwrap()).unwrap();
    assert_eq!(row["query"], "telemetry export");
    assert_eq!(row["candidates"][0]["path"], "src/telemetry.rs");
    let terms = row["candidates"][0]["terms"].as_array().unwrap();
    assert!(terms.iter().any(|t| t == "telemetry"));
    assert_eq!(row["values"][0], json!(0.8)); // min(0.9, 0.8)

    // And learn() can fold that trail (no usage row yet → no groups).
    let lex = code_map::lexicon::learn(
        &home.join(".cache/code-parser/trails.jsonl"),
        &home.join(".cache/code-parser/usage.jsonl"),
        0.4,
        5,
    )
    .unwrap();
    assert_eq!(lex.stats.get("groups"), Some(&0));

    // restore
    match prev_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}

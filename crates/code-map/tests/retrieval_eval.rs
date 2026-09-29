//! E2 retrieval relevance eval — engine vs. keyword baseline.
//!
//! Dataset: `eval/retrieval-queries.jsonl` (`eval_version: 1`).
//! Protocol, metrics, and gate semantics are documented in `eval/README.md`.
//!
//! Run verbosely:
//!   cargo test -p code-map --test retrieval_eval -- --nocapture

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use code_parser_core::language::Language;
use code_parser_ir::FileParseIR;

/// Dataset version — bump when queries/gold change materially, and note it
/// in eval/README.md. Consumers that trend results key on this.
const EVAL_VERSION: u32 = 1;
const MIN_QUERIES: usize = 20;
const TOP_K: usize = 10;
/// Hits requested before deduping to file order (search ranks symbol units;
/// an agent reads each file once, so first mention defines file rank).
const HIT_WINDOW: usize = 30;

struct QueryCase {
    query: String,
    gold: Vec<String>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn load_queries() -> Vec<QueryCase> {
    let path = repo_root().join("eval/retrieval-queries.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let mut cases = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("bad JSONL line: {e}\n  {line}"));
        cases.push(QueryCase {
            query: v["query"].as_str().expect("query").to_string(),
            gold: v["gold"]
                .as_array()
                .expect("gold array")
                .iter()
                .map(|g| g.as_str().expect("gold str").to_string())
                .collect(),
        });
    }
    cases
}

/// Search hits → ranked files: dedupe preserving first-mention order, cap at k.
fn engine_files(irs: &[FileParseIR], query: &str, k: usize) -> Vec<String> {
    let hits = code_map::search::run(irs, query, HIT_WINDOW)
        .unwrap_or_else(|e| panic!("search rejected query {query:?}: {e}"));
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for h in &hits {
        if seen.insert(h.path.clone()) {
            files.push(h.path.clone());
        }
    }
    files.truncate(k);
    files
}

/// Keyword baseline (grep floor): term-frequency ranking over raw source.
fn baseline_files(irs: &[FileParseIR], root: &Path, query: &str, k: usize) -> Vec<String> {
    let terms: Vec<String> = code_map::search::tokenize(query);
    let mut scored: Vec<(u64, String)> = Vec::new();
    for ir in irs {
        let raw = std::fs::read_to_string(root.join(&ir.path))
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", ir.path));
        let hay = raw.to_lowercase();
        let score: u64 = terms
            .iter()
            .map(|t| count_occurrences(&hay, t) as u64)
            .sum();
        scored.push((score, ir.path.clone()));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(k).map(|(_, p)| p).collect()
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.match_indices(needle).count()
}

fn recall_at_k(gold: &[String], ranked: &[String], k: usize) -> f64 {
    let top: HashSet<&String> = ranked.iter().take(k).collect();
    let hit = gold.iter().filter(|g| top.contains(g)).count();
    hit as f64 / gold.len() as f64
}

fn mrr_at_k(gold: &[String], ranked: &[String], k: usize) -> f64 {
    for (i, path) in ranked.iter().take(k).enumerate() {
        if gold.contains(path) {
            return 1.0 / (i + 1) as f64;
        }
    }
    0.0
}

fn first_gold_rank(gold: &[String], ranked: &[String]) -> String {
    ranked
        .iter()
        .position(|p| gold.contains(p))
        .map(|i| format!("#{}", i + 1))
        .unwrap_or_else(|| "—".to_string())
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len() as f64
}

#[test]
fn retrieval_eval_v1_engine_vs_keyword_baseline() {
    let root = repo_root();

    // Corpus: this repo's Rust sources, pinned Rust-only for determinism
    // across feature builds. Our own repo must parse clean (dogfood gate).
    let results =
        code_parser_core::parse_repo(&root, Some(vec![Language::Rust])).expect("parse repo");
    for r in &results {
        assert!(
            r.errors.is_empty(),
            "dogfood gate: {} failed to parse: {:?}",
            r.ir.path,
            r.errors
        );
    }
    let irs: Vec<FileParseIR> = results.into_iter().map(|r| r.ir).collect();

    let queries = load_queries();
    assert!(
        queries.len() >= MIN_QUERIES,
        "dataset shrunk below {MIN_QUERIES}: {} queries — did eval/retrieval-queries.jsonl lose lines?",
        queries.len()
    );

    // Dataset sanity: every gold path must exist in the corpus — gold that
    // points at a moved file is stale dataset, not a search failure.
    let corpus: HashSet<&String> = irs.iter().map(|ir| &ir.path).collect();
    for c in &queries {
        for g in &c.gold {
            assert!(
                corpus.contains(g),
                "stale gold: {g} is not in the corpus (code moved? update eval/retrieval-queries.jsonl)"
            );
        }
    }

    println!(
        "E2 retrieval eval v{EVAL_VERSION} — {} queries, {} files",
        queries.len(),
        irs.len()
    );
    println!("{:<58} {:>10} {:>10}", "query", "engine", "grep");

    let mut eng_r5 = Vec::new();
    let mut eng_r10 = Vec::new();
    let mut eng_mrr = Vec::new();
    let mut base_r5 = Vec::new();
    let mut base_r10 = Vec::new();
    let mut base_mrr = Vec::new();

    for c in &queries {
        let eng = engine_files(&irs, &c.query, TOP_K);
        let base = baseline_files(&irs, &root, &c.query, TOP_K);

        eng_r5.push(recall_at_k(&c.gold, &eng, 5));
        eng_r10.push(recall_at_k(&c.gold, &eng, TOP_K));
        eng_mrr.push(mrr_at_k(&c.gold, &eng, TOP_K));
        base_r5.push(recall_at_k(&c.gold, &base, 5));
        base_r10.push(recall_at_k(&c.gold, &base, TOP_K));
        base_mrr.push(mrr_at_k(&c.gold, &base, TOP_K));

        let label: String = c.query.chars().take(56).collect();
        println!(
            "{:<58} {:>10} {:>10}",
            label,
            first_gold_rank(&c.gold, &eng),
            first_gold_rank(&c.gold, &base)
        );
    }

    let (er5, er10, emrr) = (mean(&eng_r5), mean(&eng_r10), mean(&eng_mrr));
    let (br5, br10, bmrr) = (mean(&base_r5), mean(&base_r10), mean(&base_mrr));
    println!();
    println!("           recall@5  recall@10   MRR@10");
    println!("engine    {:>9.3} {:>10.3} {:>8.3}", er5, er10, emrr);
    println!("grep      {:>9.3} {:>10.3} {:>8.3}", br5, br10, bmrr);

    // Gate 1: the engine must not lose to keyword search — that is the
    // bar a retrieval layer has to clear to justify existing.
    assert!(
        er10 >= br10 && emrr >= bmrr,
        "engine lost to the grep baseline: engine (R@10 {er10:.3}, MRR {emrr:.3}) vs baseline (R@10 {br10:.3}, MRR {bmrr:.3})"
    );
    // Gate 2: ratchet floors — v1 measured level (R@5 0.917, R@10 0.917,
    // MRR@10 0.712 vs grep 0.667/0.875/0.607), rounded down. Tighten when a
    // change improves the numbers; never loosen to make a failing run pass.
    assert!(
        er5 >= 0.85,
        "engine Recall@5 {er5:.3} below the 0.85 ratchet floor"
    );
    assert!(
        er10 >= 0.90,
        "engine Recall@10 {er10:.3} below the 0.90 ratchet floor"
    );
    assert!(
        emrr >= 0.70,
        "engine MRR@10 {emrr:.3} below the 0.70 ratchet floor"
    );
}

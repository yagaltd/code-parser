//! `code-map search` — rank map content for a query.
//!
//! Deterministic pipeline: units (files + symbols) scored by IDF-weighted
//! term overlap over card text, plus fuzzy match on symbol names, combined
//! `0.6·idf + 0.4·fuzzy`. `re:` queries switch to plain regex filtering with
//! fixed score. Ties break by (path, name, line) — same map, same output.

use std::collections::HashMap;

use anyhow::{bail, Result};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use regex::Regex;
use serde::Serialize;

use code_parser_ir::FileParseIR;

const W_IDF: f64 = 0.6;
const W_FUZZY: f64 = 0.4;
/// Skim scores roughly scale with pattern length; ~12 pts per char for
/// consecutive matches. Normalize against a generous cap.
const FUZZY_NORM_DIVISOR: f64 = 12.0;

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub path: String,
    pub line: u32,
    pub kind: String,
    pub name: String,
    pub score: f64,
    pub content_hash: String,
    pub tokens_est: u32,
    pub card: String,
}

struct Unit {
    path: String,
    line: u32,
    kind: String,
    name: String,
    hash: String,
    terms: Vec<String>,
    /// Fuzzy target — qualified name for symbols, path for files.
    target: String,
    card: String,
    est_tokens: u32,
}

pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

fn units(irs: &[FileParseIR]) -> Vec<Unit> {
    let mut out = Vec::new();
    for ir in irs {
        let mut file_terms = tokenize(&ir.retrieval_card.text);
        out.push(Unit {
            path: ir.path.clone(),
            line: 0,
            kind: "file".into(),
            name: ir.path.clone(),
            hash: ir.content_hash.clone(),
            terms: file_terms.clone(),
            target: ir.path.clone(),
            card: ir.retrieval_card.text.clone(),
            est_tokens: ir.retrieval_card.est_tokens,
        });
        for (i, sym) in ir.symbols.iter().enumerate() {
            let mut text = sym.qualified_name.clone();
            if let Some(sig) = &sym.signature {
                text.push(' ');
                text.push_str(sig);
            }
            if let Some(doc) = &sym.docstring {
                text.push(' ');
                text.push_str(doc);
            }
            if let Some(card) = ir.symbol_cards.get(i) {
                text.push(' ');
                text.push_str(&card.text);
            }
            file_terms.extend(tokenize(&text));
            out.push(Unit {
                path: ir.path.clone(),
                line: sym.start_line,
                kind: sym.kind.as_str().into(),
                name: sym.qualified_name.clone(),
                hash: ir.content_hash.clone(),
                terms: tokenize(&text),
                target: sym.qualified_name.clone(),
                card: ir
                    .symbol_cards
                    .get(i)
                    .map(|c| c.text.clone())
                    .unwrap_or_default(),
                est_tokens: ir.symbol_cards.get(i).map(|c| c.est_tokens).unwrap_or(0),
            });
        }
        // file_terms folded back into the file unit: symbols describe the file.
        if let Some(u) = out
            .iter_mut()
            .find(|u| u.path == ir.path && u.kind == "file")
        {
            u.terms.append(&mut file_terms);
            u.terms.sort();
            u.terms.dedup();
        }
    }
    out
}

/// IDF-weighted term overlap: rare query terms weigh more than common ones.
fn idf_score(
    unit_terms: &[String],
    query_terms: &[String],
    df: &HashMap<String, usize>,
    total: usize,
) -> f64 {
    let idf = |t: &str| (1.0 + total as f64 / (1.0 + df.get(t).copied().unwrap_or(0) as f64)).ln();
    let denom: f64 = query_terms.iter().map(|t| idf(t)).sum();
    if denom <= 0.0 {
        return 0.0;
    }
    let matched: f64 = query_terms
        .iter()
        .filter(|t| unit_terms.iter().any(|u| u == *t))
        .map(|t| idf(t))
        .sum();
    matched / denom
}

/// Run a search. `n` caps results; determinism: sort (score desc, path, name, line).
pub fn run(irs: &[FileParseIR], query: &str, n: usize) -> Result<Vec<Hit>> {
    run_with_lexicon(irs, query, n, None)
}

/// Same, with a learned lexicon applied on top (consumed by default by the
/// CLI when `~/.config/code-parser/learned/gate-lexicon.json` exists).
pub fn run_with_lexicon(
    irs: &[FileParseIR],
    query: &str,
    n: usize,
    lex: Option<&crate::lexicon::Lexicon>,
) -> Result<Vec<Hit>> {
    let query = query.trim();
    if query.is_empty() {
        bail!("empty query");
    }

    // Regex mode: `re:<pattern>` — fixed score, no ranking heuristics.
    if let Some(pattern) = query.strip_prefix("re:") {
        let re = Regex::new(pattern).map_err(|e| anyhow::anyhow!("bad regex: {e}"))?;
        let mut hits: Vec<Hit> = units(irs)
            .into_iter()
            .filter(|u| re.is_match(&u.target) || re.is_match(&u.card))
            .map(|u| Hit {
                path: u.path,
                line: u.line,
                kind: u.kind,
                name: u.name,
                score: 1.0,
                content_hash: u.hash,
                tokens_est: u.est_tokens,
                card: u.card,
            })
            .collect();
        sort_and_truncate(&mut hits, n);
        return Ok(hits);
    }

    let all = units(irs);
    let query_terms = tokenize(query);
    if query_terms.is_empty() {
        bail!("query has no alphanumeric terms");
    }

    // Document frequency per unit (unique terms per unit).
    let total = all.len();
    let mut df: HashMap<String, usize> = HashMap::new();
    for u in &all {
        let mut seen = u.terms.clone();
        seen.sort();
        seen.dedup();
        for t in seen {
            *df.entry(t).or_default() += 1;
        }
    }

    let matcher = SkimMatcherV2::default();
    let mut hits: Vec<Hit> = all
        .into_iter()
        .map(|u| {
            let idf = idf_score(&u.terms, &query_terms, &df, total);
            let fuzzy = matcher
                .fuzzy_match(&u.target, query)
                .map(|f| ((f.max(0) as f64) / (query.len() as f64 * FUZZY_NORM_DIVISOR)).min(1.0));
            let score = match fuzzy {
                Some(f) => W_IDF * idf + W_FUZZY * f,
                None => W_IDF * idf,
            };
            let score = match lex {
                Some(lx) => (score + lx.boost_for(&u.terms)).clamp(0.0, 1.5),
                None => score,
            };
            Hit {
                path: u.path,
                line: u.line,
                kind: u.kind,
                name: u.name,
                score,
                content_hash: u.hash,
                tokens_est: u.est_tokens,
                card: u.card,
            }
        })
        .filter(|h| h.score > 0.0)
        .collect();
    sort_and_truncate(&mut hits, n);
    Ok(hits)
}

fn sort_and_truncate(hits: &mut Vec<Hit>, n: usize) {
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.line.cmp(&b.line))
    });
    hits.truncate(n);
}

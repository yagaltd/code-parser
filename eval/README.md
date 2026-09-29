# Retrieval relevance eval (E2)

`eval_version: 1`

Measures whether `code-map search` retrieves the right context for an
agent — not just whether it is deterministic or fast. Modeled on
CodeSearchNet (expert-annotated queries over a frozen corpus), methods2test
(mechanical ground truth), and Agent Retrieval Bench (issue → files an
agent must read, scored file-level).

## Dataset

[`retrieval-queries.jsonl`](retrieval-queries.jsonl) — one JSON object per
line:

| Field | Meaning |
|---|---|
| `query` | Agent-style natural-language question |
| `gold` | File(s) an agent must read to answer it (repo-relative) |
| `kind` | `agent` (hand-written); `mechanical` (derived, future) |
| `note` | What the query targets |

Gold is part of the dataset: when code moves, update gold with the move.
That maintenance is the price of a live corpus.

## Corpus

This repository's Rust sources at HEAD, parsed with
`parse_repo(root, [Rust])` — pinned to Rust-only so results are identical
under every feature combination. Includes `fixtures/`, tests, and the
scip-workspace fixture as realistic noise, exactly like a real target repo.

## Protocol

- **Engine**: `code_map::search::run(irs, query, 30)` — plain run, **no
  lexicon** (a developer's locally learned `gate-lexicon.json` must not
  change eval results). Hits are deduplicated to file order
  (first mention wins — mirrors how an agent reads a file once); the top
  10 files score.
- **Baseline (grep floor)**: term-frequency keyword ranking — each query
  term's substring occurrence count summed over raw source bytes, ranked
  desc, ties by path, top 10 files. The engine has no reason to exist if
  it cannot beat this.

## Metrics (file-level)

- **Recall@k** — `|gold ∩ top-k| / |gold|`: did the gold files make the
  context budget?
- **MRR@10** — `1/rank` of the first gold file: how fast does the agent
  land on the right file?
- Macro-averaged over the dataset; per-query table printed by the test
  (`cargo test -p code-map --test retrieval_eval -- --nocapture`).

## Gate

The eval runs as `cargo test -p code-map --test retrieval_eval`:

1. Engine **≥ baseline** on Recall@10 and MRR@10 (the beat-grep bar)
2. **Ratchet floors**: Recall@5 ≥ 0.85, Recall@10 ≥ 0.90, MRR@10 ≥ 0.70
3. Every gold path exists in the corpus, and the dataset has ≥ 20 queries

Floors are the v1 measured level rounded down — a ratchet, not an
aspiration. Tighten when a change improves the numbers; never loosen to
make a failing run pass. Current v1 level (24 queries, 40-file corpus):
engine R@5 0.917 / R@10 0.917 / MRR@10 0.712 vs grep 0.667 / 0.875 / 0.607.

Known gap (kept honest, not hidden): queries whose vocabulary lives only
in file *bodies* (not module docs, symbol names, signatures, or imports)
under-rank — e.g. "tree sitter parse errors diagnostics" misses `parser.rs`
because `errors`/`diagnostics` appear in no indexed text of that file.
Future: index symbol-adjacent comment lines or a body-term sketch.

## Future layers

- **E1** — parser accuracy vs. the `code-parser-scip` oracle
  (rust-analyzer's own resolved bindings: precision/recall of our edges)
- **E3** — TypeSafe gate validation: judge-vs-human agreement on a
  hand-labeled sample, plus gate lift over ungated search on this set
- **E4** — mechanical queries from git history (commit message → changed
  files), the Agent Retrieval Bench pattern, self-updating

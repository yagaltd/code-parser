# Changelog

IR contract history, newest first. The current contract is
[`ir_version: 5`](README.md#ir-shape-ir_version-5). Consumers that persist
parsed output should re-ingest once per version bump (guard on `ir_version`).

## 2026-09-29

- **feat(ir): `ir_version` 5 — module-level `docstring` on `FileParseIR`**
  (additive, `#[serde(default)]` so v4 JSONL maps still load): leading `//!`
  or `///` in Rust, `/** */` in TS/JS, module docstring in Python, markers
  stripped. Cards unchanged; indexed by `code-map search`.

- **feat(code-map): search indexes file docstrings + imports, code-aware
  tokenization** — file units gain the module docstring and import
  specifier terms; `tokenize` now splits snake_case and camelCase
  (`est_tokens` → `est`, `tokens`; `FileParseIR` → `file`, `parse`, `ir`).
  Driven by the E2 eval: engine went from losing to grep on Recall@10
  (0.792 vs 0.833) to R@5 0.917 / R@10 0.917 / MRR@10 0.712 vs grep
  0.667 / 0.875 / 0.607.

- **feat(eval): E2 retrieval relevance eval** — `eval/retrieval-queries.jsonl`
  (24 agent-style queries with gold files, CodeSearchNet-style hand
  annotation), file-level Recall@5/@10 + MRR@10, grep term-frequency
  baseline, ratchet floors, wired as
  `cargo test -p code-map --test retrieval_eval`. See `eval/README.md`.

- **feat(code-map): new crate — query layer over the JSONL map** (M1–M4):
  `refresh` (atomic snapshot),
  `search` (IDF + fuzzy + `re:` regex, deterministic output, `tokens_est`),
  `callers`/`callees`/`path` (BFS over IR edges + conservative globally-unique
  inference tier, marked `inferred`; ambiguity surfaced, never guessed),
  TypeSafe gate (feature `typesafe`: relevance ⊗ scope min-gate, 0.7/0.5/0.25
  thresholds, content-hash verdict cache, interactive `typesafe setup`), and
  the learning loop (`mark` + trails → `learn` → `gate-lexicon.json`,
  holdout/min-samples/flip guardrails, consumed by default by `search`).
- **fix(core): TS e2e import tests gated behind the `typescript` feature** —
  `e2e_parse_repo_fills_resolved_for_alias_fixture` and
  `e2e_nested_tsconfigs_nearest_match_and_extends` failed under default
  features; they now skip cleanly (no contract change).

- **ir_version stays 4** — code-map is a consumer; the IR contract is
  untouched.

## 2026-09-28

- **feat(core): incremental watch stream (`RepoState`)** — `watch` now emits
  an initial snapshot plus delta events (`updated` / `deleted` /
  `batch_end`) with repo-relative paths. Only hash-changed files are
  re-parsed; cross-file resolution is re-run per batch so edges never go
  stale (reverse-dependents re-emit, tombstones on delete, tsconfig
  hot-reload). Fixed a watch-loop liveness bug and failed-parse retry.
- **feat(core): nearest-tsconfig import resolution** — `ImportIR.resolved`
  aliases now come from the nearest `tsconfig.json` on the importing file's
  ancestor chain (`extends` merged, TS ≥ 4.1 `baseUrl` rules). Root-only
  configs behave as before; monorepos resolve per-package.
- **fix(ir): repair stale unit fixture** — the IR test fixture had not been
  updated for later fields and no longer compiled.

## 2026-08-18

- **feat(scip): `code-parser-scip` crate** — out-of-process
  `rust-analyzer scip` → resolved call-bindings artifact (definition-position
  matching, cargo-fingerprint cache, trait-dispatch impl candidates).
- **feat(core): per-file hardening** — extraction caps
  (4 096 symbols / 8 192 calls / 2 048 imports per file, truncation
  Warnings) and a 4 MiB collection size gate (diagnostic-only IRs, never
  `Err`).
- **feat(core): TS/JS import specifier resolution** — `ImportIR.resolved`
  (new, serde-default `None`) filled by the repo pass: relative paths,
  extension/index probing, tsconfig `baseUrl`/`paths`.
- **fix(core): module-qualified Rust symbol keys** — `mod a { fn foo() }`
  emits `local_key: "a::foo"`; same-file module collisions impossible;
  in-file resolution is scope-aware. **Bumped `ir_version` 3 → 4.**
- **fix(core): Rust `use` globs + renames** emitted as imports.
- **fix(core): parse diagnostics wired into all four extractors** —
  tree-sitter ERROR regions → `Error`, `missing` markers → `Warning`,
  bounded at 64 per file.

## 2026-08-03

- **feat(ir,core): `is_test` flag** — marks test code per language
  (attributes/mods/files). **Bumped `ir_version` 2 → 3.**

## 2026-07-30

- **feat: retrieval cards in IR** — `retrieval_card` + index-aligned
  `symbol_cards` become required fields, always built after
  extract+resolve; card-aware golden comparison. **Bumped
  `ir_version` 1 → 2.**

## 2026-07-29

- Initial release: tree-sitter engine producing `FileParseIR`
  (symbols, calls, imports, diagnostics), Rust/TS/JS/Python extractors,
  `parse` / `parse-repo` / `check` CLI, hash cache, golden fixtures.

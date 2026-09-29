# Changelog

IR contract history, newest first. The current contract is
[`ir_version: 4`](README.md#ir-shape-ir_version-4). Consumers that persist
parsed output should re-ingest once per version bump (guard on `ir_version`).

## 2026-09-29

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

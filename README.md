# code-parser

Tree-sitter engine that parses source files into versioned `FileParseIR` — symbols, calls, imports, diagnostics, and **always-present mechanical retrieval cards** — for consumption by downstream code indexes.

Parser always emits cards. The consumer chooses whether to store, lex, embed, or ignore them (ingest policy). There is no "parse without cards" mode.

Ships with **`code-map`**: a zero-infra query layer over the JSONL map — keyword search, call-graph queries, and an optional TypeSafe gate with a self-learning lexicon. See [Code map](#code-map-code-map).

Requires Rust **1.82+**. Contract history: [CHANGELOG](CHANGELOG.md).

## Contents

- [Components](#components)
- [Supported languages](#supported-languages)
- [Install](#install)
- [Quick start](#quick-start)
- [IR shape](#ir-shape-ir_version-5)
- [Retrieval cards](#retrieval-cards)
- [Code map (`code-map`)](#code-map-code-map)
- [Evaluation](#evaluation)
- [Library usage](#library-usage)
- [Watch stream (incremental updates)](#watch-stream-incremental-updates)
- [Resolution](#resolution)
- [Tests](#tests)
- [Hardening](#hardening-per-file-caps--size-gate)
- [License](#license)

## Components

- `crates/code-parser-ir` — IR types + serde + card builders (zero heavy deps)
- `crates/code-parser-core` — tree-sitter engine: parse, extract, resolve, cards, watch, hash cache
- `crates/code-parser-scip` — SCIP overlay producer: `rust-analyzer scip` out-of-process → resolved call bindings (cache by cargo fingerprint; committed fixture artifacts)
- `crates/code-parser-cli` — thin CLI (`parse`, `parse-repo`, `watch`, `check`)
- `crates/code-map` — query layer over the JSONL map: `refresh`, `search`, `callers`/`callees`/`path`, optional TypeSafe gate + learning loop

## Supported languages

| Language | Feature flag | Extensions | Grammar crate |
|----------|-------------|------------|---------------|
| Rust | `rust` (default) | `.rs` | `tree-sitter-rust` |
| TypeScript | `typescript` | `.ts`, `.tsx` | `tree-sitter-typescript` |
| JavaScript | `javascript` | `.js`, `.jsx`, `.mjs`, `.cjs` | `tree-sitter-javascript` |
| Python | `python` | `.py`, `.pyi` | `tree-sitter-python` |

Each language is feature-gated. Build only what you need. The optional `watcher`
feature enables the `watch` subcommand (`notify` backend); only `rust` is on
by default.

## Install

From a checkout:

```bash
cargo install --path crates/code-parser-cli --features all
```

From GitHub:

```bash
cargo install --git https://github.com/yagaltd/code-parser code-parser-cli --features all
```

The command is `code-parser`. `--features all` = Rust + TypeScript + JavaScript + Python + watcher; default is Rust only.

The map query tool installs separately:

```bash
cargo install --path crates/code-map --features typesafe   # or omit the feature for no network deps
```

The command is `code-map`.

## Quick start

```bash
# Build with everything
cargo build --features all

# Parse a single file → JSON (includes retrieval_card + symbol_cards)
cargo run -- parse src/main.rs --json

# Print only the file retrieval card (debug)
cargo run -- parse src/main.rs --card-only

# Parse a whole repo → newline-delimited JSON
cargo run -- parse-repo . --languages rust --jsonl

# Watch a repo: initial snapshot, then incremental JSONL events
# (requires --features watcher)
cargo run --features all -- watch . --emit jsonl

# Validate a file parses without errors
cargo run -- check src/main.rs

# Build the map query tool (TypeSafe gate optional)
cargo build --release -p code-map --features typesafe

# Snapshot the repo to a queryable map (atomic swap, hash-cache fast)
cargo run --release -p code-map -- refresh . -o code-map.jsonl

# Query it: search + call graph — no server, no index
cargo run --release -p code-map -- search "watch debounce" -m code-map.jsonl
cargo run --release -p code-map -- callers parse_repo -m code-map.jsonl
```

## IR shape (`ir_version`: 5)

One JSON object per file. Cards are required fields built after extract+resolve:

```json
{
  "ir_version": 5,
  "path": "src/main.rs",
  "language": "Rust",
  "content_hash": "<blake3>",
  "byte_len": 128,
  "line_count": 10,
  "docstring": "Module doc, markers stripped (v5; null when absent)",
  "symbols": [{ "local_key": "main", "name": "main", "kind": "Function", "start_line": 1, "is_test": false }],
  "calls":    [{ "caller_local_key": "main", "callee_name": "println", "line": 2 }],
  "imports":  [{ "import_name": "HashMap", "target_module": "std::collections", "kind": "Named" }],
  "diagnostics": [],
  "retrieval_card": {
    "card_version": 1,
    "est_tokens": 42,
    "text": "FILE path=src/main.rs lang=Rust lines=10 hash=abcd1234\nIMPORTS …\nSYMBOLS\n  …"
  },
  "symbol_cards": [
    { "card_version": 1, "est_tokens": 12, "text": "SYM path=src/main.rs qname=main kind=Function L1-3\n…" }
  ]
}
```

Invariant: `symbol_cards.len() == symbols.len()`.

`diagnostics` is populated on broken input (never dead code): each maximal
tree-sitter ERROR region yields one `Error` diagnostic with its line and byte
span; `missing` markers yield `Warning` diagnostics. Bounded at 64 entries
per file — further parse issues collapse into one overflow `Warning`
(`MAX_DIAGNOSTICS_PER_FILE` in `extractors/utils.rs`).

`SymbolIR.is_test` (serde-default false) marks test code so consumers can
answer "which tests cover this symbol". Detection per language:

- **Rust** — `#[test]` / `#[cfg(test)]` attributes, symbols inside `mod tests`,
  files under a `tests/` dir or named `tests.rs`
- **Python** — `test_*` / `Test*` names (methods inherit from their `Test*`
  class), `test_*.py` files, `tests/`/`test/` dirs
- **TS/JS** — `.test.` / `.spec.` filenames, `__tests__/` / `test/` / `tests/` dirs

The parser is stateless: it never removes or versions anything — `is_test` is
computed per snapshot; all state (mirrors, sweeps, parity) lives in the
consumer's store.

Rust symbol keys are module-qualified — `mod a { fn foo() }`
emits `local_key: "a::foo"` (structs/enums/traits/impl-methods likewise, e.g.
`a::S::m`), so same-file collisions between modules are impossible and in-file
resolution is scope-aware (caller's module prefix first, then file root).
Consumers that persist parsed output should re-ingest once per `ir_version`
bump (guard on the field).

`ImportIR.resolved` (serde-default `None`): filled by the repo-level
pass in `parse_repo` for TS/JS imports whose specifier maps to a real repo
file — relative paths (`./` `../`) normalized against the importing file,
extension probing (`.ts` `.tsx` `.js` `.jsx`, then `/index.*`), and
`tsconfig.json` `baseUrl`/`paths` aliases (`~/*` etc.). Aliases come from
the **nearest** `tsconfig.json` on the importing file's ancestor chain
(monorepo / project-references model), with `extends` chains merged — see
[Resolution](#resolution). Resolution lives in `imports.rs`
(`TsConfigSet` / `resolve_import` / `resolve_import_paths`).

See `fixtures/*/simple.ir.json` for golden examples of the full contract.

## Retrieval cards

Mechanical, deterministic text for lexical search / optional later embeddings:

- **File card** — path, lang, hash, imports, symbol outline, call histogram, pub names (capped ~2k chars)
- **Symbol card** — one per symbol: qname, kind, lines, sig, doc snippet, local calls

The consumer maps these onto its own payloads/indexes under its ingest policy
(store / lex / embed / ignore). The parser knows nothing about the consumer's
node model.

## Code map (`code-map`)

A zero-infra query layer over the map: every command loads a `code-map.jsonl`
(the `parse-repo --jsonl` output), answers, exits. No store, no server, no
index — grep-scale fast, stateless by design.

```bash
code-map refresh . -o code-map.jsonl                # atomic snapshot (hash-cache fast)
code-map search "watch debounce" -n 20              # IDF + fuzzy over cards/symbols
code-map search "re:callee_file.*tsconfig" --json   # regex mode; --json adds tokens_est
code-map callers parse_repo                         # reverse call edges
code-map path main collect_source_files             # shortest path (BFS, hop-capped)
```

- **Search** ranks card/symbol text: rare-query-term weighting (IDF) plus
  fuzzy on symbol names, deterministic tie-breaking (same map → same output).
  File units additionally index the module docstring and import specifiers
  (v5), and tokenization is code-aware (`est_tokens` → `est`, `tokens`;
  `FileParseIR` → `file`, `parse`, `ir`).
- **Graph** resolves the edges the IR already carries (in-file
  `callee_local_key`, cross-file `callee_file` + exact `qualified_name`) and
  adds a conservative inference tier: a cross-crate call like
  `code_parser_core::parse_repo` binds to the unique symbol named
  `parse_repo`, marked `(inferred)`. Ambiguous lookups list every match —
  never guessed.
- **TypeSafe gate** (feature `typesafe`): `search --json | code-map gate
  --query "…"` filters the shortlist with two judgments per candidate
  (relevance ⊗ scope, min-combined); verdicts are `inline` ≥ 0.7, `include`
  ≥ 0.5, `lead` ≥ 0.25, else dropped. One-time setup:
  `code-map typesafe setup` (paste the key; stored chmod 600 at
  `~/.config/code-parser/typesafe.key`; `TYPESAFEAI_API_KEY` env works too).
  Verdicts are cached by content hash — re-gates are API-free.
- **Learning loop**: fresh gates append trails; `code-map mark "<query>"
  <paths…>` records what you actually used; `code-map learn` folds both into
  `~/.config/code-parser/learned/gate-lexicon.json` (holdout-validated,
  min-samples guard, flip-dropped rules) which `search` consumes by default —
  the lexicon answers first, free; the gate judges only the rest.

Files: key `~/.config/code-parser/typesafe.key` · cache
`~/.cache/code-parser/{verdicts,trails,usage}.jsonl` · lexicon
`~/.config/code-parser/learned/gate-lexicon.json`.

## Evaluation

[`eval/`](eval/) holds the retrieval relevance eval (E2): 24 agent-style
queries with gold files over this repo, scored file-level
(Recall@5/@10, MRR@10) against a grep baseline — wired as a cargo test
gate. The eval found and drove the v5 search improvements above. Methodology,
current numbers, and how to extend: [eval/README.md](eval/README.md).

## Library usage

```rust
use code_parser_core::{parse_file, parse_file_bytes, parse_repo, HashCache};

// Single file — IR always includes cards.
let result = parse_file(Path::new("src/main.rs"))?;
println!("{} symbols", result.ir.symbols.len());
println!("{}", result.ir.retrieval_card.text);

// Bytes-in (no filesystem read) — primary entry point for downstream pipelines.
let ir = parse_file_bytes("src/main.rs", &source_bytes)?;

// Whole repo with cross-file resolution.
let results = parse_repo(Path::new("."), None)?;

// Cached: skip re-parse when content hash unchanged.
let mut cache = HashCache::new();
if let Some(result) = parse_file_cached(Path::new("src/main.rs"), &mut cache)? {
    // File changed — process IR.
}
```

## Watch stream (incremental updates)

`watch` emits an **initial snapshot** (every file, same IRs as `parse-repo --jsonl`)
and then **incremental deltas** as files change. The watcher is registered
before the snapshot, so nothing is missed in between. Only hash-changed
files are re-parsed; the cross-file resolution passes (`callee_file`,
`ImportIR.resolved`) are re-run over the whole set each batch, so edges
never go stale — including **reverse-dependents**: renaming a callee
re-emits the callers, deleting an import target re-emits the importer with
`resolved: null`, and editing a `tsconfig.json` re-resolves the importers it
governs (nearest-config hot-reload).

The stream is a JSONL envelope (paths are always repo-relative):

```json
{"event":"updated","ir":{ …FileParseIR… }}
{"event":"deleted","path":"src/foo.rs"}
{"event":"batch_end","batch":7,"changed":3}
```

- `updated` — upsert the IR into your store, keyed by `ir.path`
- `deleted` — remove the file and its edges (tombstone)
- `batch_end` — commit point for one debounced batch (`changed` counts the
  events above it); safe to flush there

Deltas follow the same collection policy as the snapshot: non-source files,
gitignored paths (e.g. `target/`), and paths outside the root are filtered.
One exception: `tsconfig*.json` changes are captured — they re-run import
resolution for the importers they govern (no IR event for the tsconfig
itself). `--debounce-ms` (default 200) tunes the aggregation window.

The same logic is available as a library, no filesystem events required:

```rust
use code_parser_core::{FileChange, RepoState};

let mut state = RepoState::new(Path::new("."));
for event in state.scan(None)? { /* initial snapshot */ }
// on a debounced watcher batch:
let changes = vec![FileChange { path: path_buf, deleted: false }];
for event in state.apply_batch(&changes)? { /* delta */ }
```

## Resolution

- **In-file (scope-aware):** bare name → caller's module scope first (`b::foo`
  for a call inside `mod b`), then file-root symbols (`bar()` → `bar`);
  qualified names (`a::foo`) match exact qualified names. Misses stay
  unresolved, never external.
- **Cross-file:** qualified names with `::` → `callee_file` set to declaring file path
- **External heuristic:** qualified name not found in repo → `callee_external: true`

### TS/JS imports: nearest tsconfig (monorepo-aware)

`ImportIR.resolved` is filled from the **nearest `tsconfig.json`** on the
importing file's ancestor chain — matching how tsserver assigns files to
projects. Nested tsconfigs are the norm (TypeScript project references,
Nx/Turborepo/pnpm workspaces), so a single repo-root config would resolve
nothing in real monorepos.

Semantics (`TsConfigSet` in `imports.rs`):

- **Nearest match** — a config governs every file beneath its directory;
ancestor configs are *not* consulted as alias fallbacks (TS project
semantics). A repo with only a root `tsconfig.json` behaves exactly as
before (the root is the last stop on the chain).
- **`extends` merged** — relative chains only (package-style extends targets
are external). Child overrides; `paths` replace wholesale when declared.
`baseUrl` resolves against the directory of the config that declares it
(TS ≥ 4.1); with no `baseUrl` anywhere, `paths` targets resolve against the
config that declares them.
- Configs with neither `baseUrl` nor `paths` have no alias power and are
skipped — files under them fall through to the next ancestor that has one.
- `baseUrl`/`paths` from configs *above* the repo root still work via the
legacy up-walk fallback when no tsconfig exists inside the root.

Fixtures: `fixtures/ts/alias` (root config), `fixtures/ts/nested`
(monorepo: package `a` extends a shared base with its own `baseUrl`,
package `b` governed by the root config — same `~/*` alias, two different
targets).

In the watch stream, **tsconfig edits hot-reload**: any `tsconfig*.json`
change (including `extends` targets like `tsconfig.base.json`) re-runs
import resolution and re-emits exactly the importers it governs.

Method calls, field chains, and trait resolution are out of scope for V1.

## Tests

```bash
cargo test --features rust,typescript,javascript,python --all
```

Golden fixtures in `fixtures/` are compared structurally on every test run. Volatile fields (byte offsets, columns, content hash) are excluded; cards are rebuilt with hash cleared so FILE `hash=` line is stable.

## Hardening: per-file caps + size gate

Hostile inputs are bounded, never silent:

- `parse_file_bytes` truncates at `MAX_SYMBOLS_PER_FILE = 4096`, `MAX_CALLS_PER_FILE = 8192`, `MAX_IMPORTS_PER_FILE = 2048` and emits one Warning diagnostic per truncated kind (`truncated: {n} symbols (cap 4096)`). Normal files are untouched — parity is guarded by the golden fixtures.
- `collect_source_files` skips files over `MAX_FILE_BYTES = 4 MiB`; `parse_repo` emits a diagnostic-only `FileParseIR::empty` with a Warning (`skipped: {bytes} > cap …`) per skipped file — never `Err`, never invisible.

Caps and the size gate are covered by unit tests in `lib.rs` / `file_collect.rs` (synthetic 5k-fn file, 5 MB repo file, at-cap boundary).

## License

MIT

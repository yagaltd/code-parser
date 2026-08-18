# SCIP spike notes (D0) — rust-analyzer SCIP on this machine

**Verdict: PASS** — all four D0 checks hold. The overlay is implementable with
the invocation below; call sites are distinguishable from type references
(by target-symbol kind, not syntax kind), and method-call occurrences resolve
to impl-method definitions with exact `(file, line)`.

Date: 2026-08-18. Machine: 14 GB RAM, Fedora Linux, rustup 1.29.0, stable
toolchain rustc 1.95.0.

## 1. Invocation

The rust-atlas pattern (agent-spec `crates/rust-atlas/src/lib.rs:3769`,
`generate_scip`): run the rust-analyzer binary with subcommand `scip` and
argument `.`, with the **workspace root as the working directory**; the
artifact is written by rust-analyzer as `index.scip` **into that cwd** and is
relocated/consumed afterwards.

The pre-flight fact was confirmed: the official stable toolchain ships **no**
rust-analyzer binary (`rust-analyzer` is a rustup shim; `rust-analyzer
--version` → `error: Unknown binary 'rust-analyzer' in official toolchain
'stable-x86_64-unknown-linux-gnu'`). The documented install path — the one
rust-atlas's own error message suggests — works:

```
$ rustup component add rust-analyzer     # rustup ≥1.28 distributes the component
$ rust-analyzer --version
rust-analyzer 1.95.0 (5980761 2026-04-14)
$ rust-analyzer scip .                    # cwd = cargo workspace root
$ ls -la index.scip                       # 755 KB (code-parser workspace) / 11 KB (fixture)
```

No custom build from source was needed; no nightly toolchain involvement.

**Toolchain-pinning gotcha (observed 2026-08-18):** the `rust-analyzer` on
PATH is a rustup shim that resolves the toolchain from the **CWD context**
(`rust-toolchain.toml` / `RUSTUP_TOOLCHAIN`). Repos pinning an older
toolchain (CognitiveOS pins `1.91.0`) get `Unknown binary 'rust-analyzer' in
official toolchain '1.91.0-…'` even after `rustup component add rust-analyzer`
for the default toolchain — install the component for the pinned toolchain
too: `rustup component add rust-analyzer --toolchain 1.91.0-x86_64-unknown-linux-gnu`.

## 2. Runtime

| Workspace | Elapsed | Max RSS | Output |
|---|---|---|---|
| code-parser (18 files, 4 crates) | ~4.9 s | 739 MB | 755 513 B |
| scip fixture (3 files) | ~2.8 s | 524 MB | 11 KB |

Output is deterministic: two consecutive runs over the same workspace produce
byte-identical `index.scip` files (verified by sha256). RA prints progress to
stderr (`Loading cargo metadata: finished` / `Generating SCIP finished …`).

## 3. What the artifact contains (protobuf, `scip` crate 0.9 + `protobuf` 3)

- `metadata.tool_info` → `rust-analyzer 1.95.0 (5980761 2026-04-14)`.
- Per document: `occurrences` with `symbol`, `symbol_roles`, `range`, and
  `symbols` with `kind` + `relationships`.
- **Relationships are empty** (RA 1.95, confirming rust-atlas's note for
  1.92): `is_implementation` links are not emitted. Trait-impl candidate sets
  must be recovered from symbol descriptors (below).

### 3a. Definitions vs references — and why call sites are distinguishable

- Definition vs reference: `symbol_roles & 1` (SCIP role bit 1). Defs carry
  the symbol's declaration span; refs carry the identifier span.
- **Syntax kinds are useless**: every occurrence has `syntax_kind = 0`
  (Unspecified) — rust-analyzer does not populate them. Call sites cannot be
  told apart from other references by syntax.
- Call sites ARE distinguishable by **target symbol kind** (from the
  document's `SymbolInformation.kind`):
  `Method`, `StaticMethod`, `TraitMethod`, `Function`, `Macro` → callable
  references; `Struct`, `Trait`, `TypeAlias`, `Field`, `Enum`, `EnumMember`,
  `Variable`, `Parameter`, `TypeParameter` → type/value references (dropped).
  This is exactly rust-atlas's `edge_kind` classification.
- Second disambiguator (used by the binding layer): the occurrence range
  carries the exact callee-name span, and the tree-sitter IR already has the
  call on the same `(file, line)` — position overlap is checked at ingest.
- **Range shape**: RA emits `[line, start_col, end_col]` (3 elements) for
  single-line spans — the trailing end-line is omitted. Full-file spans
  (`crate/`, module headers) have 4 elements. Normalize to `len >= 2`.
- Local variables/params appear as symbols `local N` with no index-level
  definitions — filter on the `local ` prefix.

### 3b. Method calls resolve to impl definitions with (file, line) — CONFIRMED

From the committed fixture (`fixtures/scip/index.scip`), call-site reference →
definition position (all 0-based in the artifact, 1-based in bindings):

| Call site | SCIP symbol (ref) | Def position |
|---|---|---|
| `src/b.rs:4` `A::make(21)` (associated, cross-file) | `a/impl#[A]make().` | `src/a.rs:5` |
| `src/b.rs:5` `a.double()` (method, cross-file) | `a/impl#[A]double().` | `src/a.rs:9` |
| `src/b.rs:5` `a_fn()` (fn, cross-file) | `a/a_fn().` | `src/a.rs:14` |
| `src/lib.rs:45` `Cache::new()` (associated) | `impl#[Cache]new().` | `src/lib.rs:26` |
| `src/lib.rs:46` `c.get("x")` (method) | `impl#[Cache]get().` | `src/lib.rs:30` |
| `src/lib.rs:37` `g.greet()` (`&dyn Greeter`) | `Greeter#greet().` | `src/lib.rs:4` |
| `src/lib.rs:41` `g.greet()` (generic `T: Greeter`) | `Greeter#greet().` | `src/lib.rs:4` |

Receiver-typed calls through a concrete impl (`impl Trait` parameter, e.g.
`RustExtractor` in code-parser's `lib.rs:223`) resolve to the **concrete impl
method** symbol (`impl#[RustExtractor][LanguageExtractor]extract().`) — even
better than the trait symbol.

### 3c. Trait dispatch — CONFIRMED (and how candidates are recovered)

`dyn Trait` and generic receivers both reference the **trait method symbol**
(`Greeter#greet().`, kind `TraitMethod`) — the call is honestly ambiguous.
The impl candidates are recoverable from the same index by descriptor shape:
each `impl Trait for Type` method is its own symbol
`impl#[Type][Trait]name().` (kind `Method`) with its own definition span
(`impl#[English][Greeter]greet().` at `src/lib.rs:9`,
`impl#[German][Greeter]greet().` at `src/lib.rs:16`). With relationships
empty, candidate sets for a trait dispatch site are built by matching the
`[Trait]` slot of impl-method symbols against the referenced trait — a
descriptor-pattern lookup, not a guess. **Artifact consequence (v2):** each
`dispatch: "trait"` binding carries `impl_candidates` — the definition
positions of those impls, recovered exactly this way — so consumers get a
real, ranked Ambiguous candidate set instead of a guess.

### 3d. SCIP symbol grammar notes (RA 1.95)

- Symbol = `rust-analyzer cargo <package> <version> <path>descriptor`.
- Inherent method: `impl#[Type]name().` — parses (via `scip::symbol::parse_symbol`)
  as `Type("impl")` marker + `TypeParameter(impl-type)` + `Method(name)`.
- Trait impl method: `impl#[Type][Trait]name().` — marker + impl-type +
  trait + method. The impl type is the first TypeParameter after the `impl`
  marker.
- Trait method: `Trait#name().` → `Type(trait)` + `Method(name)`.
- Free fn: `path/name().` → `Namespace*` + `Method(name)`.
- Macro: `name!.` → **fails** the strict SCIP parser; string fallback needed.
- Generic impl types carry backticks: ``impl#[`Vec<T>`]new().``.

## 4. Abort criteria — evaluated

| Criterion | Result |
|---|---|
| Call-site occurrences distinguishable from other references? | **Yes** — by target symbol kind (Method/StaticMethod/TraitMethod/Function/Macro) + position overlap with tree-sitter calls; syntax kinds are all Unspecified and unused |
| Artifact lacks definition positions? | **No** — every definition occurrence carries `(file, line, col)`; every callable reference in the fixture has exactly one in-index definition (zero → external, skipped; several → ambiguous, skipped) |
| Toolchain install within bounded effort? | **Yes** — `rustup component add rust-analyzer` (one download), no source build |

## 5. Consequences for D1/D2

1. Bindings carry `(call_file, line, col)` and `(callee_file, callee_line)`,
   1-indexed lines, 0-based column — definition-position matching, never
   textual names.
2. `dispatch: "trait"` bindings must NOT become a single callee: consumers
   turn them into ranked Ambiguous candidate sets (impl methods recovered by
   the `[Trait]` descriptor slot), per the plan.
3. The cargo fingerprint covers source bytes + indexer version + package
   identity so cache reuse is honest; artifact bytes are byte-deterministic,
   so committed fixtures stay stable across re-runs on the same toolchain.

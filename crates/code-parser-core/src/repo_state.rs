//! Incremental repository state — snapshot + watcher deltas with cross-file
//! resolution preserved across incremental updates.
//!
//! [`RepoState`] holds the last-known [`FileParseIR`] per file plus content
//! hashes. [`RepoState::scan`] produces the initial snapshot;
//! [`RepoState::apply_batch`] applies a debounced batch of filesystem events
//! without re-parsing unchanged files, then re-runs the (cheap) cross-file
//! resolution passes over the whole set and emits every file whose IR
//! actually changed — including reverse-dependents whose call/import edges
//! shifted when another file appeared, changed, or vanished.
//!
//! The expensive part (tree-sitter parsing) is strictly incremental; the
//! resolution passes are O(calls + imports) map lookups per batch.
//!
//! Resolution is recomputed from scratch each batch: the cross-file fields
//! (`CallIR.callee_file`, `CallIR.callee_external`, `ImportIR.resolved`) are
//! reset before the passes run, so they are a pure function of the current
//! IR set — no stale edge can survive a delete or rename.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Context;
use code_parser_ir::FileParseIR;
use serde::{Deserialize, Serialize};

use crate::cache::HashCache;
use crate::file_collect;
use crate::hash;
use crate::language::Language;
use crate::{imports, resolve};

// ── Events ───────────────────────────────────────────────────────────────

/// A state change emitted by [`RepoState`] — the delta a consumer applies
/// to its store. Serialized with an `event` tag:
/// `"event":"updated","ir":{…}` / `"event":"deleted","path":"…"`.
// Events are transient one-at-a-time values; boxing the IR variant would
// complicate consumers for no real win.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ChangeEvent {
    /// A file was added or (re)parsed with changes: upsert this IR, keyed
    /// by `ir.path` (repo-relative).
    Updated { ir: FileParseIR },
    /// A previously-indexed file is gone: remove it and its edges from the
    /// store.
    Deleted { path: String },
}

/// One incoming filesystem change, from a watcher or any other source.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Path of the changed file — absolute under the repo root, or already
    /// repo-relative.
    pub path: PathBuf,
    /// True when the last event for this path was a removal.
    pub deleted: bool,
}

// ── RepoState ────────────────────────────────────────────────────────────

/// Incremental repository parse state.
///
/// Typical use: `scan` once for the initial snapshot, then feed debounced
/// watcher batches into `apply_batch` and forward the returned
/// [`ChangeEvent`]s to the store.
pub struct RepoState {
    /// Canonicalized repository root.
    root: PathBuf,
    /// Repo-relative path → last content hash. Updated only after a
    /// successful parse, so failed parses are retried on the next batch.
    hashes: HashCache,
    /// Repo-relative path → last-emitted IR (resolution applied, cards built).
    irs: BTreeMap<String, FileParseIR>,
    /// Repo-relative path → resolution fingerprint at last emission.
    fingerprints: BTreeMap<String, String>,
    /// Gitignore matcher for delta filtering (parity with the gitignore-aware
    /// initial scan).
    ignore_matcher: ignore::gitignore::Gitignore,
    /// Discovered tsconfig set (nearest-match per importing file). Refreshed
    /// on scan and whenever a tsconfig file changes in a batch.
    tsconfigs: crate::imports::TsConfigSet,
}

impl RepoState {
    /// Create empty state for the repository rooted at `root` (canonicalized
    /// internally). Builds the gitignore matcher used to filter deltas.
    pub fn new(root: &Path) -> Self {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let ignore_matcher = build_ignore_matcher(&canonical);
        let tsconfigs = crate::imports::TsConfigSet::discover(&canonical);
        Self {
            root: canonical,
            hashes: HashCache::new(),
            irs: BTreeMap::new(),
            fingerprints: BTreeMap::new(),
            ignore_matcher,
            tsconfigs,
        }
    }

    /// Repository root (canonicalized).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Number of tracked files (last snapshot/delta state).
    pub fn len(&self) -> usize {
        self.irs.len()
    }

    /// True when no files are tracked.
    pub fn is_empty(&self) -> bool {
        self.irs.is_empty()
    }

    /// Build the initial snapshot: parse every source file under `root`
    /// (gitignore-aware, size-gated like `parse_repo`), run the cross-file
    /// passes, and emit one `Updated` event per file. Replaces any previous
    /// state.
    pub fn scan(
        &mut self,
        languages: Option<Vec<Language>>,
    ) -> Result<Vec<ChangeEvent>, anyhow::Error> {
        let languages = languages.unwrap_or_else(default_languages);
        self.hashes.clear();
        self.irs.clear();
        self.fingerprints.clear();
        self.tsconfigs = crate::imports::TsConfigSet::discover(&self.root);

        let root_str = self.root.to_string_lossy().to_string();
        let (paths, skipped) = file_collect::collect_source_files(&root_str, &languages)
            .context("Failed to collect source files")?;

        // Size-gate parity with parse_repo: skipped files get a
        // diagnostic-only IR so they stay visible to consumers.
        for s in &skipped {
            let mut ir = FileParseIR::empty(&s.path, "unknown");
            ir.byte_len = s.byte_len;
            ir.diagnostics.push(code_parser_ir::DiagnosticIR {
                severity: code_parser_ir::DiagnosticSeverity::Warning,
                message: format!(
                    "skipped: {} bytes > cap {} (MAX_FILE_BYTES)",
                    s.byte_len,
                    file_collect::MAX_FILE_BYTES
                ),
                line: None,
                byte_span: None,
            });
            self.irs.insert(s.path.clone(), ir);
        }

        for rel in &paths {
            let full = self.root.join(rel);
            match std::fs::read(&full)
                .map_err(anyhow::Error::from)
                .and_then(|source| {
                    let h = hash::hash_bytes(&source);
                    crate::parse_file_bytes(rel, &source).map(|ir| (ir, h))
                }) {
                Ok((ir, h)) => {
                    self.hashes.update(rel, &h);
                    self.irs.insert(rel.clone(), ir);
                }
                Err(_) => {
                    // Read or parse failure: diagnostic-only IR (never Err),
                    // mirroring parse_repo's error path. Not hash-tracked, so
                    // a later event retries the file.
                    let ir = FileParseIR::empty(rel.as_str(), "unknown");
                    self.irs.insert(rel.clone(), ir);
                }
            }
        }

        self.recompute_resolution();

        let mut events = Vec::with_capacity(self.irs.len());
        for ir in self.irs.values_mut() {
            rebuild_cards(ir);
            let fp = resolution_fingerprint(ir);
            self.fingerprints.insert(ir.path.clone(), fp);
            events.push(ChangeEvent::Updated { ir: ir.clone() });
        }
        Ok(events)
    }

    /// Apply a debounced batch of filesystem changes and return the delta.
    ///
    /// Only hash-changed files are re-parsed. Afterwards the cross-file
    /// resolution passes run over the whole set and every file whose IR
    /// changed — reparsed, or whose edges shifted — is emitted as `Updated`.
    /// Removals of tracked files emit `Deleted` (before any `Updated`, so
    /// consumers drop stale symbol tables before re-ingesting).
    ///
    /// Events for non-source files, gitignored paths, and paths outside the
    /// root are ignored; transient read failures are skipped and retried on
    /// the next batch.
    pub fn apply_batch(
        &mut self,
        changes: &[FileChange],
    ) -> Result<Vec<ChangeEvent>, anyhow::Error> {
        // Normalize + dedup: repo-relative path → last-write-wins deleted flag.
        let mut batch: BTreeMap<String, bool> = BTreeMap::new();
        for change in changes {
            if let Some(rel) = self.relativize(&change.path) {
                batch.insert(rel, change.deleted);
            }
        }

        let mut deleted: Vec<String> = Vec::new();
        let mut reparsed: Vec<String> = Vec::new();
        let mut configs_changed = false;

        for (rel, flagged_deleted) in &batch {
            if self.is_ignored(rel) {
                continue; // gitignore parity with the initial scan
            }
            // tsconfig changes (incl. `extends` targets like
            // tsconfig.base.json) re-resolve every importer they govern.
            if is_tsconfig_path(rel) {
                configs_changed = true;
            }
            if Language::from_path(rel).is_none() {
                continue; // not a source file
            }

            let full = self.root.join(rel);
            let gone = *flagged_deleted || !full.exists();
            if gone {
                if self.irs.remove(rel).is_some() {
                    self.hashes.remove(rel);
                    self.fingerprints.remove(rel);
                    deleted.push(rel.clone());
                }
                continue;
            }

            let source = match std::fs::read(&full) {
                Ok(s) => s,
                Err(_) => continue, // transient (atomic-save race) — next batch retries
            };
            let h = hash::hash_bytes(&source);
            if self.hashes.is_unchanged(rel, &h) {
                continue; // content unchanged — no reparse, no event
            }
            let ir = match crate::parse_file_bytes(rel, &source) {
                Ok(ir) => ir,
                Err(_) => continue, // parse failed — hash NOT cached, retried next batch
            };
            self.hashes.update(rel, &h);
            self.irs.insert(rel.clone(), ir);
            reparsed.push(rel.clone());
        }

        let mut events: Vec<ChangeEvent> = deleted
            .iter()
            .map(|path| ChangeEvent::Deleted { path: path.clone() })
            .collect();

        if deleted.is_empty() && reparsed.is_empty() && !configs_changed {
            // Nothing changed — skip the O(repo) resolution pass.
            return Ok(events);
        }

        if configs_changed {
            self.tsconfigs = crate::imports::TsConfigSet::discover(&self.root);
        }
        self.recompute_resolution();

        let reparsed_set: HashSet<&str> = reparsed.iter().map(|s| s.as_str()).collect();
        for (rel, ir) in self.irs.iter_mut() {
            let fp = resolution_fingerprint(ir);
            let changed = reparsed_set.contains(rel.as_str())
                || self.fingerprints.get(rel).is_none_or(|old| *old != fp);
            if changed {
                rebuild_cards(ir);
                self.fingerprints.insert(rel.clone(), fp);
                events.push(ChangeEvent::Updated { ir: ir.clone() });
            }
        }
        Ok(events)
    }

    /// Normalize any path to a repo-relative `a/b/c.rs` string, or `None`
    /// when the path is outside the repository root. Best-effort: paths that
    /// no longer exist (delete events) are stripped as-is.
    fn relativize(&self, path: &Path) -> Option<String> {
        let candidate = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let rel = candidate.strip_prefix(&self.root).ok()?;
        let s = rel.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// Gitignore parity for delta events (the initial scan is filtered by
    /// the gitignore-aware walker; raw watcher events are not). `.git`
    /// internals are always ignored. Best-effort: nested `.gitignore` files
    /// are honored; global/parent ignores are not.
    fn is_ignored(&self, rel: &str) -> bool {
        if rel == ".git" || rel.starts_with(".git/") {
            return true;
        }
        // Check the path and each ancestor directory: dir-only patterns
        // (`gen/`) apply to everything beneath them — the walker gets this
        // for free during traversal, a point check does not.
        let parts: Vec<&str> = rel.split('/').collect();
        let mut prefix = String::new();
        for (i, part) in parts.iter().enumerate() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let is_dir = i + 1 < parts.len();
            if matches!(
                self.ignore_matcher.matched(Path::new(&prefix), is_dir),
                ignore::Match::Ignore(_)
            ) {
                return true;
            }
        }
        false
    }

    /// Reset the cross-file resolution fields on every IR, then re-run both
    /// repo-level passes over the whole set. Resolution becomes a pure
    /// function of the current IRs — no stale edges survive deletes/renames
    /// (the passes skip already-resolved calls, so the reset is essential).
    fn recompute_resolution(&mut self) {
        let mut vec: Vec<FileParseIR> = std::mem::take(&mut self.irs).into_values().collect();
        for ir in &mut vec {
            reset_cross_file_fields(ir);
        }
        resolve::resolve_cross_file(&mut vec);
        imports::resolve_import_paths_with(&mut vec, &self.root, &self.tsconfigs);
        self.irs = vec.into_iter().map(|ir| (ir.path.clone(), ir)).collect();
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Repo-relative basename check for tsconfig files: any `tsconfig*.json`
/// (covers `tsconfig.json`, `tsconfig.base.json`, `tsconfig.spec.json` —
/// all can participate in an `extends` chain).
fn is_tsconfig_path(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.starts_with("tsconfig") && name.ends_with(".json")
}

fn default_languages() -> Vec<Language> {
    vec![
        Language::Rust,
        Language::TypeScript,
        Language::JavaScript,
        Language::Python,
    ]
}

/// Reset the repo-level resolution fields (the ones filled by
/// `resolve_cross_file` / `resolve_import_paths`) so the passes can be
/// re-run from a clean slate. In-file fields (`callee_local_key`) are
/// extractor output and stay untouched.
fn reset_cross_file_fields(ir: &mut FileParseIR) {
    for call in &mut ir.calls {
        call.callee_file = None;
        call.callee_external = false;
    }
    for imp in &mut ir.imports {
        imp.resolved = None;
    }
}

/// Rebuild retrieval cards after resolution changed (parity with parse_repo).
fn rebuild_cards(ir: &mut FileParseIR) {
    let (file_card, symbol_cards) = code_parser_ir::build_all_cards(ir, &Default::default());
    ir.retrieval_card = file_card;
    ir.symbol_cards = symbol_cards;
}

/// Fingerprint of the resolution-relevant fields of an IR. For files that
/// were not re-parsed, every other field is byte-stable, so a fingerprint
/// change ⇔ a call/import edge changed.
fn resolution_fingerprint(ir: &FileParseIR) -> String {
    let mut buf = String::new();
    for c in &ir.calls {
        buf.push_str(&c.caller_local_key);
        buf.push('\u{1}');
        buf.push_str(&c.callee_name);
        buf.push('\u{1}');
        buf.push_str(c.callee_local_key.as_deref().unwrap_or("\u{0}"));
        buf.push('\u{1}');
        buf.push_str(c.callee_file.as_deref().unwrap_or("\u{0}"));
        buf.push('\u{1}');
        buf.push(if c.callee_external { '1' } else { '0' });
        buf.push('\n');
    }
    for i in &ir.imports {
        buf.push_str(&i.target_module);
        buf.push('\u{1}');
        buf.push_str(i.resolved.as_deref().unwrap_or("\u{0}"));
        buf.push('\n');
    }
    hash::hash_str(&buf)
}

/// Build a gitignore matcher from every `.gitignore` under `root` (skipping
/// `.git` internals). Gitignored subtrees cannot contribute meaningful rules
/// (everything under them is already ignored), so the search walk uses the
/// standard filters.
fn build_ignore_matcher(root: &Path) -> ignore::gitignore::Gitignore {
    use ignore::gitignore::GitignoreBuilder;

    let mut builder = GitignoreBuilder::new(root);
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_file()) && entry.file_name() == ".gitignore" {
            builder.add(entry.path());
        }
    }
    // Build failures (unreadable .gitignore) degrade to an empty matcher —
    // delta filtering stays best-effort, never fatal.
    builder.build().unwrap_or_else(|_| {
        ignore::gitignore::GitignoreBuilder::new(root)
            .build()
            .expect("empty gitignore builder always builds")
    })
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "code-parser-repo-state-{}-{}",
            std::process::id(),
            label
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_init(dir: &Path) {
        // The ignore crate only honors .gitignore inside git repos.
        let _ = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(dir)
            .status();
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let p = dir.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    fn change(path: impl Into<PathBuf>, deleted: bool) -> FileChange {
        FileChange {
            path: path.into(),
            deleted,
        }
    }

    fn updated_paths(events: &[ChangeEvent]) -> Vec<String> {
        let mut v: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ChangeEvent::Updated { ir } => Some(ir.path.clone()),
                ChangeEvent::Deleted { .. } => None,
            })
            .collect();
        v.sort();
        v
    }

    fn deleted_paths(events: &[ChangeEvent]) -> Vec<String> {
        let mut v: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ChangeEvent::Deleted { path } => Some(path.clone()),
                ChangeEvent::Updated { .. } => None,
            })
            .collect();
        v.sort();
        v
    }

    fn find_updated<'a>(events: &'a [ChangeEvent], path: &str) -> &'a FileParseIR {
        events
            .iter()
            .find_map(|e| match e {
                ChangeEvent::Updated { ir } if ir.path == path => Some(ir),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{path} not among updated events"))
    }

    const LIB: &str = "pub mod util {\n    pub fn helper() -> u32 { 42 }\n}\n";
    const MAIN: &str = "pub fn main() -> u32 {\n    util::helper()\n}\n";

    fn scanned(dir: &Path) -> RepoState {
        let mut state = RepoState::new(dir);
        let events = state.scan(Some(vec![Language::Rust])).unwrap();
        assert!(!events.is_empty(), "snapshot must emit events");
        state
    }

    #[test]
    fn scan_emits_repo_relative_paths() {
        let dir = tmp_dir("scan-rel");
        git_init(&dir);
        write_file(&dir, "src/lib.rs", LIB);
        write_file(&dir, "src/main.rs", MAIN);
        let state = scanned(&dir);
        assert_eq!(state.len(), 2);
        cleanup(&dir);
    }

    #[test]
    fn scan_resolves_cross_file_calls() {
        let dir = tmp_dir("scan-resolve");
        git_init(&dir);
        write_file(&dir, "lib.rs", LIB);
        write_file(&dir, "main.rs", MAIN);
        let mut state = RepoState::new(&dir);
        let events = state.scan(Some(vec![Language::Rust])).unwrap();

        let main_ir = find_updated(&events, "main.rs");
        let call = main_ir
            .calls
            .iter()
            .find(|c| c.callee_name == "util::helper")
            .expect("util::helper call site");
        assert_eq!(call.callee_file.as_deref(), Some("lib.rs"));
        assert!(!call.callee_external);
        cleanup(&dir);
    }

    #[test]
    fn apply_batch_unchanged_file_emits_nothing() {
        let dir = tmp_dir("unchanged");
        git_init(&dir);
        write_file(&dir, "lib.rs", LIB);
        write_file(&dir, "main.rs", MAIN);
        let mut state = scanned(&dir);

        let events = state
            .apply_batch(&[change(dir.join("main.rs"), false)])
            .unwrap();
        assert!(
            events.is_empty(),
            "unchanged file must not re-emit: {events:?}"
        );
        cleanup(&dir);
    }

    #[test]
    fn apply_batch_modified_file_emits_single_update() {
        let dir = tmp_dir("modify");
        git_init(&dir);
        write_file(&dir, "lib.rs", LIB);
        write_file(&dir, "main.rs", MAIN);
        let mut state = scanned(&dir);

        write_file(&dir, "main.rs", "pub fn main() -> u32 {\n    7\n}\n");
        let events = state
            .apply_batch(&[change(dir.join("main.rs"), false)])
            .unwrap();
        assert_eq!(updated_paths(&events), vec!["main.rs"]);
        cleanup(&dir);
    }

    #[test]
    fn apply_batch_reemits_reverse_dependents_when_edges_shift() {
        let dir = tmp_dir("reverse-dep");
        git_init(&dir);
        write_file(&dir, "lib.rs", LIB);
        write_file(&dir, "main.rs", MAIN);
        let mut state = scanned(&dir);

        // Rename the callee. main.rs is NOT touched on disk, yet its edge
        // util::helper → lib.rs is now stale and must be re-emitted.
        write_file(
            &dir,
            "lib.rs",
            "pub mod util {\n    pub fn helper2() -> u32 { 42 }\n}\n",
        );
        let events = state
            .apply_batch(&[change(dir.join("lib.rs"), false)])
            .unwrap();
        assert_eq!(
            updated_paths(&events),
            vec!["lib.rs", "main.rs"],
            "reparsed file + reverse-dependent must re-emit"
        );

        let main_ir = find_updated(&events, "main.rs");
        let call = main_ir
            .calls
            .iter()
            .find(|c| c.callee_name == "util::helper")
            .expect("call site");
        assert!(call.callee_file.is_none(), "edge must drop after rename");
        assert!(call.callee_external, "qualified name gone → external");
        cleanup(&dir);
    }

    #[test]
    fn apply_batch_delete_emits_tombstone_and_clears_dependent_edges() {
        let dir = tmp_dir("delete");
        git_init(&dir);
        write_file(&dir, "lib.rs", LIB);
        write_file(&dir, "main.rs", MAIN);
        let mut state = scanned(&dir);

        fs::remove_file(dir.join("lib.rs")).unwrap();
        let events = state
            .apply_batch(&[change(dir.join("lib.rs"), true)])
            .unwrap();
        assert_eq!(deleted_paths(&events), vec!["lib.rs"]);
        assert_eq!(updated_paths(&events), vec!["main.rs"]);

        let main_ir = find_updated(&events, "main.rs");
        let call = main_ir
            .calls
            .iter()
            .find(|c| c.callee_name == "util::helper")
            .expect("call site");
        assert!(call.callee_file.is_none());
        assert!(call.callee_external);
        assert_eq!(state.len(), 1, "lib.rs must leave the tracked set");
        cleanup(&dir);
    }

    #[test]
    fn apply_batch_rename_in_one_batch_emits_tombstone_and_update() {
        let dir = tmp_dir("rename");
        git_init(&dir);
        write_file(&dir, "old.rs", LIB);
        let mut state = scanned(&dir);

        fs::rename(dir.join("old.rs"), dir.join("new.rs")).unwrap();
        let events = state
            .apply_batch(&[
                change(dir.join("old.rs"), true),
                change(dir.join("new.rs"), false),
            ])
            .unwrap();
        assert_eq!(deleted_paths(&events), vec!["old.rs"]);
        assert_eq!(updated_paths(&events), vec!["new.rs"]);
        cleanup(&dir);
    }

    #[test]
    fn apply_batch_ignores_gitignored_and_non_source_paths() {
        let dir = tmp_dir("ignored");
        git_init(&dir);
        write_file(&dir, ".gitignore", "gen/\n");
        write_file(&dir, "gen/x.rs", "fn x() {}\n");
        write_file(&dir, "src/lib.rs", LIB);
        let mut state = RepoState::new(&dir);
        let events = state.scan(Some(vec![Language::Rust])).unwrap();
        // The gitignore-aware scan never collected gen/x.rs.
        assert_eq!(updated_paths(&events), vec!["src/lib.rs"]);

        // Watcher events for gitignored + non-source paths must be filtered
        // with the same policy.
        let events = state
            .apply_batch(&[
                change(dir.join("gen/x.rs"), false),
                change(dir.join("notes.md"), false),
                change(dir.join(".git/config"), false),
            ])
            .unwrap();
        assert!(
            events.is_empty(),
            "ignored paths must emit nothing: {events:?}"
        );
        cleanup(&dir);
    }

    #[test]
    fn change_event_serializes_with_event_tag() {
        let ev = ChangeEvent::Deleted {
            path: "src/foo.rs".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(json, r#"{"event":"deleted","path":"src/foo.rs"}"#);
    }

    /// tsconfig hot-reload: editing a config re-resolves its importers.
    #[cfg(feature = "typescript")]
    #[test]
    fn apply_batch_tsconfig_change_reresolves_importers() {
        let dir = tmp_dir("tsconfig-reload");
        git_init(&dir);
        write_file(
            &dir,
            "tsconfig.json",
            "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"~/*\":[\"src/*\"]}}}",
        );
        write_file(&dir, "src/util.ts", "export const u = 1;\n");
        write_file(&dir, "other/util.ts", "export const o = 1;\n");
        write_file(
            &dir,
            "src/app.ts",
            "import { u } from '~/util';\nconsole.log(u);\n",
        );
        let mut state = RepoState::new(&dir);
        let events = state.scan(Some(vec![Language::TypeScript])).unwrap();
        let app = find_updated(&events, "src/app.ts");
        let imp = app
            .imports
            .iter()
            .find(|i| i.target_module == "~/util")
            .unwrap();
        assert_eq!(imp.resolved.as_deref(), Some("src/util.ts"));

        // Flip the alias to src/other — only the tsconfig changes on disk.
        write_file(
            &dir,
            "tsconfig.json",
            "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"~/*\":[\"other/*\"]}}}",
        );
        let events = state
            .apply_batch(&[change(dir.join("tsconfig.json"), false)])
            .unwrap();
        assert_eq!(
            updated_paths(&events),
            vec!["src/app.ts"],
            "importer must re-emit with fresh resolution: {events:?}"
        );
        let app = find_updated(&events, "src/app.ts");
        let imp = app
            .imports
            .iter()
            .find(|i| i.target_module == "~/util")
            .unwrap();
        assert_eq!(imp.resolved.as_deref(), Some("other/util.ts"));
        cleanup(&dir);
    }
}

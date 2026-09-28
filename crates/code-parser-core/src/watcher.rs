//! File watcher using the `notify` crate.
//!
//! Cross-platform via `notify::recommended_watcher`. Debounces events:
//! aggregates within a configurable window (default 200ms) before yielding.
//! Per-path last-write-wins semantics: a Remove followed by a Create within
//! one window (editor atomic save, rename) is reported as a modification,
//! not a deletion.
//!
//! `next_changes` returns `None` only when the event channel is closed —
//! irrelevant or empty batches never terminate the consumer's loop.
//!
//! Enabled behind the `watcher` feature flag.

#[cfg(feature = "watcher")]
pub use watcher_impl::*;

#[cfg(not(feature = "watcher"))]
pub use stub::*;

#[cfg(feature = "watcher")]
mod watcher_impl {
    use notify::{Event, EventKind, RecursiveMode, Watcher as _};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Default debounce window — aggregate events within this duration.
    pub const DEFAULT_DEBOUNCE_MS: u64 = 200;

    /// A debounced filesystem change for one path.
    #[derive(Debug, Clone)]
    pub struct WatchEvent {
        /// Changed path, as reported by the backend (typically absolute).
        pub path: PathBuf,
        /// True when the last event seen for this path within the debounce
        /// window was a removal.
        pub deleted: bool,
    }

    impl From<WatchEvent> for crate::repo_state::FileChange {
        fn from(e: WatchEvent) -> Self {
            crate::repo_state::FileChange {
                path: e.path,
                deleted: e.deleted,
            }
        }
    }

    /// A cross-platform file watcher with debounce.
    pub struct FileWatcher {
        rx: mpsc::Receiver<notify::Result<Event>>,
        debounce_ms: u64,
        _watcher: Box<dyn notify::Watcher>,
    }

    impl FileWatcher {
        /// Start watching `root` recursively with the default debounce
        /// window ([`DEFAULT_DEBOUNCE_MS`]).
        pub fn new(root: &Path) -> Result<Self, anyhow::Error> {
            Self::with_debounce(root, DEFAULT_DEBOUNCE_MS)
        }

        /// Start watching `root` recursively with a custom debounce window
        /// in milliseconds.
        pub fn with_debounce(root: &Path, debounce_ms: u64) -> Result<Self, anyhow::Error> {
            let (tx, rx) = mpsc::channel();
            let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
                let _ = tx.send(res);
            })?;
            watcher.watch(root, RecursiveMode::Recursive)?;
            Ok(Self {
                rx,
                debounce_ms: debounce_ms.max(1),
                _watcher: Box::new(watcher),
            })
        }

        /// Block until a debounced batch of filesystem changes arrives.
        ///
        /// Returns `None` only when the event channel is closed (watcher
        /// died). Irrelevant events are filtered and never terminate the
        /// stream — a window that collects nothing relevant keeps waiting
        /// for the next event instead.
        pub fn next_changes(&self) -> Option<Vec<WatchEvent>> {
            // Blocking wait for the first relevant event of the batch.
            let mut deleted_flags: HashMap<PathBuf, bool> = HashMap::new();
            loop {
                match self.rx.recv() {
                    Ok(Ok(event)) => {
                        if is_relevant(&event) {
                            merge_event(&event, &mut deleted_flags);
                            break;
                        }
                    }
                    Ok(Err(_)) => continue, // transient backend error
                    Err(_) => return None,  // channel closed
                }
            }

            // Drain the rest of the debounce window.
            let deadline = Instant::now() + Duration::from_millis(self.debounce_ms);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match self.rx.recv_timeout(remaining) {
                    Ok(Ok(event)) => {
                        if is_relevant(&event) {
                            merge_event(&event, &mut deleted_flags);
                        }
                    }
                    Ok(Err(_)) => continue,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                }
            }

            Some(
                deleted_flags
                    .into_iter()
                    .map(|(path, deleted)| WatchEvent { path, deleted })
                    .collect(),
            )
        }

        /// Block until the next change (single path), legacy API.
        pub fn next_change(&self) -> Option<PathBuf> {
            self.next_changes()
                .and_then(|events| events.into_iter().next())
                .map(|e| e.path)
        }
    }

    /// Only file modification, creation, and removal events are relevant.
    fn is_relevant(event: &Event) -> bool {
        matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        )
    }

    /// Merge one event into the per-path deleted-flags map. Last write
    /// wins: Remove→Create within one window reports a live file.
    fn merge_event(event: &Event, deleted_flags: &mut HashMap<PathBuf, bool>) {
        let deleted = matches!(event.kind, EventKind::Remove(_));
        for p in &event.paths {
            deleted_flags.insert(p.clone(), deleted);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};

        fn ev(kind: EventKind, path: &str) -> Event {
            Event {
                kind,
                paths: vec![PathBuf::from(path)],
                attrs: Default::default(),
            }
        }

        #[test]
        fn remove_then_create_is_not_deleted() {
            // Editor atomic save: Remove followed by Create in one window.
            let mut flags = HashMap::new();
            merge_event(&ev(EventKind::Remove(RemoveKind::Any), "a.rs"), &mut flags);
            merge_event(&ev(EventKind::Create(CreateKind::Any), "a.rs"), &mut flags);
            assert_eq!(flags.get(&PathBuf::from("a.rs")), Some(&false));
        }

        #[test]
        fn create_then_remove_is_deleted() {
            let mut flags = HashMap::new();
            merge_event(&ev(EventKind::Create(CreateKind::Any), "a.rs"), &mut flags);
            merge_event(&ev(EventKind::Remove(RemoveKind::Any), "a.rs"), &mut flags);
            assert_eq!(flags.get(&PathBuf::from("a.rs")), Some(&true));
        }

        #[test]
        fn modify_is_never_deleted() {
            let mut flags = HashMap::new();
            merge_event(&ev(EventKind::Modify(ModifyKind::Any), "a.rs"), &mut flags);
            assert_eq!(flags.get(&PathBuf::from("a.rs")), Some(&false));
        }

        #[test]
        fn access_events_are_irrelevant() {
            assert!(!is_relevant(&ev(
                EventKind::Access(AccessKind::Any),
                "a.rs"
            )));
            assert!(is_relevant(&ev(EventKind::Modify(ModifyKind::Any), "a.rs")));
            assert!(is_relevant(&ev(EventKind::Create(CreateKind::Any), "a.rs")));
            assert!(is_relevant(&ev(EventKind::Remove(RemoveKind::Any), "a.rs")));
        }
    }
}

#[cfg(not(feature = "watcher"))]
mod stub {
    use std::path::{Path, PathBuf};

    /// Stub watcher when the 'watcher' feature is disabled.
    pub struct FileWatcher;

    /// Stub event mirroring the real `WatchEvent` shape.
    #[allow(unused)]
    #[derive(Debug, Clone)]
    pub struct WatchEvent {
        pub path: PathBuf,
        pub deleted: bool,
    }

    impl FileWatcher {
        pub fn new(_root: &Path) -> Result<Self, anyhow::Error> {
            anyhow::bail!(
                "FileWatcher requires the 'watcher' feature. Rebuild with --features watcher."
            )
        }

        #[allow(unused)]
        pub fn with_debounce(_root: &Path, _debounce_ms: u64) -> Result<Self, anyhow::Error> {
            Err(anyhow::anyhow!(
                "FileWatcher requires the 'watcher' feature. Rebuild with --features watcher."
            ))
        }

        #[allow(unused)]
        pub fn next_change(&self) -> Option<PathBuf> {
            None
        }

        #[allow(unused)]
        pub fn next_changes(&self) -> Option<Vec<WatchEvent>> {
            None
        }
    }
}

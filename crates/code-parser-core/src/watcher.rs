/// File watcher using the `notify` crate.
///
/// Cross-platform via `notify::recommended_watcher`.
/// Debounces events: aggregates within a 200ms window before yielding.
///
/// Enabled behind the `watcher` feature flag.

#[cfg(feature = "watcher")]
pub use watcher_impl::*;

#[cfg(not(feature = "watcher"))]
pub use stub::*;

#[cfg(feature = "watcher")]
mod watcher_impl {
    use notify::{Event, EventKind, RecursiveMode, Watcher as _};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Debounce window — aggregate events within this duration.
    const DEBOUNCE_MS: u64 = 200;

    /// A cross-platform file watcher with debounce.
    pub struct FileWatcher {
        rx: mpsc::Receiver<notify::Result<Event>>,
        _watcher: Box<dyn notify::Watcher>,
    }

    impl FileWatcher {
        /// Start watching `root` recursively for file modifications and creations.
        pub fn new(root: &Path) -> Result<Self, anyhow::Error> {
            let (tx, rx) = mpsc::channel();
            let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
                let _ = tx.send(res);
            })?;
            watcher.watch(root, RecursiveMode::Recursive)?;
            Ok(Self {
                rx,
                _watcher: Box::new(watcher),
            })
        }

        /// Block until a debounced batch of filesystem changes arrives.
        /// Returns the set of changed paths.
        pub fn next_changes(&self) -> Option<Vec<PathBuf>> {
            let mut paths: HashSet<PathBuf> = HashSet::new();
            let deadline = Instant::now() + Duration::from_millis(DEBOUNCE_MS);

            // Collect the first event.
            match self.rx.recv() {
                Ok(Ok(event)) => {
                    if is_relevant(&event) {
                        for p in &event.paths {
                            paths.insert(p.clone());
                        }
                    }
                }
                Ok(Err(_)) => {}
                Err(_) => return None,
            }

            // Drain remaining events within the debounce window.
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match self.rx.recv_timeout(remaining) {
                    Ok(Ok(event)) => {
                        if is_relevant(&event) {
                            for p in &event.paths {
                                paths.insert(p.clone());
                            }
                        }
                    }
                    Ok(Err(_)) => continue,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                }
            }

            if paths.is_empty() {
                None
            } else {
                Some(paths.into_iter().collect())
            }
        }

        /// Block until the next change (single path), legacy API.
        pub fn next_change(&self) -> Option<PathBuf> {
            self.next_changes()
                .and_then(|paths| paths.into_iter().next())
        }
    }

    /// Filter to only modification and creation events.
    fn is_relevant(event: &Event) -> bool {
        matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        )
    }
}

#[cfg(not(feature = "watcher"))]
mod stub {
    use std::path::{Path, PathBuf};

    /// Stub watcher when the `watcher` feature is disabled.
    pub struct FileWatcher;

    impl FileWatcher {
        pub fn new(_root: &Path) -> Result<Self, anyhow::Error> {
            anyhow::bail!(
                "FileWatcher requires the 'watcher' feature. Rebuild with --features watcher."
            )
        }

        #[allow(unused)]
        pub fn next_change(&self) -> Option<PathBuf> {
            None
        }

        #[allow(unused)]
        pub fn next_changes(&self) -> Option<Vec<PathBuf>> {
            None
        }
    }
}

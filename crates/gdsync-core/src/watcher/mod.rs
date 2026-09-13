use crate::filter::GitignoreFilter;
use anyhow::{Context, Result};
use notify_debouncer_full::{
    new_debouncer,
    notify::{EventKind, RecommendedWatcher, RecursiveMode},
    DebouncedEvent, Debouncer, NoCache,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info, trace};

#[derive(Debug, Clone, PartialEq)]
pub enum FsChange {
    CreateOrModify(PathBuf),
    Delete(PathBuf),
}

pub struct FsWatcher {
    root: PathBuf,
    filter: GitignoreFilter,
    debounce_duration: Duration,
}

impl FsWatcher {
    pub fn new(root: &Path, debounce_duration: Duration) -> Result<Self> {
        let canonical = std::fs::canonicalize(root)
            .with_context(|| format!("Failed to canonicalize watch path {:?}", root))?;
        let filter = GitignoreFilter::new(&canonical)?;

        Ok(Self {
            root: canonical,
            filter,
            debounce_duration,
        })
    }

    /// Starts watching the filesystem, forwarding debounced, gitignore-filtered
    /// filesystem changes to the provided Tokio async channel.
    pub fn start_watching(
        self,
        tx: tokio_mpsc::Sender<FsChange>,
    ) -> Result<Debouncer<RecommendedWatcher, NoCache>> {
        let (sync_tx, sync_rx) = mpsc::channel();

        let mut debouncer = new_debouncer(self.debounce_duration, None, sync_tx)
            .context("Failed to create inotify debouncer")?;

        debouncer
            .watch(&self.root, RecursiveMode::Recursive)
            .with_context(|| format!("Failed to watch directory {:?}", self.root))?;

        let filter = self.filter;
        let root = self.root.clone();

        // Spawn a background thread to process debounced events and filter via Gitignore
        std::thread::spawn(move || {
            while let Ok(result) = sync_rx.recv() {
                match result {
                    Ok(events) => {
                        for event in events {
                            Self::process_debounced_event(&event, &root, &filter, &tx);
                        }
                    }
                    Err(errors) => {
                        for err in errors {
                            error!("Inotify debouncer error: {:?}", err);
                        }
                    }
                }
            }
            debug!("Watcher thread terminated.");
        });

        info!("Inotify watcher successfully attached to {:?}", self.root);
        Ok(debouncer)
    }

    fn process_debounced_event(
        event: &DebouncedEvent,
        _root: &Path,
        filter: &GitignoreFilter,
        tx: &tokio_mpsc::Sender<FsChange>,
    ) {
        for path in &event.paths {
            let is_dir = path.is_dir();
            if filter.is_ignored(path, is_dir) {
                trace!("Watcher ignoring event on path: {:?}", path);
                continue;
            }

            // Determine if created/modified or deleted
            let change = match event.kind {
                EventKind::Remove(_) => FsChange::Delete(path.clone()),
                _ => {
                    if path.exists() {
                        FsChange::CreateOrModify(path.clone())
                    } else {
                        FsChange::Delete(path.clone())
                    }
                }
            };

            let _ = tx.blocking_send(change);
        }
    }
}

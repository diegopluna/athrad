//! The file-watcher seam for `athrad watch`.
//!
//! The watch loop is written against [`Watcher`], and [`OsWatcher`] gives it
//! one backend per host through the `notify` crate: `inotify` on Linux,
//! `kqueue` on macOS, `ReadDirectoryChangesW` on Windows. A loop that
//! reached for one of those directly would have grown a one-OS core.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// What happened to a watched path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Modified(PathBuf),
    Created(PathBuf),
    Removed(PathBuf),
    /// The backend lost events (a full queue, a renamed root). Rescan.
    Overflow,
}

pub trait Watcher {
    /// Watch `path`, and everything under it if it is a directory.
    fn watch(&mut self, path: &Path) -> std::io::Result<()>;

    fn unwatch(&mut self, path: &Path) -> std::io::Result<()>;

    /// Block until changes arrive or `timeout` passes, then return what
    /// arrived. Several writes to one file may come back as one change.
    fn wait(&mut self, timeout: Option<Duration>) -> std::io::Result<Vec<Change>>;
}

/// The host's own file-change API.
pub struct OsWatcher {
    inner: notify::RecommendedWatcher,
    events: mpsc::Receiver<notify::Result<notify::Event>>,
}

impl OsWatcher {
    pub fn new() -> std::io::Result<Self> {
        let (tx, events) = mpsc::channel();
        let inner = notify::recommended_watcher(move |e| {
            let _ = tx.send(e);
        })
        .map_err(io_error)?;
        Ok(OsWatcher { inner, events })
    }
}

fn io_error(e: notify::Error) -> std::io::Error {
    std::io::Error::other(e)
}

fn changes(event: notify::Result<notify::Event>, out: &mut Vec<Change>) {
    use notify::EventKind;
    let Ok(event) = event else {
        out.push(Change::Overflow);
        return;
    };
    if event.need_rescan() {
        out.push(Change::Overflow);
        return;
    }
    for path in event.paths {
        let change = match event.kind {
            EventKind::Create(_) => Change::Created(path),
            EventKind::Remove(_) => Change::Removed(path),
            EventKind::Modify(_) | EventKind::Any | EventKind::Other => Change::Modified(path),
            EventKind::Access(_) => continue,
        };
        if !out.contains(&change) {
            out.push(change);
        }
    }
}

impl Watcher for OsWatcher {
    fn watch(&mut self, path: &Path) -> std::io::Result<()> {
        use notify::Watcher as _;
        self.inner.watch(path, notify::RecursiveMode::Recursive).map_err(io_error)
    }

    fn unwatch(&mut self, path: &Path) -> std::io::Result<()> {
        use notify::Watcher as _;
        self.inner.unwatch(path).map_err(io_error)
    }

    fn wait(&mut self, timeout: Option<Duration>) -> std::io::Result<Vec<Change>> {
        let first = match timeout {
            Some(t) => match self.events.recv_timeout(t) {
                Ok(e) => e,
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(Vec::new()),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(std::io::Error::other("watcher stopped")),
            },
            None => self.events.recv().map_err(|_| std::io::Error::other("watcher stopped"))?,
        };
        let mut out = Vec::new();
        changes(first, &mut out);
        // An editor's save is often several events (write, then rename, then
        // metadata); take what arrives right after the first as one change.
        let settle = Instant::now() + Duration::from_millis(2);
        while let Some(left) = settle.checked_duration_since(Instant::now()) {
            match self.events.recv_timeout(left) {
                Ok(e) => changes(e, &mut out),
                Err(_) => break,
            }
        }
        Ok(out)
    }
}

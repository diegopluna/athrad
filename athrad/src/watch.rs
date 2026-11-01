//! The file-watcher seam for the daemon of milestone 2.
//!
//! Nothing implements it yet. It exists now so the daemon is written against
//! this trait from its first line, and gets one backend per host: `inotify`
//! on Linux, `kqueue` on macOS, `ReadDirectoryChangesW` on Windows. A daemon
//! that reaches for one of those directly has grown a one-OS core.

use std::path::{Path, PathBuf};
use std::time::Duration;

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

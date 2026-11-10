//! Changes made to the notes folder by other programs.
//!
//! The watching itself — debouncing, resolving each path to what is on disk, leaving out the app's
//! own writes, noticing a watch that went quiet — is `tauri_kit_watch`. What is decided here is what
//! this app means by the folder: which paths are not notes, which one says the whole folder may have
//! been replaced, where the liveness probe may write, and what a change is turned into — an
//! `fs_changed` or `fs_rescan` event for the screen, and an update of the search index.

use crate::search::IndexHandle;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri_kit_watch::{Notice, OwnWrites, Watch, Watcher};

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    /// Something is at the path now: made or changed.
    Modified,
    Removed,
}

/// Change payload sent to the frontend (app IPC wire type — separate from library types).
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct FsChange {
    pub kind: ChangeKind,
    pub path: String,
}

/// What the screen is told about one batch.
#[derive(Debug, PartialEq, Eq)]
pub enum Surface {
    /// These paths changed.
    Changed(Vec<FsChange>),
    /// What was known about the folder can no longer be trusted: read it all again.
    Rescan,
}

/// The running watch of the open folder. Managed as a Tauri `State`; opening another folder
/// replaces it, and dropping it stops watching.
#[derive(Default)]
pub struct WatcherHandle(pub Mutex<Option<Watcher>>);

/// Dot segments relative to the vault root (`.textree`, `.git`, etc.) are hidden in the tree, so
/// the watcher ignores them too. Dots in the root path itself are not counted.
fn is_ignored(relative: &Path) -> bool {
    relative
        .components()
        .any(|c| c.as_os_str().to_str().is_some_and(|s| s.starts_with('.')))
}

/// The one path inside repository storage worth reacting to: the record of which branch is
/// checked out.
///
/// When another tool switches branches, the folder's contents are replaced wholesale. Per-path
/// changes cannot describe that reliably, so it is treated the same way as the operating system
/// dropping events — read everything again. Everything else under `.git` stays ignored: object and
/// index writes happen constantly, and none of them says anything about the notes.
///
/// Only reachable when the folder is itself the repository root. A folder sitting inside a
/// repository has that record outside the watched folder, and watching outside the folder someone
/// opened is a different decision from this one.
fn is_head_reference(relative: &Path) -> bool {
    relative == Path::new(".git").join("HEAD")
}

/// Directory, inside repository storage, that the liveness probe writes into.
const PROBE_DIR_NAME: &str = "textree-canary";

/// Where the liveness probe for the folder at `root` may write, relative to it — if anywhere.
///
/// Only inside repository storage that itself sits inside the watched folder: the folder is a
/// repository's root. Anywhere else the watch cannot see the write (the storage is outside the
/// folder) or the write would land in the person's folder. A folder that becomes a repository
/// while open is probed from the next time it is opened.
fn probe_dir(root: &Path) -> Option<PathBuf> {
    let storage = crate::git_engine::git_dir(root)?;
    let storage = std::fs::canonicalize(storage).ok()?;
    let root = std::fs::canonicalize(root).ok()?;
    storage
        .strip_prefix(&root)
        .ok()
        .map(|inside| inside.join(PROBE_DIR_NAME))
}

/// How often a watch is probed: every five minutes, unless `TEXTREE_WATCHDOG_INTERVAL_MS` says
/// otherwise. The override exists so an end-to-end run can watch whole probe cycles without waiting
/// minutes for each; nothing else sets it.
fn probe_interval() -> Duration {
    crate::dev_env::var("TEXTREE_WATCHDOG_INTERVAL_MS")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(300))
}

/// Watches the folder at `root`: keeps the search index in step and tells `surface` what changed.
/// `own` holds what the app writes, which is not reported back. Lists the folder once before
/// watching starts, so it is called off the main thread (opening a folder already is).
pub fn watch_vault(
    root: &Path,
    own: &OwnWrites,
    index: Arc<IndexHandle>,
    mut surface: impl FnMut(Surface) + Send + 'static,
) -> Result<Watcher, String> {
    let mut watch = Watch::new(root)
        .own_writes(own)
        .ignore(is_ignored)
        .rescan_on(is_head_reference);
    if let Some(dir) = probe_dir(root) {
        watch = watch.probe_liveness(dir, probe_interval());
    }
    let at = root.to_path_buf();
    watch
        .start(move |notice| match notice {
            Notice::Changed(changes) => {
                let batch: Vec<FsChange> = changes
                    .into_iter()
                    .map(|c| FsChange {
                        kind: match c.kind {
                            tauri_kit_watch::ChangeKind::Removed => ChangeKind::Removed,
                            _ => ChangeKind::Modified,
                        },
                        path: at.join(&c.path).display().to_string(),
                    })
                    .collect();
                apply_changes_to_index(&index, &at, &batch);
                surface(Surface::Changed(batch));
            }
            Notice::Rescan => {
                log::warn!("watcher: what is known about the folder may be stale — full refresh");
                rebuild_index(&index, &at);
                surface(Surface::Rescan);
            }
        })
        .map_err(|e| e.to_string())
}

/// Applies a batch of changes to the index and commits once (.md only). No-op when no index is
/// installed.
pub fn apply_changes_to_index(handle: &IndexHandle, root: &Path, changes: &[FsChange]) {
    let mut guard = handle.0.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else { return };
    let mut touched = false;
    for c in changes {
        let p = Path::new(&c.path);
        if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")) {
            continue;
        }
        state.apply_change(root, p, matches!(c.kind, ChangeKind::Removed));
        touched = true;
    }
    if touched {
        let _ = state.commit();
    }
}

/// Rebuilds the whole index from disk (delete-all + re-index). Used after a rescan, when
/// incremental per-path updates can no longer be trusted. No-op when no index is installed, or
/// when the one installed is another folder's (a watch outlives its folder's index for a moment
/// when the next folder is opened).
pub fn rebuild_index(handle: &IndexHandle, root: &Path) {
    let mut guard = handle.0.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else { return };
    if !state.serves(root) {
        log::debug!(
            "watcher: rescan of {} is for an index no longer installed; skipped",
            root.display()
        );
        return;
    }
    if let Err(e) = state.rebuild(root) {
        log::warn!("watcher: index rebuild after rescan failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::IndexState;
    use std::sync::mpsc;
    use std::time::Instant;
    use tempfile::TempDir;

    #[test]
    fn dot_segments_below_the_root_are_not_notes() {
        assert!(is_ignored(Path::new(".textree/db.json")));
        assert!(is_ignored(Path::new(".git/config")));
        assert!(is_ignored(Path::new("journal/.hidden/x.md")));
        assert!(!is_ignored(Path::new("note.md")));
        assert!(!is_ignored(Path::new("journal/2026.md")));
    }

    #[test]
    fn the_checked_out_branch_record_is_singled_out_from_repository_storage() {
        assert!(is_head_reference(&Path::new(".git").join("HEAD")));
        // Everything else in there stays ignored: these change constantly, and none of them
        // says anything about the notes.
        for noisy in [
            ".git/index",
            ".git/config",
            ".git/objects/ab/cdef",
            ".git/logs/HEAD",
            ".git/refs/heads/main",
        ] {
            let path = PathBuf::from(noisy);
            assert!(!is_head_reference(&path), "{noisy}");
            assert!(is_ignored(&path), "{noisy}");
        }
        // And a note that merely has a similar name is not it.
        assert!(!is_head_reference(Path::new("HEAD")));
    }

    #[test]
    fn the_probe_writes_only_into_repository_storage_inside_the_folder() {
        let repo = TempDir::new().unwrap();
        git2::Repository::init(repo.path()).unwrap();
        assert_eq!(
            probe_dir(repo.path()),
            Some(Path::new(".git").join(PROBE_DIR_NAME))
        );

        // A folder inside a repository: its storage is outside the folder.
        let inside = repo.path().join("docs");
        std::fs::create_dir(&inside).unwrap();
        assert_eq!(probe_dir(&inside), None);

        // A folder with no repository: nowhere is ours.
        let plain = TempDir::new().unwrap();
        assert_eq!(probe_dir(plain.path()), None);
    }

    #[test]
    fn rebuild_index_recovers_changes_the_watcher_missed() {
        let vault = TempDir::new().unwrap();
        let idx = TempDir::new().unwrap();
        std::fs::write(vault.path().join("a.md"), "stale note").unwrap();

        let handle: Arc<IndexHandle> = Arc::new(IndexHandle::default());
        *handle.0.lock().unwrap() =
            Some(IndexState::open_or_create(idx.path(), vault.path()).unwrap());
        apply_changes_to_index(
            &handle,
            vault.path(),
            &[FsChange {
                kind: ChangeKind::Modified,
                path: vault.path().join("a.md").display().to_string(),
            }],
        );

        // Simulate dropped events: a.md vanished and b.md appeared with no per-path event.
        std::fs::remove_file(vault.path().join("a.md")).unwrap();
        std::fs::write(vault.path().join("b.md"), "fresh note").unwrap();

        rebuild_index(&handle, vault.path());

        let guard = handle.0.lock().unwrap();
        let st = guard.as_ref().unwrap();
        assert_eq!(
            st.search("stale", 10).unwrap().len(),
            0,
            "stale doc must be gone"
        );
        assert_eq!(
            st.search("fresh", 10).unwrap().len(),
            1,
            "missed file must be indexed"
        );
    }

    #[test]
    fn apply_changes_to_index_upserts_and_deletes() {
        let vault = TempDir::new().unwrap();
        let idx = TempDir::new().unwrap();
        std::fs::write(vault.path().join("a.md"), "initial body").unwrap();

        let handle: Arc<IndexHandle> = Arc::new(IndexHandle::default());
        *handle.0.lock().unwrap() =
            Some(IndexState::open_or_create(idx.path(), vault.path()).unwrap());
        let at = vault.path().join("a.md").display().to_string();

        apply_changes_to_index(
            &handle,
            vault.path(),
            &[FsChange {
                kind: ChangeKind::Modified,
                path: at.clone(),
            }],
        );
        assert_eq!(
            handle
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .search("body", 10)
                .unwrap()
                .len(),
            1
        );

        apply_changes_to_index(
            &handle,
            vault.path(),
            &[FsChange {
                kind: ChangeKind::Removed,
                path: at,
            }],
        );
        assert_eq!(
            handle
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .search("body", 10)
                .unwrap()
                .len(),
            0
        );
    }

    /// Surfaces delivered until `done` holds, or ten seconds pass.
    fn until(rx: &mpsc::Receiver<Surface>, done: impl Fn(&[Surface]) -> bool) -> Vec<Surface> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while !done(&seen) {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(s) => seen.push(s),
                Err(_) => break,
            }
        }
        seen
    }

    fn changed(seen: &[Surface], path: &Path, kind: ChangeKind) -> bool {
        let path = path.display().to_string();
        seen.iter().any(|s| matches!(s, Surface::Changed(c) if c.iter().any(|c| c.path == path && c.kind == kind)))
    }

    #[test]
    fn a_running_watch_reports_others_changes_not_ours_and_rereads_on_a_branch_switch() {
        let vault = TempDir::new().unwrap();
        let repo = git2::Repository::init(vault.path()).unwrap();
        drop(repo);
        let own = OwnWrites::new();
        let (tx, rx) = mpsc::channel();
        let _watcher = watch_vault(
            vault.path(),
            &own,
            Arc::new(IndexHandle::default()),
            move |s| {
                let _ = tx.send(s);
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));

        // Ours: recorded before writing, never reported.
        let mine = vault.path().join("mine.md");
        own.record(&mine, b"typed here");
        tauri_kit_fs::write_atomic(&mine, b"typed here").unwrap();
        // Someone else's: reported once, as it now is, with the folder's own path.
        let theirs = vault.path().join("theirs.md");
        std::fs::write(&theirs, "written elsewhere").unwrap();
        let seen = until(&rx, |s| changed(s, &theirs, ChangeKind::Modified));
        assert!(changed(&seen, &theirs, ChangeKind::Modified), "{seen:?}");
        assert!(
            !changed(&seen, &mine, ChangeKind::Modified),
            "our own write came back: {seen:?}"
        );

        // Repository storage is quiet, except the record of the checked-out branch.
        std::fs::write(
            vault.path().join(".git").join("HEAD"),
            "ref: refs/heads/other\n",
        )
        .unwrap();
        let seen = until(&rx, |s| s.contains(&Surface::Rescan));
        assert!(seen.contains(&Surface::Rescan), "{seen:?}");
    }
}

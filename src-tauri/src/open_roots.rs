//! What the app holds for each open notes folder — its search index, its watch — keyed by the
//! folder itself, however its path is spelled.
//!
//! A folder's state is found by the folder a command names, never by "the folder open now": a save
//! that finishes after another folder was opened still reaches its own folder's index, and two
//! folders open at once (the sidebar's notes folders) do not share one.
//!
//! The window shows one folder at a time until the sidebar lists several, so opening a folder keeps
//! only that one ([`RootTable::keep_only`]). The table itself does not assume it.

use crate::search::vault_identity;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One value per open folder.
pub struct RootTable<T>(Mutex<HashMap<PathBuf, T>>);

impl<T> Default for RootTable<T> {
    fn default() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
}

impl<T> RootTable<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, T>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Puts `value` in place for `root`; what was there for it before is returned (and stops when
    /// dropped, for a watch).
    pub fn install(&self, root: &Path, value: T) -> Option<T> {
        self.lock().insert(vault_identity(root), value)
    }

    /// Keeps `root` and lets every other folder go; what was let go is returned so the caller drops
    /// it outside the lock.
    pub fn keep_only(&self, root: &Path) -> Vec<T> {
        let keep = vault_identity(root);
        let mut table = self.lock();
        let others: Vec<PathBuf> = table.keys().filter(|k| **k != keep).cloned().collect();
        others
            .into_iter()
            .filter_map(|k| table.remove(&k))
            .collect()
    }

    /// How many folders are open.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().len()
    }
}

impl<T: Clone> RootTable<T> {
    /// What is held for `root`, if it is open.
    pub fn of(&self, root: &Path) -> Option<T> {
        self.lock().get(&vault_identity(root)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{IndexHandle, IndexState};
    use std::sync::Arc;

    fn folder(notes: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in notes {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    fn open(table: &RootTable<Arc<IndexHandle>>, root: &Path, data: &Path) {
        let state =
            IndexState::open_or_create(&data.join(root.file_name().unwrap()), root).unwrap();
        let handle = Arc::new(IndexHandle::default());
        *handle.0.lock().unwrap() = Some(state);
        table.install(root, handle);
    }

    fn hits(table: &RootTable<Arc<IndexHandle>>, root: &Path, query: &str) -> Vec<String> {
        let handle = table.of(root).expect("open");
        let guard = handle.0.lock().unwrap();
        guard
            .as_ref()
            .unwrap()
            .search(query, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.path)
            .collect()
    }

    /// Two folders open at once: a note indexed in one is found there and nowhere else, and each
    /// is found by its own path however it is spelled.
    #[test]
    fn two_open_folders_keep_their_indexes_apart() {
        let data = tempfile::tempdir().unwrap();
        let a = folder(&[("plan.md", "apples")]);
        let b = folder(&[("plan.md", "bananas")]);
        let table = RootTable::default();
        open(&table, a.path(), data.path());
        open(&table, b.path(), data.path());
        assert_eq!(table.len(), 2);

        for (root, body) in [(a.path(), "apples"), (b.path(), "bananas")] {
            let handle = table.of(root).unwrap();
            let mut guard = handle.0.lock().unwrap();
            let state = guard.as_mut().unwrap();
            state.index_note(root, &root.join("plan.md"), body).unwrap();
            state.commit().unwrap();
        }

        assert_eq!(hits(&table, a.path(), "apples").len(), 1);
        assert!(hits(&table, a.path(), "bananas").is_empty());
        assert_eq!(hits(&table, b.path(), "bananas").len(), 1);
        assert!(hits(&table, b.path(), "apples").is_empty());

        // The same folder named another way (a trailing `.` piece) is the same entry.
        assert!(table.of(&a.path().join(".")).is_some());
    }

    #[test]
    fn keeping_one_folder_lets_the_others_go() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let table: RootTable<u8> = RootTable::default();
        table.install(a.path(), 1);
        table.install(b.path(), 2);
        assert_eq!(table.keep_only(b.path()), vec![1]);
        assert_eq!(table.of(a.path()), None);
        assert_eq!(table.of(b.path()), Some(2));
        // Installing again for the same folder replaces and hands back the old value.
        assert_eq!(table.install(b.path(), 3), Some(2));
    }
}

//! Writes to one note happen one at a time.
//!
//! Saves run off the main thread, so two of them can be in flight together. For two different
//! notes that is the point — a folder that stops answering for one note must not hold up another.
//! For the same note it would be a loss: a save that stalls after its check and lands late would
//! overwrite a newer one that went ahead meanwhile. A write can't be called back once the disk has
//! it, so the only safe order is the one enforced here: the next save to a note starts after the
//! previous one has actually finished, and checks the note against what that one left.
//!
//! Changes to the tree — renaming, moving, deleting, bringing back — run off the main thread too,
//! and take a turn over everything at or under the paths they change ([`NoteLocks::subtree`]). A
//! save that lands after its note was renamed away would put the old file back under the old name,
//! and one that lands after a delete would bring the note back: so a change to the tree waits for
//! the saves under it to finish, and saves under it wait for the change. Two changes to the tree
//! wait for each other only when one lies inside the other — a folder that stops answering holds
//! up changes to itself, not to the rest of the notes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

/// What is running right now.
#[derive(Default)]
pub struct NoteLocks {
    busy: Mutex<Busy>,
    freed: Condvar,
}

#[derive(Default)]
struct Busy {
    /// Notes being saved.
    notes: HashSet<PathBuf>,
    /// Places whose tree is being changed, each covering everything under it.
    trees: Vec<PathBuf>,
}

impl Busy {
    fn note_is_free(&self, note: &Path) -> bool {
        !self.notes.contains(note) && !self.trees.iter().any(|t| note.starts_with(t))
    }

    fn tree_is_free(&self, place: &Path) -> bool {
        !self.notes.iter().any(|n| n.starts_with(place))
            && !self
                .trees
                .iter()
                .any(|t| t.starts_with(place) || place.starts_with(t))
    }
}

/// Holds one note's turn. Dropping it lets the next save to that note start.
pub struct Turn<'a> {
    locks: &'a NoteLocks,
    key: PathBuf,
}

/// Holds a turn over everything at or under some places. Dropping it lets what waits there go.
pub struct TreeTurn<'a> {
    locks: &'a NoteLocks,
    keys: Vec<PathBuf>,
}

impl NoteLocks {
    /// Waits until no other save to `path` is running and nothing is changing the tree above it,
    /// then holds its turn.
    pub fn turn(&self, path: &Path) -> Turn<'_> {
        let key = key(path);
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        while !busy.note_is_free(&key) {
            busy = self.freed.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        busy.notes.insert(key.clone());
        Turn { locks: self, key }
    }

    /// Waits until nothing at or under any of `places` is being saved or changed, then holds them
    /// all — together, so two changes over overlapping places cannot each hold half.
    ///
    /// **Never take a note's [`turn`](Self::turn) while holding this over that note**: the turn
    /// waits for this one to end, which it never will. A change to the tree that has to save a
    /// note does it before taking this, or writes the file itself.
    pub fn subtree(&self, places: &[&Path]) -> TreeTurn<'_> {
        let keys: Vec<PathBuf> = places.iter().map(|p| key(p)).collect();
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        while !keys.iter().all(|k| busy.tree_is_free(k)) {
            busy = self.freed.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        busy.trees.extend(keys.iter().cloned());
        TreeTurn { locks: self, keys }
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        let busy = self.busy.lock().unwrap();
        busy.notes.len() + busy.trees.len()
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut busy = self.locks.busy.lock().unwrap_or_else(|e| e.into_inner());
        busy.notes.remove(&self.key);
        self.locks.freed.notify_all();
    }
}

impl Drop for TreeTurn<'_> {
    fn drop(&mut self) {
        let mut busy = self.locks.busy.lock().unwrap_or_else(|e| e.into_inner());
        for k in &self.keys {
            if let Some(i) = busy.trees.iter().position(|t| t == k) {
                busy.trees.swap_remove(i);
            }
        }
        self.locks.freed.notify_all();
    }
}

/// One key per file: Windows paths differ in case and separator without naming a different file.
fn key(path: &Path) -> PathBuf {
    let components: PathBuf = path.components().collect();
    if cfg!(windows) {
        PathBuf::from(components.to_string_lossy().to_lowercase())
    } else {
        components
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    #[test]
    fn a_second_turn_on_the_same_note_waits_for_the_first() {
        let locks = Arc::new(NoteLocks::default());
        let first = locks.turn(Path::new("/v/a.md"));

        let (done, finished) = mpsc::channel();
        let waiter = {
            let locks = Arc::clone(&locks);
            std::thread::spawn(move || {
                let _t = locks.turn(Path::new("/v/a.md"));
                done.send(()).unwrap();
            })
        };
        assert!(
            finished.recv_timeout(Duration::from_millis(200)).is_err(),
            "the second save to a note must not start while the first runs"
        );
        drop(first);
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("it starts once the first ends");
        waiter.join().unwrap();
    }

    #[test]
    fn another_note_does_not_wait() {
        let locks = NoteLocks::default();
        let _a = locks.turn(Path::new("/v/a.md"));
        let _b = locks.turn(Path::new("/v/b.md")); // would deadlock this thread if it waited
    }

    #[cfg(windows)]
    #[test]
    fn spellings_of_one_file_share_a_turn() {
        assert_eq!(key(Path::new(r"C:\V\A.md")), key(Path::new("c:/v/a.md")));
    }

    #[test]
    fn a_note_nobody_is_saving_is_forgotten() {
        let locks = NoteLocks::default();
        drop(locks.turn(Path::new("/v/a.md")));
        drop(locks.subtree(&[Path::new("/v/box"), Path::new("/v/c.md")]));
        assert_eq!(locks.tracked(), 0);
    }

    /// Runs `take` on another thread; reports whether it was still waiting after a short while,
    /// and hands back a way to wait for it to finish.
    fn blocked(take: impl FnOnce() + Send + 'static) -> (bool, std::thread::JoinHandle<()>) {
        let (done, finished) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            take();
            let _ = done.send(()); // nobody listens once the wait is over
        });
        let waited = finished.recv_timeout(Duration::from_millis(200)).is_err();
        (waited, handle)
    }

    #[test]
    fn a_change_to_the_tree_waits_for_saves_under_it() {
        let locks = Arc::new(NoteLocks::default());
        let saving = locks.turn(Path::new("/v/box/a.md"));
        let (waited, change) = blocked({
            let locks = Arc::clone(&locks);
            move || drop(locks.subtree(&[Path::new("/v/box")]))
        });
        assert!(waited, "renaming a folder must wait for a save inside it");
        drop(saving);
        change.join().unwrap();
    }

    #[test]
    fn saves_under_a_changing_tree_wait_for_it() {
        let locks = Arc::new(NoteLocks::default());
        let changing = locks.subtree(&[Path::new("/v/box")]);
        let (waited, save) = blocked({
            let locks = Arc::clone(&locks);
            move || drop(locks.turn(Path::new("/v/box/deep/a.md")))
        });
        assert!(
            waited,
            "a save under a folder being moved must wait for the move"
        );
        drop(changing);
        save.join().unwrap();
    }

    #[test]
    fn creating_in_a_folder_being_deleted_waits_for_the_delete() {
        // Creating takes the folder's own turn: the folder itself, not something under it.
        let locks = Arc::new(NoteLocks::default());
        let deleting = locks.subtree(&[Path::new("/v/box")]);
        let (waited, create) = blocked({
            let locks = Arc::clone(&locks);
            move || drop(locks.turn(Path::new("/v/box")))
        });
        assert!(
            waited,
            "a note created in a folder being deleted would be deleted unkept"
        );
        drop(deleting);
        create.join().unwrap();

        let creating = locks.turn(Path::new("/v/box"));
        let (waited, delete) = blocked({
            let locks = Arc::clone(&locks);
            move || drop(locks.subtree(&[Path::new("/v")]))
        });
        assert!(
            waited,
            "and deleting the folder above waits for the creation"
        );
        drop(creating);
        delete.join().unwrap();
    }

    #[test]
    fn what_lies_outside_a_changing_tree_does_not_wait() {
        let locks = NoteLocks::default();
        let _changing = locks.subtree(&[Path::new("/v/box")]);
        // Each would deadlock this thread if it waited.
        let _save = locks.turn(Path::new("/v/boxes/a.md"));
        let _other = locks.subtree(&[Path::new("/v/other")]);
    }

    #[test]
    fn changes_to_nested_places_wait_for_each_other_both_ways() {
        let locks = Arc::new(NoteLocks::default());
        let outer = locks.subtree(&[Path::new("/v/box")]);
        let (waited, inner) = blocked({
            let locks = Arc::clone(&locks);
            move || drop(locks.subtree(&[Path::new("/v/box/in")]))
        });
        assert!(waited, "a change inside a folder being changed waits");
        drop(outer);
        inner.join().unwrap();

        let inner = locks.subtree(&[Path::new("/v/box/in")]);
        let (waited, outer) = blocked({
            let locks = Arc::clone(&locks);
            move || drop(locks.subtree(&[Path::new("/v/box")]))
        });
        assert!(waited, "a change around a folder being changed waits");
        drop(inner);
        outer.join().unwrap();
    }
}

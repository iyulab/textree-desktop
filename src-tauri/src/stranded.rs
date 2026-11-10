//! Edits that could not be written to a note already left, kept until they are.
//!
//! While the application runs, such an edit waits in memory and is tried again with every save.
//! Kept only there, it would be lost when the application closes before the note's folder takes it —
//! a folder that stopped answering, or a note that changed on disk and could not be copied beside.
//! So each one is also written here, one file per edit, and removed once it lands; the next time the
//! folder is opened, whatever is still here is tried again.
//!
//! These live with the folder's settings, outside the folder itself. They are not drafts — a draft is
//! something being worked on; these are saves that did not happen — so they have a place of their own.
//! Older releases do not look here, so adding it changes nothing they read.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;
use tauri_kit_state::Outbox;

/// The directory, inside a folder's settings directory.
pub const DIR: &str = "stranded";

/// One kept edit as stored: the note's path inside its folder, what to write, and what the note held
/// when the edit started from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    pub rel: String,
    pub text: String,
    pub base: String,
    /// When it was kept, in milliseconds since the Unix epoch.
    pub at: u64,
}

/// Where kept edits are, as files `<when>-<n>.json`.
fn outbox(dir: &Path) -> Outbox {
    Outbox::new(dir, "json")
}

/// Keeps `kept` in `dir`, under a name that orders by when it was kept. Returns the name.
///
/// `write` must create the file and refuse with [`io::ErrorKind::AlreadyExists`] when one is
/// there: edits of different notes are kept at the same moment (closing keeps every unwritten
/// one at once), and a name checked free and then written could be taken in between — writing
/// over another note's edit.
pub fn keep(
    dir: &Path,
    kept: &Kept,
    write: impl Fn(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<String> {
    let body = serde_json::to_vec(kept).map_err(io::Error::other)?;
    outbox(dir).keep(kept.at, &body, write)
}

/// Every kept edit in `dir`, oldest first. A file that cannot be read is left where it is — it may be
/// the only copy of what someone typed — and skipped.
pub fn list(dir: &Path) -> Vec<(String, Kept)> {
    outbox(dir)
        .list()
        .into_iter()
        .filter_map(|(id, bytes)| match serde_json::from_slice::<Kept>(&bytes) {
            Ok(kept) => Some((id, kept)),
            Err(e) => {
                log::warn!("stranded: {id} could not be read, left in place: {e}");
                None
            }
        })
        .collect()
}

/// Removes a kept edit once it has landed.
pub fn forget(dir: &Path, id: &str) -> io::Result<()> {
    outbox(dir).forget(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn plain_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::File::create_new(path)?.write_all(bytes)
    }

    fn kept(rel: &str, text: &str, at: u64) -> Kept {
        Kept {
            rel: rel.into(),
            text: text.into(),
            base: "before".into(),
            at,
        }
    }

    #[test]
    fn kept_edits_come_back_oldest_first_and_go_once_forgotten() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(DIR);
        let second = keep(&dir, &kept("b.md", "b", 20), plain_write).unwrap();
        let first = keep(&dir, &kept("a.md", "a", 10), plain_write).unwrap();
        let same_moment = keep(&dir, &kept("a.md", "a again", 10), plain_write).unwrap();
        assert_ne!(
            first, same_moment,
            "two edits kept in the same moment are both kept"
        );

        let listed: Vec<String> = list(&dir).into_iter().map(|(_, k)| k.text).collect();
        assert_eq!(listed, ["a", "a again", "b"]);

        forget(&dir, &first).unwrap();
        forget(&dir, &first).unwrap(); // already gone is fine
        let listed: Vec<String> = list(&dir).into_iter().map(|(_, k)| k.text).collect();
        assert_eq!(listed, ["a again", "b"]);
        assert!(forget(&dir, &second).is_ok());
    }

    #[test]
    fn a_file_that_cannot_be_read_is_left_in_place() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("0000000000010-0.json"), b"{half").unwrap();
        assert!(list(dir).is_empty());
        assert!(dir.join("0000000000010-0.json").exists());
    }

    #[test]
    fn nothing_outside_the_directory_can_be_named() {
        let tmp = TempDir::new().unwrap();
        assert!(forget(tmp.path(), "../settings").is_err());
        assert!(forget(tmp.path(), "").is_err());
    }
}

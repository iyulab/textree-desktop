//! What someone is typing into an alternative, kept until it becomes one of its versions.
//!
//! An alternative's versions live in history; the text being worked on between versions does not
//! belong there yet, and it must not touch the note's file either — the note is the other side of
//! the choice. So it is kept with the folder's settings, outside the folder: one file per
//! alternative, at `drafts/<alternative>/<the note's path>`. Going back to the note leaves it there
//! (nothing is asked), and coming back to the alternative picks it up again.
//!
//! When the alternative ends — used or set aside — whatever is still here is added to it as a
//! version first, so the ended alternative holds what was last typed, and then this is removed.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The directory, inside a folder's settings directory.
pub const DIR: &str = "drafts";

/// Held across "is the alternative still open → write its draft" and across "add the draft as a
/// version → end the alternative → remove the draft", so a draft written while an alternative ends
/// can neither be missed nor left behind for an alternative that no longer exists.
pub static TURN: Mutex<()> = Mutex::new(());

/// Where the draft of alternative `id` for the note at `rel` (`/`-separated, inside the folder) is
/// kept, under `dir`. `None` when either could name a file anywhere else.
pub fn file(dir: &Path, id: &str, rel: &str) -> Option<PathBuf> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    crate::vault_paths::plain(rel).ok()?;
    Some(rel.split('/').fold(dir.join(id), |p, name| p.join(name)))
}

/// The draft at `file`, or `None` when nothing is being worked on.
pub fn read(file: &Path) -> io::Result<Option<String>> {
    match std::fs::read(file) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| io::Error::other("the draft is not text")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Removes everything kept for alternative `id`.
pub fn remove(dir: &Path, id: &str) -> io::Result<()> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::other("not an alternative"));
    }
    match std::fs::remove_dir_all(dir.join(id)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_draft_sits_under_its_alternative_at_the_notes_path() {
        let dir = Path::new("/state/drafts");
        assert_eq!(
            file(dir, "ab12", "notes/plan.md"),
            Some(dir.join("ab12").join("notes").join("plan.md"))
        );
    }

    #[test]
    fn nothing_that_could_name_a_file_elsewhere_is_accepted() {
        let dir = Path::new("/state/drafts");
        for (id, rel) in [
            ("", "a.md"),
            ("../x", "a.md"),
            ("zz", "a.md"),
            ("ab", ""),
            ("ab", "../a.md"),
            ("ab", "a/../../b.md"),
            ("ab", "/a.md"),
            ("ab", "a//b.md"),
            ("ab", "."),
        ] {
            assert_eq!(file(dir, id, rel), None, "{id:?} {rel:?}");
        }
    }

    #[test]
    fn reading_where_nothing_was_kept_is_none() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(read(&tmp.path().join("ab").join("a.md")).unwrap(), None);
    }

    #[test]
    fn removing_takes_the_whole_alternative_and_nothing_else() {
        let tmp = TempDir::new().unwrap();
        for (id, rel) in [("ab", "x/a.md"), ("cd", "a.md")] {
            let f = file(tmp.path(), id, rel).unwrap();
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, "typed").unwrap();
        }
        remove(tmp.path(), "ab").unwrap();
        assert!(!tmp.path().join("ab").exists());
        assert_eq!(
            read(&file(tmp.path(), "cd", "a.md").unwrap())
                .unwrap()
                .as_deref(),
            Some("typed")
        );
        remove(tmp.path(), "ab").unwrap(); // already gone is fine
    }
}

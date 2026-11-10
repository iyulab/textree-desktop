//! Structural edits (M4): note/folder creation, deletion, leaf↔container promotion.
//!
//! Every operation works only within the vault root (parent validation + name validation) and is
//! reflected to the filesystem immediately. Deleting keeps the deleted state in the history
//! first (see `keep_state_of` in commands), so nothing deleted is beyond reach.

use crate::pathsafe::is_valid_name;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri_kit_fs::{claim_free_path, free_path, NameKind};

/// Runs `op` again for a moment while Windows refuses it because another program has a file it
/// touches open — a virus scanner, a search indexer or a sync tool looking at what was just
/// written. That refusal passes on its own; reporting it would say a rename or a save failed when
/// a moment later it would have gone through. Any other failure is reported at once, and so is
/// this one when it lasts (a second): then something really does hold the file.
pub(crate) use tauri_kit_fs::patiently;

/// Renames a file or folder, waiting out a refusal that passes on its own ([`patiently`]).
/// Every move in this module goes through here — the ones that undo a half-done change most of
/// all: an undo that gives up on a scanner's glance leaves the change half-done.
///
/// Never replaces: anything already at `to` is refused with [`io::ErrorKind::AlreadyExists`], in
/// the same step as the move. Every move here goes to a place meant to be free, and checking that
/// first is not enough — something can arrive in between (another move, a sync client), and
/// `std::fs::rename` would then destroy it without a trace.
pub(crate) use tauri_kit_fs::rename_new as rename;

/// Supported image extensions (lowercase). Other formats are rejected.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "avif"];

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// Validates that the parent directory is a real directory inside the vault, and the new name is safe.
fn check_parent_and_name(parent: &Path, name: &str) -> io::Result<()> {
    if !is_valid_name(name) {
        return Err(err("invalid name"));
    }
    if !parent.is_dir() {
        return Err(err("parent is not a directory"));
    }
    Ok(())
}

/// Creates the `parent/stem.md` leaf note. Errors if it already exists.
pub fn create_note(parent: &Path, stem: &str) -> io::Result<PathBuf> {
    check_parent_and_name(parent, stem)?;
    let path = parent.join(format!("{stem}.md"));
    write_new(&path, b"").map_err(|e| match e.kind() {
        io::ErrorKind::AlreadyExists => err("an item with the same name already exists"),
        _ => e,
    })?;
    Ok(path)
}

/// Creates an empty `Untitled.md` leaf in `parent`, auto-numbering on collision
/// (`Untitled`, `Untitled (1)`, …). The name is generated and always
/// valid, so unlike `create_note` there is no explicit name to validate.
pub fn create_untitled_note(parent: &Path) -> io::Result<PathBuf> {
    if !parent.is_dir() {
        return Err(err("parent is not a directory"));
    }
    write_unique(parent, "Untitled.md", b"")
}

/// Creates `parent/<stem>.md` with `content`, auto-numbering on collision
/// (`Summary of X`, `Summary of X (1)`, …). Like the other `create_*` fns it only ever creates a
/// file ([`write_new`]), so no existing data is at risk and no atomic_write / self_write is needed
/// (the watcher reflects the new file into the tree).
pub fn create_note_with_content(parent: &Path, stem: &str, content: &str) -> io::Result<PathBuf> {
    check_parent_and_name(parent, stem)?;
    write_unique(parent, &format!("{stem}.md"), content.as_bytes())
}

/// Creates the `parent/name/` container and its folder note `parent/name/name.md`.
/// Also creates the folder note so it becomes an immediately editable container note (design §3.1).
pub fn create_folder(parent: &Path, name: &str) -> io::Result<PathBuf> {
    check_parent_and_name(parent, name)?;
    let dir = parent.join(name);
    if dir.exists() {
        return Err(err("an item with the same name already exists"));
    }
    std::fs::create_dir(&dir)?;
    write_new(&dir.join(format!("{name}.md")), b"")?;
    Ok(dir)
}

/// Promotes the leaf note `dir/foo.md` into the container `dir/foo/foo.md` (design §3.3).
/// An operation that makes room to create children. Returns the new container directory.
pub fn promote_leaf(leaf_md: &Path) -> io::Result<PathBuf> {
    if leaf_md.extension().and_then(|e| e.to_str()) != Some("md") || !leaf_md.is_file() {
        return Err(err("not a leaf note (.md file)"));
    }
    let parent = leaf_md.parent().ok_or_else(|| err("no parent directory"))?;
    let stem = leaf_md
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| err("cannot read file name"))?;
    let new_dir = parent.join(stem);
    if new_dir.exists() {
        return Err(err("a folder with the same name already exists"));
    }
    std::fs::create_dir(&new_dir)?;
    let new_body = new_dir.join(format!("{stem}.md"));
    rename(leaf_md, &new_body)?;
    Ok(new_dir)
}

/// Creates `path` holding `bytes`, refusing with [`io::ErrorKind::AlreadyExists`] if anything is
/// there. Creating is never writing over: a name checked free can be taken before the write — by a
/// sync client, another program — and `std::fs::write` would then empty that file into this one.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    std::fs::File::create_new(path)?.write_all(bytes)
}

/// Creates a new file for `file_name` in `dir` under the first free name (`name (1)`, …), moving on
/// to the next if that one is taken before it is created. Returns where it went.
fn write_unique(dir: &Path, file_name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    claim_free_path(dir, file_name, NameKind::File, |path| {
        write_new(path, bytes)
    })
    .map(|(path, ())| path)
}

/// Renames a node. A leaf `.md` renames the file; a container renames the directory and its
/// folder note together (node identity = file/folder name, design §3.5). Returns the new path.
pub fn rename_node(target: &Path, new_name: &str) -> io::Result<PathBuf> {
    if !is_valid_name(new_name) {
        return Err(err("invalid name"));
    }
    let parent = target.parent().ok_or_else(|| err("no parent directory"))?;

    if target.is_dir() {
        let old_name = target
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| err("cannot read name"))?
            .to_string();
        let new_dir = parent.join(new_name);
        if new_dir.exists() {
            return Err(err("an item with the same name already exists"));
        }
        rename(target, &new_dir)?;
        // If a folder note exists, rename it along to match the new folder name.
        let old_body = new_dir.join(format!("{old_name}.md"));
        if old_body.is_file() {
            if let Err(e) = rename(&old_body, &new_dir.join(format!("{new_name}.md"))) {
                // Step 2 failed → put the folder back, so it is not left renamed without its note.
                // If that fails too, say where things are rather than leave a half-done rename
                // looking like one that did not happen.
                return Err(match rename(&new_dir, target) {
                    Ok(()) => e,
                    Err(back) => err(format!(
                        "the folder was renamed but its note could not be, and putting the folder \
                         back failed too. The folder is now at {}. (rename error: {e}; rollback \
                         error: {back})",
                        new_dir.display()
                    )),
                });
            }
        }
        Ok(new_dir)
    } else {
        if target.extension().and_then(|e| e.to_str()) != Some("md") {
            return Err(err("rename target is not a note/folder"));
        }
        let new_path = parent.join(format!("{new_name}.md"));
        if new_path.exists() {
            return Err(err("an item with the same name already exists"));
        }
        rename(target, &new_path)?;
        Ok(new_path)
    }
}

/// Renames a leaf `.md` note to `desired_name`, auto-numbering on collision (`name`, `name (1)`,
/// …). Used by the T4 H1→filename sync of *unnamed* notes — distinct from
/// `rename_node`, whose explicit-rename collision is a hard error (never auto-numbered). Never
/// overwrites an existing file. Returns the new path.
pub fn rename_note_unique(target: &Path, desired_name: &str) -> io::Result<PathBuf> {
    if !is_valid_name(desired_name) {
        return Err(err("invalid name"));
    }
    if target.extension().and_then(|e| e.to_str()) != Some("md") || !target.is_file() {
        return Err(err("rename target is not a leaf note"));
    }
    let parent = target.parent().ok_or_else(|| err("no parent directory"))?;
    // A name found free can be taken before the move lands (a sync client): the next one instead.
    claim_free_path(
        parent,
        &format!("{desired_name}.md"),
        NameKind::File,
        |dest| rename(target, dest),
    )
    .map(|(dest, ())| dest)
}

/// Moves a node to a different folder (changes its parent). Returns the new path.
pub fn move_node(src: &Path, dest_dir: &Path) -> io::Result<PathBuf> {
    if !dest_dir.is_dir() {
        return Err(err("invalid destination folder"));
    }
    let name = src
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| err("cannot read name"))?;
    // Block moving a folder into itself or its descendants.
    let src_c = src.canonicalize()?;
    let dest_dir_c = dest_dir.canonicalize()?;
    if dest_dir_c == src_c || dest_dir_c.starts_with(&src_c) {
        return Err(err("cannot move into itself or a descendant folder"));
    }
    let dest = dest_dir.join(name);
    if dest.exists() {
        return Err(err(
            "an item with the same name already exists in the destination folder",
        ));
    }
    rename(src, &dest)?;
    Ok(dest)
}

/// Promotes a leaf note into a container, then moves the `src` node inside it (design §3.3).
/// "Drop X onto note Y = make X a child of Y". Bundles promote+move into a single operation, and
/// on move failure rolls back the promotion best-effort so no partial state (a lone folder) is left.
/// Returns the new path of the moved `src`.
pub fn adopt_into_leaf(src: &Path, leaf_md: &Path) -> io::Result<PathBuf> {
    // Pre-mutation validation: disallowed if leaf equals src or is its descendant (move into self).
    let src_c = src.canonicalize()?;
    let leaf_c = leaf_md.canonicalize()?;
    if leaf_c == src_c || leaf_c.starts_with(&src_c) {
        return Err(err("cannot move into itself or a descendant"));
    }
    // Promote (promote_leaf validates the .md leaf condition).
    let new_dir = promote_leaf(leaf_md)?;
    // Move; on failure roll back the promotion (right after promotion new_dir holds only the folder note).
    match move_node(src, &new_dir) {
        Ok(p) => Ok(p),
        Err(move_err) => {
            let stem = new_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let body = new_dir.join(format!("{stem}.md"));
            // If the rollback rename fails (e.g. a transient lock from Windows AV/indexer), the
            // original note vanishes trapped inside new_dir. Rather than swallow it silently, report
            // its location so it can be recovered.
            match rename(&body, leaf_md) {
                Ok(()) => {
                    let _ = std::fs::remove_dir(&new_dir);
                    Err(move_err)
                }
                Err(rollback_err) => Err(err(format!(
                    "the move failed and the rollback also failed. The original note is at {}. \
                     (move error: {move_err}; rollback error: {rollback_err})",
                    body.display()
                ))),
            }
        }
    }
}

/// Saves attachment image bytes to `assets/` in the same folder as the current note body
/// (`note_body`, .md) (design §3.2 attachment mapping). Returns the **relative path** to insert into
/// the body (always forward-slash, e.g. `assets/Pasted-1718....png`) — consistent because the .md
/// and `assets/` share a parent.
pub fn save_attachment(note_body: &Path, bytes: &[u8], ext: &str) -> io::Result<String> {
    // Attachments are placed only relative to the note body (.md) — prevents creating assets/ next to an arbitrary path (server-side contract).
    if note_body.extension().and_then(|e| e.to_str()) != Some("md") {
        return Err(err("attachment target is not a note (.md)"));
    }
    let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    if !IMAGE_EXTS.contains(&ext.as_str()) {
        return Err(err("unsupported image format"));
    }
    let parent = note_body
        .parent()
        .ok_or_else(|| err("the note has no parent directory"))?;
    let assets = parent.join("assets");
    std::fs::create_dir_all(&assets)?;
    // Ensure uniqueness without dependencies: an epoch-millis-based name, numbered on collision.
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dest = write_unique(&assets, &format!("Pasted-{millis}.{ext}"), bytes)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| err("cannot read file name"))?;
    Ok(format!("assets/{name}"))
}

/// Decides where a file coming back from history should land, creating the folders it needs.
///
/// Nothing is written here and nothing existing is replaced: a name already in use is numbered
/// alongside rather than overwritten, so bringing something back can never cost what is there.
///
/// Callers still check the names themselves against the vault's naming policy (`is_valid_name`).
pub(crate) fn place_restored(root: &Path, rel: &str) -> io::Result<PathBuf> {
    // Nothing is created until the destination has been vouched for: every part a plain name, and
    // nothing on the way — a folder that is a link to somewhere else — leading out of the vault.
    let dest = crate::vault_paths::resolve(&root.to_string_lossy(), rel).map_err(err)?;
    let parent = dest.parent().ok_or_else(|| err("no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let file_name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| err("cannot read name"))?;
    Ok(free_path(parent, file_name, NameKind::File))
}

/// Answers "is this file there?" for many files by listing each folder once, instead of asking
/// the filesystem about every file. On Windows a per-file check costs tens of microseconds, so
/// ten thousand of them cost the better part of a second — while the folders holding them list in
/// a few milliseconds.
///
/// Names are compared ignoring case on Windows and macOS, exactly elsewhere; a name not found that
/// way is checked with the filesystem, so the answer is the one a per-file check would give.
#[derive(Default)]
pub(crate) struct PresentFiles {
    folders: std::collections::HashMap<PathBuf, Option<std::collections::HashSet<String>>>,
}

impl PresentFiles {
    fn key(name: &std::ffi::OsStr) -> String {
        let name = name.to_string_lossy();
        if cfg!(any(windows, target_os = "macos")) {
            name.to_lowercase()
        } else {
            name.into_owned()
        }
    }

    /// Whether a file (not a folder) is at `path`.
    pub(crate) fn contains(&mut self, path: &Path) -> bool {
        let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
            return path.is_file();
        };
        let listing = self.folders.entry(folder.to_path_buf()).or_insert_with(|| {
            let entries = std::fs::read_dir(folder).ok()?;
            Some(
                entries
                    .flatten()
                    .filter(|e| e.file_type().map(|t| !t.is_dir()).unwrap_or(false))
                    .map(|e| Self::key(&e.file_name()))
                    .collect(),
            )
        });
        // A name the listing does not have is confirmed with the filesystem itself. Folding case is
        // not all a filesystem does to names — macOS also treats Unicode NFC and NFD spellings of a
        // name as one — and a note wrongly reported missing would be listed as deleted. Missing
        // files are the few (deleted notes), so this costs little.
        listing
            .as_ref()
            .is_some_and(|names| names.contains(&Self::key(name)))
            || path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(windows)]
    #[test]
    fn a_refusal_that_passes_is_waited_out() {
        let mut tries = 0;
        let done = patiently(|| {
            tries += 1;
            if tries < 3 {
                Err(io::Error::from_raw_os_error(32))
            } else {
                Ok(tries)
            }
        });
        assert_eq!(done.unwrap(), 3);
    }

    #[test]
    fn any_other_failure_is_reported_at_once() {
        let mut tries = 0;
        let done: io::Result<()> = patiently(|| {
            tries += 1;
            Err(io::Error::other("the disk is full"))
        });
        assert!(done.is_err());
        assert_eq!(tries, 1);
    }

    /// Opens `path` the way a scanner or indexer does: letting others read it, but not move it.
    #[cfg(windows)]
    fn hold_open(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
            .unwrap()
    }

    /// A folder `box` holding its note and one other note, with the other note held open.
    #[cfg(windows)]
    fn folder_with_a_held_note(root: &Path) -> std::fs::File {
        let dir = root.join("box");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("box.md"), "folder").unwrap();
        std::fs::write(dir.join("inside.md"), "inside").unwrap();
        hold_open(&dir.join("inside.md"))
    }

    #[cfg(windows)]
    #[test]
    fn a_folder_another_program_looks_into_for_a_moment_is_still_renamed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let held = folder_with_a_held_note(root);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(held);
        });

        let renamed = rename_node(&root.join("box"), "crate").unwrap();
        release.join().unwrap();

        assert_eq!(renamed, root.join("crate"));
        assert!(
            root.join("crate").join("crate.md").is_file(),
            "its note follows"
        );
        assert!(root.join("crate").join("inside.md").is_file());
        assert!(!root.join("box").exists());
    }

    #[cfg(windows)]
    #[test]
    fn negative_control_a_folder_held_the_whole_time_is_refused_by_the_system() {
        // Holding a note inside really makes the system refuse the rename — so the test above
        // passes because the rename waited, not because nothing was refused.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let _held = folder_with_a_held_note(root);

        let refused = rename_node(&root.join("box"), "crate").unwrap_err();
        assert!(
            matches!(refused.raw_os_error(), Some(5 | 32)),
            "{refused:?}"
        );
        assert!(root.join("box").join("box.md").is_file(), "nothing moved");
        assert!(!root.join("crate").exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_note_moved_while_another_program_looks_at_it_still_moves() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("to")).unwrap();
        std::fs::write(root.join("a.md"), "a").unwrap();
        let held = hold_open(&root.join("a.md"));
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(held);
        });

        let moved = move_node(&root.join("a.md"), &root.join("to")).unwrap();
        release.join().unwrap();
        assert_eq!(std::fs::read_to_string(moved).unwrap(), "a");
    }

    #[test]
    fn present_files_agrees_with_a_per_file_check() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/one.md"), "").unwrap();
        std::fs::write(root.join("a/b/two.md"), "").unwrap();
        std::fs::write(root.join("Mixed.md"), "").unwrap();

        let mut present = PresentFiles::default();
        for rel in [
            "a/one.md",
            "a/b/two.md",
            "Mixed.md",
            "mixed.md",
            "a/missing.md",
            "gone/folder.md",
            "a/b",
            "a/b/two.md/deeper.md",
        ] {
            let path = root.join(rel);
            assert_eq!(present.contains(&path), path.is_file(), "{rel}");
        }
    }

    #[test]
    fn place_restored_creates_nothing_when_the_destination_is_refused() {
        // The reason the check has to come first is a folder on the way being a link to
        // somewhere else: the folders below it would be made at the far end before anyone
        // asked where that was. The invariant is the same whatever the reason for refusing —
        // a refused destination leaves the filesystem as it found it.
        let tmp = TempDir::new().unwrap();
        let absent_root = tmp.path().join("no-such-vault");

        assert!(place_restored(&absent_root, "sub/deep/a.md").is_err());
        assert!(
            !absent_root.exists(),
            "nothing on the way to a refused destination is created"
        );
    }

    #[test]
    fn place_restored_never_points_at_something_that_exists() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("notes")).unwrap();
        std::fs::write(tmp.path().join("notes/a.md"), "current").unwrap();

        let dest = place_restored(tmp.path(), "notes/a.md").unwrap();
        assert_eq!(dest, tmp.path().join("notes").join("a (1).md"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("notes/a.md")).unwrap(),
            "current",
            "what is already there must be left alone"
        );

        // A name not in use is taken as-is, and its folder is created on the way.
        let fresh = place_restored(tmp.path(), "made/up/b.md").unwrap();
        assert_eq!(fresh, tmp.path().join("made").join("up").join("b.md"));
        assert!(tmp.path().join("made").join("up").is_dir());
    }

    #[test]
    fn create_note_makes_md_file() {
        let tmp = TempDir::new().unwrap();
        let p = create_note(tmp.path(), "new-note").unwrap();
        assert_eq!(p, tmp.path().join("new-note.md"));
        assert!(p.is_file());
    }

    #[test]
    fn create_note_rejects_duplicate_and_bad_name() {
        let tmp = TempDir::new().unwrap();
        create_note(tmp.path(), "a").unwrap();
        assert!(create_note(tmp.path(), "a").is_err());
        assert!(create_note(tmp.path(), "../escape").is_err());
        assert!(create_note(tmp.path(), ".hidden").is_err());
    }

    #[test]
    fn create_folder_makes_dir_and_folder_note() {
        let tmp = TempDir::new().unwrap();
        let dir = create_folder(tmp.path(), "journal").unwrap();
        assert!(dir.is_dir());
        assert!(
            dir.join("journal.md").is_file(),
            "the folder note should be created"
        );
    }

    #[test]
    fn promote_leaf_moves_md_into_namesake_folder() {
        let tmp = TempDir::new().unwrap();
        let leaf = tmp.path().join("foo.md");
        std::fs::write(&leaf, "body text").unwrap();
        let dir = promote_leaf(&leaf).unwrap();
        assert_eq!(dir, tmp.path().join("foo"));
        assert!(!leaf.exists(), "the original leaf file is moved");
        let body = dir.join("foo.md");
        assert!(body.is_file());
        assert_eq!(
            std::fs::read_to_string(body).unwrap(),
            "body text",
            "content preserved"
        );
    }

    #[test]
    fn rename_leaf_note() {
        let tmp = TempDir::new().unwrap();
        let leaf = tmp.path().join("old.md");
        std::fs::write(&leaf, "body text").unwrap();
        let new = rename_node(&leaf, "new").unwrap();
        assert_eq!(new, tmp.path().join("new.md"));
        assert!(!leaf.exists());
        assert_eq!(std::fs::read_to_string(new).unwrap(), "body text");
    }

    #[test]
    fn rename_container_renames_dir_and_folder_note() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("diary");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("diary.md"), "diary body").unwrap();
        std::fs::write(dir.join("2026.md"), "child").unwrap();

        let new_dir = rename_node(&dir, "journal").unwrap();
        assert_eq!(new_dir, tmp.path().join("journal"));
        assert!(
            new_dir.join("journal.md").is_file(),
            "the folder note is renamed too"
        );
        assert_eq!(
            std::fs::read_to_string(new_dir.join("journal.md")).unwrap(),
            "diary body"
        );
        assert!(new_dir.join("2026.md").is_file(), "children are unchanged");
        assert!(!dir.exists());
    }

    #[test]
    fn renaming_a_folder_after_one_of_its_notes_leaves_that_note_alone() {
        // The folder note would take the name the child already has inside the folder.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("diary");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("diary.md"), "diary body").unwrap();
        std::fs::write(dir.join("journal.md"), "a child called journal").unwrap();

        assert!(rename_node(&dir, "journal").is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("journal.md")).unwrap(),
            "a child called journal"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("diary.md")).unwrap(),
            "diary body"
        );
        assert!(
            !tmp.path().join("journal").exists(),
            "the folder is put back"
        );
    }

    #[test]
    fn creating_never_writes_over_a_file_that_is_there() {
        let tmp = TempDir::new().unwrap();
        let taken = tmp.path().join("taken.md");
        std::fs::write(&taken, "someone else's").unwrap();

        assert_eq!(
            write_new(&taken, b"").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "someone else's");
        assert!(create_note(tmp.path(), "taken").is_err());
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "someone else's");

        let made = write_unique(tmp.path(), "taken.md", b"mine").unwrap();
        assert_eq!(made, tmp.path().join("taken (1).md"));
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "someone else's");
    }

    #[test]
    fn a_link_that_points_nowhere_is_a_taken_name() {
        let tmp = TempDir::new().unwrap();
        let link = tmp.path().join("a.md");
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(tmp.path().join("gone.md"), &link);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(tmp.path().join("gone.md"), &link);
        if made.is_err() {
            return; // creating links needs a privilege this account may not have
        }
        assert!(!link.exists(), "precondition: the link points nowhere");

        let made = write_unique(tmp.path(), "a.md", b"new").unwrap();
        assert_eq!(made, tmp.path().join("a (1).md"));
        assert!(link.symlink_metadata().is_ok(), "the link is left alone");
    }

    #[test]
    fn a_move_never_replaces_what_is_already_there() {
        let tmp = TempDir::new().unwrap();
        let (from, to) = (tmp.path().join("a.md"), tmp.path().join("b.md"));
        std::fs::write(&from, "a").unwrap();
        std::fs::write(&to, "b").unwrap();

        assert_eq!(
            rename(&from, &to).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "b");
        assert_eq!(std::fs::read_to_string(&from).unwrap(), "a");
    }

    #[test]
    fn rename_container_carries_obsidian_and_canvas_inside() {
        // Renaming a folder must move everything it holds — including an Obsidian config folder and
        // a JSON Canvas file — so a vault shared with Obsidian stays lossless across the rename.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("sketchbook");
        std::fs::create_dir_all(dir.join(".obsidian")).unwrap();
        std::fs::write(dir.join("sketchbook.md"), "body text").unwrap();
        std::fs::write(dir.join(".obsidian").join("app.json"), "{}").unwrap();
        std::fs::write(dir.join("drawing.canvas"), "{\"nodes\":[]}").unwrap();

        let new_dir = rename_node(&dir, "project").unwrap();
        assert!(new_dir.join("project.md").is_file(), "folder note renamed");
        assert_eq!(
            std::fs::read_to_string(new_dir.join(".obsidian").join("app.json")).unwrap(),
            "{}",
            "the Obsidian config moved intact"
        );
        assert_eq!(
            std::fs::read_to_string(new_dir.join("drawing.canvas")).unwrap(),
            "{\"nodes\":[]}",
            "the canvas moved intact"
        );
        assert!(!dir.exists());
    }

    #[test]
    fn removing_a_note_leaves_the_files_beside_it_untouched() {
        // Only the note goes. Anything another tool keeps beside it is never collateral.
        let tmp = TempDir::new().unwrap();
        let note = tmp.path().join("doomed-note.md");
        std::fs::write(&note, "x").unwrap();
        std::fs::create_dir_all(tmp.path().join(".obsidian")).unwrap();
        std::fs::write(tmp.path().join(".obsidian").join("app.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("board.canvas"), "{}").unwrap();

        std::fs::remove_file(&note).unwrap();
        assert!(!note.exists());
        assert!(tmp.path().join(".obsidian").join("app.json").is_file());
        assert!(tmp.path().join("board.canvas").is_file());
    }

    #[test]
    fn rename_rejects_duplicate_and_bad_name() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.md"), "").unwrap();
        std::fs::write(tmp.path().join("b.md"), "").unwrap();
        assert!(rename_node(&tmp.path().join("a.md"), "b").is_err());
        assert!(rename_node(&tmp.path().join("a.md"), "..").is_err());
    }

    #[test]
    fn move_leaf_into_folder() {
        let tmp = TempDir::new().unwrap();
        let note = tmp.path().join("noteX.md");
        std::fs::write(&note, "z").unwrap();
        let folder = tmp.path().join("box");
        std::fs::create_dir(&folder).unwrap();

        let dest = move_node(&note, &folder).unwrap();
        assert_eq!(dest, folder.join("noteX.md"));
        assert!(!note.exists());
        assert!(dest.is_file());
    }

    #[test]
    fn move_rejects_into_self_or_descendant() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("p");
        let child = parent.join("c");
        std::fs::create_dir_all(&child).unwrap();
        // Cannot move p into its descendant c.
        assert!(move_node(&parent, &child).is_err());
        // Cannot move p into itself.
        assert!(move_node(&parent, &parent).is_err());
    }

    #[test]
    fn move_rejects_duplicate_at_dest() {
        let tmp = TempDir::new().unwrap();
        let note = tmp.path().join("dup.md");
        std::fs::write(&note, "").unwrap();
        let folder = tmp.path().join("box");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("dup.md"), "").unwrap(); // already exists
        assert!(move_node(&note, &folder).is_err());
    }

    #[test]
    fn adopt_promotes_leaf_and_moves_node_inside() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.md");
        let b = tmp.path().join("b.md");
        std::fs::write(&a, "alpha").unwrap();
        std::fs::write(&b, "beta").unwrap();

        let moved = adopt_into_leaf(&a, &b).unwrap();
        // b is promoted to a container and the folder note is preserved.
        let b_dir = tmp.path().join("b");
        assert!(b_dir.is_dir());
        assert_eq!(std::fs::read_to_string(b_dir.join("b.md")).unwrap(), "beta");
        // a is moved into b/, content preserved.
        assert_eq!(moved, b_dir.join("a.md"));
        assert!(!a.exists());
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "alpha");
    }

    #[test]
    fn adopt_rolls_back_promote_on_move_collision() {
        // If src's name equals the leaf stem, it collides with the new folder note (b/b.md) → move fails → rollback.
        let tmp = TempDir::new().unwrap();
        let leaf = tmp.path().join("b.md");
        std::fs::write(&leaf, "beta").unwrap();
        let box_dir = tmp.path().join("box");
        std::fs::create_dir(&box_dir).unwrap();
        let src = box_dir.join("b.md"); // triggers the collision (b/b.md already exists on move)
        std::fs::write(&src, "clash").unwrap();

        assert!(adopt_into_leaf(&src, &leaf).is_err());
        // Rollback verification: leaf restored, promotion folder removed, src preserved.
        assert!(leaf.is_file(), "the leaf should be restored");
        assert!(
            !tmp.path().join("b").exists(),
            "the promotion folder should be rolled back"
        );
        assert_eq!(std::fs::read_to_string(&leaf).unwrap(), "beta");
        assert!(src.is_file(), "src is unchanged");
    }

    #[test]
    fn save_attachment_writes_to_assets_and_returns_rel_link() {
        let tmp = TempDir::new().unwrap();
        let note = tmp.path().join("note.md");
        std::fs::write(&note, "body text").unwrap();
        let bytes = b"\x89PNG\r\n\x1a\n fake png";

        let rel = save_attachment(&note, bytes, "png").unwrap();
        assert!(
            rel.starts_with("assets/"),
            "the relative link is under assets/"
        );
        assert!(rel.ends_with(".png"));
        // The actual file exists on disk in the same folder's assets/ and its content matches.
        let name = rel.strip_prefix("assets/").unwrap();
        let on_disk = tmp.path().join("assets").join(name);
        assert!(on_disk.is_file());
        assert_eq!(std::fs::read(on_disk).unwrap(), bytes);
    }

    #[test]
    fn save_attachment_normalizes_and_rejects_extension() {
        let tmp = TempDir::new().unwrap();
        let note = tmp.path().join("n.md");
        std::fs::write(&note, "").unwrap();
        // Allows normalization of uppercase / leading dot.
        assert!(save_attachment(&note, b"x", ".JPEG")
            .unwrap()
            .ends_with(".jpeg"));
        // Rejects non-images.
        assert!(save_attachment(&note, b"x", "exe").is_err());
    }

    #[test]
    fn save_attachment_does_not_overwrite_on_repeat() {
        let tmp = TempDir::new().unwrap();
        let note = tmp.path().join("n.md");
        std::fs::write(&note, "").unwrap();
        let a = save_attachment(&note, b"a", "png").unwrap();
        let b = save_attachment(&note, b"b", "png").unwrap();
        assert_ne!(
            a, b,
            "the two attachments must be different files (no overwriting)"
        );
        let count = std::fs::read_dir(tmp.path().join("assets"))
            .unwrap()
            .count();
        assert_eq!(count, 2);
    }

    #[test]
    fn save_attachment_rejects_non_md_target() {
        // Passing an existing non-.md path (e.g. an image file) as the note is rejected — prevents a misplaced assets/.
        let tmp = TempDir::new().unwrap();
        let img = tmp.path().join("pic.png");
        std::fs::write(&img, "x").unwrap();
        assert!(save_attachment(&img, b"y", "png").is_err());
        assert!(
            !tmp.path().join("assets").exists(),
            "assets is not created on rejection"
        );
    }

    #[test]
    fn save_attachment_folder_note_uses_sibling_assets() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("bar");
        std::fs::create_dir(&dir).unwrap();
        let body = dir.join("bar.md"); // folder note
        std::fs::write(&body, "diary body").unwrap();

        let rel = save_attachment(&body, b"img", "gif").unwrap();
        assert!(
            rel.starts_with("assets/"),
            "for a folder note too, the relative link is assets/"
        );
        let name = rel.strip_prefix("assets/").unwrap();
        assert!(
            dir.join("assets").join(name).is_file(),
            "assets sits next to the folder note (bar/assets)"
        );
    }

    #[test]
    fn adopt_rejects_leaf_inside_src() {
        // If the leaf is inside the dragged node (src), reject before any mutation (don't even promote).
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("p");
        std::fs::create_dir(&src).unwrap();
        let leaf = src.join("child.md");
        std::fs::write(&leaf, "child").unwrap();

        assert!(adopt_into_leaf(&src, &leaf).is_err());
        assert!(leaf.is_file(), "no mutation on rejection");
        assert!(!src.join("child").exists(), "not promoted");
    }

    #[test]
    fn create_untitled_note_generates_and_auto_numbers() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let p1 = create_untitled_note(root).unwrap();
        assert_eq!(p1.file_name().unwrap().to_str().unwrap(), "Untitled.md");
        assert!(p1.is_file());
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "");

        let p2 = create_untitled_note(root).unwrap();
        assert_eq!(p2.file_name().unwrap().to_str().unwrap(), "Untitled (1).md");

        let p3 = create_untitled_note(root).unwrap();
        assert_eq!(p3.file_name().unwrap().to_str().unwrap(), "Untitled (2).md");
    }

    #[test]
    fn rename_note_unique_renames_leaf() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let src = root.join("Untitled.md");
        std::fs::write(&src, "# Meeting\n").unwrap();

        let dest = rename_note_unique(&src, "Meeting").unwrap();
        assert_eq!(dest.file_name().unwrap().to_str().unwrap(), "Meeting.md");
        assert!(dest.is_file());
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "# Meeting\n");
    }

    #[test]
    fn rename_note_unique_auto_numbers_on_collision() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("Notes.md"), "existing\n").unwrap();
        let src = root.join("Untitled.md");
        std::fs::write(&src, "new\n").unwrap();

        let dest = rename_note_unique(&src, "Notes").unwrap();
        assert_eq!(dest.file_name().unwrap().to_str().unwrap(), "Notes (1).md");
        assert!(root.join("Notes.md").is_file()); // existing note untouched
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new\n");
    }

    #[test]
    fn rename_note_unique_rejects_an_invalid_name() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let src = root.join("Untitled.md");
        std::fs::write(&src, "x\n").unwrap();

        assert!(rename_note_unique(&src, "a/b").is_err()); // separator rejected by is_valid_name
        assert!(src.exists()); // not renamed on rejection
    }

    #[test]
    fn rename_note_unique_rejects_non_leaf_targets() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // A directory is not a leaf .md note.
        let dir = root.join("folder");
        std::fs::create_dir(&dir).unwrap();
        assert!(rename_note_unique(&dir, "x").is_err());
        // A non-.md file is not a leaf note.
        let txt = root.join("note.txt");
        std::fs::write(&txt, "x").unwrap();
        assert!(rename_note_unique(&txt, "x").is_err());
    }

    #[test]
    fn rename_node_still_rejects_collision() {
        // Regression: the explicit rename path must keep rejecting (not auto-number).
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.md"), "a\n").unwrap();
        std::fs::write(root.join("b.md"), "b\n").unwrap();
        assert!(rename_node(&root.join("a.md"), "b").is_err());
    }

    #[test]
    fn create_note_with_content_writes_and_auto_numbers() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let p1 = create_note_with_content(root, "Summary of X", "# hi\n").unwrap();
        assert_eq!(p1, root.join("Summary of X.md"));
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "# hi\n");
        // Second save with the same base name auto-numbers instead of overwriting.
        let p2 = create_note_with_content(root, "Summary of X", "# other\n").unwrap();
        assert_eq!(p2, root.join("Summary of X (1).md"));
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "# hi\n"); // original untouched
    }

    #[test]
    fn create_note_with_content_rejects_a_bad_name() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("vault");
        std::fs::create_dir(&root).unwrap();
        // separator in the stem -> is_valid_name rejects
        assert!(create_note_with_content(&root, "a/b", "x").is_err());
    }
}

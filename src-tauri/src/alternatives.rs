//! Alternatives: a second version of a note, kept beside the one in the folder until a person
//! picks one.
//!
//! An alternative comes from one of two places. Someone starts one to try another take on a
//! note, or an exchange brings in a note that was changed both here and elsewhere — the version
//! from elsewhere waits as an alternative instead of being merged into this one.
//!
//! Everything is held in references, so nothing new is stored beside the notes:
//!
//! - an open alternative is `refs/textree/alternatives/<id>`. Its first revision says which note
//!   it is an alternative of (and, for one that arrived, the revision it arrived with); the ones
//!   after it are versions added to it;
//! - an alternative that has ended — picked or set aside — moves to `refs/textree/ended/<id>`
//!   unchanged, so what it held stays reachable;
//! - picking one is an ordinary revision on the notes reference that names the alternative.
//!
//! Nothing here touches the index or the working tree. The caller writes the picked version to
//! the note's file.

use crate::git_engine::{
    advancing, blob_in_tree, commit_paths, extend_once, slashed, tip_tree, WhenUnchanged, NOTES_REF,
};
use git2::{Oid, Repository, Signature};
use std::path::{Path, PathBuf};

/// Where open alternatives live.
pub const ALTERNATIVES: &str = "refs/textree/alternatives/";
/// Where alternatives go when they end, whichever way they end.
pub const ENDED: &str = "refs/textree/ended/";

/// Names the note an alternative is an alternative of, in its first revision.
const ALTERNATIVE_OF: &str = "Alternative-of";
/// Names the revision an alternative arrived with, in its first revision.
const ARRIVED_FROM: &str = "Arrived-from";
/// Names the alternative a revision of the notes reference took its content from.
const PICKED: &str = "Picked-alternative";

/// How far back an alternative's first revision may be. An alternative holds versions of one
/// note, added one at a time; this bound only keeps a damaged reference from walking all history.
const MAX_WALK: usize = 10_000;

/// One open alternative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alternative {
    /// Opaque, stable for the alternative's whole life (including after it ends).
    pub id: String,
    /// The note it is an alternative of, as the repository spells it (`/`-separated).
    pub path: String,
    /// The revision it arrived with, when it came from elsewhere rather than being started here.
    pub arrived_from: Option<Oid>,
    /// Its newest revision.
    pub tip: Oid,
    /// When its newest revision was written, in seconds since the epoch.
    pub seconds: i64,
    /// Who its newest revision is recorded as being written by.
    pub author: String,
}

fn error(message: impl AsRef<str>) -> git2::Error {
    git2::Error::from_str(message.as_ref())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn open_ref(id: &str) -> String {
    format!("{ALTERNATIVES}{id}")
}

fn ended_ref(id: &str) -> String {
    format!("{ENDED}{id}")
}

fn target(repo: &Repository, name: &str) -> Option<Oid> {
    repo.find_reference(name).ok().and_then(|r| r.target())
}

/// The value of `key` among the trailing `Key: value` lines of a revision message.
fn trailer<'m>(message: &'m str, key: &str) -> Option<&'m str> {
    let prefix = format!("{key}: ");
    message
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .map(str::trim)
}

fn random_id() -> Result<String, git2::Error> {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| error(format!("no randomness for a new id: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The id of the alternative a revision brings for a note. Taking in the same revision again
/// finds the same alternative instead of making another.
fn arrival_id(theirs: Oid, path: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{theirs}\0{path}").as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Writes an alternative's first revision on top of `parent` and opens its reference.
fn open(
    repo: &Repository,
    id: &str,
    parent: &git2::Commit<'_>,
    message: &str,
    author: &Signature<'_>,
    committer: &Signature<'_>,
) -> Result<(), git2::Error> {
    let tree = parent.tree()?;
    let first = repo.commit(None, author, committer, message, &tree, &[parent])?;
    repo.reference(&open_ref(id), first, false, "alternative: start")?;
    Ok(())
}

/// Starts an alternative of the note at `subject` from its last recorded version.
///
/// Only notes can have alternatives — every other file is shared by all versions of the folder —
/// and only recorded ones: an alternative is a second
/// version beside one that exists, and until a version is added there is nothing to be beside.
pub fn start(
    repo: &Repository,
    subject: &Path,
    author: &Signature<'_>,
    committer: &Signature<'_>,
) -> Result<String, git2::Error> {
    let path = slashed(subject);
    if !path.to_ascii_lowercase().ends_with(".md") {
        return Err(error("only notes can have alternatives"));
    }
    let tip = repo
        .find_reference(NOTES_REF)
        .and_then(|r| r.peel_to_commit())
        .map_err(|_| {
            error("add a version of this note first — an alternative starts from its last version")
        })?;
    if blob_in_tree(repo, &tip.tree()?, subject).is_none() {
        return Err(error(
            "add a version of this note first — an alternative starts from its last version",
        ));
    }
    let id = random_id()?;
    let message = format!("Alternative\n\n{ALTERNATIVE_OF}: {path}");
    open(repo, &id, &tip, &message, author, committer)?;
    Ok(id)
}

/// Keeps the version of `subject` that `theirs` holds as an alternative, for a note that was
/// changed both here and elsewhere. Returns the alternative's id; taking in the same revision
/// again returns the same one, open or ended, without writing anything.
pub fn arrive(
    repo: &Repository,
    theirs: Oid,
    subject: &Path,
    author: &Signature<'_>,
    committer: &Signature<'_>,
) -> Result<String, git2::Error> {
    let path = slashed(subject);
    let id = arrival_id(theirs, &path);
    if target(repo, &open_ref(&id)).is_some() || target(repo, &ended_ref(&id)).is_some() {
        return Ok(id);
    }
    let parent = repo.find_commit(theirs)?;
    if blob_in_tree(repo, &parent.tree()?, subject).is_none() {
        return Err(error(format!(
            "the arriving revision does not hold '{path}'"
        )));
    }
    let message =
        format!("Changed elsewhere\n\n{ALTERNATIVE_OF}: {path}\n{ARRIVED_FROM}: {theirs}");
    match open(repo, &id, &parent, &message, author, committer) {
        // Another writer opened it first: that one is this one.
        Err(e) if e.code() == git2::ErrorCode::Exists => Ok(id),
        other => other.map(|()| id),
    }
}

/// Reads the alternative whose newest revision is `tip`.
fn describe(repo: &Repository, id: &str, tip: Oid) -> Result<Alternative, git2::Error> {
    let newest = repo.find_commit(tip)?;
    let mut commit = newest.clone();
    for _ in 0..MAX_WALK {
        let message = commit.message().unwrap_or("");
        if let Some(path) = trailer(message, ALTERNATIVE_OF) {
            let arrived_from = trailer(message, ARRIVED_FROM).and_then(|s| Oid::from_str(s).ok());
            return Ok(Alternative {
                id: id.to_string(),
                path: path.to_string(),
                arrived_from,
                tip,
                seconds: newest.time().seconds(),
                author: newest.author().name().unwrap_or("").to_string(),
            });
        }
        commit = match commit.parent(0) {
            Ok(parent) => parent,
            Err(_) => break,
        };
    }
    Err(error(format!(
        "alternative {id} does not say which note it belongs to"
    )))
}

fn listed(repo: &Repository, prefix: &str) -> Result<Vec<Alternative>, git2::Error> {
    let mut found = Vec::new();
    for reference in repo.references_glob(&format!("{prefix}*"))? {
        let reference = reference?;
        let (Some(name), Some(tip)) = (reference.name().ok(), reference.target()) else {
            continue;
        };
        let id = &name[prefix.len()..];
        if !valid_id(id) {
            continue;
        }
        // One that has ended and is still listed as open was interrupted between the two steps
        // of ending: it has ended.
        if prefix == ALTERNATIVES && target(repo, &ended_ref(id)).is_some() {
            continue;
        }
        match describe(repo, id, tip) {
            Ok(alternative) => found.push(alternative),
            Err(e) => log::warn!("alternatives: skipping {name}: {}", e.message()),
        }
    }
    found.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.seconds.cmp(&b.seconds))
            .then(a.id.cmp(&b.id))
    });
    Ok(found)
}

/// Every open alternative, by note.
pub fn list(repo: &Repository) -> Result<Vec<Alternative>, git2::Error> {
    listed(repo, ALTERNATIVES)
}

/// Every alternative that has ended, picked or set aside.
pub fn ended(repo: &Repository) -> Result<Vec<Alternative>, git2::Error> {
    listed(repo, ENDED)
}

/// The open alternative `id`.
pub fn get(repo: &Repository, id: &str) -> Result<Alternative, git2::Error> {
    if !valid_id(id) {
        return Err(error("no such alternative"));
    }
    if target(repo, &ended_ref(id)).is_some() {
        return Err(error("that alternative has ended"));
    }
    let tip = target(repo, &open_ref(id)).ok_or_else(|| error("no such alternative"))?;
    describe(repo, id, tip)
}

/// What the alternative holds for its note, or `None` when it holds no version of it.
pub fn content(
    repo: &Repository,
    alternative: &Alternative,
) -> Result<Option<Vec<u8>>, git2::Error> {
    let tree = repo.find_commit(alternative.tip)?.tree()?;
    let Some(blob) = blob_in_tree(repo, &tree, Path::new(&alternative.path)) else {
        return Ok(None);
    };
    Ok(Some(repo.find_blob(blob)?.content().to_vec()))
}

/// Whether the alternative holds something other than the note's last recorded version. One that
/// holds the same is nothing to choose between.
pub fn differs(repo: &Repository, alternative: &Alternative) -> Result<bool, git2::Error> {
    let subject = Path::new(&alternative.path);
    let theirs = blob_in_tree(repo, &repo.find_commit(alternative.tip)?.tree()?, subject);
    let ours = tip_tree(repo, NOTES_REF).and_then(|tree| blob_in_tree(repo, &tree, subject));
    Ok(theirs != ours)
}

/// Adds a version of the alternative's note to the alternative. Returns `None` when it already
/// holds exactly this.
pub fn record(
    repo: &Repository,
    id: &str,
    content: &[u8],
    message: &str,
    author: &Signature<'_>,
    committer: &Signature<'_>,
) -> Result<Option<Oid>, git2::Error> {
    // Read and written in one turn, on top of the revision read: an alternative that ends in
    // between is not opened again under its old name.
    advancing(|| {
        let alternative = get(repo, id)?;
        let entries = [(PathBuf::from(&alternative.path), content.to_vec())];
        extend_once(
            repo,
            &open_ref(id),
            alternative.tip,
            &entries,
            message,
            author,
            committer,
            WhenUnchanged::Skip,
        )
    })
}

/// Ends the alternative: it moves, unchanged, to where ended ones are kept.
///
/// Two steps, each checked: the ended name is created for the newest revision just read, and
/// the open name is removed only if it still points there. A version added in between is not
/// lost — the open name then stays, and the alternative is still listed as ended (see
/// [`list`]) with its ended name holding what was decided on.
pub fn end(repo: &Repository, id: &str) -> Result<(), git2::Error> {
    advancing(|| {
        let alternative = get(repo, id)?;
        match repo.reference(&ended_ref(id), alternative.tip, false, "alternative: end") {
            Ok(_) => {}
            Err(e) if e.code() == git2::ErrorCode::Exists => {}
            Err(e) => return Err(e),
        }
        let mut open_name = repo.find_reference(&open_ref(id))?;
        if open_name.target() == Some(alternative.tip) {
            open_name.delete()?;
        }
        Ok(())
    })
}

/// Makes the alternative's version the note's recorded state and ends the alternative. Returns
/// the version, which the caller writes to the note's file.
///
/// The revision is an ordinary one on the notes reference, naming the alternative it took its
/// content from — not a join of the two histories: an alternative that arrived may rest on
/// revisions this folder has not taken in, and claiming them as joined would keep them from ever
/// being taken in.
pub fn pick(
    repo: &Repository,
    id: &str,
    author: &Signature<'_>,
    committer: &Signature<'_>,
) -> Result<Vec<u8>, git2::Error> {
    let alternative = get(repo, id)?;
    let chosen = content(repo, &alternative)?
        .ok_or_else(|| error("that alternative holds no version of its note"))?;
    let entries = [(PathBuf::from(&alternative.path), chosen.clone())];
    let message = format!("Used an alternative\n\n{PICKED}: {id}");
    commit_paths(
        repo,
        NOTES_REF,
        &entries,
        &message,
        author,
        committer,
        WhenUnchanged::Record,
    )?;
    end(repo, id)?;
    Ok(chosen)
}

/// Whether a person already decided about `blob` arriving for `path`: an alternative that arrived
/// with it has ended. Taking in the same change again must not ask again.
pub fn decided(repo: &Repository, path: &str, blob: Oid) -> Result<bool, git2::Error> {
    for alternative in ended(repo)? {
        if alternative.path != path || alternative.arrived_from.is_none() {
            continue;
        }
        let tree = repo.find_commit(alternative.tip)?.tree()?;
        if blob_in_tree(repo, &tree, Path::new(path)) == Some(blob) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The ids of alternatives whose version was picked, from the notes reference's history.
#[allow(dead_code)] // read by the count of alternatives brought to an end
pub fn picked(repo: &Repository) -> Result<Vec<String>, git2::Error> {
    let Some(tip) = target(repo, NOTES_REF) else {
        return Ok(Vec::new());
    };
    let mut walk = repo.revwalk()?;
    walk.push(tip)?;
    let mut ids = Vec::new();
    for oid in walk {
        let commit = repo.find_commit(oid?)?;
        if let Some(id) = trailer(commit.message().unwrap_or(""), PICKED) {
            ids.push(id.to_string());
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_engine::{commit_paths, content_at_tip};
    use tempfile::TempDir;

    fn sig() -> Signature<'static> {
        Signature::now("Writer", "writer@example.com").unwrap()
    }

    fn repo_with(files: &[(&str, &str)]) -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let entries: Vec<(PathBuf, Vec<u8>)> = files
            .iter()
            .map(|(p, c)| (PathBuf::from(p), c.as_bytes().to_vec()))
            .collect();
        commit_paths(
            &repo,
            NOTES_REF,
            &entries,
            "first",
            &sig(),
            &sig(),
            WhenUnchanged::Skip,
        )
        .unwrap();
        (dir, repo)
    }

    fn notes_blob(repo: &Repository, path: &str) -> Option<Oid> {
        tip_tree(repo, NOTES_REF).and_then(|t| blob_in_tree(repo, &t, Path::new(path)))
    }

    fn at_tip(repo: &Repository, path: &str) -> String {
        String::from_utf8(
            content_at_tip(repo, NOTES_REF, Path::new(path))
                .unwrap()
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn starting_one_needs_a_recorded_note() {
        let (_dir, repo) = repo_with(&[("a.md", "one")]);
        assert!(start(&repo, Path::new("b.md"), &sig(), &sig()).is_err());
        assert!(start(&repo, Path::new("a.png"), &sig(), &sig()).is_err());
        let empty = TempDir::new().unwrap();
        let bare = Repository::init(empty.path()).unwrap();
        assert!(start(&bare, Path::new("a.md"), &sig(), &sig()).is_err());
    }

    #[test]
    fn an_alternative_holds_its_own_versions_and_leaves_the_note_alone() {
        let (_dir, repo) = repo_with(&[("notes/a.md", "one"), ("b.md", "bee")]);
        let before = target(&repo, NOTES_REF);
        let id = start(&repo, Path::new("notes/a.md"), &sig(), &sig()).unwrap();

        let listed = list(&repo).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, "notes/a.md");
        assert_eq!(listed[0].arrived_from, None);
        // Starting it changes nothing to choose between yet.
        assert!(!differs(&repo, &listed[0]).unwrap());

        record(&repo, &id, b"two", "try", &sig(), &sig()).unwrap();
        let alternative = get(&repo, &id).unwrap();
        assert_eq!(content(&repo, &alternative).unwrap().unwrap(), b"two");
        assert!(differs(&repo, &alternative).unwrap());
        // The same content again adds nothing.
        assert_eq!(
            record(&repo, &id, b"two", "again", &sig(), &sig()).unwrap(),
            None
        );

        assert_eq!(
            target(&repo, NOTES_REF),
            before,
            "the note's history is untouched"
        );
        assert_eq!(at_tip(&repo, "notes/a.md"), "one");
    }

    #[test]
    fn picking_records_one_version_that_changes_only_that_note_and_ends_it() {
        let (_dir, repo) = repo_with(&[("a.md", "one"), ("b.md", "bee")]);
        let b_before = notes_blob(&repo, "b.md");
        let before = target(&repo, NOTES_REF).unwrap();
        let id = start(&repo, Path::new("a.md"), &sig(), &sig()).unwrap();
        record(&repo, &id, b"two", "try", &sig(), &sig()).unwrap();
        let tip = get(&repo, &id).unwrap().tip;

        let chosen = pick(&repo, &id, &sig(), &sig()).unwrap();
        assert_eq!(chosen, b"two");

        let after = repo
            .find_reference(NOTES_REF)
            .unwrap()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            after.parent_id(0).unwrap(),
            before,
            "exactly one new version"
        );
        assert_eq!(after.parent_count(), 1, "not a join of the two histories");
        assert_eq!(at_tip(&repo, "a.md"), "two");
        assert_eq!(
            notes_blob(&repo, "b.md"),
            b_before,
            "other notes are untouched"
        );

        assert!(list(&repo).unwrap().is_empty());
        assert!(get(&repo, &id).is_err());
        let ended = ended(&repo).unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].tip, tip, "the ended alternative is kept unchanged");
        assert_eq!(picked(&repo).unwrap(), vec![id]);
    }

    #[test]
    fn setting_aside_keeps_the_note_and_the_alternative() {
        let (_dir, repo) = repo_with(&[("a.md", "one")]);
        let before = target(&repo, NOTES_REF);
        let id = start(&repo, Path::new("a.md"), &sig(), &sig()).unwrap();
        record(&repo, &id, b"two", "try", &sig(), &sig()).unwrap();
        let tip = get(&repo, &id).unwrap().tip;

        end(&repo, &id).unwrap();
        assert_eq!(target(&repo, NOTES_REF), before);
        assert!(list(&repo).unwrap().is_empty());
        assert_eq!(target(&repo, &ended_ref(&id)), Some(tip));
        assert!(picked(&repo).unwrap().is_empty());
        // Nothing more can be added to one that ended.
        assert!(record(&repo, &id, b"three", "late", &sig(), &sig()).is_err());
    }

    #[test]
    fn a_version_written_on_top_of_a_name_that_ended_meanwhile_does_not_bring_it_back() {
        // The step a version takes after reading the alternative: the open name is gone by the
        // time it writes (the alternative ended in between).
        let (_dir, repo) = repo_with(&[("a.md", "one")]);
        let id = start(&repo, Path::new("a.md"), &sig(), &sig()).unwrap();
        let read = get(&repo, &id).unwrap();
        end(&repo, &id).unwrap();

        let entries = [(PathBuf::from("a.md"), b"late".to_vec())];
        let late = extend_once(
            &repo,
            &open_ref(&id),
            read.tip,
            &entries,
            "late",
            &sig(),
            &sig(),
            WhenUnchanged::Skip,
        );
        assert!(late.is_err());
        assert_eq!(
            target(&repo, &open_ref(&id)),
            None,
            "the open name stays gone"
        );
        assert_eq!(target(&repo, &ended_ref(&id)), Some(read.tip));
    }

    #[test]
    fn versions_and_ends_racing_never_leave_an_ended_alternative_open() {
        let (dir, repo) = repo_with(&[("a.md", "one")]);
        for round in 0..40 {
            let id = start(&repo, Path::new("a.md"), &sig(), &sig()).unwrap();
            let at = dir.path().to_path_buf();
            let (writer, ender) = (id.clone(), id.clone());
            let (w_at, e_at) = (at.clone(), at.clone());
            let w = std::thread::spawn(move || {
                let repo = Repository::open(&w_at).unwrap();
                let _ = record(
                    &repo,
                    &writer,
                    format!("take {round}").as_bytes(),
                    "try",
                    &sig(),
                    &sig(),
                );
            });
            let e = std::thread::spawn(move || {
                let repo = Repository::open(&e_at).unwrap();
                end(&repo, &ender).unwrap();
            });
            w.join().unwrap();
            e.join().unwrap();
            assert_eq!(
                target(&repo, &open_ref(&id)),
                None,
                "round {round}: ended, so not open"
            );
            assert!(target(&repo, &ended_ref(&id)).is_some());
        }
    }

    #[test]
    fn an_end_interrupted_between_its_two_steps_reads_as_ended() {
        let (_dir, repo) = repo_with(&[("a.md", "one")]);
        let id = start(&repo, Path::new("a.md"), &sig(), &sig()).unwrap();
        let tip = get(&repo, &id).unwrap().tip;
        repo.reference(&ended_ref(&id), tip, false, "half").unwrap();
        assert!(list(&repo).unwrap().is_empty());
        assert!(get(&repo, &id).is_err());
        end(&repo, &id).expect_err("already ended");
    }

    #[test]
    fn an_arrival_is_kept_once_and_its_decision_is_remembered() {
        let (_dir, repo) = repo_with(&[("a.md", "one"), ("b.md", "bee")]);
        // What elsewhere recorded: the same note, changed differently.
        let base = repo
            .find_reference(NOTES_REF)
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let blob = repo.blob(b"elsewhere").unwrap();
        let tree = crate::git_engine::tree_with_file(
            &repo,
            Some(&base.tree().unwrap()),
            Path::new("a.md"),
            Some(blob),
        )
        .unwrap();
        let theirs = repo
            .commit(
                None,
                &sig(),
                &sig(),
                "theirs",
                &repo.find_tree(tree).unwrap(),
                &[&base],
            )
            .unwrap();

        let id = arrive(&repo, theirs, Path::new("a.md"), &sig(), &sig()).unwrap();
        assert_eq!(
            arrive(&repo, theirs, Path::new("a.md"), &sig(), &sig()).unwrap(),
            id
        );
        let open = list(&repo).unwrap();
        assert_eq!(
            open.len(),
            1,
            "taking the same revision in again does not ask again"
        );
        assert_eq!(open[0].arrived_from, Some(theirs));
        assert!(differs(&repo, &open[0]).unwrap());
        assert!(!decided(&repo, "a.md", blob).unwrap());

        // Kept mine.
        end(&repo, &id).unwrap();
        assert!(decided(&repo, "a.md", blob).unwrap());
        assert_eq!(
            arrive(&repo, theirs, Path::new("a.md"), &sig(), &sig()).unwrap(),
            id
        );
        assert!(
            list(&repo).unwrap().is_empty(),
            "an ended arrival is not opened again"
        );
        assert!(!decided(&repo, "b.md", blob).unwrap());
    }

    #[test]
    fn the_index_and_the_folder_are_never_touched() {
        let (dir, repo) = repo_with(&[("a.md", "one")]);
        std::fs::write(dir.path().join("a.md"), "on disk").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.md")).unwrap();
        index.write().unwrap();
        let staged = repo
            .index()
            .unwrap()
            .get_path(Path::new("a.md"), 0)
            .unwrap()
            .id;

        let id = start(&repo, Path::new("a.md"), &sig(), &sig()).unwrap();
        record(&repo, &id, b"two", "try", &sig(), &sig()).unwrap();
        pick(&repo, &id, &sig(), &sig()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
            "on disk"
        );
        assert_eq!(
            repo.index()
                .unwrap()
                .get_path(Path::new("a.md"), 0)
                .unwrap()
                .id,
            staged
        );
        assert!(repo.head().is_err(), "no branch was created or moved");
    }
}

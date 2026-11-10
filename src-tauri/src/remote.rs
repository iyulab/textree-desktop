//! Exchanging recorded notes with a remote.
//!
//! Only the app's own references travel: everything under `refs/textree/`. What arrives is kept
//! under [`TRACKING`] first — outside `refs/textree/`, so it is never sent back — and then weighed
//! against what this machine recorded before anything moves:
//!
//! - one side is simply ahead: the other catches up;
//! - the two sides changed **different** notes: they are joined path by path. No note's text is
//!   merged, and both sides' revisions stay in the history, so nothing is lost;
//! - the two sides changed the **same** note differently (or one deleted what the other changed):
//!   nothing moves. The incoming state stays where it arrived until a person decides, and for a
//!   note both sides still hold, the other side's version waits as an alternative of it
//!   ([`crate::alternatives`]). Once every such note is decided — the other version picked, or set
//!   aside — the two sides join, each decided note as it now stands here.
//!
//! Working out what would happen ([`plan`]) is separate from making it happen, and reads only:
//! a join is prepared as an unreferenced revision, which is harmless if never used. Moving a
//! reference has to happen together with writing the notes it changes to disk — a reference that
//! ran ahead of the files would let the next recorded note quietly undo what arrived.

use std::collections::BTreeSet;
use std::io;
use std::path::{Component, Path, PathBuf};

use git2::{Diff, DiffOptions, ErrorCode, Index, IndexEntry, IndexTime, Oid, Repository, Tree};

use crate::git_engine::{commit_identities, VaultRepo, NOTES_REF, SNAPSHOT_REF};
use crate::git_transport::{with_credentials, Credentials};
use crate::note_locks::NoteLocks;

/// Where the remote's references are kept once fetched: `refs/textree/notes` arrives as
/// `refs/textree-remote/notes`.
pub const TRACKING: &str = "refs/textree-remote/";

const OWN: &str = "refs/textree/";

/// The message of a revision that joins two sides which changed different notes.
pub const JOIN_MESSAGE: &str = "Joined changes from another device";

pub(crate) fn tracking_of(local: &str) -> String {
    format!("{TRACKING}{}", local.strip_prefix(OWN).unwrap_or(local))
}

/// How sending went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// The remote now holds what this machine recorded.
    Done,
    /// Nothing has been recorded here yet.
    NothingToSend,
    /// The remote holds something this machine has not taken in. Fetch, take it in, send again.
    Behind,
}

/// Sends every reference under `refs/textree/`. Never forces: a remote that moved on since this
/// machine last took it in refuses, and that is reported as [`Sent::Behind`].
pub fn push(
    repo: &Repository,
    url: &str,
    credentials: Option<Credentials>,
) -> Result<Sent, git2::Error> {
    crate::git_transport::install();
    let names: Vec<String> = own_references(repo)?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    if names.is_empty() {
        return Ok(Sent::NothingToSend);
    }
    // libgit2 resolves each source by name, so a glob cannot be pushed as one spec.
    let specs: Vec<String> = names.iter().map(|n| format!("{n}:{n}")).collect();
    let specs: Vec<&str> = specs.iter().map(String::as_str).collect();

    let mut refused = false;
    let outcome = {
        let mut callbacks = git2::RemoteCallbacks::new();
        callbacks.push_update_reference(|_, status| {
            if status.is_some() {
                refused = true;
            }
            Ok(())
        });
        let mut options = git2::PushOptions::new();
        options.remote_callbacks(callbacks);
        with_credentials(credentials, || {
            repo.remote_anonymous(url)?.push(&specs, Some(&mut options))
        })
    };
    match outcome {
        Err(e) if e.code() == ErrorCode::NotFastForward => Ok(Sent::Behind),
        Err(e) => Err(e),
        Ok(()) if refused => Ok(Sent::Behind),
        Ok(()) => Ok(Sent::Done),
    }
}

/// Brings the remote's references in under [`TRACKING`]. Moves nothing this machine recorded.
pub fn fetch(
    repo: &Repository,
    url: &str,
    credentials: Option<Credentials>,
) -> Result<(), git2::Error> {
    crate::git_transport::install();
    // Forced into the tracking names only: they mirror the remote, whatever it did.
    let spec = format!("+{OWN}*:{TRACKING}*");
    with_credentials(credentials, || {
        repo.remote_anonymous(url)?
            .fetch(&[spec.as_str()], None, None)
    })
}

/// What taking in one reference would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing arrived, or this machine already has it.
    Nothing,
    /// This machine had recorded nothing on this reference; it takes the remote's as it is.
    Adopt { to: Oid },
    /// The remote is simply ahead.
    FastForward { from: Oid, to: Oid },
    /// The two sides changed different notes; `commit` holds both. `incoming` are the paths the
    /// remote's side changed, which this machine's files have to take.
    Join {
        from: Oid,
        commit: Oid,
        incoming: Vec<String>,
    },
    /// Both sides changed these paths differently, and nobody has decided about the version that
    /// arrived. Nothing moves until a person decides. `theirs` is the revision that arrived.
    Held { overlap: Vec<String>, theirs: Oid },
}

/// What taking in everything that arrived would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub notes: Step,
    pub snapshots: Step,
    /// Notes the remote changed whose file here holds edits nobody recorded yet. Writing the
    /// remote's version would overwrite them, so these must not be written.
    pub unrecorded_here: Vec<String>,
}

/// Works out what taking in the fetched references would do. Reads only; see the module notes.
pub fn plan(vault: &VaultRepo, root: &Path) -> Result<Plan, git2::Error> {
    let repo = vault.repo();
    // Two notes changed on the two sides are two notes: the notes reference joins only when the
    // changed paths do not meet.
    let notes = plan_ref(repo, NOTES_REF, Overlap::Hold)?;
    // What was kept of deleted notes is only ever added to. The same path kept on both sides is
    // kept on both — this side's copy in the joined tree, the other's in its own revision.
    let snapshots = plan_ref(repo, SNAPSHOT_REF, Overlap::KeepOurs)?;
    let unrecorded_here = unrecorded_among(vault, root, &notes)?;
    Ok(Plan {
        notes,
        snapshots,
        unrecorded_here,
    })
}

#[derive(Clone, Copy)]
enum Overlap {
    Hold,
    KeepOurs,
}

fn plan_ref(repo: &Repository, local: &str, overlap: Overlap) -> Result<Step, git2::Error> {
    let Some(theirs) = id_of(repo, &tracking_of(local)) else {
        return Ok(Step::Nothing);
    };
    let Some(ours) = id_of(repo, local) else {
        return Ok(Step::Adopt { to: theirs });
    };
    if ours == theirs || repo.graph_descendant_of(ours, theirs)? {
        return Ok(Step::Nothing);
    }
    if repo.graph_descendant_of(theirs, ours)? {
        return Ok(Step::FastForward {
            from: ours,
            to: theirs,
        });
    }

    let our_tree = repo.find_commit(ours)?.tree()?;
    let their_tree = repo.find_commit(theirs)?.tree()?;
    // Two histories that never shared a revision still share the empty folder they started from.
    let base_tree = match repo.merge_base(ours, theirs) {
        Ok(base) => Some(repo.find_commit(base)?.tree()?),
        Err(e) if e.code() == ErrorCode::NotFound => None,
        Err(e) => return Err(e),
    };
    let ours_changed = changed(repo, base_tree.as_ref(), &our_tree)?;
    let theirs_changed = changed(repo, base_tree.as_ref(), &their_tree)?;
    let met: Vec<String> = ours_changed
        .intersection(&theirs_changed)
        .cloned()
        .collect();
    if let Overlap::Hold = overlap {
        // Changed on both sides is not yet a question: both may have arrived at the same text (a
        // person picked the version that arrived), or a person already kept this side's version
        // over exactly this one. Only the rest waits.
        let mut undecided = Vec::new();
        for path in &met {
            let ours_blob = blob_id(&our_tree, path);
            let theirs_blob = blob_id(&their_tree, path);
            if ours_blob == theirs_blob {
                continue;
            }
            if let Some(blob) = theirs_blob {
                if crate::alternatives::decided(repo, path, blob)? {
                    continue;
                }
            }
            undecided.push(path.clone());
        }
        if !undecided.is_empty() {
            return Ok(Step::Held {
                overlap: undecided,
                theirs,
            });
        }
    }
    let incoming: Vec<String> = theirs_changed.difference(&ours_changed).cloned().collect();

    let tree = joined_tree(repo, &our_tree, &their_tree, &incoming)?;
    let (author, committer) = commit_identities(repo)?;
    let parents = [&repo.find_commit(ours)?, &repo.find_commit(theirs)?];
    let commit = repo.commit(
        None,
        &author,
        &committer,
        JOIN_MESSAGE,
        &repo.find_tree(tree)?,
        &parents,
    )?;
    Ok(Step::Join {
        from: ours,
        commit,
        incoming,
    })
}

fn blob_id(tree: &Tree<'_>, path: &str) -> Option<Oid> {
    tree.get_path(Path::new(path)).ok().map(|e| e.id())
}

fn id_of(repo: &Repository, name: &str) -> Option<Oid> {
    repo.find_reference(name).ok().and_then(|r| r.target())
}

/// Every path whose content differs between `from` (the empty tree when `None`) and `to`.
/// Without rename detection on purpose: a rename is its old path removed and its new one added,
/// so an edit to the old path on the other side counts as meeting it.
fn changed(
    repo: &Repository,
    from: Option<&Tree<'_>>,
    to: &Tree<'_>,
) -> Result<BTreeSet<String>, git2::Error> {
    let mut options = DiffOptions::new();
    options.ignore_submodules(true);
    let diff: Diff<'_> = repo.diff_tree_to_tree(from, Some(to), Some(&mut options))?;
    let mut out = BTreeSet::new();
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            if let Some(path) = file.path().and_then(Path::to_str) {
                out.insert(path.replace('\\', "/"));
            }
        }
    }
    Ok(out)
}

/// This side's tree with the other side's state of each `incoming` path laid over it.
fn joined_tree(
    repo: &Repository,
    ours: &Tree<'_>,
    theirs: &Tree<'_>,
    incoming: &[String],
) -> Result<Oid, git2::Error> {
    let mut index = Index::new()?;
    index.read_tree(ours)?;
    for path in incoming {
        match theirs.get_path(Path::new(path)) {
            Ok(entry) => {
                let zero = IndexTime::new(0, 0);
                index.add(&IndexEntry {
                    ctime: zero,
                    mtime: zero,
                    dev: 0,
                    ino: 0,
                    mode: entry.filemode() as u32,
                    uid: 0,
                    gid: 0,
                    file_size: 0,
                    id: entry.id(),
                    flags: 0,
                    flags_extended: 0,
                    path: path.as_bytes().to_vec(),
                })?;
            }
            Err(e) if e.code() == ErrorCode::NotFound => index.remove_path(Path::new(path))?,
            Err(e) => return Err(e),
        }
    }
    index.write_tree_to(repo)
}

/// The notes a step would write to disk whose file here holds something else than both what
/// this machine last recorded and what arrived — an edit that was saved but not yet added as a
/// version. A file that already matches what arrived is not one: a step that stopped half-way
/// leaves exactly that.
fn unrecorded_among(
    vault: &VaultRepo,
    root: &Path,
    step: &Step,
) -> Result<Vec<String>, git2::Error> {
    let repo = vault.repo();
    let Some((from, to)) = movement(step) else {
        return Ok(Vec::new());
    };
    let from_tree = match from {
        Some(id) => Some(repo.find_commit(id)?.tree()?),
        None => None,
    };
    let to_tree = repo.find_commit(to)?.tree()?;
    let mut out = Vec::new();
    for path in changed(repo, from_tree.as_ref(), &to_tree)? {
        // A path that cannot be a note's place is refused by `apply`, not weighed here.
        let Some(place) = place_of(vault, root, &path) else {
            continue;
        };
        let on_disk = read_if_present(&place.file).ok().flatten();
        if on_disk != blob_at(repo, Some(&to_tree), &path)?
            && on_disk != blob_at(repo, from_tree.as_ref(), &path)?
        {
            out.push(path);
        }
    }
    Ok(out)
}

/// Where a folder's remote is, as this machine remembers it. The secret sent with it is kept in
/// the operating system's credential store, never here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub url: String,
    pub username: String,
}

/// The file in a folder's settings directory that holds its [`Connection`].
pub const CONNECTION_FILE: &str = "remote.json";

const SECRET_SERVICE: &str = "com.textree.remote";

fn secrets() -> tauri_kit_credentials::Credentials {
    crate::secret_store::credentials(SECRET_SERVICE)
}

/// Keeps the secret for the folder named by `key` (its settings key).
pub fn set_secret(key: &str, secret: &str) -> Result<(), String> {
    secrets().set(key, secret).map_err(|e| e.to_string())
}

/// The secret kept for the folder named by `key`, if any. A store that cannot be read counts as
/// keeping none.
pub fn secret(key: &str) -> Option<String> {
    secrets().get(key).ok().flatten()
}

/// Forgets the secret for the folder named by `key`. Nothing kept is not an error.
pub fn clear_secret(key: &str) -> Result<(), String> {
    secrets().delete(key).map_err(|e| e.to_string())
}

/// What one exchange with the remote did. Notes are named relative to the folder, as everywhere
/// else on the screen's side — not relative to a repository the folder may sit inside.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Exchange {
    /// Notes written here because they arrived.
    pub received: Vec<String>,
    /// Notes removed here because the other side removed them (kept among deleted notes).
    pub removed: Vec<String>,
    /// Notes both sides changed. Nothing moved; a person decides.
    pub held: Vec<String>,
    /// Of those, the notes whose version from elsewhere now waits as an alternative.
    pub alternatives: Vec<String>,
    /// Notes the other side changed that hold unrecorded edits here. Nothing moved.
    pub kept_back: Vec<String>,
    /// Notes of another folder in the same repository that are held or kept back, named relative
    /// to the repository. They hold this folder's exchange too — nothing is taken in until they
    /// are decided — and only that folder can show them.
    pub waiting_elsewhere: Vec<String>,
    /// The remote now holds everything recorded here.
    pub sent: bool,
}

/// One exchange: fetch, take in what can be taken in, send. A remote that moved on while this
/// ran is fetched from once more before giving up on sending this time.
pub fn exchange(
    vault: &VaultRepo,
    root: &Path,
    url: &str,
    credentials: Option<Credentials>,
    locks: &NoteLocks,
) -> Result<Exchange, String> {
    let repo = vault.repo();
    let err = |e: git2::Error| e.message().to_string();
    let mut out = Exchange::default();
    for _ in 0..2 {
        fetch(repo, url, credentials.clone()).map_err(err)?;
        crate::publish_scope::take_in(repo).map_err(err)?;
        let plan = plan(vault, root).map_err(err)?;
        let taken = apply(vault, root, &plan, locks)?;
        // Decided on what was taken in, not on what this folder names: a note waiting in another
        // folder of the same repository holds the send back just the same.
        let waits = matches!(taken, Taken::Held { .. } | Taken::KeptBack { .. });
        match taken {
            Taken::Done { written, removed } => {
                out.received.extend(in_folder(vault, written));
                out.removed.extend(in_folder(vault, removed));
            }
            Taken::Held {
                overlap,
                alternatives,
            } => {
                out.waiting_elsewhere = elsewhere(vault, &overlap);
                out.held = in_folder(vault, overlap);
                out.alternatives = in_folder(vault, alternatives);
            }
            Taken::KeptBack { notes } => {
                out.waiting_elsewhere = elsewhere(vault, &notes);
                out.kept_back = in_folder(vault, notes);
            }
            Taken::Nothing | Taken::Overtaken => {}
        }
        match push(repo, url, credentials.clone()).map_err(err)? {
            Sent::Done => {
                out.sent = true;
                note_what_the_remote_holds(repo).map_err(err)?;
                break;
            }
            Sent::NothingToSend => break,
            // Held or kept back: sending waits until a person has decided.
            Sent::Behind if waits => break,
            Sent::Behind => continue,
        }
    }
    Ok(out)
}

/// Repository paths as the folder names them. A path outside the folder is another folder's
/// note sharing the repository, and is not this folder's to name.
fn in_folder(vault: &VaultRepo, paths: Vec<String>) -> Vec<String> {
    paths
        .iter()
        .filter_map(|p| vault.path_in_vault(Path::new(p)))
        .map(|p| crate::git_engine::slashed(&p))
        .collect()
}

/// The repository paths outside the folder: another folder's notes sharing the repository.
fn elsewhere(vault: &VaultRepo, paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| vault.path_in_vault(Path::new(p)).is_none())
        .cloned()
        .collect()
}

/// After a send the remote holds exactly this machine's references. Recording that under
/// [`TRACKING`] is what lets [`backed_up`] answer without asking the remote.
fn note_what_the_remote_holds(repo: &Repository) -> Result<(), git2::Error> {
    for (name, id) in own_references(repo)? {
        repo.reference(&tracking_of(&name), id, true, "sent to remote")?;
    }
    Ok(())
}

fn own_references(repo: &Repository) -> Result<Vec<(String, Oid)>, git2::Error> {
    let mut out = Vec::new();
    for reference in repo.references_glob(&format!("{OWN}*"))? {
        let reference = reference?;
        if let (Ok(name), Some(id)) = (reference.name(), reference.target()) {
            // What was published is derived from the notes and is kept per machine.
            if name == crate::publish_assembly::PUBLISHED_REF {
                continue;
            }
            out.push((name.to_string(), id));
        }
    }
    Ok(out)
}

/// Whether the remote, as last seen, holds everything recorded here. Something recorded since
/// the last exchange is only on this machine until the next one.
pub fn backed_up(repo: &Repository) -> Result<bool, git2::Error> {
    for (name, id) in own_references(repo)? {
        if id_of(repo, &tracking_of(&name)) != Some(id) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// What taking in the notes reference did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// Nothing arrived that this machine does not have.
    Nothing,
    /// The notes here now match what arrived.
    Done {
        written: Vec<String>,
        removed: Vec<String>,
    },
    /// Both sides changed these notes; nothing moved. `alternatives` are the ones whose version
    /// from elsewhere now waits beside them.
    Held {
        overlap: Vec<String>,
        alternatives: Vec<String>,
    },
    /// These notes hold edits here that nobody recorded yet, and what arrived changes them.
    /// Nothing moved: taking the rest in would let recording them later quietly undo what
    /// arrived.
    KeptBack { notes: Vec<String> },
    /// Something was recorded here while this ran. What was written matches what arrived, so the
    /// next exchange picks up from there.
    Overtaken,
}

/// Takes in what [`plan`] worked out: what was kept of deleted notes first, then the notes —
/// files before the reference, all of them or none.
///
/// Files first, because the other order is the one that loses work: a reference that moved
/// ahead of its files makes their old contents look like edits, and recording them undoes what
/// arrived. Files first, a stop half-way leaves files that already match what arrived, and the
/// next exchange finds them so.
///
/// Every note involved is held for the whole step, so a save cannot land between the check
/// that its file is untouched and the write that replaces it.
pub fn apply(
    vault: &VaultRepo,
    root: &Path,
    plan: &Plan,
    locks: &NoteLocks,
) -> Result<Taken, String> {
    let repo = vault.repo();
    let err = |e: git2::Error| e.message().to_string();
    take_in_alternatives(repo).map_err(err)?;
    if let Some((from, to)) = movement(&plan.snapshots) {
        // Nothing on disk follows this reference, so it moves alone. A lost race leaves it for
        // the next exchange.
        let _ = advance(repo, SNAPSHOT_REF, from, to);
    }
    let (from, to) = match &plan.notes {
        Step::Held { overlap, theirs } => {
            return Ok(Taken::Held {
                overlap: overlap.clone(),
                alternatives: keep_as_alternatives(repo, overlap, *theirs).map_err(err)?,
            })
        }
        step => match movement(step) {
            Some(m) => m,
            None => return Ok(Taken::Nothing),
        },
    };

    let from_tree = match from {
        Some(id) => Some(repo.find_commit(id).map_err(err)?.tree().map_err(err)?),
        None => None,
    };
    let to_tree = repo.find_commit(to).map_err(err)?.tree().map_err(err)?;
    // Every note the reference moves over, not only this folder's: the reference is the whole
    // repository's, so a sibling folder's notes left unwritten would be recorded as received
    // without ever being on disk — and that folder's next exchange would find nothing to take.
    let mut changes: Vec<(String, Place)> = Vec::new();
    for path in changed(repo, from_tree.as_ref(), &to_tree).map_err(err)? {
        let place = place_of(vault, root, &path).ok_or_else(|| {
            format!("{path}: this cannot be the place of a note, so nothing was taken in")
        })?;
        changes.push((path, place));
    }
    // One order for every caller that holds several notes at once, so two of them cannot each
    // wait for the other.
    changes.sort_by(|a, b| a.1.file.cmp(&b.1.file));
    let _turns: Vec<_> = changes
        .iter()
        .map(|(_, place)| locks.turn(&place.file))
        .collect();

    let mut writes = Vec::new();
    let mut kept_back = Vec::new();
    for (path, place) in &changes {
        let recorded = blob_at(repo, from_tree.as_ref(), path).map_err(err)?;
        let arriving = blob_at(repo, Some(&to_tree), path).map_err(err)?;
        let on_disk = read_if_present(&place.file).map_err(|e| e.to_string())?;
        if on_disk == arriving {
            continue;
        }
        if on_disk != recorded {
            kept_back.push(path.clone());
            continue;
        }
        writes.push((path.clone(), place.clone(), on_disk.is_some(), arriving));
    }
    if !kept_back.is_empty() {
        return Ok(Taken::KeptBack { notes: kept_back });
    }

    // Said back only for this folder: it is the one the person is looking at.
    let mut written = Vec::new();
    let mut removed = Vec::new();
    for (path, place, present, arriving) in writes {
        let Place { file, top, here } = place;
        match arriving {
            Some(content) => {
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                // Not registered as the app's own write: an open note has to see it and reload.
                let wrote = if present {
                    crate::commands::atomic_write_bytes(&top, &file, &content)
                } else {
                    crate::commands::atomic_create_bytes(&top, &file, &content)
                };
                wrote.map_err(|e| format!("{path}: {e}"))?;
                if here {
                    written.push(path);
                }
            }
            None => {
                // A note leaving this folder is a deletion here like any other: what it held is
                // kept where deleted notes are listed, whatever the other side did about it.
                crate::commands::keep_state_of(&top, &file).map_err(|e| format!("{path}: {e}"))?;
                crate::fs_ops::patiently(|| std::fs::remove_file(&file))
                    .map_err(|e| format!("{path}: {e}"))?;
                if here {
                    removed.push(path);
                }
            }
        }
    }
    match advance(repo, NOTES_REF, from, to) {
        Ok(()) => Ok(Taken::Done { written, removed }),
        Err(_) => Ok(Taken::Overtaken),
    }
}

/// Keeps the arriving version of each held note both sides still hold as an alternative of it.
/// A note one side deleted has no version here to be an alternative of; it stays held.
fn keep_as_alternatives(
    repo: &Repository,
    overlap: &[String],
    theirs: Oid,
) -> Result<Vec<String>, git2::Error> {
    let their_tree = repo.find_commit(theirs)?.tree()?;
    let Some(our_tree) = crate::git_engine::tip_tree(repo, NOTES_REF) else {
        return Ok(Vec::new());
    };
    let (author, committer) = commit_identities(repo)?;
    let mut kept = Vec::new();
    for path in overlap {
        if blob_id(&our_tree, path).is_none() || blob_id(&their_tree, path).is_none() {
            continue;
        }
        crate::alternatives::arrive(repo, theirs, Path::new(path), &author, &committer)?;
        kept.push(path.clone());
    }
    Ok(kept)
}

/// Takes in the alternatives the remote holds, so one started on one machine is seen on the
/// others: open ones this machine does not know yet,
/// and endings. An alternative that ended here is never opened again from what arrives, and one
/// that ended elsewhere ends here too. Moves references only; no note's file is touched.
fn take_in_alternatives(repo: &Repository) -> Result<(), git2::Error> {
    use crate::alternatives::{ALTERNATIVES, ENDED};
    let arrived_prefix =
        |local: &str| format!("{TRACKING}{}", local.strip_prefix(OWN).unwrap_or(local));
    for reference in repo.references_glob(&format!("{}*", arrived_prefix(ENDED)))? {
        let reference = reference?;
        let (Ok(name), Some(tip)) = (reference.name(), reference.target()) else {
            continue;
        };
        let id = &name[arrived_prefix(ENDED).len()..];
        let ended_here = format!("{ENDED}{id}");
        if id_of(repo, &ended_here).is_none() {
            repo.reference(&ended_here, tip, false, "alternative ended elsewhere")?;
        }
    }
    for reference in repo.references_glob(&format!("{}*", arrived_prefix(ALTERNATIVES)))? {
        let reference = reference?;
        let (Ok(name), Some(tip)) = (reference.name(), reference.target()) else {
            continue;
        };
        let id = &name[arrived_prefix(ALTERNATIVES).len()..];
        if id_of(repo, &format!("{ENDED}{id}")).is_some() {
            continue;
        }
        let open_here = format!("{ALTERNATIVES}{id}");
        match id_of(repo, &open_here) {
            None => {
                repo.reference(&open_here, tip, false, "alternative from elsewhere")?;
            }
            // Versions added elsewhere are taken when they only add to what is here.
            Some(here) if here != tip && repo.graph_descendant_of(tip, here)? => {
                repo.reference_matching(&open_here, tip, true, here, "alternative from elsewhere")?;
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Where a note the notes reference holds lives on disk.
#[derive(Clone)]
struct Place {
    file: PathBuf,
    /// The folder a write to `file` is made from: this folder for its own notes, the
    /// repository's working tree for a sibling folder's.
    top: PathBuf,
    /// Whether `file` is in this folder.
    here: bool,
}

/// The place of `path` (relative to the repository), or `None` when it cannot be a note's. What
/// arrives comes from outside this machine: a path that climbs out, names a drive or a stream, or
/// reaches into the repository's own storage is never a place to write. The app records none of
/// those, so refusing them refuses nothing it sent. A hidden folder is not refused for being
/// hidden: a notes folder may itself sit in one, and every note it holds is named through it.
/// These are repository paths, possibly of another folder, so the vault's own path checks do not
/// apply to them.
fn place_of(vault: &VaultRepo, root: &Path, path: &str) -> Option<Place> {
    let plain = !path.is_empty()
        && Path::new(path).components().all(|c| match c {
            Component::Normal(name) => name
                .to_str()
                .is_some_and(|name| !name.contains(':') && !is_repository_storage(name)),
            _ => false,
        });
    if !plain {
        return None;
    }
    if let Some(in_vault) = vault.path_in_vault(Path::new(path)) {
        return Some(Place {
            file: root.join(in_vault),
            top: root.to_path_buf(),
            here: true,
        });
    }
    let top = vault.repo().workdir()?.to_path_buf();
    Some(Place {
        file: top.join(path),
        top,
        here: false,
    })
}

/// Whether a name reaches the repository's storage folder — also by the spellings Windows takes
/// for it: trailing dots and spaces dropped, any case, or its short name.
fn is_repository_storage(name: &str) -> bool {
    let name = name.trim_end_matches(['.', ' ']).to_ascii_lowercase();
    name == ".git" || name.starts_with("git~")
}

fn movement(step: &Step) -> Option<(Option<Oid>, Oid)> {
    match step {
        Step::Adopt { to } => Some((None, *to)),
        Step::FastForward { from, to } => Some((Some(*from), *to)),
        Step::Join { from, commit, .. } => Some((Some(*from), *commit)),
        Step::Nothing | Step::Held { .. } => None,
    }
}

/// Moves `reference` from `from` to `to` only if nothing moved it meanwhile.
fn advance(
    repo: &Repository,
    reference: &str,
    from: Option<Oid>,
    to: Oid,
) -> Result<(), git2::Error> {
    let message = "take in from remote";
    match from {
        Some(from) => repo.reference_matching(reference, to, true, from, message),
        None => repo.reference(reference, to, false, message),
    }
    .map(drop)
}

fn blob_at(
    repo: &Repository,
    tree: Option<&Tree<'_>>,
    path: &str,
) -> Result<Option<Vec<u8>>, git2::Error> {
    let Some(tree) = tree else {
        return Ok(None);
    };
    match tree.get_path(Path::new(path)) {
        Ok(entry) => Ok(Some(repo.find_blob(entry.id())?.content().to_vec())),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn read_if_present(file: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::read(file) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_engine::{commit_paths, WhenUnchanged};
    use std::path::PathBuf;

    struct Machine {
        root: PathBuf,
        vault: VaultRepo,
    }

    impl Machine {
        fn new(root: PathBuf) -> Machine {
            std::fs::create_dir_all(&root).unwrap();
            let vault = crate::git_engine::prepare(&root).unwrap();
            Machine { root, vault }
        }

        fn repo(&self) -> &Repository {
            self.vault.repo()
        }

        /// Saves `text` to the note's file and records it, as adding a version does.
        fn record(&self, rel: &str, text: &str) {
            self.save(rel, text);
            let (author, committer) = commit_identities(self.repo()).unwrap();
            commit_paths(
                self.repo(),
                NOTES_REF,
                &[(
                    self.vault.path_in_repo(Path::new(rel)),
                    text.as_bytes().to_vec(),
                )],
                "version",
                &author,
                &committer,
                WhenUnchanged::Skip,
            )
            .unwrap();
        }

        fn save(&self, rel: &str, text: &str) {
            let path = self.root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn url_of(remote: &Path) -> String {
            remote.to_string_lossy().into_owned()
        }

        fn send(&self, remote: &Path) -> Sent {
            push(self.repo(), &Machine::url_of(remote), None).unwrap()
        }

        fn take(&self, remote: &Path) -> Plan {
            fetch(self.repo(), &Machine::url_of(remote), None).unwrap();
            plan(&self.vault, &self.root).unwrap()
        }

        fn tip(&self) -> Oid {
            id_of(self.repo(), NOTES_REF).unwrap()
        }

        /// Takes in what arrived, as the app will.
        fn apply(&self, plan: &Plan) -> Taken {
            apply(&self.vault, &self.root, plan, &NoteLocks::default()).unwrap()
        }

        fn read(&self, rel: &str) -> Option<String> {
            std::fs::read_to_string(self.root.join(rel)).ok()
        }
    }

    fn setup() -> (tempfile::TempDir, PathBuf, Machine, Machine) {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        Repository::init_bare(&remote).unwrap();
        let a = Machine::new(tmp.path().join("a"));
        let b = Machine::new(tmp.path().join("b"));
        (tmp, remote, a, b)
    }

    #[test]
    fn publish_choices_changed_on_both_machines_do_not_stop_sending() {
        let (_tmp, remote, a, b) = setup();
        a.record("a.md", "# A");
        a.send(&remote);
        b.take(&remote);
        crate::publish_scope::set(a.repo(), "a.md", Some(true)).unwrap();
        crate::publish_scope::set(b.repo(), "a.md", Some(false)).unwrap();
        assert_eq!(a.send(&remote), Sent::Done);
        let url = Machine::url_of(&remote);
        let out = exchange(&b.vault, &b.root, &url, None, &NoteLocks::default()).unwrap();
        assert!(out.sent);
        let got = crate::publish_scope::read(b.repo(), crate::publish_scope::PUBLIC_REF).unwrap();
        assert!(!crate::publish_scope::is_included("a.md", &got));
    }

    #[test]
    fn a_first_machine_sends_and_a_second_adopts() {
        let (_tmp, remote, a, b) = setup();
        assert_eq!(
            push(a.repo(), &Machine::url_of(&remote), None).unwrap(),
            Sent::NothingToSend
        );
        a.record("manual.md", "# Manual\n");
        assert_eq!(a.send(&remote), Sent::Done);

        let plan = b.take(&remote);
        assert_eq!(plan.notes, Step::Adopt { to: a.tip() });
        assert!(plan.unrecorded_here.is_empty());
        // Arriving is not taking in: this machine's own reference has not moved.
        assert!(id_of(b.repo(), NOTES_REF).is_none());
    }

    #[test]
    fn a_remote_that_is_simply_ahead_is_caught_up_with() {
        let (_tmp, remote, a, b) = setup();
        a.record("manual.md", "# Manual\n");
        a.send(&remote);
        b.apply(&b.take(&remote));
        a.record("manual.md", "# Manual\n\nMore.\n");
        a.send(&remote);

        let plan = b.take(&remote);
        assert!(matches!(plan.notes, Step::FastForward { to, .. } if to == a.tip()));
        // Nothing moves on this side's own references by planning.
        assert_ne!(b.tip(), a.tip());
    }

    #[test]
    fn different_notes_changed_on_two_machines_are_joined_without_losing_either() {
        let (_tmp, remote, a, b) = setup();
        a.record("manual.md", "# Manual\n");
        a.send(&remote);
        b.apply(&b.take(&remote));

        a.record("release-notes.md", "# 1.0\n");
        a.send(&remote);
        b.record("faq.md", "# FAQ\n");
        // b is behind: its send is refused rather than dropping a's release notes.
        assert_eq!(b.send(&remote), Sent::Behind);

        let plan = b.take(&remote);
        let Step::Join {
            commit, incoming, ..
        } = &plan.notes
        else {
            panic!("expected a join, got {:?}", plan.notes);
        };
        assert_eq!(incoming, &vec!["release-notes.md".to_string()]);
        let joined = b.repo().find_commit(*commit).unwrap();
        assert_eq!(joined.parent_count(), 2);
        let tree = joined.tree().unwrap();
        for path in ["manual.md", "release-notes.md", "faq.md"] {
            assert!(
                tree.get_path(Path::new(path)).is_ok(),
                "{path} missing from the join"
            );
        }
        // The file the remote brought has no edits here, so it may be written.
        assert!(plan.unrecorded_here.is_empty());

        assert!(matches!(b.apply(&plan), Taken::Done { .. }));
        assert_eq!(b.read("release-notes.md").as_deref(), Some("# 1.0\n"));
        assert_eq!(b.send(&remote), Sent::Done);
    }

    #[test]
    fn the_same_note_changed_on_two_machines_is_held() {
        let (_tmp, remote, a, b) = setup();
        a.record("manual.md", "# Manual\n");
        a.send(&remote);
        b.apply(&b.take(&remote));

        a.record("manual.md", "# Manual\n\nFrom a.\n");
        a.send(&remote);
        b.record("manual.md", "# Manual\n\nFrom b.\n");
        let before = b.tip();

        let plan = b.take(&remote);
        assert!(
            matches!(&plan.notes, Step::Held { overlap, .. } if overlap == &vec!["manual.md".to_string()]),
            "expected held, got {:?}",
            plan.notes
        );
        assert_eq!(b.tip(), before);
        assert!(
            plan.unrecorded_here.is_empty(),
            "nothing is written while held"
        );
    }

    /// Both machines hold manual.md; a and b then change it differently, a sends first.
    fn diverged() -> (tempfile::TempDir, PathBuf, Machine, Machine) {
        let (tmp, remote, a, b) = setup();
        a.record("manual.md", "# Manual\n");
        a.send(&remote);
        b.apply(&b.take(&remote));
        a.record("manual.md", "# Manual\n\nFrom a.\n");
        a.send(&remote);
        b.record("manual.md", "# Manual\n\nFrom b.\n");
        (tmp, remote, a, b)
    }

    fn open_alternatives(m: &Machine) -> Vec<crate::alternatives::Alternative> {
        crate::alternatives::list(m.repo()).unwrap()
    }

    #[test]
    fn the_version_from_elsewhere_waits_as_an_alternative_and_is_not_asked_twice() {
        let (_tmp, remote, _a, b) = diverged();
        let before = b.tip();
        let taken = b.apply(&b.take(&remote));
        assert_eq!(
            taken,
            Taken::Held {
                overlap: vec!["manual.md".to_string()],
                alternatives: vec!["manual.md".to_string()],
            }
        );
        assert_eq!(b.tip(), before, "nothing moved");
        assert_eq!(
            b.read("manual.md").as_deref(),
            Some("# Manual\n\nFrom b.\n")
        );
        let open = open_alternatives(&b);
        assert_eq!(open.len(), 1);
        assert_eq!(
            crate::alternatives::content(b.repo(), &open[0])
                .unwrap()
                .unwrap(),
            b"# Manual\n\nFrom a.\n"
        );

        // Taking the same in again finds the same alternative.
        b.apply(&b.take(&remote));
        assert_eq!(open_alternatives(&b).len(), 1);
    }

    #[test]
    fn once_the_version_from_elsewhere_is_picked_the_two_sides_join() {
        let (_tmp, remote, a, b) = diverged();
        b.apply(&b.take(&remote));
        let id = open_alternatives(&b)[0].id.clone();
        let (author, committer) = commit_identities(b.repo()).unwrap();
        let chosen = crate::alternatives::pick(b.repo(), &id, &author, &committer).unwrap();
        b.save("manual.md", std::str::from_utf8(&chosen).unwrap());

        let plan = b.take(&remote);
        assert!(
            matches!(plan.notes, Step::Join { .. }),
            "got {:?}",
            plan.notes
        );
        assert!(matches!(b.apply(&plan), Taken::Done { .. }));
        assert_eq!(b.send(&remote), Sent::Done);

        // a takes in b's decision, which is a's own text.
        assert!(matches!(a.apply(&a.take(&remote)), Taken::Done { .. }));
        assert_eq!(
            a.read("manual.md").as_deref(),
            Some("# Manual\n\nFrom a.\n")
        );
        assert!(
            open_alternatives(&a).is_empty(),
            "the ended alternative is not opened on a"
        );
    }

    #[test]
    fn once_the_version_from_elsewhere_is_set_aside_this_version_is_the_one_both_keep() {
        let (_tmp, remote, a, b) = diverged();
        b.apply(&b.take(&remote));
        let id = open_alternatives(&b)[0].id.clone();
        crate::alternatives::end(b.repo(), &id).unwrap();

        let plan = b.take(&remote);
        let Step::Join {
            commit, incoming, ..
        } = &plan.notes
        else {
            panic!("expected a join, got {:?}", plan.notes);
        };
        assert!(
            incoming.is_empty(),
            "the decided note is not taken from elsewhere"
        );
        let joined = b.repo().find_commit(*commit).unwrap();
        assert_eq!(joined.parent_count(), 2, "both histories are kept");
        assert!(matches!(b.apply(&plan), Taken::Done { .. }));
        assert_eq!(
            b.read("manual.md").as_deref(),
            Some("# Manual\n\nFrom b.\n")
        );
        assert_eq!(b.send(&remote), Sent::Done);

        assert!(matches!(a.apply(&a.take(&remote)), Taken::Done { .. }));
        assert_eq!(
            a.read("manual.md").as_deref(),
            Some("# Manual\n\nFrom b.\n")
        );
        // a's own version is not lost: it is the alternative b set aside, now ended on a too.
        let ended = crate::alternatives::ended(a.repo()).unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(
            crate::alternatives::content(a.repo(), &ended[0])
                .unwrap()
                .unwrap(),
            b"# Manual\n\nFrom a.\n"
        );
    }

    #[test]
    fn a_new_change_after_a_decision_is_asked_about_again() {
        let (_tmp, remote, a, b) = diverged();
        b.apply(&b.take(&remote));
        let id = open_alternatives(&b)[0].id.clone();
        crate::alternatives::end(b.repo(), &id).unwrap();

        a.record("manual.md", "# Manual\n\nFrom a, again.\n");
        a.send(&remote);
        let taken = b.apply(&b.take(&remote));
        assert!(matches!(taken, Taken::Held { .. }), "got {taken:?}");
        let open = open_alternatives(&b);
        assert_eq!(open.len(), 1);
        assert_ne!(open[0].id, id);
    }

    #[test]
    fn an_alternative_started_on_one_machine_reaches_the_other() {
        let (_tmp, remote, a, b) = setup();
        a.record("manual.md", "# Manual\n");
        let (author, committer) = commit_identities(a.repo()).unwrap();
        let id = crate::alternatives::start(a.repo(), Path::new("manual.md"), &author, &committer)
            .unwrap();
        crate::alternatives::record(
            a.repo(),
            &id,
            b"# Another take\n",
            "try",
            &author,
            &committer,
        )
        .unwrap();
        a.send(&remote);

        b.apply(&b.take(&remote));
        let open = open_alternatives(&b);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, id);
        assert!(crate::alternatives::differs(b.repo(), &open[0]).unwrap());
        assert_eq!(
            b.read("manual.md").as_deref(),
            Some("# Manual\n"),
            "the note itself is unchanged"
        );
    }

    #[test]
    fn a_note_deleted_on_one_machine_and_changed_on_the_other_is_held() {
        let (_tmp, remote, a, b) = setup();
        a.record("manual.md", "# Manual\n");
        a.record("old.md", "# Old\n");
        a.send(&remote);
        b.apply(&b.take(&remote));

        // a deletes old.md: the recorded tree loses it.
        {
            let repo = a.repo();
            let tip = repo.find_commit(a.tip()).unwrap();
            let mut index = Index::new().unwrap();
            index.read_tree(&tip.tree().unwrap()).unwrap();
            index.remove_path(Path::new("old.md")).unwrap();
            let tree = repo.find_tree(index.write_tree_to(repo).unwrap()).unwrap();
            let (author, committer) = commit_identities(repo).unwrap();
            repo.commit(
                Some(NOTES_REF),
                &author,
                &committer,
                "delete",
                &tree,
                &[&tip],
            )
            .unwrap();
        }
        a.send(&remote);
        b.record("old.md", "# Old, kept\n");

        let plan = b.take(&remote);
        assert!(
            matches!(&plan.notes, Step::Held { overlap, .. } if overlap == &vec!["old.md".to_string()]),
            "expected held, got {:?}",
            plan.notes
        );
    }

    #[test]
    fn an_incoming_note_with_unrecorded_edits_here_is_not_to_be_written() {
        let (_tmp, remote, a, b) = setup();
        a.record("manual.md", "# Manual\n");
        a.send(&remote);
        b.apply(&b.take(&remote));
        b.save("manual.md", "# Manual\n\nTyped here, not yet a version.\n");

        a.record("manual.md", "# Manual\n\nFrom a.\n");
        a.send(&remote);

        let plan = b.take(&remote);
        assert!(matches!(plan.notes, Step::FastForward { .. }));
        assert_eq!(plan.unrecorded_here, vec!["manual.md".to_string()]);
    }

    #[test]
    fn a_join_is_not_a_version_of_the_notes_it_carried() {
        let (_tmp, remote, a, b) = setup();
        a.record("manual.md", "# Manual\n");
        a.send(&remote);
        b.apply(&b.take(&remote));
        a.record("release-notes.md", "# 1.0\n");
        a.send(&remote);
        b.record("faq.md", "# FAQ\n");
        let plan = b.take(&remote);
        b.apply(&plan);

        for (path, expected) in [("release-notes.md", 1), ("faq.md", 1), ("manual.md", 1)] {
            let versions =
                crate::git_engine::history(b.repo(), NOTES_REF, Path::new(path)).unwrap();
            assert_eq!(versions.len(), expected, "{path}: {versions:?}");
            assert!(
                versions.iter().all(|v| v.message != JOIN_MESSAGE),
                "{path}: {versions:?}"
            );
        }
    }

    fn keep(machine: &Machine, rel: &str, text: &str) {
        let (author, committer) = commit_identities(machine.repo()).unwrap();
        commit_paths(
            machine.repo(),
            SNAPSHOT_REF,
            &[(PathBuf::from(rel), text.as_bytes().to_vec())],
            "kept",
            &author,
            &committer,
            WhenUnchanged::Skip,
        )
        .unwrap();
    }

    #[test]
    fn what_was_kept_of_deleted_notes_is_joined_even_when_both_sides_kept_the_same_path() {
        let (_tmp, remote, a, b) = setup();
        keep(
            &a,
            "draft.md",
            "# From a
",
        );
        keep(
            &a,
            "only-a.md",
            "# A
",
        );
        a.send(&remote);
        keep(
            &b,
            "draft.md",
            "# From b
",
        );
        keep(
            &b,
            "only-b.md",
            "# B
",
        );

        let plan = b.take(&remote);
        let Step::Join { commit, .. } = plan.snapshots else {
            panic!("expected a join, got {:?}", plan.snapshots);
        };
        let joined = b.repo().find_commit(commit).unwrap();
        let tree = joined.tree().unwrap();
        for path in ["draft.md", "only-a.md", "only-b.md"] {
            assert!(tree.get_path(Path::new(path)).is_ok(), "{path} missing");
        }
        // This side's copy of the shared path is in the tree; the other side's stays reachable
        // through the second parent.
        let ours = tree.get_path(Path::new("draft.md")).unwrap().id();
        assert_eq!(
            b.repo().find_blob(ours).unwrap().content(),
            b"# From b
"
        );
        let theirs = joined.parent(1).unwrap().tree().unwrap();
        let theirs = theirs.get_path(Path::new("draft.md")).unwrap().id();
        assert_eq!(
            b.repo().find_blob(theirs).unwrap().content(),
            b"# From a
"
        );
    }

    #[test]
    fn what_an_exchange_names_is_relative_to_a_folder_inside_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        Repository::init_bare(&remote).unwrap();
        // Each machine opens a folder inside a repository that also holds other things.
        let machine = |name: &str| {
            Repository::init(tmp.path().join(name)).unwrap();
            Machine::new(tmp.path().join(name).join("notes"))
        };
        let (a, b) = (machine("a"), machine("b"));
        let url = Machine::url_of(&remote);
        let exchange_on =
            |m: &Machine| exchange(&m.vault, &m.root, &url, None, &NoteLocks::default()).unwrap();

        a.record("sub/manual.md", "# Manual\n");
        a.record("old.md", "# Old\n");
        a.send(&remote);
        let mut received = exchange_on(&b).received;
        received.sort();
        assert_eq!(
            received,
            vec!["old.md".to_string(), "sub/manual.md".to_string()]
        );
        assert_eq!(b.read("sub/manual.md").as_deref(), Some("# Manual\n"));

        b.save("old.md", "# Old\n\nTyped here, not yet a version.\n");
        a.record("old.md", "# Old\n\nFrom a.\n");
        a.send(&remote);
        assert_eq!(exchange_on(&b).kept_back, vec!["old.md".to_string()]);
        b.record("old.md", "# Old\n\nFrom a.\n");

        a.record("sub/manual.md", "# Manual\n\nFrom a.\n");
        a.send(&remote);
        b.record("sub/manual.md", "# Manual\n\nFrom b.\n");
        let held = exchange_on(&b);
        assert_eq!(held.held, vec!["sub/manual.md".to_string()]);
        assert_eq!(held.alternatives, vec!["sub/manual.md".to_string()]);
    }

    #[test]
    fn a_note_waiting_in_another_folder_of_the_repository_holds_the_send_without_being_named() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        Repository::init_bare(&remote).unwrap();
        for name in ["a", "b"] {
            Repository::init(tmp.path().join(name)).unwrap();
        }
        let a = Machine::new(tmp.path().join("a").join("x"));
        let (bx, by) = (
            Machine::new(tmp.path().join("b").join("x")),
            Machine::new(tmp.path().join("b").join("y")),
        );
        let url = Machine::url_of(&remote);
        a.record("n.md", "# N\n");
        a.send(&remote);
        exchange(&bx.vault, &bx.root, &url, None, &NoteLocks::default()).unwrap();

        a.record("n.md", "# N\n\nFrom a.\n");
        a.send(&remote);
        bx.record("n.md", "# N\n\nFrom b.\n");
        // Exchanged from the other folder: the note that waits is not one this folder can name,
        // and nothing is sent until it is decided.
        let out = exchange(&by.vault, &by.root, &url, None, &NoteLocks::default()).unwrap();
        assert!(
            out.held.is_empty() && out.alternatives.is_empty(),
            "{out:?}"
        );
        assert!(!out.sent);
    }

    #[test]
    fn what_arrives_is_never_sent_back() {
        let (_tmp, remote, a, b) = setup();
        a.record("manual.md", "# Manual\n");
        a.send(&remote);
        b.take(&remote);
        b.record("faq.md", "# FAQ\n");
        // b has not taken a's notes in; its send must not carry the tracking copy along.
        assert_eq!(b.send(&remote), Sent::Behind);
        let bare = Repository::open_bare(&remote).unwrap();
        let names: Vec<String> = bare
            .references()
            .unwrap()
            .filter_map(|r| r.ok().and_then(|r| r.name().ok().map(str::to_string)))
            .collect();
        assert!(names.iter().all(|n| !n.starts_with(TRACKING)), "{names:?}");
    }

    #[test]
    fn catching_up_writes_the_notes_before_moving_the_reference() {
        let (_tmp, remote, a, b) = setup();
        a.record(
            "manual.md",
            "# Manual
",
        );
        a.send(&remote);
        assert!(matches!(b.apply(&b.take(&remote)), Taken::Done { .. }));
        assert_eq!(
            b.read("manual.md").as_deref(),
            Some(
                "# Manual
"
            )
        );
        assert_eq!(b.tip(), a.tip());

        a.record(
            "manual.md",
            "# Manual

More.
",
        );
        a.send(&remote);
        let taken = b.apply(&b.take(&remote));
        assert_eq!(
            taken,
            Taken::Done {
                written: vec!["manual.md".into()],
                removed: vec![]
            }
        );
        assert_eq!(
            b.read("manual.md").as_deref(),
            Some(
                "# Manual

More.
"
            )
        );
        assert_eq!(b.tip(), a.tip());
    }

    #[test]
    fn an_edit_nobody_recorded_keeps_everything_back() {
        let (_tmp, remote, a, b) = setup();
        a.record(
            "manual.md",
            "# Manual
",
        );
        a.record(
            "faq.md", "# FAQ
",
        );
        a.send(&remote);
        b.apply(&b.take(&remote));
        let before = b.tip();
        b.save(
            "manual.md",
            "# Manual

Typed here.
",
        );

        a.record(
            "manual.md",
            "# Manual

From a.
",
        );
        a.record(
            "faq.md",
            "# FAQ

From a.
",
        );
        a.send(&remote);
        let taken = b.apply(&b.take(&remote));
        assert_eq!(
            taken,
            Taken::KeptBack {
                notes: vec!["manual.md".into()]
            }
        );
        // All or nothing: not even the untouched note was written, and nothing moved.
        assert_eq!(
            b.read("manual.md").as_deref(),
            Some(
                "# Manual

Typed here.
"
            )
        );
        assert_eq!(
            b.read("faq.md").as_deref(),
            Some(
                "# FAQ
"
            )
        );
        assert_eq!(b.tip(), before);
    }

    #[test]
    fn a_step_that_stopped_half_way_is_finished_by_the_next() {
        let (_tmp, remote, a, b) = setup();
        a.record(
            "manual.md",
            "# Manual
",
        );
        a.send(&remote);
        b.apply(&b.take(&remote));
        a.record(
            "manual.md",
            "# Manual

More.
",
        );
        a.send(&remote);
        // As if the file was written and the app stopped before the reference moved.
        b.save(
            "manual.md",
            "# Manual

More.
",
        );

        let plan = b.take(&remote);
        assert!(
            plan.unrecorded_here.is_empty(),
            "a file matching what arrived is not an edit"
        );
        assert_eq!(
            b.apply(&plan),
            Taken::Done {
                written: vec![],
                removed: vec![]
            }
        );
        assert_eq!(b.tip(), a.tip());
    }

    #[test]
    fn a_note_the_remote_removed_is_removed_here_and_stays_in_the_history() {
        let (_tmp, remote, a, b) = setup();
        a.record(
            "manual.md",
            "# Manual
",
        );
        a.record(
            "old.md", "# Old
",
        );
        a.send(&remote);
        b.apply(&b.take(&remote));
        assert!(b.read("old.md").is_some());
        {
            let repo = a.repo();
            let tip = repo.find_commit(a.tip()).unwrap();
            let mut index = Index::new().unwrap();
            index.read_tree(&tip.tree().unwrap()).unwrap();
            index.remove_path(Path::new("old.md")).unwrap();
            let tree = repo.find_tree(index.write_tree_to(repo).unwrap()).unwrap();
            let (author, committer) = commit_identities(repo).unwrap();
            repo.commit(
                Some(NOTES_REF),
                &author,
                &committer,
                "delete",
                &tree,
                &[&tip],
            )
            .unwrap();
        }
        a.send(&remote);

        let taken = b.apply(&b.take(&remote));
        assert_eq!(
            taken,
            Taken::Done {
                written: vec![],
                removed: vec!["old.md".into()]
            }
        );
        assert!(b.read("old.md").is_none());
        // Kept where deleted notes are listed, as any deletion here is.
        let kept =
            crate::git_engine::content_at_tip(b.repo(), SNAPSHOT_REF, Path::new("old.md")).unwrap();
        assert_eq!(
            kept.as_deref(),
            Some(
                &b"# Old
"[..]
            )
        );
    }

    #[test]
    fn a_version_added_while_taking_in_is_not_overwritten() {
        let (_tmp, remote, a, b) = setup();
        a.record(
            "manual.md",
            "# Manual
",
        );
        a.send(&remote);
        b.apply(&b.take(&remote));
        a.record(
            "manual.md",
            "# Manual

More.
",
        );
        a.send(&remote);
        let plan = b.take(&remote);
        // Between planning and taking in, this machine records another note.
        b.record(
            "faq.md", "# FAQ
",
        );
        let mine = b.tip();

        assert_eq!(b.apply(&plan), Taken::Overtaken);
        assert_eq!(b.tip(), mine, "the version recorded meanwhile stays");
        // What was written matches what arrived, so the next exchange joins cleanly.
        let next = b.take(&remote);
        assert!(matches!(next.notes, Step::Join { .. }), "{:?}", next.notes);
        assert!(next.unrecorded_here.is_empty());
        assert!(matches!(b.apply(&next), Taken::Done { .. }));
    }

    #[test]
    fn the_published_state_stays_on_this_machine() {
        let (_tmp, remote, a, _b) = setup();
        a.record("a.md", "# A");
        let repo = a.repo();
        let tree = repo
            .find_commit(repo.refname_to_id(NOTES_REF).unwrap())
            .unwrap()
            .tree_id();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        crate::git_engine::commit_tree_on(
            repo,
            crate::publish_assembly::PUBLISHED_REF,
            tree,
            "p",
            &sig,
            &sig,
        )
        .unwrap();
        a.send(&remote);
        // Backed up is read from what the remote was last seen to hold.
        a.take(&remote);
        let bare = git2::Repository::open(&remote).unwrap();
        assert!(bare
            .find_reference(crate::publish_assembly::PUBLISHED_REF)
            .is_err());
        assert!(backed_up(repo).unwrap());
    }

    /// A folder inside `machine`'s repository, opened as its own vault.
    fn folder(machine: &Machine, name: &str) -> Machine {
        let root = machine.root.join(name);
        std::fs::create_dir_all(&root).unwrap();
        let vault = crate::git_engine::prepare(&root).unwrap();
        Machine { root, vault }
    }

    #[test]
    fn notes_of_a_sibling_folder_are_written_where_that_folder_is() {
        let (_tmp, remote, a, b) = setup();
        a.record("x/plan.md", "# Plan");
        a.record("y/n.md", "# N");
        a.send(&remote);
        let x = folder(&b, "x");
        folder(&b, "y");

        let taken = x.apply(&x.take(&remote));
        // Only this folder's notes are said; the sibling's are on disk all the same.
        assert_eq!(
            taken,
            Taken::Done {
                written: vec!["x/plan.md".into()],
                removed: vec![]
            }
        );
        assert_eq!(b.read("y/n.md").as_deref(), Some("# N"));
        assert_eq!(b.tip(), a.tip());

        // The sibling, opened next, has nothing left to take: it already matches the record.
        let y = folder(&b, "y");
        let next = y.take(&remote);
        assert!(matches!(next.notes, Step::Nothing), "{:?}", next.notes);

        // A note the other side removes from the sibling leaves it here too, kept among deleted
        // notes.
        std::fs::remove_file(a.root.join("y/n.md")).unwrap();
        let repo = a.repo();
        let tip = repo.find_commit(a.tip()).unwrap();
        let mut index = Index::new().unwrap();
        index.read_tree(&tip.tree().unwrap()).unwrap();
        index.remove_path(Path::new("y/n.md")).unwrap();
        let tree = repo.find_tree(index.write_tree_to(repo).unwrap()).unwrap();
        let (author, committer) = commit_identities(repo).unwrap();
        repo.commit(
            Some(NOTES_REF),
            &author,
            &committer,
            "remove",
            &tree,
            &[&tip],
        )
        .unwrap();
        a.send(&remote);
        let taken = x.apply(&x.take(&remote));
        assert_eq!(
            taken,
            Taken::Done {
                written: vec![],
                removed: vec![]
            }
        );
        assert!(b.read("y/n.md").is_none());
        let kept = crate::git_engine::tip_tree(b.repo(), SNAPSHOT_REF).unwrap();
        assert!(kept.get_path(Path::new("y/n.md")).is_ok());
    }

    #[test]
    fn an_unrecorded_edit_in_a_sibling_folder_keeps_everything_back() {
        let (_tmp, remote, a, b) = setup();
        a.record("x/plan.md", "# Plan");
        a.record("y/n.md", "# N");
        a.send(&remote);
        let x = folder(&b, "x");
        b.save("y/n.md", "# Written here, never recorded");

        let plan = x.take(&remote);
        assert_eq!(plan.unrecorded_here, vec!["y/n.md".to_string()]);
        assert_eq!(
            x.apply(&plan),
            Taken::KeptBack {
                notes: vec!["y/n.md".into()]
            }
        );
        assert!(x.read("plan.md").is_none(), "all of it or none of it");
        assert_eq!(
            b.read("y/n.md").as_deref(),
            Some("# Written here, never recorded")
        );
        assert!(id_of(b.repo(), NOTES_REF).is_none());

        // The folder that was synced says why nothing arrived, though the note is not its own.
        let out = exchange(
            &x.vault,
            &x.root,
            &Machine::url_of(&remote),
            None,
            &NoteLocks::default(),
        )
        .unwrap();
        assert!(out.kept_back.is_empty());
        assert_eq!(out.waiting_elsewhere, vec!["y/n.md".to_string()]);
        assert!(!out.sent);
    }

    #[test]
    fn a_path_that_cannot_be_a_note_is_never_written() {
        let (_tmp, remote, a, b) = setup();
        a.record("plan.md", "# Plan");
        // Recorded without a file: on Windows the path names a stream of another file.
        let (author, committer) = commit_identities(a.repo()).unwrap();
        commit_paths(
            a.repo(),
            NOTES_REF,
            &[(PathBuf::from("notes/n.md:hidden"), b"stream".to_vec())],
            "version",
            &author,
            &committer,
            WhenUnchanged::Skip,
        )
        .unwrap();
        a.send(&remote);

        let plan = b.take(&remote);
        let refused = apply(&b.vault, &b.root, &plan, &NoteLocks::default()).unwrap_err();
        assert!(refused.contains("n.md:hidden"), "{refused}");
        assert!(b.read("plan.md").is_none(), "nothing was taken in");
        assert!(id_of(b.repo(), NOTES_REF).is_none());
    }

    #[test]
    fn only_the_repository_storage_and_paths_that_leave_are_refused() {
        let (_tmp, _remote, _a, b) = setup();
        for refused in [
            "../x.md",
            ".git/config",
            "a/.Git/x",
            "a/.git. /x",
            "GIT~1/x",
            "a:b.md",
            "",
        ] {
            assert!(place_of(&b.vault, &b.root, refused).is_none(), "{refused}");
        }
        // A notes folder may sit in a hidden folder; its notes are named through it.
        for kept in [
            ".notes/a.md",
            "a/.hidden.md",
            "git/x.md",
            ".github-notes/a.md",
        ] {
            assert!(place_of(&b.vault, &b.root, kept).is_some(), "{kept}");
        }
    }
}

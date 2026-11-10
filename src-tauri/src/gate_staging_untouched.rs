//! Gate: recording notes never disturbs what someone else has staged or checked out.
//!
//! This is a repository-level invariant rather than a unit test of any one function. It holds
//! whatever route recording takes, so it stays meaningful as the code underneath is rewritten.
//! It lives inside the crate because the layers it drives are internal, and in its own file so
//! it reads as the gate it is.
//!
//! A negative control at the bottom shows the invariant is falsifiable: the same scenario, run
//! the naive way, breaks it.

#![cfg(test)]

use crate::commands::commit_notes;
use git2::Repository;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Everything about the repository that belongs to whoever else is working in it.
#[derive(Debug, PartialEq, Eq)]
struct TheirState {
    /// Every staged entry as (path, object, mode).
    index: Vec<(String, String, u32)>,
    /// What the checked-out branch points at, if anything.
    head: Option<String>,
    /// Files present in the folder, so nothing is materialised behind their back.
    worktree: Vec<String>,
}

fn capture(repo: &Repository, root: &Path) -> TheirState {
    let index = repo.index().unwrap();
    let mut entries: Vec<(String, String, u32)> = index
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(&e.path).to_string(),
                e.id.to_string(),
                e.mode,
            )
        })
        .collect();
    entries.sort();

    let mut worktree: Vec<String> = walk(root, root);
    worktree.sort();

    TheirState {
        index: entries,
        head: repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .map(|o| o.to_string()),
        worktree,
    }
}

/// Relative paths of every file under `dir`, skipping the repository's own storage.
fn walk(root: &Path, dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if path.is_dir() {
            found.extend(walk(root, &path));
        } else if let Ok(rel) = path.strip_prefix(root) {
            found.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    found
}

/// A repository someone is working in: a branch checked out, work staged, files on disk.
fn their_repository() -> (TempDir, Repository) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let repo = Repository::init(root).unwrap();
    let signature = git2::Signature::now("Them", "them@example.invalid").unwrap();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/app.rs"), "fn main() {}").unwrap();
    std::fs::write(root.join("README.md"), "# their project").unwrap();

    // A first revision, so the branch exists and is checked out.
    {
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "their first revision",
            &tree,
            &[],
        )
        .unwrap();
    }

    // Work in progress, staged but not recorded — the state most easily destroyed.
    std::fs::write(root.join("src/app.rs"), "fn main() { /* half done */ }").unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_path(&PathBuf::from("src").join("app.rs"))
        .unwrap();
    index.write().unwrap();

    (tmp, repo)
}

#[test]
fn recording_notes_repeatedly_leaves_their_state_identical() {
    let (tmp, repo) = their_repository();
    let root = tmp.path();
    let before = capture(&repo, root);
    assert!(
        before.index.iter().any(|(p, _, _)| p == "src/app.rs"),
        "the scenario must actually have staged work"
    );

    let note = root.join("notes.md");
    let mut revisions = Vec::new();
    for n in 0..10 {
        std::fs::write(&note, format!("# revision {n}")).unwrap();
        let oid = commit_notes(
            root.to_string_lossy().to_string(),
            vec![note.to_string_lossy().to_string()],
            format!("record revision {n}"),
        )
        .expect("recording must succeed");
        revisions.push(oid);
    }

    let after = capture(&repo, root);
    assert_eq!(after.index, before.index, "not one staged entry may change");
    assert_eq!(
        after.head, before.head,
        "the checked-out branch may not move"
    );
    assert_eq!(
        after.worktree,
        {
            let mut expected = before.worktree.clone();
            expected.push("notes.md".to_string());
            expected.sort();
            expected
        },
        "only the note itself appears on disk, and only because it was written there"
    );

    // The work genuinely happened: ten distinct revisions, all reachable.
    revisions.sort();
    revisions.dedup();
    assert_eq!(revisions.len(), 10, "each recording is its own revision");
    assert!(repo.find_reference(crate::git_engine::NOTES_REF).is_ok());
}

#[test]
fn negative_control_the_naive_route_destroys_their_state() {
    // Establishes that the invariant above can fail. Building the revision from the index and
    // pointing HEAD at it — the obvious way to commit a file — sweeps their staged work into
    // it and moves the branch they are working on.
    let (tmp, repo) = their_repository();
    let root = tmp.path();
    let before = capture(&repo, root);

    std::fs::write(root.join("notes.md"), "# a note").unwrap();
    let signature = git2::Signature::now("Them", "them@example.invalid").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("notes.md")).unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "record a note the naive way",
        &tree,
        &[&parent],
    )
    .unwrap();

    let after = capture(&repo, root);
    assert_ne!(
        after.head, before.head,
        "the naive route is expected to move their branch"
    );
    assert!(
        tree.get_path(&PathBuf::from("src").join("app.rs")).is_ok(),
        "and to carry their half-finished work into the revision"
    );
}

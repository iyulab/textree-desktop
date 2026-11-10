//! Gate: deleting anything always leaves a way back to the state it was deleted in.
//!
//! An earlier reading of this invariant exempted notes that had been recorded, on the grounds
//! that history already held them. That reading is wrong, and the shape named "a note edited
//! since it was recorded" below is why: being in history says the *path* was written once, not
//! that the state on disk now is the state history holds. Skipping those loses precisely the
//! work that was never saved — the case where losing it hurts most.
//!
//! So the invariant is stated over the state at the moment of deletion, for *every* file, and
//! over a table of shapes rather than a single case.
//!
//! A negative control at the bottom shows the check is falsifiable.

#![cfg(test)]

use crate::commands::delete_node;
use crate::git_engine::{commit_identities, commit_paths, WhenUnchanged, NOTES_REF, SNAPSHOT_REF};
use git2::Repository;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// One vault shape to delete something from: files to create, what to record first, what to
/// change on disk after recording, and what to delete.
struct Shape {
    name: &'static str,
    files: &'static [(&'static str, &'static str)],
    recorded: &'static [&'static str],
    /// Written over the file after recording, so disk and history disagree.
    edited_after: &'static [(&'static str, &'static str)],
    delete: &'static str,
}

const SHAPES: &[Shape] = &[
    Shape {
        name: "a note that was never recorded",
        files: &[("a.md", "# a")],
        recorded: &[],
        edited_after: &[],
        delete: "a.md",
    },
    Shape {
        name: "a note inside a folder",
        files: &[("sub/a.md", "# a")],
        recorded: &[],
        edited_after: &[],
        delete: "sub/a.md",
    },
    Shape {
        name: "a whole folder of notes",
        files: &[
            ("sub/a.md", "# a"),
            ("sub/b.md", "# b"),
            ("sub/deep/c.md", "# c"),
        ],
        recorded: &[],
        edited_after: &[],
        delete: "sub",
    },
    Shape {
        name: "a folder where only some notes were recorded",
        files: &[("sub/a.md", "# a"), ("sub/b.md", "# b")],
        recorded: &["sub/a.md"],
        edited_after: &[],
        delete: "sub",
    },
    Shape {
        name: "a note that was recorded and never touched again",
        files: &[("a.md", "# a")],
        recorded: &["a.md"],
        edited_after: &[],
        delete: "a.md",
    },
    Shape {
        name: "a note edited since it was recorded",
        files: &[("a.md", "# first")],
        recorded: &["a.md"],
        edited_after: &[("a.md", "# second, never recorded")],
        delete: "a.md",
    },
    Shape {
        name: "a folder holding one edited note among untouched ones",
        files: &[("sub/a.md", "# a"), ("sub/b.md", "# b")],
        recorded: &["sub/a.md", "sub/b.md"],
        edited_after: &[("sub/b.md", "# b, moved on")],
        delete: "sub",
    },
];

/// Builds the shape and returns the vault, its repository, and what each file held on disk at
/// the moment of deletion.
fn build(shape: &Shape) -> (TempDir, Repository, Vec<(String, Vec<u8>)>) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let repo = Repository::init(root).unwrap();

    for (rel, body) in shape.files {
        let target = root.join(rel);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, body).unwrap();
    }

    if !shape.recorded.is_empty() {
        let entries: Vec<(PathBuf, Vec<u8>)> = shape
            .recorded
            .iter()
            .map(|rel| (PathBuf::from(rel), std::fs::read(root.join(rel)).unwrap()))
            .collect();
        let (author, committer) = commit_identities(&repo).unwrap();
        commit_paths(
            &repo,
            NOTES_REF,
            &entries,
            "record",
            &author,
            &committer,
            WhenUnchanged::Skip,
        )
        .unwrap();
    }

    for (rel, body) in shape.edited_after {
        std::fs::write(root.join(rel), body).unwrap();
    }

    let at_deletion = shape
        .files
        .iter()
        .map(|(rel, _)| ((*rel).to_string(), std::fs::read(root.join(rel)).unwrap()))
        .collect();

    (tmp, repo, at_deletion)
}

/// Contents kept for `rel`, if any.
fn kept(repo: &Repository, rel: &str) -> Option<Vec<u8>> {
    let tree = repo
        .find_reference(SNAPSHOT_REF)
        .ok()?
        .peel_to_commit()
        .ok()?
        .tree()
        .ok()?;
    let entry = tree.get_path(Path::new(rel)).ok()?;
    Some(repo.find_blob(entry.id()).ok()?.content().to_vec())
}

#[test]
fn everything_deleted_is_recoverable_as_it_was_when_it_was_deleted() {
    for shape in SHAPES {
        let (tmp, repo, at_deletion) = build(shape);
        let root = tmp.path();

        delete_node(
            root.to_string_lossy().to_string(),
            root.join(shape.delete).to_string_lossy().to_string(),
        )
        .unwrap_or_else(|e| panic!("{}: deleting failed: {e}", shape.name));

        for (rel, body) in &at_deletion {
            let was_under_delete =
                rel == shape.delete || rel.starts_with(&format!("{}/", shape.delete));
            if !was_under_delete {
                continue;
            }
            assert_eq!(
                kept(&repo, rel).as_deref(),
                Some(body.as_slice()),
                "{}: '{rel}' was deleted without its state being kept",
                shape.name
            );
        }
    }
}

#[test]
fn deleting_the_same_contents_twice_records_two_moments() {
    // The kept revision is also the only record of *when* something left the folder. Treating
    // a second deletion of identical contents as a duplicate would date it to the first one.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let repo = Repository::init(root).unwrap();

    let count_kept_revisions = || {
        let mut walk = repo.revwalk().unwrap();
        walk.push_ref(SNAPSHOT_REF).unwrap();
        walk.count()
    };

    std::fs::write(root.join("a.md"), "# a").unwrap();
    delete_node(
        root.to_string_lossy().to_string(),
        root.join("a.md").to_string_lossy().to_string(),
    )
    .unwrap();
    assert_eq!(count_kept_revisions(), 1);

    std::fs::write(root.join("a.md"), "# a").unwrap();
    delete_node(
        root.to_string_lossy().to_string(),
        root.join("a.md").to_string_lossy().to_string(),
    )
    .unwrap();
    assert_eq!(
        count_kept_revisions(),
        2,
        "two deletions are two events, whatever the contents were"
    );

    // Counting the revisions is not enough: the second one only counts if anything reading the
    // history can see it. Asking when the note was last kept has to answer with the second
    // deletion, not the first — otherwise the note is dated to a moment it was still in the
    // folder, and a restore hands back whatever was recorded in between.
    let kept_at =
        crate::git_engine::last_written(&repo, SNAPSHOT_REF, &["a.md".to_string()]).unwrap();
    let mut walk = repo.revwalk().unwrap();
    walk.push_ref(SNAPSHOT_REF).unwrap();
    let newest = repo
        .find_commit(walk.next().unwrap().unwrap())
        .unwrap()
        .time()
        .seconds();
    assert_eq!(
        kept_at.get("a.md"),
        Some(&newest),
        "the latest deletion is the one the history reports, not the first"
    );
}

#[test]
fn negative_control_deleting_without_keeping_anything_loses_it() {
    // Establishes that the gate is not guarding against nothing: remove the same file the
    // plain way and its contents exist nowhere afterwards.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let repo = Repository::init(root).unwrap();
    std::fs::write(root.join("a.md"), "# a").unwrap();

    std::fs::remove_file(root.join("a.md")).unwrap();

    assert!(kept(&repo, "a.md").is_none());
    assert!(!root.join("a.md").exists());
}

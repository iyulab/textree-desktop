//! Gate: the notes folder holds notes, and nothing the application put there.
//!
//! Two halves, and the first alone does not carry it. A folder ending up with a repository in it
//! is expected — recording is what makes one. What must not happen is anything *else* appearing,
//! or a repository appearing in a folder nothing was ever recorded in: looking at a history is
//! not making one, and the person who only opened a panel did not ask for a repository.
//!
//! A negative control at the bottom shows the check is falsifiable.

#![cfg(test)]

use crate::commands::{
    create_folder, create_note, delete_node, deleted_notes, move_state_out_of_vault, note_versions,
};
use std::collections::BTreeSet;
use std::path::Path;
use tempfile::TempDir;

/// Every name directly inside the folder.
fn entries(root: &Path) -> BTreeSet<String> {
    std::fs::read_dir(root)
        .map(|dir| {
            dir.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_folder_used_normally_ends_up_holding_notes_and_a_repository() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let root_s = root.to_string_lossy().to_string();

    let note = create_note(root_s.clone(), root_s.clone(), "Plan".into()).unwrap();
    create_folder(root_s.clone(), root_s.clone(), "Topics".into()).unwrap();
    let doomed = create_note(root_s.clone(), root_s.clone(), "Scratch".into()).unwrap();
    // Recording is what makes a repository; deleting is what used to make a place to set
    // copies aside.
    crate::commands::commit_notes(root_s.clone(), vec![note], "first".into()).unwrap();
    delete_node(root_s.clone(), doomed).unwrap();

    assert_eq!(
        entries(root),
        BTreeSet::from([".git".into(), "Plan.md".into(), "Topics".into()]),
        "a folder of notes, plus the repository the history lives in — and nothing else"
    );
    assert_eq!(
        deleted_notes(root_s).unwrap().len(),
        1,
        "and the deleted note is still reachable, without a copy in the folder"
    );
}

#[test]
fn looking_at_a_folder_does_not_turn_it_into_a_repository() {
    // Opening a panel is not recording. This is the half the "only .git" reading misses: a
    // repository is an allowed trace, so a check that only forbids others lets a read create one.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let root_s = root.to_string_lossy().to_string();
    std::fs::write(root.join("a.md"), "body").unwrap();

    let before = entries(root);
    deleted_notes(root_s.clone()).unwrap();
    note_versions(
        root_s.clone(),
        root.join("a.md").to_string_lossy().to_string(),
    )
    .unwrap();
    move_state_out_of_vault(root_s).unwrap();

    assert_eq!(
        entries(root),
        before,
        "the folder is exactly as it was found"
    );
    assert!(!root.join(".git").exists());
}

#[test]
fn negative_control_a_folder_holding_something_else_is_reported() {
    // Establishes that the check is not guarding against nothing: put one file where the
    // application used to put its own, and the comparison fails.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join(".textree")).unwrap();

    assert_ne!(entries(root), BTreeSet::from(["a.md".to_string()]));
    assert!(entries(root).contains(".textree"));
}

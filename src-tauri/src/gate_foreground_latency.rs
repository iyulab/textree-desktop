//! Gate: what the interface waits on does not grow with how long the vault has been in use.
//!
//! The constraint is on the foreground, and the axis that matters here is not how many notes a
//! folder holds but **how many revisions it has accumulated**. A vault stays the same size
//! while its history grows every time anything is recorded, so a read whose cost tracks the
//! history gets slower for someone who has done nothing but use the application.
//!
//! Measured before it was changed: `measure_reads_against_history_length` is the instrument,
//! kept so the numbers can be taken again rather than remembered. It is ignored by default
//! because it reports rather than asserts, and because timings are not something to fail a
//! build on.
//!
//! What is asserted is **work, not wall clock**: reading one note's history must not visit
//! revisions that never touched it. That is the property the timings were a symptom of, and
//! counting is deterministic where timing is not.

#![cfg(test)]

use crate::commands::{deleted_notes, note_version_text, note_versions};
use crate::git_engine::{
    commit_identities, commit_paths, full_comparisons, revisions_visited, WhenUnchanged, NOTES_REF,
};
use git2::Repository;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::TempDir;

/// A vault with `notes` notes and `revisions` revisions per note.
fn vault_with_history(notes: usize, revisions: usize) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let repo = Repository::init(root).unwrap();
    let (author, committer) = commit_identities(&repo).unwrap();

    for round in 0..revisions {
        for note in 0..notes {
            let rel = format!("note-{note}.md");
            let body = format!("note {note}, revision {round}");
            std::fs::write(root.join(&rel), &body).unwrap();
            commit_paths(
                &repo,
                NOTES_REF,
                &[(PathBuf::from(&rel), body.into_bytes())],
                &format!("revision {round} of note {note}"),
                &author,
                &committer,
                WhenUnchanged::Skip,
            )
            .unwrap();
        }
    }
    tmp
}

fn millis(f: impl FnOnce()) -> u128 {
    let started = Instant::now();
    f();
    started.elapsed().as_millis()
}

#[test]
#[ignore = "reports timings rather than asserting; run with --ignored to take the numbers"]
fn measure_reads_against_history_length() {
    println!("revisions | notes | deleted_notes | note_versions | note_version_text");
    for (notes, revisions) in [(5usize, 2usize), (5, 20), (5, 100), (5, 200)] {
        let tmp = vault_with_history(notes, revisions);
        let root = tmp.path().to_string_lossy().to_string();
        let one = tmp.path().join("note-0.md").to_string_lossy().to_string();
        // A list with nothing in it is not what the list costs. One note is taken out so the
        // reading is of the work actually done rather than of the shortcut past it.
        crate::commands::delete_node(
            root.clone(),
            tmp.path().join("note-1.md").to_string_lossy().to_string(),
        )
        .unwrap();

        let listing = millis(|| {
            deleted_notes(root.clone()).unwrap();
        });
        let mut versions = Vec::new();
        let history = millis(|| {
            versions = note_versions(root.clone(), one.clone()).unwrap();
        });
        let oldest = versions.last().unwrap().id.clone();
        let text = millis(|| {
            note_version_text(root.clone(), one.clone(), oldest.clone()).unwrap();
        });

        println!(
            "{:>9} | {:>5} | {listing:>13} | {history:>13} | {text:>17}",
            notes * revisions,
            notes
        );
    }
}

/// How many revisions the reference holds in total.
fn revisions_held(repo: &Repository) -> usize {
    let mut walk = repo.revwalk().unwrap();
    walk.push_ref(NOTES_REF).unwrap();
    walk.count()
}

/// Reads one note's history and reports how many whole-revision comparisons it took.
fn cost_of_reading_one(tmp: &TempDir) -> (usize, usize) {
    let root = tmp.path().to_string_lossy().to_string();
    let one = tmp.path().join("note-0.md").to_string_lossy().to_string();
    full_comparisons::taken();
    let found = note_versions(root, one).unwrap().len();
    (found, full_comparisons::taken())
}

#[test]
fn reading_one_notes_history_costs_the_same_however_much_else_was_recorded() {
    // Two vaults holding the same note with the same number of its own revisions, differing
    // only in how much unrelated history surrounds it. The answer is the same, and so is the
    // work — which is the part that used to grow.
    let few = vault_with_history(1, 10);
    let many = vault_with_history(10, 10);

    let (found_few, cost_few) = cost_of_reading_one(&few);
    let (found_many, cost_many) = cost_of_reading_one(&many);

    assert_eq!(found_few, 10);
    assert_eq!(found_many, 10, "other notes' revisions are not this note's");
    assert_eq!(
        cost_many, cost_few,
        "ten times the history must not be ten times the work"
    );
    assert!(
        cost_few <= 1,
        "a note that was never renamed needs no whole-revision comparison beyond the one that          introduced it, and got {cost_few}"
    );

    // And the vaults really do differ in how much history is there, so the comparison above is
    // not between two identical things.
    let held = |tmp: &TempDir| revisions_held(&Repository::open(tmp.path()).unwrap());
    assert_eq!(held(&few), 10);
    assert_eq!(held(&many), 100);
}

#[test]
fn the_deleted_list_asks_nothing_about_notes_that_are_still_there() {
    // Every note is present, so there is no question to put to the history at all.
    let tmp = vault_with_history(5, 10);
    let root = tmp.path().to_string_lossy().to_string();
    full_comparisons::taken();

    assert!(
        deleted_notes(root).unwrap().is_empty(),
        "nothing was deleted, so the list has nothing to say"
    );
    assert_eq!(full_comparisons::taken(), 0);
}

#[test]
fn asking_about_a_note_named_by_both_references_still_stops_early() {
    // A note that was recorded and then deleted is named by the recorded paths AND by the kept
    // ones, so it reaches the history twice. The walk stops when every path asked about has an
    // answer, and that test counts answers against questions — two questions with one answer
    // between them never balances, and the walk then reads the whole history instead. The
    // answers look the same either way, so only the number of revisions visited shows it.
    let tmp = vault_with_history(1, 40);
    let root = tmp.path();
    let note = root.join("note-0.md");
    crate::commands::delete_node(
        root.to_string_lossy().to_string(),
        note.to_string_lossy().to_string(),
    )
    .unwrap();

    revisions_visited::taken();
    let listed = deleted_notes(root.to_string_lossy().to_string()).unwrap();
    let visited = revisions_visited::taken();

    assert_eq!(
        listed.len(),
        1,
        "the deleted note is listed once, not twice"
    );
    assert!(
        visited <= 4,
        "the newest revisions hold the answer, so the walk should stop within a few of them;          it visited {visited} of 41"
    );
}

/// A vault holding `notes` notes, `per_folder` to a folder, all recorded in one revision.
fn vault_of_size(notes: usize, per_folder: usize) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let repo = Repository::init(root).unwrap();
    let (author, committer) = commit_identities(&repo).unwrap();
    let mut entries = Vec::with_capacity(notes);
    for note in 0..notes {
        let rel =
            PathBuf::from(format!("folder-{}", note / per_folder)).join(format!("note-{note}.md"));
        let body = format!("# note {note}\n\nsome text\n").into_bytes();
        std::fs::create_dir_all(root.join(rel.parent().unwrap())).unwrap();
        std::fs::write(root.join(&rel), &body).unwrap();
        entries.push((rel, body));
    }
    commit_paths(
        &repo,
        NOTES_REF,
        &entries,
        "everything",
        &author,
        &committer,
        WhenUnchanged::Skip,
    )
    .unwrap();
    tmp
}

/// The other axis: how the foreground grows with how many notes the vault holds. The history is
/// one revision here, so what is measured is the size of the tree alone.
#[test]
#[ignore = "reports timings rather than asserting; run with --ignored to take the numbers"]
fn measure_foreground_against_tree_size() {
    println!(
        "notes | per folder | list_tree | read_note | add version | note_versions | deleted_notes"
    );
    for (notes, per_folder) in [
        (1_000usize, 100usize),
        (5_000, 100),
        (10_000, 100),
        (1_000, 1_000),
        (5_000, 5_000),
        (10_000, 10_000),
    ] {
        let tmp = vault_of_size(notes, per_folder);
        let root = tmp.path().to_string_lossy().to_string();
        let one = tmp.path().join("folder-0").join("note-0.md");
        let one_s = one.to_string_lossy().to_string();
        crate::commands::delete_node(
            root.clone(),
            tmp.path()
                .join("folder-0")
                .join("note-1.md")
                .to_string_lossy()
                .to_string(),
        )
        .unwrap();

        let listing = millis(|| {
            crate::commands::tree_of(root.clone()).unwrap();
        });
        let read = millis(|| {
            crate::commands::note_text(root.clone(), one_s.clone()).unwrap();
        });
        let add = millis(|| {
            let repo = Repository::open(tmp.path()).unwrap();
            let (author, committer) = commit_identities(&repo).unwrap();
            let rel = PathBuf::from("folder-0").join("note-0.md");
            commit_paths(
                &repo,
                NOTES_REF,
                &[(rel, b"# note 0\n\nchanged\n".to_vec())],
                "one note",
                &author,
                &committer,
                WhenUnchanged::Skip,
            )
            .unwrap();
        });
        let history = millis(|| {
            note_versions(root.clone(), one_s.clone()).unwrap();
        });
        let deleted = millis(|| {
            deleted_notes(root.clone()).unwrap();
        });
        println!(
            "{notes:>5} | {per_folder:>10} | {listing:>9} | {read:>9} | {add:>11} | {history:>13} | {deleted:>13}"
        );
    }
}

#[test]
#[ignore = "reports timings rather than asserting; run with --ignored to take the numbers"]
fn measure_where_the_deleted_list_spends_its_time() {
    let tmp = vault_of_size(10_000, 100);
    let repo = Repository::open(tmp.path()).unwrap();
    let mut paths = Vec::new();
    let walk = millis(|| {
        paths = crate::git_engine::tip_paths(&repo, NOTES_REF).unwrap();
    });
    let stat = millis(|| {
        for p in &paths {
            let _ = tmp.path().join(p).exists();
        }
    });
    let listing = millis(|| {
        let mut seen = 0usize;
        for dir in std::fs::read_dir(tmp.path()).unwrap().flatten() {
            if dir.path().is_dir() {
                seen += std::fs::read_dir(dir.path()).unwrap().count();
            }
        }
        assert!(seen >= 10_000);
    });
    println!(
        "tip_paths walk {walk} ms · exists() x{} {stat} ms · read_dir per folder {listing} ms",
        paths.len()
    );
}

//! Gate: nothing the application does takes anything out of a folder's history.
//!
//! Every state the application has put into its own history — versions a person added, and the
//! states kept when something was deleted or overwritten — stays reachable from its refs, whatever
//! is done afterwards. This is checked as a property rather than case by case: many sequences of
//! operations (edit, add version, delete, bring back, rename, move, go back to a version, create),
//! drawn from a fixed seed so a failure reproduces, with the reachable history compared before and
//! after every step.
//!
//! A negative control at the bottom shows the check notices a ref moved back.

#![cfg(test)]

use crate::commands::{
    commit_notes, delete_node, deleted_notes, move_node, note_versions, rename_node,
    restore_deleted, restore_version,
};
use git2::{Oid, Repository};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const SEEDS: u64 = 24;
const STEPS: usize = 30;

/// Every commit reachable from the application's own refs in the repository at `root`.
fn reachable(root: &Path) -> HashSet<Oid> {
    let Ok(repo) = Repository::open(root) else {
        return HashSet::new();
    };
    let mut walk = repo.revwalk().unwrap();
    let mut any = false;
    for reference in repo.references().unwrap().flatten() {
        let ours = reference
            .name()
            .ok()
            .is_some_and(|n: &str| n.starts_with("refs/textree/"));
        if let (true, Some(target)) = (ours, reference.target()) {
            walk.push(target).unwrap();
            any = true;
        }
    }
    if !any {
        return HashSet::new();
    }
    walk.map(|oid| oid.unwrap()).collect()
}

/// `Err` naming how much of `before` is no longer reachable in `after`.
fn nothing_lost(before: &HashSet<Oid>, after: &HashSet<Oid>) -> Result<(), String> {
    let lost = before.difference(after).count();
    if lost == 0 {
        Ok(())
    } else {
        Err(format!("{lost} recorded state(s) no longer reachable"))
    }
}

/// A small deterministic generator, so a failing sequence can be replayed from its seed.
struct Draw(u64);
impl Draw {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn pick<'a, T>(&mut self, from: &'a [T]) -> Option<&'a T> {
        (!from.is_empty()).then(|| &from[self.next() as usize % from.len()])
    }
}

/// The notes in the folder, as absolute paths, in a stable order.
fn notes(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn s(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

const KINDS: [&str; 8] = [
    "edit",
    "add version",
    "delete",
    "bring back",
    "rename",
    "move",
    "go back",
    "create",
];

/// Runs one operation of kind `kind`. `Ok(false)`: nothing to do it to in this state.
fn step(root: &Path, kind: &str, draw: &mut Draw, n: usize) -> Result<bool, String> {
    let r = s(root);
    let present = notes(root);
    match kind {
        "edit" => {
            let Some(note) = draw.pick(&present) else {
                return Ok(false);
            };
            std::fs::write(note, format!("# edit {n}\n")).map_err(|e| e.to_string())?;
        }
        "add version" => {
            if present.is_empty() {
                return Ok(false);
            }
            let chosen: Vec<String> = present
                .iter()
                .filter(|_| draw.next().is_multiple_of(2))
                .map(|p| s(p))
                .collect();
            let chosen = if chosen.is_empty() {
                vec![s(&present[0])]
            } else {
                chosen
            };
            commit_notes(r, chosen, format!("v{n}"))?;
        }
        "delete" => {
            let Some(note) = draw.pick(&present) else {
                return Ok(false);
            };
            delete_node(r, s(note))?;
        }
        "bring back" => {
            let gone = deleted_notes(r.clone())?;
            let Some(note) = draw.pick(&gone) else {
                return Ok(false);
            };
            restore_deleted(r, note.rel.clone())?;
        }
        "rename" => {
            let Some(note) = draw.pick(&present) else {
                return Ok(false);
            };
            rename_node(r, s(note), format!("r{n}"))?;
        }
        "move" => {
            let Some(note) = draw.pick(&present) else {
                return Ok(false);
            };
            let into = if note.parent() == Some(root) {
                root.join("sub")
            } else {
                root.to_path_buf()
            };
            std::fs::create_dir_all(&into).map_err(|e| e.to_string())?;
            move_node(r, s(note), s(&into))?;
        }
        "go back" => {
            let Some(note) = draw.pick(&present) else {
                return Ok(false);
            };
            let versions = note_versions(r.clone(), s(note))?;
            let Some(version) = draw.pick(&versions) else {
                return Ok(false);
            };
            restore_version(r, s(note), version.id.clone())?;
        }
        "create" => {
            std::fs::write(root.join(format!("n{n}.md")), format!("# new {n}\n"))
                .map_err(|e| e.to_string())?;
        }
        _ => unreachable!(),
    }
    Ok(true)
}

#[test]
fn nothing_ever_recorded_stops_being_reachable() {
    let mut done: BTreeMap<&str, usize> = BTreeMap::new();
    for seed in 0..SEEDS {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.md"), "# a\n").unwrap();
        std::fs::write(root.join("b.md"), "# b\n").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("c.md"), "# c\n").unwrap();

        let mut draw = Draw(seed);
        for n in 0..STEPS {
            let kind = *draw.pick(&KINDS).unwrap();
            let before = reachable(root);
            let ran = step(root, kind, &mut draw, n)
                .unwrap_or_else(|e| panic!("seed {seed}, step {n} ({kind}) failed: {e}"));
            if ran {
                *done.entry(kind).or_default() += 1;
            }
            nothing_lost(&before, &reachable(root))
                .unwrap_or_else(|e| panic!("seed {seed}, step {n} ({kind}): {e}"));
        }
    }
    // The property only means something if every kind of operation actually ran.
    for kind in KINDS {
        assert!(
            done.get(kind).copied().unwrap_or(0) > 0,
            "'{kind}' never ran: {done:?}"
        );
    }
}

#[test]
fn negative_control_moving_a_ref_back_is_noticed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let note = root.join("a.md");
    for text in ["# one\n", "# two\n"] {
        std::fs::write(&note, text).unwrap();
        commit_notes(s(root), vec![s(&note)], text.into()).unwrap();
    }
    let before = reachable(root);

    let repo = Repository::open(root).unwrap();
    let tip = repo
        .find_reference(crate::git_engine::NOTES_REF)
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let parent = tip.parent(0).unwrap();
    repo.reference(
        crate::git_engine::NOTES_REF,
        parent.id(),
        true,
        "moved back",
    )
    .unwrap();

    let err = nothing_lost(&before, &reachable(root)).unwrap_err();
    assert!(err.contains("1 recorded state"), "{err}");
}

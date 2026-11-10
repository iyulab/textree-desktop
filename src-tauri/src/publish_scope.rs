//! What goes out when a folder is published, as the person chose it: nothing unless chosen,
//! a chosen folder covers everything under it, and the nearest explicit choice wins.

use serde::{Deserialize, Serialize};

/// One explicit choice. `path` is `/`-separated; `""` is the whole folder it is resolved against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Choice {
    pub path: String,
    pub include: bool,
}

fn covers(choice: &str, rel: &str) -> bool {
    choice.is_empty()
        || rel.eq_ignore_ascii_case(choice)
        || (rel.len() > choice.len()
            && rel.is_char_boundary(choice.len())
            && rel.as_bytes()[choice.len()] == b'/'
            && rel[..choice.len()].eq_ignore_ascii_case(choice))
}

/// The explicit choice that decides `rel`: the nearest one covering it (last wins on ties).
pub fn nearest<'a>(rel: &str, choices: &'a [Choice]) -> Option<&'a Choice> {
    choices
        .iter()
        .filter(|c| covers(&c.path, rel))
        .max_by_key(|c| c.path.len())
}

/// Whether `rel` goes out under `choices`.
pub fn is_included(rel: &str, choices: &[Choice]) -> bool {
    nearest(rel, choices).is_some_and(|c| c.include)
}

use git2::{Repository, Tree};
use std::path::{Path, PathBuf};

/// Where the choices are recorded. It travels with the notes to a connected remote.
pub const PUBLIC_REF: &str = "refs/textree/public";
const FILE: &str = "choices";

fn parse(text: &str) -> Vec<Choice> {
    text.lines()
        .filter_map(|line| {
            let (kind, path) = line.split_once('\t')?;
            let path = if path == "." {
                String::new()
            } else {
                path.to_string()
            };
            match kind {
                "include" => Some(Choice {
                    path,
                    include: true,
                }),
                "leave-out" => Some(Choice {
                    path,
                    include: false,
                }),
                _ => None,
            }
        })
        .collect()
}

fn render(choices: &[Choice]) -> String {
    let mut sorted = choices.to_vec();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    sorted
        .iter()
        .map(|c| {
            let kind = if c.include { "include" } else { "leave-out" };
            let path = if c.path.is_empty() {
                "."
            } else {
                c.path.as_str()
            };
            format!("{kind}\t{path}\n")
        })
        .collect()
}

fn choices_in(repo: &Repository, tree: Option<&Tree<'_>>) -> Result<Vec<Choice>, git2::Error> {
    let Some(tree) = tree else {
        return Ok(Vec::new());
    };
    let Some(id) = crate::git_engine::blob_in_tree(repo, tree, Path::new(FILE)) else {
        return Ok(Vec::new());
    };
    Ok(parse(&String::from_utf8_lossy(
        repo.find_blob(id)?.content(),
    )))
}

/// The choices recorded on `at` (a reference name), `/`-separated and relative to the repository.
pub fn read(repo: &Repository, at: &str) -> Result<Vec<Choice>, git2::Error> {
    choices_in(repo, crate::git_engine::tip_tree(repo, at).as_ref())
}

/// Reads the choices at the tip, lets `change` alter them, and records the result, all in one
/// turn: a change made by someone else meanwhile is read again, never overwritten.
fn update(
    repo: &Repository,
    message: &str,
    mut change: impl FnMut(&mut Vec<Choice>) -> bool,
) -> Result<(), git2::Error> {
    let (author, committer) = crate::git_engine::commit_identities(repo)?;
    crate::git_engine::update_tree_on(repo, PUBLIC_REF, message, &author, &committer, |tree| {
        let mut choices = choices_in(repo, tree)?;
        if !change(&mut choices) {
            return Ok(None);
        }
        let blob = repo.blob(render(&choices).as_bytes())?;
        crate::git_engine::tree_with_file(repo, None, Path::new(FILE), Some(blob)).map(Some)
    })?;
    Ok(())
}

/// Records the choice for `rel_in_repo`: include, leave out, or (`None`) follow the folder above.
pub fn set(repo: &Repository, rel_in_repo: &str, choice: Option<bool>) -> Result<(), git2::Error> {
    update(repo, "Publish choice", |choices| {
        choices.retain(|c| !c.path.eq_ignore_ascii_case(rel_in_repo));
        if let Some(include) = choice {
            choices.push(Choice {
                path: rel_in_repo.to_string(),
                include,
            });
        }
        true
    })
}

fn slashed(p: &Path) -> String {
    crate::git_engine::slashed(p)
}

/// Drops the choices at or under `path` that `drops` picks; says whether any went. The whole
/// folder (`""`) is never a place something comes into existence, so it drops nothing.
fn drop_under(choices: &mut Vec<Choice>, path: &str, drops: impl Fn(&Choice) -> bool) -> bool {
    if path.is_empty() {
        return false;
    }
    let before = choices.len();
    choices.retain(|c| !(covers(path, &c.path) && drops(c)));
    choices.len() != before
}

/// Drops the include choices at or under each of `paths`. A path that comes into existence never
/// inherits an include it did not ask for; a leave-out is kept (it can only hold back) — so a note
/// left out, deleted and brought back is still left out. Runs for what went away and for what was
/// just made. Writes nothing when nothing matches, and makes no repository.
pub fn vacate(repo: &Repository, paths: &[PathBuf]) -> Result<(), git2::Error> {
    if repo.refname_to_id(PUBLIC_REF).is_err() {
        return Ok(());
    }
    update(repo, "Publish choice cleared", |choices| {
        paths.iter().fold(false, |changed, p| {
            drop_under(choices, &slashed(p), |c| c.include) | changed
        })
    })
}

/// Carries the choices of moved notes and folders to where they went. A choice left at the
/// destination by something that went away is dropped first, so the carried one is the only one.
pub fn follow_moves(repo: &Repository, moves: &[(PathBuf, PathBuf)]) -> Result<(), git2::Error> {
    update(repo, "Publish choice follows a move", |choices| {
        let mut changed = false;
        for (from, to) in moves {
            let (from, to) = (slashed(from), slashed(to));
            if from.is_empty() || to.is_empty() {
                continue;
            }
            // Taken out before the destination is cleared: a rename that only changes case has a
            // destination covering its source.
            let (mut moving, mut rest): (Vec<Choice>, Vec<Choice>) = std::mem::take(choices)
                .into_iter()
                .partition(|c| covers(&from, &c.path));
            // Every kind: the carried choice is the only one left at the destination.
            changed |= drop_under(&mut rest, &to, |_| true);
            for c in &mut moving {
                c.path = format!("{to}{}", &c.path[from.len()..]);
                changed = true;
            }
            rest.append(&mut moving);
            *choices = rest;
        }
        changed
    })
}

/// What one side did to the recorded notes since the two sides last agreed: the renames git finds
/// between the common revision and that side's tip, and the tip itself.
struct Moved<'r> {
    renames: Vec<(String, String)>,
    tip: Tree<'r>,
}

impl Moved<'_> {
    /// Where the choice at `path` went: a note's own rename, or, for a folder that is gone from the
    /// tip, the place its notes went with their names below the folder kept. A note moved out of a
    /// folder that is still there says nothing about the folder.
    fn destination(&self, repo: &Repository, path: &str) -> Option<String> {
        if path.is_empty() {
            return None;
        }
        if let Some((_, to)) = self
            .renames
            .iter()
            .find(|(from, _)| from.eq_ignore_ascii_case(path))
        {
            return Some(to.clone());
        }
        if holds(repo, &self.tip, path) {
            return None;
        }
        self.renames.iter().find_map(|(from, to)| {
            let below = from
                .get(path.len()..)
                .filter(|_| covers(path, from) && from.len() > path.len())?;
            (to.len() > below.len() && to.ends_with(below))
                .then(|| to[..to.len() - below.len()].to_string())
        })
    }

    /// `choices` with each one carried to where this side moved what it names. Each is placed on
    /// its own — a folder and a note inside it each have their own move — and where a carried
    /// choice lands on one that stayed, the carried one is kept.
    fn carried(&self, repo: &Repository, choices: &[Choice]) -> Vec<Choice> {
        let (mut moved, mut stayed): (Vec<Choice>, Vec<Choice>) = (Vec::new(), Vec::new());
        for c in choices {
            match self.destination(repo, &c.path) {
                Some(path) => moved.push(Choice {
                    path,
                    include: c.include,
                }),
                None => stayed.push(c.clone()),
            }
        }
        stayed.retain(|s| !moved.iter().any(|m| m.path.eq_ignore_ascii_case(&s.path)));
        stayed.append(&mut moved);
        stayed
    }
}

/// Whether anything is recorded at `rel` (a note or a folder) in `tree`.
fn holds(repo: &Repository, tree: &Tree<'_>, rel: &str) -> bool {
    let mut current = tree.clone();
    let mut names = rel.split('/').peekable();
    while let Some(name) = names.next() {
        let Some(id) = current.get_name(name).map(|e| e.id()) else {
            return false;
        };
        if names.peek().is_none() {
            return true;
        }
        match repo.find_tree(id).ok() {
            Some(next) => current = next,
            None => return false,
        }
    }
    false
}

/// The renames on each side of the recorded notes since the two last agreed, as (here, there).
/// `None` for a side when there is nothing to compare: no notes recorded here or received.
fn moved_on_each_side(
    repo: &Repository,
) -> Result<(Option<Moved<'_>>, Option<Moved<'_>>), git2::Error> {
    let notes = crate::git_engine::NOTES_REF;
    let (Ok(ours), Ok(theirs)) = (
        repo.refname_to_id(notes),
        repo.refname_to_id(&crate::remote::tracking_of(notes)),
    ) else {
        return Ok((None, None));
    };
    let base = match repo.merge_base(ours, theirs) {
        Ok(id) => repo.find_commit(id)?.tree()?,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok((None, None)),
        Err(e) => return Err(e),
    };
    let side = |tip: git2::Oid| -> Result<Option<Moved<'_>>, git2::Error> {
        let tip = repo.find_commit(tip)?.tree()?;
        let mut diff = repo.diff_tree_to_tree(Some(&base), Some(&tip), None)?;
        let mut finding = git2::DiffFindOptions::new();
        finding.renames(true);
        diff.find_similar(Some(&mut finding))?;
        let renames: Vec<(String, String)> = diff
            .deltas()
            .filter(|d| d.status() == git2::Delta::Renamed)
            .filter_map(|d| Some((slashed(d.old_file().path()?), slashed(d.new_file().path()?))))
            .collect();
        Ok((!renames.is_empty()).then_some(Moved { renames, tip }))
    };
    Ok((side(ours)?, side(theirs)?))
}

fn value_of(choices: &[Choice], path: &str) -> Option<bool> {
    choices
        .iter()
        .find(|c| c.path.eq_ignore_ascii_case(path))
        .map(|c| c.include)
}

/// Joins two sides against what both started from. A side that did not touch a path takes the
/// other side's value (so a choice one device withdrew stays withdrawn); where both changed it
/// differently, the note stays out — one device withholding it is enough.
fn join(base: &[Choice], mine: &[Choice], other: &[Choice]) -> Vec<Choice> {
    let mut paths: Vec<&str> = Vec::new();
    for c in mine.iter().chain(other.iter()).chain(base.iter()) {
        if !paths.iter().any(|p| p.eq_ignore_ascii_case(&c.path)) {
            paths.push(&c.path);
        }
    }
    let mut joined = Vec::new();
    for path in paths {
        let (b, m, o) = (
            value_of(base, path),
            value_of(mine, path),
            value_of(other, path),
        );
        let (value, from) = if m == b {
            (o, other)
        } else if o == b || m == o {
            (m, mine)
        } else {
            (Some(false), mine)
        };
        if let Some(include) = value {
            let spelled = from
                .iter()
                .chain(mine)
                .chain(other)
                .find(|c| c.path.eq_ignore_ascii_case(path));
            joined.push(Choice {
                path: spelled.map_or(path, |c| c.path.as_str()).to_string(),
                include,
            });
        }
    }
    joined
}

/// Takes in the choices a remote holds: adopt when this side has none, move forward when it is
/// simply behind, keep when it is ahead, and otherwise join the two in one revision. Everything is
/// read inside one turn, so a choice made here meanwhile is never joined from a stale tip.
pub fn take_in(repo: &Repository) -> Result<(), git2::Error> {
    let Ok(theirs) = repo.refname_to_id(&crate::remote::tracking_of(PUBLIC_REF)) else {
        return Ok(());
    };
    let (author, committer) = crate::git_engine::commit_identities(repo)?;
    const MESSAGE: &str = "Publish choices from another device";
    crate::git_engine::advancing(|| {
        match repo.refname_to_id(PUBLIC_REF).ok() {
            None => crate::git_engine::advance(repo, PUBLIC_REF, theirs, None, MESSAGE),
            Some(ours) if ours == theirs || repo.graph_descendant_of(ours, theirs)? => Ok(()),
            Some(ours) if repo.graph_descendant_of(theirs, ours)? => {
                crate::git_engine::advance(repo, PUBLIC_REF, theirs, Some(ours), MESSAGE)
            }
            Some(ours) => {
                // The other side is read from the commit taken above, never from the moving reference.
                let (a, b) = (repo.find_commit(ours)?, repo.find_commit(theirs)?);
                let base = match repo.merge_base(ours, theirs) {
                    Ok(id) => choices_in(repo, Some(&repo.find_commit(id)?.tree()?))?,
                    Err(e) if e.code() == git2::ErrorCode::NotFound => Vec::new(),
                    Err(e) => return Err(e),
                };
                // Choices name paths; a note one side moved is still the same note. Each side's
                // choices are carried along the other side's moves first, so a choice made on one
                // device about a note never stays behind at a name the other device moved it from.
                let (mut mine, mut other) = (
                    choices_in(repo, Some(&a.tree()?))?,
                    choices_in(repo, Some(&b.tree()?))?,
                );
                let mut base = base;
                let (here, there) = moved_on_each_side(repo)?;
                if let Some(here) = &here {
                    other = here.carried(repo, &other);
                    base = here.carried(repo, &base);
                }
                if let Some(there) = &there {
                    mine = there.carried(repo, &mine);
                    base = there.carried(repo, &base);
                }
                let joined = join(&base, &mine, &other);
                let blob = repo.blob(render(&joined).as_bytes())?;
                let tree =
                    crate::git_engine::tree_with_file(repo, None, Path::new(FILE), Some(blob))?;
                let tree = repo.find_tree(tree)?;
                let commit = repo.commit(
                    None,
                    &author,
                    &committer,
                    "Publish choices joined with another device",
                    &tree,
                    &[&a, &b],
                )?;
                crate::git_engine::advance(repo, PUBLIC_REF, commit, Some(ours), MESSAGE)
            }
        }
    })
}

/// The choices as seen from a notes folder at `vault_in_repo` (`""` when it is the repository root).
pub fn for_vault(choices: &[Choice], vault_in_repo: &str) -> Vec<Choice> {
    if vault_in_repo.is_empty() {
        return choices.to_vec();
    }
    let mut out = Vec::new();
    // The nearest choice above the folder decides what the folder starts as.
    if let Some(above) = choices
        .iter()
        .filter(|c| covers(&c.path, vault_in_repo) && !c.path.eq_ignore_ascii_case(vault_in_repo))
        .max_by_key(|c| c.path.len())
    {
        out.push(Choice {
            path: String::new(),
            include: above.include,
        });
    }
    for c in choices {
        if c.path.eq_ignore_ascii_case(vault_in_repo) {
            out.retain(|o: &Choice| !o.path.is_empty());
            out.push(Choice {
                path: String::new(),
                include: c.include,
            });
        } else if covers(vault_in_repo, &c.path) {
            out.push(Choice {
                path: c.path[vault_in_repo.len() + 1..].to_string(),
                include: c.include,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_is_the_closest_explicit_choice_or_none() {
        let choices = vec![
            Choice {
                path: "a".into(),
                include: true,
            },
            Choice {
                path: "a/private".into(),
                include: false,
            },
        ];
        assert_eq!(nearest("a/x.md", &choices).map(|c| c.include), Some(true));
        assert_eq!(
            nearest("A/Private/x.png", &choices).map(|c| c.include),
            Some(false)
        );
        assert!(nearest("b/x.md", &choices).is_none());
    }

    #[derive(Deserialize)]
    struct Case {
        why: String,
        choices: Vec<Choice>,
        rel: String,
        included: bool,
    }

    #[test]
    fn the_rule_matches_the_shared_cases() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../../src/lib/publishScope.cases.json")).unwrap();
        for c in cases {
            assert_eq!(is_included(&c.rel, &c.choices), c.included, "{}", c.why);
        }
    }

    use std::path::PathBuf;

    fn repo() -> (tempfile::TempDir, git2::Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        (dir, repo)
    }

    #[test]
    fn choices_round_trip_through_the_reference() {
        let (_d, repo) = repo();
        assert_eq!(read(&repo, PUBLIC_REF).unwrap(), vec![]);
        set(&repo, "", Some(true)).unwrap();
        set(&repo, "work", Some(false)).unwrap();
        set(&repo, "work/press release.md", Some(true)).unwrap();
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(is_included("a.md", &got));
        assert!(!is_included("work/memo.md", &got));
        assert!(is_included("work/press release.md", &got));
        set(&repo, "work", None).unwrap();
        assert!(is_included(
            "work/memo.md",
            &read(&repo, PUBLIC_REF).unwrap()
        ));
    }

    #[test]
    fn each_change_is_one_revision_on_top_of_the_last() {
        let (_d, repo) = repo();
        set(&repo, "a.md", Some(true)).unwrap();
        let first = repo.refname_to_id(PUBLIC_REF).unwrap();
        set(&repo, "b.md", Some(true)).unwrap();
        let second = repo
            .find_commit(repo.refname_to_id(PUBLIC_REF).unwrap())
            .unwrap();
        assert_eq!(second.parent_ids().collect::<Vec<_>>(), vec![first]);
        set(&repo, "b.md", Some(true)).unwrap(); // nothing changed
        assert_eq!(repo.refname_to_id(PUBLIC_REF).unwrap(), second.id());
    }

    #[test]
    fn a_write_based_on_a_stale_read_cannot_erase_a_newer_choice() {
        let (_d, repo) = repo();
        set(&repo, "a.md", Some(true)).unwrap();
        let (author, committer) = crate::git_engine::commit_identities(&repo).unwrap();
        let mut first = true;
        crate::git_engine::update_tree_on(
            &repo,
            PUBLIC_REF,
            "Publish choice",
            &author,
            &committer,
            |tree| {
                let mut choices = choices_in(&repo, tree)?;
                if first {
                    first = false;
                    // Someone else records a choice after this attempt has read the tip.
                    let mut theirs = choices.clone();
                    theirs.push(Choice {
                        path: "secret".into(),
                        include: false,
                    });
                    let blob = repo.blob(render(&theirs).as_bytes())?;
                    let t = crate::git_engine::tree_with_file(
                        &repo,
                        None,
                        Path::new(FILE),
                        Some(blob),
                    )?;
                    let tip = repo.find_commit(repo.refname_to_id(PUBLIC_REF)?)?;
                    let c = repo.commit(
                        None,
                        &author,
                        &committer,
                        "other",
                        &repo.find_tree(t)?,
                        &[&tip],
                    )?;
                    repo.reference(PUBLIC_REF, c, true, "other")?;
                }
                choices.push(Choice {
                    path: "b.md".into(),
                    include: true,
                });
                let blob = repo.blob(render(&choices).as_bytes())?;
                crate::git_engine::tree_with_file(&repo, None, Path::new(FILE), Some(blob))
                    .map(Some)
            },
        )
        .unwrap();
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(got.contains(&Choice {
            path: "secret".into(),
            include: false
        }));
        assert!(got.contains(&Choice {
            path: "b.md".into(),
            include: true
        }));
        assert!(got.contains(&Choice {
            path: "a.md".into(),
            include: true
        }));
    }

    #[test]
    fn choices_from_another_device_are_taken_in_and_disagreement_leaves_out() {
        let (_d, repo) = repo();
        set(&repo, "", Some(true)).unwrap();
        let base = repo.refname_to_id(PUBLIC_REF).unwrap();
        // The other device left "work" out and included "press.md"; this one included "work".
        set(&repo, "press.md", Some(true)).unwrap();
        set(&repo, "work", Some(false)).unwrap();
        let theirs = repo.refname_to_id(PUBLIC_REF).unwrap();
        repo.reference("refs/textree-remote/public", theirs, true, "t")
            .unwrap();
        repo.reference(PUBLIC_REF, base, true, "rewind").unwrap();
        set(&repo, "work", Some(true)).unwrap();
        let ours = repo.refname_to_id(PUBLIC_REF).unwrap();

        take_in(&repo).unwrap();
        let joined = repo
            .find_commit(repo.refname_to_id(PUBLIC_REF).unwrap())
            .unwrap();
        assert_eq!(joined.parent_ids().collect::<Vec<_>>(), vec![ours, theirs]);
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(is_included("press.md", &got));
        assert!(!is_included("work/a.md", &got));

        // Taking in again changes nothing: this side now contains the other.
        take_in(&repo).unwrap();
        assert_eq!(repo.refname_to_id(PUBLIC_REF).unwrap(), joined.id());
    }

    /// Records the other device's state from `base` as the tracking reference, then rewinds this
    /// side to `base` and applies `mine` to it.
    fn diverge(
        repo: &git2::Repository,
        theirs_change: impl Fn(&git2::Repository),
        mine_change: impl Fn(&git2::Repository),
    ) {
        let base = repo.refname_to_id(PUBLIC_REF).unwrap();
        theirs_change(repo);
        let theirs = repo.refname_to_id(PUBLIC_REF).unwrap();
        repo.reference("refs/textree-remote/public", theirs, true, "t")
            .unwrap();
        repo.reference(PUBLIC_REF, base, true, "rewind").unwrap();
        mine_change(repo);
        take_in(repo).unwrap();
    }

    #[test]
    fn a_choice_withdrawn_on_one_device_stays_withdrawn_after_joining() {
        let (_d, repo) = repo();
        set(&repo, "work", Some(true)).unwrap();
        diverge(
            &repo,
            |r| set(r, "other.md", Some(true)).unwrap(),
            |r| set(r, "work", None).unwrap(),
        );
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(!is_included("work/a.md", &got));
        assert!(is_included("other.md", &got));
    }

    #[test]
    fn a_move_on_one_device_is_not_undone_by_the_stale_choice_on_the_other() {
        let (_d, repo) = repo();
        set(&repo, "blog", Some(true)).unwrap();
        diverge(
            &repo,
            |r| set(r, "other.md", Some(true)).unwrap(),
            |r| follow_moves(r, &[(PathBuf::from("blog"), PathBuf::from("site/blog"))]).unwrap(),
        );
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(!is_included("blog/x.md", &got));
        assert!(is_included("site/blog/x.md", &got));
    }

    /// A revision of the recorded notes holding `files`, on top of `parent`.
    fn notes_at(repo: &git2::Repository, parent: Option<git2::Oid>, files: &[&str]) -> git2::Oid {
        let mut tree: Option<git2::Oid> = None;
        for f in files {
            let blob = repo
                .blob(format!("note {}", f.rsplit('/').next().unwrap()).as_bytes())
                .unwrap();
            let base = tree.map(|t| repo.find_tree(t).unwrap());
            tree = Some(
                crate::git_engine::tree_with_file(repo, base.as_ref(), Path::new(f), Some(blob))
                    .unwrap(),
            );
        }
        let tree = repo.find_tree(tree.unwrap()).unwrap();
        let (author, committer) = crate::git_engine::commit_identities(repo).unwrap();
        let parents: Vec<git2::Commit<'_>> = parent
            .map(|p| repo.find_commit(p).unwrap())
            .into_iter()
            .collect();
        let refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        repo.commit(None, &author, &committer, "notes", &tree, &refs)
            .unwrap()
    }

    /// The recorded notes as this side (`ours`) and the other device (`theirs`) hold them.
    fn notes_on_each_side(repo: &git2::Repository, ours: git2::Oid, theirs: git2::Oid) {
        repo.reference(crate::git_engine::NOTES_REF, ours, true, "ours")
            .unwrap();
        repo.reference(
            &crate::remote::tracking_of(crate::git_engine::NOTES_REF),
            theirs,
            true,
            "theirs",
        )
        .unwrap();
    }

    #[test]
    fn a_leave_out_made_on_the_other_device_follows_a_folder_moved_here() {
        let (_d, repo) = repo();
        let before = notes_at(&repo, None, &["blog/a.md", "blog/draft.md"]);
        let moved = notes_at(
            &repo,
            Some(before),
            &["site/blog/a.md", "site/blog/draft.md"],
        );
        notes_on_each_side(&repo, moved, before);
        set(&repo, "blog", Some(true)).unwrap();
        diverge(
            &repo,
            |r| set(r, "blog/draft.md", Some(false)).unwrap(),
            |r| follow_moves(r, &[(PathBuf::from("blog"), PathBuf::from("site/blog"))]).unwrap(),
        );
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(!is_included("site/blog/draft.md", &got), "{got:?}");
        assert!(is_included("site/blog/a.md", &got), "{got:?}");
        assert!(
            got.iter().all(|c| !covers("blog", &c.path)),
            "nothing left at the old place: {got:?}"
        );
    }

    #[test]
    fn a_leave_out_made_here_follows_a_folder_the_other_device_moved() {
        let (_d, repo) = repo();
        let before = notes_at(&repo, None, &["blog/a.md", "blog/draft.md"]);
        let moved = notes_at(
            &repo,
            Some(before),
            &["site/blog/a.md", "site/blog/draft.md"],
        );
        notes_on_each_side(&repo, before, moved);
        set(&repo, "blog", Some(true)).unwrap();
        diverge(
            &repo,
            |r| follow_moves(r, &[(PathBuf::from("blog"), PathBuf::from("site/blog"))]).unwrap(),
            |r| set(r, "blog/draft.md", Some(false)).unwrap(),
        );
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(!is_included("site/blog/draft.md", &got), "{got:?}");
        assert!(is_included("site/blog/a.md", &got), "{got:?}");
    }

    #[test]
    fn a_note_moved_out_of_a_folder_that_stays_does_not_carry_the_folder_choice() {
        let (_d, repo) = repo();
        let before = notes_at(&repo, None, &["blog/a.md", "blog/b.md"]);
        let moved = notes_at(&repo, Some(before), &["archive/a.md", "blog/b.md"]);
        notes_on_each_side(&repo, moved, before);
        set(&repo, "blog", Some(true)).unwrap();
        diverge(
            &repo,
            |r| set(r, "blog", Some(false)).unwrap(),
            |r| set(r, "other.md", Some(true)).unwrap(),
        );
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(!is_included("blog/b.md", &got), "{got:?}");
        assert!(got.iter().all(|c| c.path != "archive"), "{got:?}");
    }

    #[test]
    fn choices_with_no_common_start_leave_out_where_they_disagree() {
        let (_d, repo) = repo();
        let (author, committer) = crate::git_engine::commit_identities(&repo).unwrap();
        let root = |choices: Vec<Choice>| {
            let blob = repo.blob(render(&choices).as_bytes()).unwrap();
            let t = crate::git_engine::tree_with_file(&repo, None, Path::new(FILE), Some(blob))
                .unwrap();
            repo.commit(
                None,
                &author,
                &committer,
                "root",
                &repo.find_tree(t).unwrap(),
                &[],
            )
            .unwrap()
        };
        let theirs = root(vec![Choice {
            path: "work".into(),
            include: false,
        }]);
        let ours = root(vec![
            Choice {
                path: "work".into(),
                include: true,
            },
            Choice {
                path: "a.md".into(),
                include: true,
            },
        ]);
        repo.reference("refs/textree-remote/public", theirs, true, "t")
            .unwrap();
        repo.reference(PUBLIC_REF, ours, true, "o").unwrap();
        take_in(&repo).unwrap();
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(!is_included("work/a.md", &got));
        assert!(is_included("a.md", &got));
    }

    #[test]
    fn taking_in_adopts_moves_forward_and_ignores_a_missing_remote() {
        let (_d, repo) = repo();
        take_in(&repo).unwrap(); // no tracking ref: nothing happens
        assert!(repo.refname_to_id(PUBLIC_REF).is_err());
        set(&repo, "a.md", Some(true)).unwrap();
        let first = repo.refname_to_id(PUBLIC_REF).unwrap();
        set(&repo, "b.md", Some(true)).unwrap();
        let second = repo.refname_to_id(PUBLIC_REF).unwrap();
        repo.reference("refs/textree-remote/public", second, true, "t")
            .unwrap();
        repo.reference(PUBLIC_REF, first, true, "rewind").unwrap();
        take_in(&repo).unwrap(); // behind: fast-forward
        assert_eq!(repo.refname_to_id(PUBLIC_REF).unwrap(), second);
        repo.reference("refs/textree-remote/public", first, true, "t")
            .unwrap();
        take_in(&repo).unwrap(); // ahead: kept
        assert_eq!(repo.refname_to_id(PUBLIC_REF).unwrap(), second);
        repo.find_reference(PUBLIC_REF).unwrap().delete().unwrap();
        take_in(&repo).unwrap(); // none here: adopt
        assert_eq!(repo.refname_to_id(PUBLIC_REF).unwrap(), first);
    }

    #[test]
    fn a_move_carries_the_choice() {
        let (_d, repo) = repo();
        set(&repo, "blog", Some(true)).unwrap();
        set(&repo, "blog/draft.md", Some(false)).unwrap();
        set(&repo, "notes/a.md", Some(true)).unwrap();
        follow_moves(
            &repo,
            &[
                (PathBuf::from("blog"), PathBuf::from("site/blog")),
                (PathBuf::from("notes/a.md"), PathBuf::from("notes/b.md")),
            ],
        )
        .unwrap();
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(is_included("site/blog/x.md", &got));
        assert!(!is_included("site/blog/draft.md", &got));
        assert!(!is_included("blog/x.md", &got));
        assert!(is_included("notes/b.md", &got));
        assert!(!is_included("notes/a.md", &got));
    }

    #[test]
    fn gate_l2_8_a_moved_choice_wins_over_one_left_at_the_destination() {
        // Both alphabetical orders: the stale choice sorting before the moved one, and after.
        for (stale, moved) in [("public", "drafts"), ("alpha", "zeta")] {
            let (_d, repo) = repo();
            set(&repo, stale, Some(true)).unwrap();
            set(&repo, moved, Some(false)).unwrap();
            follow_moves(&repo, &[(PathBuf::from(moved), PathBuf::from(stale))]).unwrap();
            let got = read(&repo, PUBLIC_REF).unwrap();
            assert!(
                !is_included(&format!("{stale}/diary.md"), &got),
                "{moved} -> {stale}: {got:?}"
            );
            assert_eq!(
                got,
                vec![Choice {
                    path: stale.into(),
                    include: false
                }]
            );
        }
    }

    #[test]
    fn gate_l2_8_a_folder_moved_onto_stale_choices_below_carries_only_its_own() {
        let (_d, repo) = repo();
        set(&repo, "target/a.md", Some(true)).unwrap();
        set(&repo, "target/sub", Some(true)).unwrap();
        set(&repo, "targets/keep.md", Some(true)).unwrap();
        set(&repo, "src", Some(false)).unwrap();
        follow_moves(&repo, &[(PathBuf::from("src"), PathBuf::from("target"))]).unwrap();
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(!is_included("target/a.md", &got));
        assert!(!is_included("target/sub/x.md", &got));
        assert!(
            is_included("targets/keep.md", &got),
            "a name that only starts the same is not under it"
        );
    }

    #[test]
    fn a_rename_that_only_changes_case_keeps_the_choice() {
        let (_d, repo) = repo();
        set(&repo, "Blog", Some(true)).unwrap();
        set(&repo, "Blog/draft.md", Some(false)).unwrap();
        follow_moves(&repo, &[(PathBuf::from("Blog"), PathBuf::from("blog"))]).unwrap();
        let got = read(&repo, PUBLIC_REF).unwrap();
        assert!(is_included("blog/post.md", &got));
        assert!(!is_included("blog/draft.md", &got));
        assert_eq!(got.len(), 2, "{got:?}");
    }

    #[test]
    fn gate_l2_8_vacating_drops_includes_at_and_under_a_path_and_keeps_leave_outs() {
        let (_d, repo) = repo();
        set(&repo, "a", Some(true)).unwrap();
        set(&repo, "A/x.md", Some(true)).unwrap();
        set(&repo, "a/secret.md", Some(false)).unwrap();
        set(&repo, "ab", Some(true)).unwrap();
        vacate(&repo, &[PathBuf::from("a")]).unwrap();
        assert_eq!(
            read(&repo, PUBLIC_REF).unwrap(),
            vec![
                Choice {
                    path: "a/secret.md".into(),
                    include: false
                },
                Choice {
                    path: "ab".into(),
                    include: true
                }
            ]
        );
        // Only a leave-out there: nothing to drop, nothing written.
        let tip = repo.refname_to_id(PUBLIC_REF).unwrap();
        vacate(&repo, &[PathBuf::from("a/secret.md")]).unwrap();
        assert_eq!(repo.refname_to_id(PUBLIC_REF).unwrap(), tip);
    }

    #[test]
    fn gate_l2_8_vacating_nothing_writes_nothing() {
        let (_d, repo) = repo();
        vacate(&repo, &[PathBuf::from("a.md")]).unwrap();
        assert!(
            repo.refname_to_id(PUBLIC_REF).is_err(),
            "no choices yet: nothing is started"
        );
        set(&repo, "a.md", Some(true)).unwrap();
        let tip = repo.refname_to_id(PUBLIC_REF).unwrap();
        vacate(
            &repo,
            &[
                PathBuf::from("b.md"),
                PathBuf::from("a.md/x"),
                PathBuf::from(""),
            ],
        )
        .unwrap();
        assert_eq!(repo.refname_to_id(PUBLIC_REF).unwrap(), tip);
    }

    #[test]
    fn a_vault_inside_a_bigger_repository_sees_its_own_choices() {
        let choices = vec![
            Choice {
                path: "docs".into(),
                include: true,
            },
            Choice {
                path: "docs/internal".into(),
                include: false,
            },
            Choice {
                path: "src".into(),
                include: true,
            },
        ];
        assert_eq!(
            for_vault(&choices, "docs"),
            vec![
                Choice {
                    path: "".into(),
                    include: true
                },
                Choice {
                    path: "internal".into(),
                    include: false
                }
            ]
        );
        assert_eq!(for_vault(&choices, ""), choices);
    }
}
